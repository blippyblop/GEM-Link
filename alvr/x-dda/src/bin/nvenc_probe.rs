//! `nvenc-probe` — the Phase 1 milestone probe: desktop duplication → GPU pool
//! texture → NVENC encode, all GPU-resident. Reports per-frame encode latency
//! against the two-tier delivery budgets (90 Hz mandatory / 120 Hz optimal).

#![allow(unsafe_code)]

use std::process::ExitCode;
use std::time::{Duration, Instant};
use windows::core::Interface;
use x_dda::{Duplicator, SOURCE_FORMATS};
use x_nvenc::NvEncoder;

const MANDATORY_MS: f64 = 1000.0 / 90.0;
const OPTIMAL_MS: f64 = 1000.0 / 120.0;

fn real_main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut seconds = 10.0f64;
    let mut output = 0u32;
    let mut fps = 120u32;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seconds" => {
                seconds = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(10.0);
                i += 2;
            }
            "--output" => {
                output = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(0);
                i += 2;
            }
            "--fps" => {
                fps = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(120);
                i += 2;
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }

    let realtime = x_dda::apply_scheduling_hygiene();
    let dup = Duplicator::new(0, output, &SOURCE_FORMATS)
        .map_err(|e| format!("duplicator failed: {e}"))?;
    let desktop = dup
        .desktop_desc()
        .map_err(|e| format!("desc failed: {e}"))?;
    let (w, h) = (desktop.width, desktop.height);
    println!(
        "desktop {}x{} format {} — realtime={}",
        w, h, desktop.format, realtime
    );

    let mut encoder = NvEncoder::new(dup.device_ptr(), w, h, fps)
        .map_err(|e| format!("nvenc session failed: {e}"))?;

    // Pool of 2 rotating BGRA8 textures (encode frame N while frame N+1 lands).
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut pool: Vec<ID3D11Texture2D> = Vec::new();
    for _ in 0..2 {
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe {
            dup.device()
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .map_err(|e| format!("CreateTexture2D failed: {e}"))?;
        }
        pool.push(tex.ok_or("CreateTexture2D returned no texture")?);
    }
    let pitch = w * 4;

    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut encode_times: Vec<f64> = Vec::new();
    let mut pipeline_times: Vec<f64> = Vec::new();
    let mut bitstream_bytes: Vec<f64> = Vec::new();
    let mut frames = 0u32;
    let mut empty_polls = 0u32;
    let mut pool_idx = 0usize;

    while Instant::now() < deadline {
        let t_acquire = Instant::now();
        let frame = match unsafe { dup.acquire(20) } {
            Ok(Some(f)) => f,
            Ok(None) => {
                empty_polls += 1;
                continue;
            }
            Err(e) => return Err(format!("acquire failed: {e}")),
        };
        unsafe {
            dup.copy_resource(&pool[pool_idx], &frame.texture);
        }
        dup.release();

        let t_encode_start = Instant::now();
        let bytes = encoder
            .encode(unsafe { pool[pool_idx].as_raw() }, pitch)
            .map_err(|e| format!("encode failed: {e}"))?;
        let encode_ms = t_encode_start.elapsed().as_secs_f64() * 1000.0;
        let pipeline_ms = t_acquire.elapsed().as_secs_f64() * 1000.0;

        encode_times.push(encode_ms);
        pipeline_times.push(pipeline_ms);
        bitstream_bytes.push(bytes.len() as f64);
        frames += 1;
        pool_idx = (pool_idx + 1) % pool.len();
    }

    let stats = |mut v: Vec<f64>| -> (f64, f64, f64) {
        v.sort_by(|a, b| a.total_cmp(b));
        let n = v.len();
        let mean = if n == 0 {
            0.0
        } else {
            v.iter().sum::<f64>() / n as f64
        };
        let p95 = if n == 0 {
            0.0
        } else {
            v[((0.95 * n as f64).ceil() as usize).clamp(1, n) - 1]
        };
        let max = v.last().copied().unwrap_or(0.0);
        (mean, p95, max)
    };
    let (e_mean, e_p95, e_max) = stats(encode_times.clone());
    let (p_mean, p_p95, p_max) = stats(pipeline_times.clone());
    let (b_mean, _, _) = stats(bitstream_bytes);
    let within_opt = pipeline_times.iter().filter(|t| **t <= OPTIMAL_MS).count() as f64
        / pipeline_times.len().max(1) as f64
        * 100.0;
    let missed_mand = pipeline_times.iter().filter(|t| **t > MANDATORY_MS).count() as f64
        / pipeline_times.len().max(1) as f64
        * 100.0;

    println!(
        "{}",
        serde_json::json!({
            "frames": frames,
            "empty_polls": empty_polls,
            "resolution": [w, h],
            "encode_ms": {"mean": e_mean, "p95": e_p95, "max": e_max},
            "pipeline_ms": {"mean": p_mean, "p95": p_p95, "max": p_max},
            "delivery": {
                "mandatory_ms": MANDATORY_MS,
                "optimal_ms": OPTIMAL_MS,
                "missed_mandatory_pct": missed_mand,
                "within_optimal_pct": within_opt,
            },
            "mean_bitstream_bytes": b_mean,
            "bitrate_at_actual_fps_mbps": if p_mean > 0.0 {
                b_mean * 8.0 * (1000.0 / p_mean) / 1_000_000.0
            } else { 0.0 },
            "frames_encoded_total": encoder.frames_encoded(),
        })
    );
    Ok(())
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}
