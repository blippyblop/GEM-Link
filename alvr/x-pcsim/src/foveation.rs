//! GPU foveation pass (Windows / D3D11).
//!
//! The per-frame centres tell the *client* where the high-density region was;
//! they do not save a single bit on their own. The saving comes from degrading
//! the periphery BEFORE the encoder sees it, which is what VD does (doc 03:
//! "the PC encodes foveated frames, centre-sharp, periphery-blurred to save
//! bitrate") and what ALVR gets from the compositor rendering the eyes foveated.
//!
//! Our sim has no compositor, so we do it here: a full-screen pass samples the
//! source at a mip level that ramps with distance from the foveation centre.
//! Mip sampling is a real downsample rather than a fake blur, so the encoder
//! genuinely has less high-frequency detail to spend bits on in the periphery.
//!
//! Cost is one full-screen pass plus `GenerateMips` -- small next to NVENC, and
//! measured separately so it can never hide inside the encode number.

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::*;
// windows-rs 0.58 hosts the d3dcompiler bindings (D3DCompile) under Fxc.
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

const SHADER: &str = r#"
Texture2D    src  : register(t0);
SamplerState samp : register(s0);

cbuffer Params : register(b0)
{
    float2 center_px;    // aligned foveation centre, pixels
    float2 view_size;    // frame size, pixels
    float2 center_half;  // half-extent of the sharp region, pixels
    float2 feather;      // feather width outside the sharp region, pixels
    float  max_lod;      // mip level at full periphery
    float3 pad;
};

struct VSOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };

// Full-screen triangle straight from SV_VertexID: no vertex buffer needed.
VSOut vs_main(uint id : SV_VertexID)
{
    VSOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.uv = uv;
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

// Peripheral degradation by genuine mip downsampling.
//
// NOT a few-tap blur: a sparse box blur produces discrete ghost copies, which
// ADDS high-frequency structure and made the encoder emit ~20% MORE data. A mip
// level is a real resolution reduction, so the periphery loses detail the
// encoder genuinely cannot spend bits on -- which is the whole point.
float4 ps_main(VSOut i) : SV_TARGET
{
    float2 px = i.uv * view_size;
    float2 d  = abs(px - center_px) - center_half;
    float  de = max(d.x, d.y);          // <= 0 inside the sharp region
    float  t  = saturate(de / feather); // 0 sharp -> 1 full periphery
    return src.SampleLevel(samp, i.uv, t * max_lod);
}
"#;

pub struct Foveator {
    ctx: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    params: ID3D11Buffer,
    /// Destination the encoder reads.
    pub out: ID3D11Texture2D,
    out_rtv: ID3D11RenderTargetView,
    width: f32,
    height: f32,
    max_lod: f32,
}

impl Foveator {
    pub fn new(
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        unsafe {
            let compile = |entry: &str, target: &str| -> Result<Vec<u8>, String> {
                let mut code = None;
                let mut errs = None;
                D3DCompile(
                    SHADER.as_ptr() as *const _,
                    SHADER.len(),
                    None,
                    None,
                    None,
                    windows::core::PCSTR(entry.as_ptr()),
                    windows::core::PCSTR(target.as_ptr()),
                    0,
                    0,
                    &mut code,
                    Some(&mut errs),
                )
                .map_err(|e| {
                    let msg = errs
                        .as_ref()
                        .map(|b| {
                            let s = b.GetBufferPointer() as *const u8;
                            let n = b.GetBufferSize();
                            String::from_utf8_lossy(std::slice::from_raw_parts(s, n)).to_string()
                        })
                        .unwrap_or_default();
                    format!("foveation shader {entry}: {e} {msg}")
                })?;
                let blob = code.ok_or_else(|| format!("no bytecode for {entry}"))?;
                let bytes = std::slice::from_raw_parts(
                    blob.GetBufferPointer() as *const u8,
                    blob.GetBufferSize(),
                )
                .to_vec();
                Ok(bytes)
            };

            let vs_bytes = compile("vs_main\0", "vs_5_0\0")?;
            let ps_bytes = compile("ps_main\0", "ps_5_0\0")?;

            let mut vs = None;
            device
                .CreateVertexShader(&vs_bytes, None, Some(&mut vs))
                .map_err(|e| format!("CreateVertexShader: {e}"))?;
            let mut ps = None;
            device
                .CreatePixelShader(&ps_bytes, None, Some(&mut ps))
                .map_err(|e| format!("CreatePixelShader: {e}"))?;

            let mut sampler: Option<ID3D11SamplerState> = None;
            device
                .CreateSamplerState(&D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    ComparisonFunc: D3D11_COMPARISON_NEVER,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                }, Some(&mut sampler))
                .map_err(|e| format!("CreateSamplerState: {e}"))?;
            let sampler = sampler.ok_or("no sampler")?;

            let mut params: Option<ID3D11Buffer> = None;
            device
                .CreateBuffer(&D3D11_BUFFER_DESC {
                    ByteWidth: 48, // 2+2+2+2 floats + float + pad
                    Usage: D3D11_USAGE_DYNAMIC,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                    ..Default::default()
                }, None, Some(&mut params))
                .map_err(|e| format!("CreateBuffer: {e}"))?;
            let params = params.ok_or("no cbuffer")?;

            let mut out: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: width,
                        Height: height,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut out),
                )
                .map_err(|e| format!("foveation target: {e}"))?;
            let out = out.ok_or("no foveation target")?;
            let mut out_rtv = None;
            device
                .CreateRenderTargetView(&out, None, Some(&mut out_rtv))
                .map_err(|e| format!("CreateRenderTargetView: {e}"))?;

            // Mip level at full periphery: ~1/8 resolution, which is the regime
            // foveated streaming actually operates in.
            let max_lod = 3.0;

            Ok(Self {
                ctx: ctx.clone(),
                // Stored as Options only to satisfy windows-rs; they are always Some here.
                vs: vs.ok_or("no vs")?,
                ps: ps.ok_or("no ps")?,
                sampler,
                params,
                out,
                out_rtv: out_rtv.ok_or("no rtv")?,
                width: width as f32,
                height: height as f32,
                max_lod,
            })
        }
    }

    /// Render `src` into the foveated target. `center_shift` is the aligned
    /// centre in ALVR's units (0 = middle, +/-1 = view edge); `center_size` is
    /// the fraction of each axis kept sharp.
    pub fn apply(
        &self,
        src: &ID3D11ShaderResourceView,
        center_shift: [f32; 2],
        center_size: f32,
    ) -> Result<(), String> {
        unsafe {
            // ALVR's shift is normalised so that +/-1 touches the view edge on
            // the movable axis; convert to pixels about the centre.
            let half = self.width.min(self.height) * 0.5;
            let center_px = [
                self.width * 0.5 + center_shift[0] * (half * 0.5),
                self.height * 0.5 + center_shift[1] * (half * 0.5),
            ];
            let center_half = [
                self.width * center_size * 0.5,
                self.height * center_size * 0.5,
            ];
            let feather = [
                (self.width * 0.5 - center_half[0]).max(1.0),
                (self.height * 0.5 - center_half[1]).max(1.0),
            ];

            let mut data = [0f32; 12];
            data[0..2].copy_from_slice(&center_px);
            data[2..4].copy_from_slice(&[self.width, self.height]);
            data[4..6].copy_from_slice(&center_half);
            data[6..8].copy_from_slice(&feather);
            data[8] = self.max_lod;

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&self.params, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .map_err(|e| format!("map params: {e}"))?;
            std::ptr::copy_nonoverlapping(
                data.as_ptr() as *const u8,
                mapped.pData as *mut u8,
                std::mem::size_of_val(&data),
            );
            self.ctx.Unmap(&self.params, 0);

            // D3D11 has NO default viewport: without this the pass rasterizes
            // nothing and the target keeps its initial (black) contents, which
            // showed up as the encoder emitting 385-byte "empty" frames.
            self.ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.width,
                Height: self.height,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            self.ctx.OMSetRenderTargets(Some(&[Some(self.out_rtv.clone())]), None);
            self.ctx.IASetPrimitiveTopology(
                windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            );
            self.ctx.VSSetShader(&self.vs, None);
            self.ctx.PSSetShader(&self.ps, None);
            self.ctx.PSSetShaderResources(0, Some(&[Some(src.clone())]));
            self.ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.ctx.PSSetConstantBuffers(0, Some(&[Some(self.params.clone())]));
            self.ctx.Draw(3, 0);
            // Unbind so the target is not still bound as input next frame.
            self.ctx.PSSetShaderResources(0, Some(&[None]));
        }
        Ok(())
    }


}
