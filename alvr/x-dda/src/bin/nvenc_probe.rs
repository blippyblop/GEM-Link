//! `nvenc_probe` — Phase 1 measurement probe.
//!
//! Two modes:
//! - **capture** (default): Desktop Duplication → GPU pool texture → NVENC.
//!   Acquire-wait is reported separately from processing, so the source's
//!   composition cadence is never conflated with our latency.
//! - **feeder** (`--feeder`): synthetic animated frames — no desktop needed.
//!   Measures the pure encode path at max throughput; `--bit10` selects the
//!   R10G10B10A2 → ARGB10 → HEVC Main10 path.

#![allow(unsafe_code)]

use std::process::ExitCode;
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAP_WRITE_DISCARD, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;
use x_dda::{Duplicator, SOURCE_FORMATS};
use x_nvenc::NvEncoder;

const MANDATORY_MS: f64 = 1000.0 / 90.0;
const OPTIMAL_MS: f64 = 1000.0 / 120.0;

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[idx - 1]
}

fn stats(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    let mean = if n == 0 {
        0.0
    } else {
        v.iter().sum::<f64>() / n as f64
    };
    (mean, percentile(&v, 0.95), v.last().copied().unwrap_or(0.0))
}

fn delivery(times: &[f64]) -> (f64, f64) {
    if times.is_empty() {
        return (0.0, 0.0);
    }
    let missed =
        times.iter().filter(|t| **t > MANDATORY_MS).count() as f64 / times.len() as f64 * 100.0;
    let optimal =
        times.iter().filter(|t| **t <= OPTIMAL_MS).count() as f64 / times.len() as f64 * 100.0;
    (missed, optimal)
}

fn create_pool(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> Result<Vec<ID3D11Texture2D>, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut pool = Vec::new();
    for _ in 0..2 {
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .map_err(|e| format!("CreateTexture2D failed: {e}"))?;
        }
        pool.push(tex.ok_or("CreateTexture2D returned no texture")?);
    }
    Ok(pool)
}

fn standalone_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
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

unsafe fn fill_frame(
    ctx: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    w: u32,
    h: u32,
    t: u32,
    ten_bit: bool,
) -> Result<(), String> {
    let mut mapped = windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(staging, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
        .map_err(|e| format!("Map failed: {e}"))?;
    let row_pitch = mapped.RowPitch as usize;
    let base = mapped.pData as *mut u8;
    for y in 0..h as usize {
        let px = base.add(y * row_pitch) as *mut u32;
        for x in 0..w as usize {
            if ten_bit {
                let r = ((x * 37 + (t as usize) * 13) & 0x3FF) as u32;
                let g = ((y * 17 + (t as usize) * 7) & 0x3FF) as u32;
                let b = (((x ^ y) + t as usize) & 0x3FF) as u32;
                px.add(x).write(0xC000_0000u32 | (b << 20) | (g << 10) | r);
            } else {
                let r = ((x * 3 + (t as usize) * 5) & 0xFF) as u32;
                let g = ((y * 5 + (t as usize) * 3) & 0xFF) as u32;
                let b = (((x ^ y) + t as usize) & 0xFF) as u32;
                px.add(x).write(0xFF00_0000u32 | (b << 16) | (g << 8) | r);
            }
        }
    }
    ctx.Unmap(staging, 0);
    Ok(())
}

fn api_version_json(raw: u32) -> serde_json::Value {
    serde_json::json!({ "major": raw & 0xFF, "minor": (raw >> 24) & 0xFF })
}

fn run_capture(seconds: f64, output: u32, fps: u32) -> Result<serde_json::Value, String> {
    let dup = Duplicator::new(0, output, &SOURCE_FORMATS)
        .map_err(|e| format!("duplicator failed: {e}"))?;
    let desktop = dup
        .desktop_desc()
        .map_err(|e| format!("desc failed: {e}"))?;
    let (w, h) = (desktop.width, desktop.height);
    let mut encoder =
        NvEncoder::new(dup.device_ptr(), w, h, fps).map_err(|e| format!("nvenc session: {e}"))?;
    let pool = create_pool(dup.device(), w, h, DXGI_FORMAT_B8G8R8A8_UNORM)?;
    let pitch = w * 4;
    let ctx = dup.context().ok_or("no device context")?.clone();

    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut wait_times: Vec<f64> = Vec::new();
    let mut proc_times: Vec<f64> = Vec::new();
    let mut bitstream: Vec<f64> = Vec::new();
    let mut cadence: Vec<f64> = Vec::new();
    let mut last_present: Option<f64> = None;
    let mut frames = 0u32;
    let mut empty_polls = 0u32;
    let mut pool_idx = 0usize;

    while Instant::now() < deadline {
        let t0 = Instant::now();
        let frame = match unsafe { dup.acquire(20) } {
            Ok(Some(f)) => f,
            Ok(None) => {
                empty_polls += 1;
                continue;
            }
            Err(e) => return Err(format!("acquire failed: {e}")),
        };
        wait_times.push(t0.elapsed().as_secs_f64() * 1000.0);

        let t1 = Instant::now();
        unsafe {
            dup.copy_resource(&pool[pool_idx], &frame.texture);
        }
        dup.release();
        if frame.meta.last_present_ms > 0.0 {
            if let Some(prev) = last_present {
                if frame.meta.last_present_ms > prev {
                    cadence.push(frame.meta.last_present_ms - prev);
                }
            }
            last_present = Some(frame.meta.last_present_ms);
        }

        let bytes = encoder
            .encode(unsafe { pool[pool_idx].as_raw() }, pitch)
            .map_err(|e| format!("encode failed: {e}"))?;
        proc_times.push(t1.elapsed().as_secs_f64() * 1000.0);
        bitstream.push(bytes.len() as f64);
        frames += 1;
        pool_idx = (pool_idx + 1) % pool.len();
    }

    let (wm, w95, wmax) = stats(wait_times);
    let (pm, p95, pmax) = stats(proc_times.clone());
    let (cm, _, _) = stats(cadence);
    let (bm, _, _) = stats(bitstream.clone());
    let (missed, optimal) = delivery(&proc_times);
    let achieved_fps = frames as f64 / seconds.max(0.001);
    Ok(serde_json::json!({
        "mode": "capture",
        "negotiated_api": api_version_json(encoder.api_version()),
        "resolution": [w, h],
        "frames": frames,
        "empty_polls": empty_polls,
        "acquire_wait_ms": {"mean": wm, "p95": w95, "max": wmax},
        "processing_ms": {"mean": pm, "p95": p95, "max": pmax},
        "source_cadence_ms": cm,
        "delivery_on_processing": {"missed_mandatory_pct": missed, "within_optimal_pct": optimal},
        "mean_bitstream_bytes": bm,
        "achieved_fps": achieved_fps,
        "bitrate_mbps": bm * 8.0 * achieved_fps / 1_000_000.0,
    }))
}

fn run_feeder(seconds: f64, fps: u32, bit10: bool) -> Result<serde_json::Value, String> {
    let (device, context) = standalone_device()?;
    let (w, h) = (2560u32, 1440u32);
    let format = if bit10 {
        DXGI_FORMAT_R10G10B10A2_UNORM
    } else {
        DXGI_FORMAT_B8G8R8A8_UNORM
    };
    let mut encoder = NvEncoder::new(unsafe { device.as_raw() }, w, h, fps)
        .map_err(|e| format!("nvenc session: {e}"))?;
    let pool = create_pool(&device, w, h, format)?;
    let mut staging = {
        let mut desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe {
            device
                .CreateTexture2D(&mut desc, None, Some(&mut tex))
                .map_err(|e| format!("staging failed: {e}"))?;
        }
        tex.ok_or("no staging texture")?
    };
    let pitch = w * if bit10 { 4 } else { 4 };

    let target_frames = (seconds * fps as f64) as u32;
    let mut proc_times: Vec<f64> = Vec::new();
    let mut bitstream: Vec<f64> = Vec::new();
    let mut pool_idx = 0usize;

    for t in 0..target_frames {
        let t0 = Instant::now();
        unsafe {
            fill_frame(&context, &staging, w, h, t, bit10)?;
            context.CopyResource(&pool[pool_idx], &staging);
        }
        let bytes = if bit10 {
            encoder.encode_10bit(unsafe { pool[pool_idx].as_raw() }, pitch)
        } else {
            encoder.encode(unsafe { pool[pool_idx].as_raw() }, pitch)
        }
        .map_err(|e| format!("encode failed: {e}"))?;
        proc_times.push(t0.elapsed().as_secs_f64() * 1000.0);
        bitstream.push(bytes.len() as f64);
        pool_idx = (pool_idx + 1) % pool.len();
    }
    let _ = &mut staging;

    let (pm, p95, pmax) = stats(proc_times.clone());
    let (bm, _, _) = stats(bitstream.clone());
    let (missed, optimal) = delivery(&proc_times);
    let elapsed = target_frames as f64 / fps.max(1) as f64;
    Ok(serde_json::json!({
        "mode": if bit10 { "feeder_10bit" } else { "feeder_8bit" },
        "negotiated_api": api_version_json(encoder.api_version()),
        "resolution": [w, h],
        "frames": target_frames,
        "processing_ms": {"mean": pm, "p95": p95, "max": pmax},
        "delivery_on_processing": {"missed_mandatory_pct": missed, "within_optimal_pct": optimal},
        "mean_bitstream_bytes": bm,
        "achieved_fps": target_frames as f64 / elapsed.max(0.001),
        "bitrate_mbps": bm * 8.0 * fps as f64 / 1_000_000.0,
    }))
}

fn real_main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut seconds = 8.0f64;
    let mut output = 0u32;
    let mut fps = 120u32;
    let mut feeder = false;
    let mut bit10 = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seconds" => {
                seconds = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(8.0);
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
            "--feeder" => {
                feeder = true;
                i += 1;
            }
            "--bit10" => {
                bit10 = true;
                i += 1;
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }

    let realtime = x_dda::apply_scheduling_hygiene();
    let mut report = if feeder {
        run_feeder(seconds, fps, bit10)?
    } else {
        run_capture(seconds, output, fps)?
    };
    report["realtime_priority"] = serde_json::Value::Bool(realtime);
    println!("{report}");
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
