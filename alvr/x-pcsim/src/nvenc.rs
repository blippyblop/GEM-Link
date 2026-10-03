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

use crate::foveation::Foveator;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_WRITE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_WRITE, D3D11_MAPPED_SUBRESOURCE,
    D3D11_RESOURCE_MISC_GENERATE_MIPS, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::Interface;
use x_dda::Duplicator;
use x_nvenc::NvEncoder;

/// How many distinct frames we cycle through. Enough that the encoder sees
/// genuine inter-frame difference, small enough to fill once cheaply.
const PATTERNS: usize = 8;
/// Side of the moving block drawn into each pattern, in pixels.
const BLOCK: u32 = 256;

/// PCSIM_FOV=0 disables the foveation pass, giving a clean A/B baseline. The
/// point of the toggle is to measure what foveation actually buys.
fn foveation_enabled() -> bool {
    std::env::var("PCSIM_FOV").map(|v| v != "0").unwrap_or(true)
}

pub struct Feeder {
    /// Kept alive: the encoder holds a reference to this device.
    _device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    pool: Vec<ID3D11Texture2D>,
    pool_srv: Vec<ID3D11ShaderResourceView>,
    fove: Foveator,
    encoder: NvEncoder,
    pitch: u32,
    idx: usize,
    foveation: bool,
    encode_times: Vec<Duration>,
}

impl Feeder {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self, String> {
        let (device, context) = standalone_device()?;

        // BGRA8 for now: the Frame's 10-bit support is a known mystery (the
        // firmware suggests it can, but it does not appear to display it), so
        // stay 8-bit until everything else is done.
        let mut pool = Vec::with_capacity(PATTERNS);
        let mut pool_srv = Vec::with_capacity(PATTERNS);
        let staging = create_texture(&device, width, height, D3D11_USAGE_STAGING)?;
        for i in 0..PATTERNS {
            let tex = create_pool_texture(&device, width, height)?;
            fill_pattern(&context, &staging, &tex, width, height, i)?;
            let srv = make_srv(&device, &tex)?;
            // Content is static here, so the chain is built once.
            unsafe { context.GenerateMips(&srv) };
            pool.push(tex);
            pool_srv.push(srv);
        }

        let encoder = NvEncoder::new(device.as_raw(), width, height, fps)
            .map_err(|e| format!("nvenc session: {e}"))?;

        let fove = Foveator::new(&device, &context, width, height)?;

        Ok(Self {
            _device: device,
            ctx: context,
            pool,
            pool_srv,
            fove,
            encoder,
            pitch: width * 4,
            idx: 0,
            foveation: foveation_enabled(),
            encode_times: Vec::new(),
        })
    }

    /// Encode one frame and return its bitstream.
    pub fn encode_next(&mut self, center: [f32; 2], center_size: f32) -> Result<Vec<u8>, String> {
        let slot = self.idx % self.pool.len();
        self.idx += 1;
        // Degrade the periphery BEFORE the encoder sees it -- this is where the
        // bitrate saving comes from; the centres alone save nothing.
        let src = if self.foveation {
            self.fove.apply(&self.pool_srv[slot], center, center_size)?;
            self.fove.out.as_raw()
        } else {
            self.pool[slot].as_raw()
        };
        let t = Instant::now();
        let out = self
            .encoder
            .encode(src, self.pitch)
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
        let mut ms: Vec<f64> = self
            .encode_times
            .iter()
            .map(|d| d.as_secs_f64() * 1e3)
            .collect();
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

/// Pool texture: mips (for real peripheral downsampling) plus an SRV so the
/// foveation pass can read it.
fn create_pool_texture(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 0, // full chain, for mip-based peripheral downsampling
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut tex))
            .map_err(|e| format!("CreateTexture2D(pool) failed: {e}"))?;
    }
    tex.ok_or_else(|| "CreateTexture2D(pool) returned no texture".to_string())
}

fn make_srv(
    device: &ID3D11Device,
    t: &ID3D11Texture2D,
) -> Result<ID3D11ShaderResourceView, String> {
    let mut srv: Option<ID3D11ShaderResourceView> = None;
    unsafe {
        device
            .CreateShaderResourceView(t, None, Some(&mut srv))
            .map_err(|e| format!("CreateShaderResourceView: {e}"))?;
    }
    srv.ok_or_else(|| "no srv".to_string())
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
        CPUAccessFlags: if staging {
            D3D11_CPU_ACCESS_WRITE.0 as u32
        } else {
            0
        },
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
                let v = if in_by && (x as u32) >= bx && (x as u32) < bx + BLOCK && bx + BLOCK <= w {
                    0x00ff_ffff // white block: a big residual for P-frames
                } else {
                    let b = ((x as u32 * 255) / w) & 0xff;
                    (g << 8) | b // green/blue gradient
                };
                *row.add(x) = v;
            }
        }

        context.Unmap(staging, 0);
        // CopySubresourceRegion, NOT CopyResource: CopyResource requires source
        // and destination to agree on mip count, and the pool now carries a full
        // mip chain while the staging texture has one. The silent failure left
        // every texture empty and the encoder producing 385-byte frames.
        // windows-rs unpacks the destination coordinates (dstx/dsty/dstz) rather
        // than taking a D3D11_BOX; passing a box here does not compile.
        context.CopySubresourceRegion(target, 0, 0, 0, 0, staging, 0, None);
    }
    Ok(())
}

/// Desktop Duplication source — real screen content, zero CPU readback.
///
/// Follows the commercial streamer's capture discipline (`VD_RE/05` §2, mined
/// from the RE), because naive DDA gets these wrong:
///  - **20 ms wait-bounded poll** (`TryAcquireNextFrame`), not a spin.
///  - **Release immediately after the GPU copy** — holding DDA ownership
///    throttles the system compositor and is the #1 cause of "streaming makes my
///    PC stutter".
///  - `Ok(None)` means the desktop did not update within the poll; we re-encode
///    the previous texture so the stream keeps its cadence. Note that a static
///    desktop therefore produces near-empty P-frames: **an idle screen is not a
///    valid bitrate sample** (`VD_RE/23` §8.1).
///
/// No scaling is done here: capture arrives at the desktop's resolution, so the
/// negotiated resolution is taken FROM the desktop. The architecturally correct
/// VR-resolution answer is a virtual display (VD's `IVirtualMonitor`, our
/// Phase 2 `x-idd`), not a scale step here.
pub struct DdaFeeder {
    dup: Duplicator,
    ctx: ID3D11DeviceContext,
    pool: Vec<ID3D11Texture2D>,
    pool_srv: Vec<ID3D11ShaderResourceView>,
    fove: Foveator,
    encoder: NvEncoder,
    pitch: u32,
    idx: usize,
    foveation: bool,
    encode_times: Vec<Duration>,
    empty_polls: u64,
    frames: u64,
}

impl DdaFeeder {
    /// Returns the feeder and the desktop resolution it captures at.
    pub fn new(adapter_idx: u32, output_idx: u32, fps: u32) -> Result<(Self, u32, u32), String> {
        let dup = Duplicator::new(adapter_idx, output_idx, &[DXGI_FORMAT_B8G8R8A8_UNORM])
            .map_err(|e| format!("duplicator: {e}"))?;
        let desktop = dup
            .desktop_desc()
            .map_err(|e| format!("desktop_desc: {e}"))?;
        let (w, h) = (desktop.width, desktop.height);

        // Two slots, same as the pattern feeder: NVENC reads one while the next
        // desktop frame is copied into the other. Mips so the foveation pass can
        // genuinely downsample the periphery.
        let mut pool = Vec::new();
        let mut pool_srv = Vec::new();
        for _ in 0..2 {
            let t = create_pool_texture(dup.device(), w, h)?;
            pool_srv.push(make_srv(dup.device(), &t)?);
            pool.push(t);
        }
        let ctx = dup
            .context()
            .ok_or("duplicator has no device context")?
            .clone();
        let fove = Foveator::new(dup.device(), &ctx, w, h)?;
        let encoder = NvEncoder::new(dup.device_ptr(), w, h, fps)
            .map_err(|e| format!("nvenc session: {e}"))?;

        Ok((
            Self {
                dup,
                ctx,
                pool,
                pool_srv,
                fove,
                encoder,
                pitch: w * 4,
                idx: 0,
                foveation: foveation_enabled(),
                encode_times: Vec::new(),
                empty_polls: 0,
                frames: 0,
            },
            w,
            h,
        ))
    }

    pub fn encode_next(&mut self, center: [f32; 2], center_size: f32) -> Result<Vec<u8>, String> {
        let slot = self.idx % self.pool.len();

        // Short wait-bounded poll, NOT VD's 20 ms.
        //
        // VD polls at 20 ms because the desktop's own composition is its clock:
        // it only encodes when LastPresentTime changes, and the frame rate
        // follows the desktop. Our harness is the opposite -- we pace at a
        // fixed 90 Hz and the desktop is just a content source. A 20 ms block
        // therefore exceeds the 11.1 ms frame interval, so on a mostly-static
        // desktop (where the poll times out) every frame takes >= 20 ms, the
        // cadence collapses, and the client dies with "Connection error: Try
        // again" (a receive timeout). Keep the poll well inside the budget and
        // re-encode the previous texture when there is nothing new.
        let frame = match unsafe { self.dup.acquire(1) } {
            Ok(Some(f)) => Some(f),
            Ok(None) => {
                self.empty_polls += 1;
                None
            }
            Err(e) => return Err(format!("acquire: {e}")),
        };

        if let Some(f) = frame.as_ref() {
            // GPU-side copy, then release IMMEDIATELY (before encoding).
            if let Some(ctx) = self.dup.context() {
                unsafe { ctx.CopyResource(&self.pool[slot], &f.texture) };
            }
            self.dup.release();
        }

        let src = if self.foveation {
            self.fove.apply(&self.pool_srv[slot], center, center_size)?;
            self.fove.out.as_raw()
        } else {
            self.pool[slot].as_raw()
        };

        let t = Instant::now();
        let out = self
            .encoder
            .encode(src, self.pitch)
            .map_err(|e| format!("nvenc encode: {e}"))?;
        self.encode_times.push(t.elapsed());
        self.idx += 1;
        self.frames += 1;
        Ok(out)
    }

    pub fn last_encode_ms(&self) -> f64 {
        self.encode_times
            .last()
            .map(|d| d.as_secs_f64() * 1e3)
            .unwrap_or(0.0)
    }

    pub fn encode_stats(&self) -> (usize, f64, f64, f64) {
        let mut ms: Vec<f64> = self
            .encode_times
            .iter()
            .map(|d| d.as_secs_f64() * 1e3)
            .collect();
        if ms.is_empty() {
            return (0, 0.0, 0.0, 0.0);
        }
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = ms.len();
        let mean = ms.iter().sum::<f64>() / n as f64;
        (n, ms[n / 2], ms[(n * 99 / 100).min(n - 1)], mean)
    }

    /// (desktop frames observed, empty polls) — a static desktop shows up here.
    pub fn capture_stats(&self) -> (u64, u64) {
        (self.frames, self.empty_polls)
    }
}
