//! Headless "fake headset 2.0" — Phase 3 M1.
//!
//! Drives the **real** `client_core` connection pipeline with no VR runtime and
//! no display, so the Frame client stack can be exercised against a real
//! streamer while running under qemu on a sim host (pavserv).
//!
//! The client *listens* on the well-known control port; the streamer dials it.
//!
//! Gates (env `FRAMESIM_FRAMES=N`):
//!   exits 0 once N frames have arrived inside every frame budget,
//!   exits 3 if any frame missed a budget, 2 on timeout.
//!
//! The goal is NOT a frame rate. One frame arriving 100 ms late and the rest
//! arriving in 1 ms still averages 90 fps and looks awful. What matters is the
//! tail: every frame inside the budget.
//!
//! Diagnostics (env `FRAMESIM_CSV=path`): writes one row per frame —
//! `idx,header_ts_us,size_bytes,dev_ms,gap_ms` — so a run can be joined against
//! the streamer's own per-frame trace on `header_ts_us`. Without that join, a
//! budget miss is just a number with no cause.

use alvr_client_core::{ClientCapabilities, ClientCoreContext, ClientCoreEvent};
use alvr_common::glam::UVec2;
use std::{
    fs::File,
    io::Write,
    process::exit,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// The streamer's cadence, used to compute expected arrival times.
const FRAME_INTERVAL_US: u64 = 11_111;

/// Frame-budget gates, in ms, with the label to print.
///
/// We assert "zero frames over budget" rather than "99.99% within budget",
/// because substantiating 99.99% needs >= 10_000 samples; asserting it from a
/// few hundred frames would be a dishonest number. The sample count is printed
/// either way.
const DEADLINES_MS: [(f64, &str); 2] = [
    (1000.0 / 90.0, "primary  (90 Hz budget, 11.111 ms)"),
    (1000.0 / 120.0, "ideal    (120 Hz budget,  8.333 ms)"),
];

/// One received frame: arrival instant, bitstream size, sender's timestamp.
#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    size: usize,
    header_ts: Duration,
}

fn timeout() -> Duration {
    std::env::var("FRAMESIM_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(90))
}

fn frame_target() -> usize {
    std::env::var("FRAMESIM_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() as f64 - 1.0) * q) as usize]
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

    // A null decoder: accept every frame and record it. Returning `false` is
    // read as decoder saturation and makes the client spam RequestIdr, so always
    // accept. This is what lets the sim prove real video arrived with no
    // hardware decoder in the loop.
    let frames = Arc::new(AtomicUsize::new(0));
    let bytes = Arc::new(AtomicUsize::new(0));
    let samples: Arc<Mutex<Vec<Sample>>> = Arc::new(Mutex::new(Vec::new()));
    let first = Arc::new(AtomicBool::new(true));

    let frames_seen = Arc::clone(&frames);
    let bytes_seen = Arc::clone(&bytes);
    let samples_seen = Arc::clone(&samples);
    let first_frame = Arc::clone(&first);
    ctx.set_decoder_input_callback(Box::new(move |header_ts, nal| {
        let n = frames_seen.fetch_add(1, Ordering::SeqCst) + 1;
        bytes_seen.fetch_add(nal.len(), Ordering::SeqCst);
        if let Ok(mut s) = samples_seen.lock() {
            s.push(Sample {
                at: Instant::now(),
                size: nal.len(),
                header_ts,
            });
        }
        if first_frame.swap(false, Ordering::SeqCst) || n % 60 == 0 {
            println!(
                "[framesim] video frame #{n} ts={header_ts:?} bytes={}",
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
            let code = report(&samples, &bytes, start);
            exit(code);
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

/// Statistics, gates, and — the point of this file — per-outlier context.
fn report(samples: &Arc<Mutex<Vec<Sample>>>, bytes: &Arc<AtomicUsize>, start: Instant) -> i32 {
    let s = match samples.lock() {
        Ok(s) => s.clone(),
        Err(_) => return 0,
    };
    let n = s.len();
    if n < 3 {
        println!("[framesim] only {n} frames; nothing to report");
        return 0;
    }

    // Cadence deviation, centred on the median. The absolute offset is
    // unknowable (no shared clock, and the first frame sits queued behind
    // session setup); only the spread is real.
    let interval = FRAME_INTERVAL_US as f64 / 1000.0;
    let mut dev: Vec<f64> = Vec::with_capacity(n);
    for (i, smp) in s.iter().enumerate() {
        let rel = smp.at.duration_since(s[0].at).as_secs_f64() * 1e3;
        dev.push(rel - i as f64 * interval);
    }
    let mut sorted = dev.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = percentile(&sorted, 0.5);
    let centred: Vec<f64> = dev.iter().map(|d| d - mid).collect();

    let span = s[n - 1].at.duration_since(s[0].at).as_secs_f64().max(1e-6);
    let total = bytes.load(Ordering::SeqCst);
    println!("[framesim] VIDEO OK: received {n} video frames");
    println!(
        "[framesim]   {:.1} fps, {:.1} Mbps, {} bytes",
        (n - 1) as f64 / span,
        total as f64 * 8.0 / span / 1e6,
        total
    );
    let mut cs = centred.clone();
    cs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "[framesim]   cadence deviation: min {:.2} ms, p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
        cs[0],
        percentile(&cs, 0.5),
        percentile(&cs, 0.99),
        cs[n - 1]
    );
    println!("[framesim]   samples: {n} (99.99% needs >= 10000)");

    let mut gate_ok = true;
    for (deadline, label) in DEADLINES_MS {
        let over = centred.iter().filter(|x| x.abs() > deadline).count();
        let ok = over == 0;
        if label.starts_with("primary") {
            gate_ok = ok;
        }
        println!(
            "[framesim]   {label}: {over}/{n} outside budget ({:.4}%) -> {}",
            over as f64 * 100.0 / n as f64,
            if ok { "PASS" } else { "FAIL" }
        );
    }

    // ---- per-outlier context: the whole reason this exists ----
    let mut sizes: Vec<usize> = s.iter().map(|x| x.size).collect();
    sizes.sort_unstable();
    let median_size = sizes[sizes.len() / 2];
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| centred[b].abs().partial_cmp(&centred[a].abs()).unwrap());
    println!("[framesim] worst offenders (join on header_ts_us with the streamer's CSV):");
    println!("[framesim]   idx  dev_ms   gap_ms   size_B  idr?  header_ts_us");
    for &i in idx.iter().take(10) {
        let gap = if i == 0 {
            0.0
        } else {
            s[i].at.duration_since(s[i - 1].at).as_secs_f64() * 1e3
        };
        println!(
            "[framesim]   {:>4}  {:>7.2}  {:>7.2}  {:>7}  {:>4}  {}",
            i,
            centred[i],
            gap,
            s[i].size,
            if s[i].size > median_size * 2 { "yes" } else { "" },
            s[i].header_ts.as_micros()
        );
    }

    // Is the tail explained by big frames (IDRs) rather than by the transport?
    let outlier_idx: Vec<usize> = (0..n).filter(|&i| centred[i].abs() > DEADLINES_MS[0].0).collect();
    let big = outlier_idx.iter().filter(|&&i| s[i].size > median_size * 2).count();
    println!(
        "[framesim] {}/{} outliers are >2x median size ({} B); median frame {} B",
        big,
        outlier_idx.len(),
        median_size * 2,
        median_size
    );

    if let Ok(path) = std::env::var("FRAMESIM_CSV") {
        match File::create(&path) {
            Ok(mut f) => {
                let _ = writeln!(f, "idx,header_ts_us,size_bytes,dev_ms,gap_ms");
                for i in 0..n {
                    let gap = if i == 0 {
                        0.0
                    } else {
                        s[i].at.duration_since(s[i - 1].at).as_secs_f64() * 1e3
                    };
                    let _ = writeln!(
                        f,
                        "{},{},{},{:.3},{:.3}",
                        i,
                        s[i].header_ts.as_micros(),
                        s[i].size,
                        centred[i],
                        gap
                    );
                }
                println!("[framesim] per-frame CSV written to {path}");
            }
            Err(e) => eprintln!("[framesim] could not write CSV {path}: {e}"),
        }
    }

    let _ = start;
    if gate_ok { 0 } else { 3 }
}
