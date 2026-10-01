//! Headless "fake headset 2.0" — Phase 3 M1.
//!
//! Drives the **real** `client_core` connection pipeline with no VR runtime and
//! no display, so the Frame client stack can be exercised against a real
//! streamer while running under qemu on a sim host (pavserv). It is
//! `alvr_client_mock`'s `client_thread` with the eframe GUI removed.
//!
//! The client *listens* on the well-known control port; the streamer dials it.
//! Exits 0 as soon as the session is negotiated (that is the M1 gate), non-zero
//! on timeout, so a rig can assert on it.
//!
//! Set `FRAMESIM_FRAMES=N` to instead require N decoded-input frames before
//! exiting 0 — that is the video-path gate.

use alvr_client_core::{ClientCapabilities, ClientCoreContext, ClientCoreEvent};
use alvr_common::glam::UVec2;
use std::{
    process::exit,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// How long to wait for the streamer to turn up and negotiate.
fn timeout() -> Duration {
    std::env::var("FRAMESIM_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(90))
}

/// If non-zero, require this many video frames before declaring success.
fn frame_target() -> usize {
    std::env::var("FRAMESIM_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn main() {
    env_logger::init();

    // A Steam Frame-ish capability set per ADR-0008: HEVC-capable, foveated
    // encoding on; no AV1 and no 10-bit, because the Frame kernel decodes
    // neither. The streamer negotiates down from here.
    let capabilities = ClientCapabilities {
        platform: alvr_system_info::platform(None, None),
        default_view_resolution: UVec2::new(2160, 2160),
        max_view_resolution: UVec2::new(2160, 2160),
        refresh_rates: vec![60.0, 72.0, 80.0, 90.0, 120.0],
        foveated_encoding: true,
        encoder_high_profile: true,
        encoder_10_bits: false,
        encoder_av1: false,
        prefer_10bit: false,
        preferred_encoding_gamma: 1.0,
        prefer_hdr: false,
    };

    println!(
        "[framesim] platform={} view={}x{} foveated={} av1={} 10bit={}",
        capabilities.platform,
        capabilities.default_view_resolution.x,
        capabilities.default_view_resolution.y,
        capabilities.foveated_encoding,
        capabilities.encoder_av1,
        capabilities.encoder_10_bits,
    );

    let ctx = ClientCoreContext::new(capabilities, vec![]);

    // A null decoder: accept every frame and count it. Returning `false` is read
    // as decoder saturation and makes the client spam RequestIdr, so always
    // accept. This is what lets the sim prove real video arrived with no
    // hardware decoder in the loop.
    let frames = Arc::new(AtomicUsize::new(0));
    let frames_seen = Arc::clone(&frames);
    let first = Arc::new(AtomicBool::new(true));
    let first_frame = Arc::clone(&first);
    let bytes = Arc::new(AtomicUsize::new(0));
    let bytes_seen = Arc::clone(&bytes);
    // Inter-arrival gaps: the receive-side cadence, independent of what the
    // streamer thinks it sent.
    let gaps: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let gaps_seen = Arc::clone(&gaps);
    let mut last: Option<Instant> = None;
    let first_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let first_at_seen = Arc::clone(&first_at);
    ctx.set_decoder_input_callback(Box::new(move |timestamp, nal| {
        let n = frames_seen.fetch_add(1, Ordering::SeqCst) + 1;
        bytes_seen.fetch_add(nal.len(), Ordering::SeqCst);
        let now = Instant::now();
        if let Ok(mut f) = first_at_seen.lock() {
            if f.is_none() {
                *f = Some(now);
            }
        }
        if let Some(prev) = last {
            if let Ok(mut g) = gaps_seen.lock() {
                g.push((now - prev).as_secs_f64() * 1e3);
            }
        }
        last = Some(now);
        if first_frame.swap(false, Ordering::SeqCst) || n % 60 == 0 {
            println!(
                "[framesim] video frame #{n} ts={timestamp:?} bytes={}",
                nal.len()
            );
        }
        true
    }));

    let want_frames = frame_target();
    ctx.resume();
    println!(
        "[framesim] resume() called; announcing + listening for the streamer (frame target: {})",
        if want_frames == 0 {
            "negotiation only".to_string()
        } else {
            format!("{want_frames} frames")
        }
    );

    let start = Instant::now();
    let limit = timeout();
    let mut last_hud = String::new();

    loop {
        while let Some(event) = ctx.poll_event() {
            match event {
                ClientCoreEvent::UpdateHudMessage(message) => {
                    println!("[framesim] hud: {message}");
                    last_hud = message;
                }
                ClientCoreEvent::StreamingStarted(config) => {
                    let n = &config.negotiated_config;
                    println!(
                        "[framesim] NEGOTIATED view={}x{} refresh={}Hz",
                        n.view_resolution.x, n.view_resolution.y, n.refresh_rate_hint,
                    );
                    if want_frames == 0 {
                        println!("[framesim] M1 OK: negotiation completed");
                        exit(0);
                    }
                    println!("[framesim] waiting for {want_frames} video frames...");
                }
                ClientCoreEvent::DecoderConfig { codec, config_nal } => {
                    println!(
                        "[framesim] decoder config: codec={codec:?} config_nal={} bytes",
                        config_nal.len()
                    );
                }
                ClientCoreEvent::StreamingStopped => {
                    println!("[framesim] stream stopped");
                }
                _ => println!("[framesim] event (unhandled variant)"),
            }
        }

        if want_frames > 0 && frames.load(Ordering::SeqCst) >= want_frames {
            let n = frames.load(Ordering::SeqCst);
            // Measure over the frame span: `start` predates the handshake and
            // qemu's startup, which made this read as a nonsense 5 fps.
            let secs = first_at
                .lock()
                .ok()
                .and_then(|f| *f)
                .map(|f| f.elapsed().as_secs_f64())
                .unwrap_or_else(|| start.elapsed().as_secs_f64())
                .max(1e-6);
            let total = bytes.load(Ordering::SeqCst);
            println!("[framesim] VIDEO OK: received {n} video frames");
            println!(
                "[framesim]   {:.0} fps, {:.1} Mbps, {} bytes",
                n as f64 / secs,
                total as f64 * 8.0 / secs / 1e6,
                total
            );
            if let Ok(mut g) = gaps.lock() {
                if !g.is_empty() {
                    g.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    let m = g.len();
                    let mean = g.iter().sum::<f64>() / m as f64;
                    println!(
                        "[framesim]   frame gap: mean {mean:.2} ms, p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
                        g[m / 2],
                        g[(m * 99 / 100).min(m - 1)],
                        g[m - 1]
                    );
                }
            }
            exit(0);
        }

        if start.elapsed() > limit {
            eprintln!(
                "[framesim] TIMEOUT after {:?} (frames={}); last hud: {last_hud}",
                start.elapsed(),
                frames.load(Ordering::SeqCst)
            );
            exit(2);
        }

        thread::sleep(Duration::from_millis(50));
    }
}
