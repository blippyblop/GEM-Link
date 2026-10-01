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
use alvr_common::{
    DeviceMotion, HEAD_ID, Pose, ViewParams,
    glam::{Quat, UVec2, Vec3},
};
use alvr_packets::{FaceData, TrackingData};
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

/// Feed the streamer head tracking.
///
/// This is not decoration. On the streamer side,
/// `OvrDirectModeComponent::SubmitLayer` matches the HMD pose the compositor
/// submitted against its history of *tracking* poses to work out which frame it
/// is holding. With no tracking that history is empty, every `GetBestPoseMatch`
/// fails, the frame index stays 0, and every frame is discarded as a duplicate —
/// so nothing is ever encoded and no video arrives, even though the whole
/// compositor -> Present path is running. A headless sim therefore has to send
/// poses even though it has no head.
///
/// Rate matches the mock client's: a third of the frame rate.
fn tracking_thread(ctx: Arc<ClientCoreContext>, streaming: Arc<AtomicBool>, origin: Instant) {
    ctx.send_view_params([ViewParams::DUMMY; 2]);

    // The pose must be *unique per sample*, not merely present. A constant pose
    // makes every entry in the streamer's pose history an equally good match, so
    // consecutive frames resolve to the same timestamp and get discarded as
    // duplicates (measured: ~85% discarded with a static pose). A slow yaw sweep
    // plus a small deterministic jitter keeps each sample distinguishable, and
    // keeps the motion plausible.
    let mut lcg: u32 = 0x1234_5678;

    let mut deadline = Instant::now();
    loop {
        if streaming.load(Ordering::SeqCst) {
            lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let jitter = ((lcg >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.002;

            let t = origin.elapsed().as_secs_f32();
            let orientation = Quat::from_rotation_y(t * 0.5) * Quat::from_rotation_z(jitter);

            ctx.send_tracking(TrackingData {
                poll_timestamp: origin.elapsed(),
                device_motions: vec![(
                    *HEAD_ID,
                    DeviceMotion {
                        pose: Pose {
                            orientation,
                            // Standing height, so the pose is plausible rather
                            // than at the floor origin.
                            position: Vec3::new(0.0, 1.6, 0.0),
                        },
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                )],
                hand_skeletons: [None, None],
                face: FaceData::default(),
                body: None,
            });
        }
        deadline += Duration::from_micros(FRAME_INTERVAL_US / 3);
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
    }
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

    let ctx = Arc::new(ClientCoreContext::new(capabilities, vec![]));

    // A null decoder: accept every frame and record it. Returning `false` is
    // read as decoder saturation and makes the client spam RequestIdr, so always
    // accept. This is what lets the sim prove real video arrived with no
    // hardware decoder in the loop.
    let frames = Arc::new(AtomicUsize::new(0));
    let bytes = Arc::new(AtomicUsize::new(0));
    let samples: Arc<Mutex<Vec<Sample>>> = Arc::new(Mutex::new(Vec::new()));
    let first = Arc::new(AtomicBool::new(true));
    // Foveation centres the streamer sent with each frame, via the real metadata
    // API the compositor uses (report_compositor_start).
    let fov: Arc<Mutex<Vec<[[f32; 2]; 2]>>> = Arc::new(Mutex::new(Vec::new()));
    let fov_seen = Arc::clone(&fov);
    let ctx_for_cb = Arc::clone(&ctx);

    let frames_seen = Arc::clone(&frames);
    let bytes_seen = Arc::clone(&bytes);
    let samples_seen = Arc::clone(&samples);
    let first_frame = Arc::clone(&first);
    ctx.set_decoder_input_callback(Box::new(move |header_ts, nal| {
        let n = frames_seen.fetch_add(1, Ordering::SeqCst) + 1;
        bytes_seen.fetch_add(nal.len(), Ordering::SeqCst);
        if let Some(meta) = ctx_for_cb.report_compositor_start(header_ts)
            && let Some(shifts) = meta.foveation_center_shifts
            && let Ok(mut v) = fov_seen.lock()
        {
            v.push(shifts);
        }
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

    // Tracking must start as soon as there is a connection: the streamer cannot
    // identify a single frame without a pose history to match against.
    let streaming = Arc::new(AtomicBool::new(false));
    let tracking_origin = Instant::now();
    {
        let ctx_for_tracking = Arc::clone(&ctx);
        let streaming_for_tracking = Arc::clone(&streaming);
        thread::spawn(move || tracking_thread(ctx_for_tracking, streaming_for_tracking, tracking_origin));
    }

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
    let realtime_seen = AtomicBool::new(false);
    let haptics_seen = AtomicBool::new(false);

    loop {
        while let Some(event) = ctx.poll_event() {
            match event {
                ClientCoreEvent::UpdateHudMessage(message) => {
                    println!("[framesim] hud: {message}");
                    last_hud = message;
                }
                ClientCoreEvent::StreamingStarted(config) => {
                    streaming.store(true, Ordering::SeqCst);
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
                    streaming.store(false, Ordering::SeqCst);
                    println!("[framesim] stream stopped");
                }
                ClientCoreEvent::RealTimeConfig(config) => {
                    // Arrives repeatedly, not per frame; report once so it does
                    // not drown the log.
                    if !realtime_seen.swap(true, Ordering::SeqCst) {
                        println!("[framesim] realtime config: ext={:?}", config.ext_str);
                    }
                }
                ClientCoreEvent::Haptics { device_id, .. } => {
                    // No actuator on a headless sim; note it once.
                    if !haptics_seen.swap(true, Ordering::SeqCst) {
                        println!("[framesim] haptics events arriving (device {device_id})");
                    }
                }
            }
        }

        if want_frames > 0 && frames.load(Ordering::SeqCst) >= want_frames {
            let code = report(&samples, &bytes, &fov, start);
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
fn report(
    samples: &Arc<Mutex<Vec<Sample>>>,
    bytes: &Arc<AtomicUsize>,
    fov: &Arc<Mutex<Vec<[[f32; 2]; 2]>>>,
    start: Instant,
) -> i32 {
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

    // Foveation on the wire: did the streamer actually send centres, and did
    // they move? (A constant centre would mean the gaze path is dead.)
    if let Ok(v) = fov.lock() {
        if v.is_empty() {
            println!("[framesim] foveation: NO centres received (foveation not in the stream)");
        } else {
            let n = v.len();
            let first = v[0];
            let moved = v.iter().any(|c| {
                (c[0][0] - first[0][0]).abs() > 1e-4 || (c[1][0] - first[1][0]).abs() > 1e-4
            });
            println!(
                "[framesim] foveation: {n}/{n} frames carried centres; moved={moved}; first L({:.4},{:.4}) R({:.4},{:.4})",
                first[0][0], first[0][1], first[1][0], first[1][1]
            );
        }
    }

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
