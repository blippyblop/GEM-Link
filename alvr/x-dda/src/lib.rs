//! x-dda — GemLink's zero-copy desktop capture (Phase 1 of the roadmap).
//!
//! Design rules (the latency playbook, applied):
//! 1. **Frames never touch the CPU.** The duplicated desktop texture is handed
//!    to the caller as a D3D11 texture and released immediately; nothing in
//!    this crate ever maps a texture to system memory.
//! 2. **The texture is hot for exactly as long as needed.** Acquire → (caller
//!    registers/uses the texture) → ReleaseFrame, within the same poll tick.
//! 3. **HDR-capable source.** `DuplicateOutput1` with an SDR→HDR format list
//!    (FP16 → R10G10B10A2 → BGRA8), falling back to plain duplication.
//! 4. **Hygiene from the first frame:** realtime process priority and 1 ms
//!    timer resolution are one call each, because they must exist before any
//!    capture host is benchmarked.
//!
//! This crate is inherently `unsafe`-heavy (COM/FFI surface) — that is the
//! reason it does not carry the `#![forbid(unsafe_code)]` banner the pure
//! crates do. All unsafe is confined to this file.

use serde::{Deserialize, Serialize};
use windows::{
    Win32::Foundation::{E_FAIL, HMODULE},
    Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
        ID3D11DeviceContext, ID3D11Texture2D,
    },
    Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM,
        DXGI_FORMAT_R16G16B16A16_FLOAT,
    },
    Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC,
        DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
        IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
    },
    Win32::Media::timeBeginPeriod,
    Win32::System::Performance::QueryPerformanceFrequency,
    Win32::System::Threading::{
        GetCurrentProcess, HIGH_PRIORITY_CLASS, REALTIME_PRIORITY_CLASS, SetPriorityClass,
    },
    core::*,
};

/// Source pixel formats we accept from the desktop, in preference order
/// (HDR first). Format list mirrors the capture design in the roadmap.
pub const SOURCE_FORMATS: [DXGI_FORMAT; 3] = [
    DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_B8G8R8A8_UNORM,
];

/// SDR-only list (no FP16): diagnoses whether the OS refuses the HDR-capable
/// list merely because the desktop is in SDR mode.
pub const SDR_FORMATS: [DXGI_FORMAT; 2] =
    [DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM];

/// Per-frame metadata sidecar — travels WITH the texture (same timestamps,
/// never a second copy of pixels). This is the substrate later phases use for
/// client-side synthesis and hot reconfiguration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameMeta {
    /// QPC timestamp of the composition that produced this frame (ms).
    pub last_present_ms: f64,
    /// QPC timestamp of the last cursor update (ms).
    pub last_mouse_ms: f64,
    /// Number of desktop updates coalesced into this frame.
    pub accumulated_frames: u32,
    /// The frame contained protected content and was masked out by the OS.
    pub protected_content_masked: bool,
    /// Desktop rectangles were coalesced for this frame.
    pub rects_coalesced: bool,
}

impl FrameMeta {
    fn from_info(info: &DXGI_OUTDUPL_FRAME_INFO, qpc_to_ms: f64) -> Self {
        Self {
            last_present_ms: info.LastPresentTime as f64 * qpc_to_ms,
            last_mouse_ms: info.LastMouseUpdateTime as f64 * qpc_to_ms,
            accumulated_frames: info.AccumulatedFrames,
            protected_content_masked: info.ProtectedContentMaskedOut.as_bool(),
            rects_coalesced: info.RectsCoalesced.as_bool(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureStats {
    pub seconds: f64,
    pub frames: u32,
    pub empty_polls: u32,
    pub fps: f64,
    /// Mean time from AcquireNextFrame returning to ReleaseFrame (ms) — the
    /// "texture hot" window. With a zero-copy pipeline this stays in the
    /// tens-of-microseconds range.
    pub mean_turnaround_ms: f64,
    pub max_turnaround_ms: f64,
    /// Mean gap between successive desktop compositions (ms) — the source's
    /// own cadence, measured, not assumed.
    pub mean_present_gap_ms: f64,
    pub desktop: DesktopDesc,
    pub realtime_priority: bool,
    pub protected_events: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DesktopDesc {
    pub width: u32,
    pub height: u32,
    pub format: i32,
    pub high_res_desktop: bool,
}

fn qpc_to_ms() -> f64 {
    unsafe {
        let mut freq = 0i64;
        let _ = QueryPerformanceFrequency(&mut freq);
        1000.0 / freq.max(1) as f64
    }
}

/// Process/thread hygiene for any capture host. Returns whether realtime was
/// actually granted (it needs an elevated token; HIGH is the graceful
/// fallback and is still far above default scheduling).
pub fn apply_scheduling_hygiene() -> bool {
    unsafe {
        let _ = timeBeginPeriod(1);
        if SetPriorityClass(GetCurrentProcess(), REALTIME_PRIORITY_CLASS).is_err() {
            let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
            return false;
        }
        true
    }
}

pub struct Duplicator {
    #[allow(dead_code)]
    device: ID3D11Device,
    #[allow(dead_code)]
    context: Option<ID3D11DeviceContext>,
    duplication: IDXGIOutputDuplication,
    qpc_ms: f64,
    qpc_freq: i64,
}

pub struct AcquiredFrame {
    pub texture: ID3D11Texture2D,
    pub meta: FrameMeta,
}

impl Duplicator {
    /// Enumerate (adapter, output) pairs with their display device names.
    pub fn enumerate() -> Result<Vec<(u32, u32, String)>> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut out = Vec::new();
            for ai in 0.. {
                let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(ai) {
                    Ok(a) => a,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(e),
                };
                for oi in 0.. {
                    let output: IDXGIOutput = match adapter.EnumOutputs(oi) {
                        Ok(o) => o,
                        Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                        Err(e) => return Err(e),
                    };
                    let desc = output.GetDesc()?;
                    let name = String::from_utf16_lossy(
                        &desc
                            .DeviceName
                            .iter()
                            .copied()
                            .take_while(|c: &u16| *c != 0)
                            .collect::<Vec<u16>>(),
                    );
                    out.push((ai, oi, name));
                }
            }
            Ok(out)
        }
    }

    /// Open a duplicator for one output. Prefers `DuplicateOutput1` with the
    /// HDR-capable format list; falls back to legacy duplication when the OS
    /// is too old.
    pub fn new(adapter_idx: u32, output_idx: u32, formats: &[DXGI_FORMAT]) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let adapter: IDXGIAdapter1 = factory.EnumAdapters1(adapter_idx)?;
            let output: IDXGIOutput = adapter.EnumOutputs(output_idx)?;

            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.ok_or(Error::from(E_FAIL))?;

            // HDR-capable path first; legacy duplication as the graceful
            // fallback (with the reason, so field diagnosis stays possible).
            // (windows-rs 0.58 parameter order: ppdevice, pfeaturelevel,
            // ppimmediatecontext — the context comes LAST)
            let duplication: IDXGIOutputDuplication = match output.cast::<IDXGIOutput5>() {
                Ok(output5) => match output5.DuplicateOutput1(&device, 0, formats) {
                    Ok(d) => d,
                    Err(hdr_err) => {
                        eprintln!(
                            "DuplicateOutput1 unavailable (0x{:08X}) - legacy fallback",
                            hdr_err.code().0
                        );
                        let output1: IDXGIOutput1 = output.cast()?;
                        output1.DuplicateOutput(&device)?
                    }
                },
                Err(_) => {
                    let output1: IDXGIOutput1 = output.cast()?;
                    output1.DuplicateOutput(&device)?
                }
            };

            let mut freq = 0i64;
            let _ = QueryPerformanceFrequency(&mut freq);

            Ok(Self {
                device,
                context,
                duplication,
                qpc_ms: 1000.0 / freq.max(1) as f64,
                qpc_freq: freq,
            })
        }
    }

    pub fn desc(&self) -> Result<DXGI_OUTDUPL_DESC> {
        Ok(unsafe { self.duplication.GetDesc() })
    }

    pub fn desktop_desc(&self) -> Result<DesktopDesc> {
        let d = self.desc()?;
        Ok(DesktopDesc {
            width: d.ModeDesc.Width,
            height: d.ModeDesc.Height,
            format: d.ModeDesc.Format.0,
            high_res_desktop: d.DesktopImageInSystemMemory.as_bool(),
        })
    }

    /// Acquire the next desktop frame (zero-copy: the returned texture lives
    /// in GPU memory). `Ok(None)` = no new composition within the timeout.
    ///
    /// You MUST call [`Duplicator::release`] before the next acquire.
    pub unsafe fn acquire(&self, timeout_ms: u32) -> Result<Option<AcquiredFrame>> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let acquired = unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        };
        match acquired {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) => return Err(e),
        }
        let texture: ID3D11Texture2D = resource
            .expect("AcquireNextFrame succeeded without a resource")
            .cast()?;
        Ok(Some(AcquiredFrame {
            texture,
            meta: FrameMeta::from_info(&info, self.qpc_ms),
        }))
    }

    /// Release the frame back to the OS. Call promptly — the "texture hot"
    /// window is the latency budget of the whole zero-copy pipeline.
    pub fn release(&self) {
        unsafe {
            let _ = self.duplication.ReleaseFrame();
        }
    }

    pub fn qpc_freq(&self) -> i64 {
        self.qpc_freq
    }
}

/// Run a timed capture session: poll at 20 ms (the latency playbook's poll
/// cadence), acquire-release immediately, and measure everything.
pub fn capture_session(
    adapter_idx: u32,
    output_idx: u32,
    seconds: f64,
    timeout_ms: u32,
    formats: &[DXGI_FORMAT],
) -> Result<CaptureStats> {
    let realtime = apply_scheduling_hygiene();
    let dup = Duplicator::new(adapter_idx, output_idx, formats)?;
    let desktop = dup.desktop_desc()?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(seconds);
    let mut frames = 0u32;
    let mut empty_polls = 0u32;
    let mut protected_events = 0u32;
    let mut turnarounds: Vec<f64> = Vec::new();
    let mut present_gaps: Vec<f64> = Vec::new();
    let mut last_present: Option<f64> = None;

    while std::time::Instant::now() < deadline {
        let t_acquire = std::time::Instant::now();
        let frame = unsafe { dup.acquire(timeout_ms) }?;
        match frame {
            Some(f) => {
                let turnaround = t_acquire.elapsed().as_secs_f64() * 1000.0;
                turnarounds.push(turnaround);
                if f.meta.protected_content_masked {
                    protected_events += 1;
                }
                // LastPresentTime == 0 means "no new composition" in some
                // driver states - skipping it keeps present-gap honest.
                if f.meta.last_present_ms > 0.0 {
                    if let Some(prev) = last_present {
                        if f.meta.last_present_ms > prev {
                            present_gaps.push(f.meta.last_present_ms - prev);
                        }
                    }
                    last_present = Some(f.meta.last_present_ms);
                }
                frames += 1;
                dup.release();
            }
            None => empty_polls += 1,
        }
    }

    let elapsed = std::time::Instant::now()
        .duration_since(deadline - std::time::Duration::from_secs_f64(seconds))
        .as_secs_f64();
    let mean = |v: &[f64]| -> f64 {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };

    Ok(CaptureStats {
        seconds: elapsed,
        frames,
        empty_polls,
        fps: frames as f64 / elapsed.max(0.001),
        mean_turnaround_ms: mean(&turnarounds),
        max_turnaround_ms: turnarounds.iter().cloned().fold(0.0, f64::max),
        mean_present_gap_ms: mean(&present_gaps),
        desktop,
        realtime_priority: realtime,
        protected_events,
    })
}
