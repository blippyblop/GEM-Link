//! Real NVENC video source for pcsim (Windows only).
//!
//! This is `nvenc_probe`'s synthetic "feeder" path packaged for the sim rig: a
//! standalone D3D11 device feeds NVENC textures directly, no desktop capture.
//! That deliberately decouples two risks — "is real encoded video flowing into
//! the sim" and "does DDA work on this box" — and it is the right shape anyway,
//! because the encoder must be fed **VR-resolution** frames, not the desktop's.
//!
//! The resolution is a parameter, never a constant: render resolution is
//! negotiable and must be allowed to change (ADR-0008 / the sim's contract).
//!
//! ## Benchmark source
//!
//! A pool of textures is filled **once** with distinct structured patterns and
//! then cycled, so consecutive frames actually differ and P-frames carry real
//! residual — an encoder fed a single flat frame reports absurdly small frames
//! and tells you nothing. The fill is one-time on purpose: per-frame CPU filling
//! at 2160x2160 (18.6 MB/frame) would dominate the frame budget and pollute the
//! very timings we want to measure. What this does NOT reproduce is true motion
//! or real scene complexity; it is a transport/encode benchmark source, and the
//! numbers should be read as such.

use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAP_WRITE, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::Interface;
use x_nvenc::NvEncoder;

/// How many distinct frames we cycle through. Enough that the encoder sees
/// genuine inter-frame difference, small enough to fill once cheaply.
const PATTERNS: usize = 8;
/// Side of the moving block drawn into each pattern, in pixels.
const BLOCK: u32 = 256;

pub struct Feeder {
    /// Kept alive: the encoder holds a reference to this device.
    _device: ID3D11Device,
    pool: Vec<ID3D11Texture2D>,
    encoder: NvEncoder,
    pitch: u32,
    idx: usize,
    encode_times: Vec<Duration>,
}

impl Feeder {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self, String> {
        let (device, context) = standalone_device()?;

        // BGRA8 for now: the Frame's 10-bit support is a known mystery (the
        // firmware suggests it can, but it does not appear to display it), so
        // stay 8-bit until everything else is done.
        let mut pool = Vec::with_capacity(PATTERNS);
        let staging = create_texture(&device, width, height, D3D11_USAGE_STAGING)?;
        for i in 0..PATTERNS {
            let tex = create_texture(&device, width, height, D3D11_USAGE_DEFAULT)?;
            fill_pattern(&context, &staging, &tex, width, height, i)?;
            pool.push(tex);
        }

        let encoder = NvEncoder::new(unsafe { device.as_raw() }, width, height, fps)
            .map_err(|e| format!("nvenc session: {e}"))?;

        Ok(Self {
            _device: device,
            pool,
            encoder,
            pitch: width * 4,
            idx: 0,
            encode_times: Vec::new(),
        })
    }

    /// Encode one frame and return its bitstream.
    pub fn encode_next(&mut self) -> Result<Vec<u8>, String> {
        let texture = &self.pool[self.idx % self.pool.len()];
        self.idx += 1;
        let t = Instant::now();
        let out = self
            .encoder
            .encode(texture.as_raw(), self.pitch)
            .map_err(|e| format!("nvenc encode: {e}"))?;
        self.encode_times.push(t.elapsed());
        Ok(out)
    }

    /// Encode time of the most recent frame, in ms.
    pub fn last_encode_ms(&self) -> f64 {
        self.encode_times
            .last()
            .map(|d| d.as_secs_f64() * 1e3)
            .unwrap_or(0.0)
    }

    /// (count, p50_ms, p99_ms, mean_ms) over the encode calls so far.
    pub fn encode_stats(&self) -> (usize, f64, f64, f64) {
        let mut ms: Vec<f64> = self.encode_times.iter().map(|d| d.as_secs_f64() * 1e3).collect();
        if ms.is_empty() {
            return (0, 0.0, 0.0, 0.0);
        }
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = ms.len();
        let mean = ms.iter().sum::<f64>() / n as f64;
        (n, ms[n / 2], ms[(n * 99 / 100).min(n - 1)], mean)
    }
}

fn standalone_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None,
            windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| format!("D3D11CreateDevice failed: {e}"))?;
        Ok((device.ok_or("no device")?, context.ok_or("no context")?))
    }
}

fn create_texture(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE,
) -> Result<ID3D11Texture2D, String> {
    let staging = usage == D3D11_USAGE_STAGING;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        // A staging texture cannot be a render target; a DEFAULT texture must be
        // one, or NVENC will not accept it as input.
        BindFlags: if staging {
            0
        } else {
            D3D11_BIND_RENDER_TARGET.0 as u32
        },
        CPUAccessFlags: if staging { D3D11_CPU_ACCESS_WRITE.0 as u32 } else { 0 },
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut tex))
            .map_err(|e| format!("CreateTexture2D failed: {e}"))?;
    }
    tex.ok_or_else(|| "CreateTexture2D returned no texture".to_string())
}

/// Write pattern `i` into `staging`, then copy it into the GPU-resident target.
fn fill_pattern(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    target: &ID3D11Texture2D,
    w: u32,
    h: u32,
    i: usize,
) -> Result<(), String> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context
            .Map(staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
            .map_err(|e| format!("Map failed: {e}"))?;

        let row_pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *mut u8;

        // A coarse gradient (so the encoder has low-frequency detail to spend
        // bits on) plus a block that walks with the frame index (so consecutive
        // frames genuinely differ).
        let bx = (i as u32 * 211) % w;
        let by = (i as u32 * 97) % h;
        for y in 0..h as usize {
            let row = base.add(y * row_pitch) as *mut u32;
            let g = ((y as u32 * 255) / h) & 0xff;
            let in_by = (y as u32) >= by && (y as u32) < by + BLOCK && by + BLOCK <= h;
            for x in 0..w as usize {
                let v = if in_by
                    && (x as u32) >= bx
                    && (x as u32) < bx + BLOCK
                    && bx + BLOCK <= w
                {
                    0x00ff_ffff // white block: a big residual for P-frames
                } else {
                    let b = ((x as u32 * 255) / w) & 0xff;
                    (g << 8) | b // green/blue gradient
                };
                *row.add(x) = v;
            }
        }

        context.Unmap(staging, 0);
        context.CopyResource(target, staging);
    }
    Ok(())
}
