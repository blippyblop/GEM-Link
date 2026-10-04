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
//!
//! ## Emulated prototype mode (env `FRAMESIM_DECODE=1`)
//!
//! Turns the null decoder into a **real** one (libde265, loaded at runtime) so
//! the harness can prove the whole client side end to end: real streamer →
//! real HEVC → real pixels. Decode goes through the *public* client_core decoder
//! API, so `client_core` is untouched.
//!
//!   FRAMESIM_DECODE=1              decode received access units
//!   FRAMESIM_LIBDE265=<path>       decoder library (default `libde265.so.0`)
//!   FRAMESIM_DECODE_THREADS=N      decoder worker threads (default 4)
//!   FRAMESIM_PNG_DIR=<dir>         write decoded frames as PNG
//!   FRAMESIM_PNG_EVERY=N           write every Nth decoded frame (default 1)
//!   FRAMESIM_PNG_MAX=N             stop writing after N files (0 = all)
//!   FRAMESIM_DUMP_NALS=<file>      also dump raw access units (u32 LE length
//!                                  prefix), so a real bitstream can be kept as
//!                                  a regression fixture
//!
//! libde265 is LGPL-3: **harness only**, never linked into shipped code.

mod libde265;
mod png;

use alvr_client_core::{ClientCapabilities, ClientCoreContext, ClientCoreEvent};
use alvr_common::{
    CONTROLLER_PROFILE_INFO, DeviceMotion, FRAME_CONTROLLER_PROFILE_ID, HAND_LEFT_ID,
    HAND_RIGHT_ID, HEAD_ID, LEFT_BUMPER_CLICK_ID, LEFT_DPAD_DOWN_CLICK_ID, LEFT_DPAD_LEFT_CLICK_ID,
    LEFT_DPAD_RIGHT_CLICK_ID, LEFT_DPAD_UP_CLICK_ID, LEFT_SQUEEZE_VALUE_ID, LEFT_THUMBSTICK_X_ID,
    LEFT_THUMBSTICK_Y_ID, LEFT_TRIGGER_VALUE_ID, LEFT_VIEW_CLICK_ID, Pose, RIGHT_A_CLICK_ID,
    RIGHT_B_CLICK_ID, RIGHT_BUMPER_CLICK_ID, RIGHT_MENU_CLICK_ID, RIGHT_SQUEEZE_VALUE_ID,
    RIGHT_THUMBSTICK_X_ID, RIGHT_THUMBSTICK_Y_ID, RIGHT_TRIGGER_VALUE_ID, RIGHT_X_CLICK_ID,
    RIGHT_Y_CLICK_ID, ViewParams,
    glam::{Quat, UVec2, Vec3},
};
use alvr_packets::{ButtonEntry, ButtonValue, FaceData, TrackingData};
use std::{
    collections::HashSet,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
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

/// One received frame: arrival instant, bitstream size, sender's timestamp, and — since
/// ADR-0011's frame index reaches here — its identity.
#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    size: usize,
    header_ts: Duration,
    /// The server's monotonic frame index, if the receive loop could name it. `None` only
    /// when no frame was in flight, which should not happen for a frame we are decoding.
    frame_index: Option<u64>,
    /// Cumulative frames the server discarded, as of this frame.
    missed_frames: u64,
    /// Whether datagrams were lost *inside* this frame, as opposed to whole frames going
    /// missing. Different cause, different fix.
    had_datagram_loss: bool,
    /// How long this client spent inside the decode callback for this frame. The receive loop
    /// calls it synchronously, so this is exactly the delay the receive path pays per frame.
    client_ms: f64,
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

/// Emulated-prototype decode settings, all env-driven so the same binary is both
/// the transport harness and the emulated client.
struct DecodeCfg {
    enabled: bool,
    lib_path: PathBuf,
    threads: usize,
    png_dir: Option<PathBuf>,
    png_every: u64,
    png_max: u64,
    dump_nals: Option<PathBuf>,
}

impl DecodeCfg {
    fn from_env() -> Self {
        let env_flag = |k: &str| std::env::var(k).is_ok_and(|v| v != "0" && !v.is_empty());
        let env_u64 = |k: &str, d: u64| {
            std::env::var(k)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(d)
        };
        Self {
            enabled: env_flag("FRAMESIM_DECODE"),
            lib_path: std::env::var("FRAMESIM_LIBDE265")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("libde265.so.0")),
            threads: env_u64("FRAMESIM_DECODE_THREADS", 4) as usize,
            png_dir: std::env::var("FRAMESIM_PNG_DIR").ok().map(PathBuf::from),
            png_every: env_u64("FRAMESIM_PNG_EVERY", 1).max(1),
            png_max: env_u64("FRAMESIM_PNG_MAX", 0),
            dump_nals: std::env::var("FRAMESIM_DUMP_NALS").ok().map(PathBuf::from),
        }
    }
}

/// Everything the decode path needs, behind one `Arc` so the decoder-input
/// callback (connection thread) and the event loop (main thread) share it.
struct DecodeState {
    decoder: Option<Mutex<libde265::HevcDecoder>>,
    /// Parameter sets from `ClientCoreEvent::DecoderConfig`. Held here because
    /// video can arrive before the event loop has polled that event.
    csd: Mutex<Option<Vec<u8>>>,
    dump: Option<Mutex<File>>,
    png_dir: Option<PathBuf>,
    png_every: u64,
    png_max: u64,
    png_written: AtomicUsize,
    decode_ns: AtomicUsize,
    reported: AtomicUsize,
}

impl DecodeState {
    fn new(cfg: &DecodeCfg) -> Self {
        let decoder = if cfg.enabled {
            match libde265::HevcDecoder::new(&cfg.lib_path, cfg.threads) {
                Ok(d) => Some(Mutex::new(d)),
                Err(e) => {
                    eprintln!("[framesim] FRAMESIM_DECODE=1 but no decoder: {e}");
                    exit(4);
                }
            }
        } else {
            None
        };

        if let Some(dir) = &cfg.png_dir {
            let _ = std::fs::create_dir_all(dir);
            println!("[framesim] decoded frames -> {}", dir.display());
        }

        let dump = cfg.dump_nals.as_ref().and_then(|p| match File::create(p) {
            Ok(f) => {
                println!("[framesim] dumping access units -> {}", p.display());
                Some(Mutex::new(f))
            }
            Err(e) => {
                eprintln!("[framesim] cannot create dump {}: {e}", p.display());
                None
            }
        });

        Self {
            decoder,
            csd: Mutex::new(None),
            dump,
            png_dir: cfg.png_dir.clone(),
            png_every: cfg.png_every,
            png_max: cfg.png_max,
            png_written: AtomicUsize::new(0),
            decode_ns: AtomicUsize::new(0),
            reported: AtomicUsize::new(0),
        }
    }

    /// Feed one access unit, report every picture it yields back to client_core
    /// with the timestamp the streamer gave it, and optionally write it out.
    fn on_access_unit(&self, ctx: Option<&ClientCoreContext>, header_ts: Duration, nal: &[u8]) {
        if let Some(f) = &self.dump
            && let Ok(mut f) = f.lock()
        {
            let _ = f.write_all(&(nal.len() as u32).to_le_bytes());
            let _ = f.write_all(nal);
        }

        let Some(decoder) = &self.decoder else {
            return;
        };
        let Ok(mut decoder) = decoder.lock() else {
            return;
        };

        if !decoder.saw_config()
            && let Ok(csd) = self.csd.lock()
            && let Some(csd) = csd.as_ref()
        {
            decoder.push_config(csd);
        }

        let started = Instant::now();
        let frames = decoder.push_access_unit(nal, header_ts.as_nanos() as i64);
        self.decode_ns
            .fetch_add(started.elapsed().as_nanos() as usize, Ordering::SeqCst);

        for frame in &frames {
            if let Some(ctx) = ctx {
                ctx.report_frame_decoded(Duration::from_nanos(frame.pts_ns.max(0) as u64));
            }
            self.reported.fetch_add(1, Ordering::SeqCst);

            let Some(dir) = &self.png_dir else {
                continue;
            };
            let idx = self.png_written.fetch_add(1, Ordering::SeqCst) as u64;
            if self.png_max != 0 && idx >= self.png_max {
                continue;
            }
            if idx % self.png_every != 0 {
                continue;
            }
            let rgb = frame.to_rgb8();
            let path = dir.join(format!("frame_{idx:06}.png"));
            if let Err(e) = png::write_rgb(&path, frame.width as u32, frame.height as u32, &rgb) {
                eprintln!("[framesim] PNG write failed for {}: {e}", path.display());
            }
        }
    }

    /// Whether the decoder can actually take frames yet.
    ///
    /// The streamer sends the HEVC parameter sets (VPS/SPS/PPS) out-of-band in
    /// `ServerControlPacket::DecoderConfig`, and it sends that packet **only in
    /// response to `ClientControlPacket::RequestIdr`**. `client_core` raises that
    /// request when the decoder callback reports it could not take the frame —
    /// which is precisely true until the parameter sets have arrived. So we
    /// report honestly, client_core asks for a keyframe, and the config lands.
    ///
    /// A "null decoder" that always returns true deadlocks here: no keyframe is
    /// ever requested, no parameter sets ever arrive, and nothing decodes. That
    /// is exactly what the first prototype run did.
    fn ready(&self) -> bool {
        let Some(decoder) = &self.decoder else {
            // Decode disabled: transport-harness behaviour, accept everything.
            return true;
        };
        decoder.lock().map(|d| d.decoded > 0).unwrap_or(true)
    }

    /// libde265's warnings, keyed by the sender's frame timestamp.
    fn warnings(&self) -> std::collections::HashMap<i64, String> {
        self.decoder
            .as_ref()
            .and_then(|d| d.lock().ok().map(|d| d.warnings.clone()))
            .unwrap_or_default()
    }

    /// Per-picture content fingerprints, keyed by the sender's frame timestamp.
    fn content_stats(&self) -> std::collections::HashMap<i64, libde265::ContentStat> {
        self.decoder
            .as_ref()
            .and_then(|d| d.lock().ok().map(|d| d.content.clone()))
            .unwrap_or_default()
    }

    /// Classify one picture the same way the grey-frame work does: a real scene
    /// has structure, the compositor's standby frame is a flat fill, and nothing
    /// rendered is black.
    fn classify(st: &libde265::ContentStat) -> &'static str {
        if st.tile_std >= 25.0 {
            "content"
        } else if st.mean < 20.0 {
            "black"
        } else if (st.mean - 128.0).abs() < 4.0 && st.tile_std < 10.0 {
            "grey"
        } else {
            "other"
        }
    }

    fn print_stats(&self) {
        let Some(decoder) = &self.decoder else {
            return;
        };
        let Ok(d) = decoder.lock() else {
            return;
        };
        if d.decoded == 0 && d.pushed_units == 0 {
            return;
        }
        let decode_ms = self.decode_ns.load(Ordering::SeqCst) as f64 / 1e6;
        println!(
            "[framesim] DECODE: {} pictures from {} NAL units ({} errors, {} skipped non-8bit-420)",
            d.decoded, d.pushed_units, d.errors, d.skipped_10bit
        );
        if d.decoded > 0 {
            println!(
                "[framesim]   decode cost: {:.2} ms/frame over {:.0} ms total; {} reported to client_core; {} PNG",
                decode_ms / d.decoded as f64,
                decode_ms,
                self.reported.load(Ordering::SeqCst),
                self.png_written.load(Ordering::SeqCst),
            );
        }
        if let Some(e) = &d.last_error {
            println!("[framesim]   last decoder error: {e}");
        }
        if let Some((mean, max, similar, count)) = d.coherence() {
            println!(
                "[framesim]   coherency: mean luma delta {mean:.2} between consecutive pictures \
                 (max {max:.2}); {similar}/{count} pairs near-identical"
            );
        }
        if d.warned > 0 {
            let total = d.decoded.max(1);
            println!(
                "[framesim]   libde265 warned about {} of {} pictures ({:.1}%):",
                d.warned,
                total,
                100.0 * d.warned as f64 / total as f64
            );
            let mut v: Vec<_> = d.warn_counts.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1));
            for (text, n) in v.into_iter().take(6) {
                println!("[framesim]     {n:>6}x  {text}");
            }
        } else {
            println!("[framesim]   libde265 reported no warnings for any picture");
        }
        if !d.content.is_empty() {
            let mut counts: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for st in d.content.values() {
                *counts.entry(Self::classify(st)).or_default() += 1;
            }
            let total = counts.values().sum::<usize>().max(1);
            let mut sorted: Vec<_> = counts.into_iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(&a.1));
            println!("[framesim]   EXACT content over all {total} decoded pictures:");
            for (class, count) in &sorted {
                println!(
                    "[framesim]     {class:<10} {count:>6}  ({:.1}%)",
                    100.0 * *count as f64 / total as f64
                );
            }
        }
    }
}

/// Decode a captured access-unit stream: repeated `[u32 LE len][bytes]`, with an
/// optional `<path>.csd` sidecar holding the stream's parameter sets.
///
/// This is the offline half of the emulated prototype: one capture from a real
/// run becomes a fixture the decoder can be iterated against (and regressed
/// against) with no streamer, no network and no box.
fn decode_file(path: &Path) -> i32 {
    let mut cfg = DecodeCfg::from_env();
    cfg.enabled = true;

    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[framesim] cannot read fixture {}: {e}", path.display());
            return 5;
        }
    };

    let state = DecodeState::new(&cfg);
    if let Ok(csd) = std::fs::read(format!("{}.csd", path.display()))
        && let Ok(mut slot) = state.csd.lock()
    {
        println!("[framesim] fixture: {} bytes of parameter sets", csd.len());
        *slot = Some(csd);
    }

    println!("[framesim] fixture: {} bytes of access units", data.len());
    let (mut off, mut n) = (0usize, 0usize);
    while off + 4 <= data.len() {
        let len =
            u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        off += 4;
        if off + len > data.len() {
            eprintln!("[framesim] truncated access unit #{n} at byte {off} (declared {len})");
            break;
        }
        // PTS is synthetic: the fixture preserves order, not arrival timing.
        state.on_access_unit(None, Duration::from_millis(n as u64), &data[off..off + len]);
        off += len;
        n += 1;
    }
    println!("[framesim] fixture: fed {n} access units");
    state.print_stats();
    0
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
///
/// Controllers are driven from here too (env `FRAMESIM_CONTROLLERS=0` to turn
/// them off). Sending a *controller motion* for a hand is what makes the driver
/// select the controller device over its separate hand-tracker twin:
/// `server_openvr` sets `isHandTracker = use_separate_hand_trackers &&
/// controller_motion.is_none() && hand_skeleton.is_some()`. With a motion and no
/// skeleton, the controller wins and the hand tracker stays disconnected — which
/// is the "has Touch controllers" state a game expects. With neither, SteamVR
/// sees the device but never tracking, and HL2VR never gets hands.
fn tracking_thread(ctx: Arc<ClientCoreContext>, streaming: Arc<AtomicBool>, origin: Instant) {
    ctx.send_view_params([ViewParams::DUMMY; 2]);

    // The pose must be *unique per sample*, not merely present. A constant pose
    // makes every entry in the streamer's pose history an equally good match, so
    // consecutive frames resolve to the same timestamp and get discarded as
    // duplicates (measured: ~85% discarded with a static pose). A slow yaw sweep
    // plus a small deterministic jitter keeps each sample distinguishable, and
    // keeps the motion plausible.
    let mut lcg: u32 = 0x1234_5678;

    let controllers = controllers_enabled();
    // Buttons go at ~20 Hz rather than at the tracking rate: the server maps each
    // entry and queues a driver event, so the tracking rate would be 270 Hz of
    // input traffic for no gain.
    let button_period = Duration::from_millis(50);
    let mut next_buttons = Instant::now();

    let mut deadline = Instant::now();
    loop {
        if streaming.load(Ordering::SeqCst) {
            lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let jitter = ((lcg >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.002;

            let t = origin.elapsed().as_secs_f32();
            let orientation = Quat::from_rotation_y(t * 0.5) * Quat::from_rotation_z(jitter);

            let mut device_motions = vec![(
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
            )];

            if controllers {
                // Hands in front of the chest, drifting on different periods so
                // each is separately identifiable in a decoded frame and neither
                // looks frozen. Held in the head's frame: `send_tracking` poses
                // are played back relative to the head pose, so this stays in
                // front of the user as the head sweeps.
                let lx = -0.22 + 0.08 * (t * 0.7).sin();
                let ly = 1.10 + 0.05 * (t * 0.9).sin();
                let rx = 0.22 + 0.08 * (t * 0.7).cos();
                let ry = 1.10 + 0.05 * (t * 1.1).sin();

                device_motions.push((
                    *HAND_LEFT_ID,
                    DeviceMotion {
                        pose: Pose {
                            orientation: Quat::from_rotation_x(-0.9),
                            position: Vec3::new(lx, ly, -0.35),
                        },
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ));
                device_motions.push((
                    *HAND_RIGHT_ID,
                    DeviceMotion {
                        pose: Pose {
                            orientation: Quat::from_rotation_x(-0.9),
                            position: Vec3::new(rx, ry, -0.35),
                        },
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ));
            }

            ctx.send_tracking(TrackingData {
                poll_timestamp: origin.elapsed(),
                device_motions,
                hand_skeletons: [None, None],
                face: FaceData::default(),
                body: None,
            });

            if controllers && Instant::now() >= next_buttons {
                next_buttons = Instant::now() + button_period;
                ctx.send_buttons(controller_buttons(t));
            }
        }
        deadline += Duration::from_micros(FRAME_INTERVAL_US / 3);
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
    }
}

/// Controller tracking is on by default: without it SteamVR registers the
/// devices but never tracks them, so a game gets no hands at all. `=0` restores
/// the pre-controller harness for A/B.
fn controllers_enabled() -> bool {
    std::env::var("FRAMESIM_CONTROLLERS").map_or(true, |v| v != "0")
}

/// A deterministic, always-moving input pattern so the button path is exercised
/// and is obviously the harness rather than a stuck controller.
///
/// Split by hand, because the Frame controller's two halves are not
/// interchangeable: the left has a d-pad, View and Bumper, the right has
/// A/B/X/Y, Menu and Bumper. Both triggers, both grips and both thumbsticks
/// sweep on different periods; the buttons toggle as edges.
fn controller_buttons(t: f32) -> Vec<ButtonEntry> {
    let scalar = |path_id: u64, value: f32| ButtonEntry {
        path_id,
        value: ButtonValue::Scalar(value),
    };
    let binary = |path_id: u64, value: bool| ButtonEntry {
        path_id,
        value: ButtonValue::Binary(value),
    };
    // A rotating one-hot over the hand's four face/d-pad directions.
    let rotate = |period: f32| ((t / period) as i32).rem_euclid(4);

    let left_dpad = [
        *LEFT_DPAD_UP_CLICK_ID,
        *LEFT_DPAD_RIGHT_CLICK_ID,
        *LEFT_DPAD_DOWN_CLICK_ID,
        *LEFT_DPAD_LEFT_CLICK_ID,
    ];
    let right_face = [
        *RIGHT_A_CLICK_ID,
        *RIGHT_B_CLICK_ID,
        *RIGHT_X_CLICK_ID,
        *RIGHT_Y_CLICK_ID,
    ];

    let mut entries = vec![
        // Both hands: trigger, grip and thumbstick, on distinct periods.
        scalar(*LEFT_TRIGGER_VALUE_ID, 0.5 + 0.5 * (t * 0.8).sin()),
        scalar(*RIGHT_TRIGGER_VALUE_ID, 0.5 + 0.5 * (t * 1.3).cos()),
        scalar(*LEFT_SQUEEZE_VALUE_ID, 0.5 + 0.5 * (t * 0.6).cos()),
        scalar(*RIGHT_SQUEEZE_VALUE_ID, 0.5 + 0.5 * (t * 1.1).sin()),
        scalar(*LEFT_THUMBSTICK_X_ID, (t * 0.5).sin()),
        scalar(*LEFT_THUMBSTICK_Y_ID, (t * 0.5).cos()),
        scalar(*RIGHT_THUMBSTICK_X_ID, (t * 0.4).cos()),
        scalar(*RIGHT_THUMBSTICK_Y_ID, (t * 0.4).sin()),
        // Bumper exists on both.
        binary(*LEFT_BUMPER_CLICK_ID, (t * 1.9).sin() > 0.0),
        binary(*RIGHT_BUMPER_CLICK_ID, (t * 2.3).sin() > 0.0),
        // Left-only: View and the d-pad.
        binary(*LEFT_VIEW_CLICK_ID, (t * 0.9).sin() > 0.7),
    ];
    for (i, id) in left_dpad.into_iter().enumerate() {
        entries.push(binary(id, rotate(1.3) == i as i32));
    }
    // Right-only: Menu and A/B/X/Y.
    entries.push(binary(*RIGHT_MENU_CLICK_ID, (t * 1.1).sin() > 0.7));
    for (i, id) in right_face.into_iter().enumerate() {
        entries.push(binary(id, rotate(0.9) == i as i32));
    }

    entries
}

/// The client's *active* input set, advertised so the streamer can build its
/// button mapping table. For the Steam Frame this is the Frame controller's own
/// profile, so a Frame-emulating server maps every input one-to-one instead of
/// translating between two different controllers. Returns `None` if the profile
/// is unknown.
fn frame_button_set() -> Option<HashSet<u64>> {
    CONTROLLER_PROFILE_INFO
        .get(&FRAME_CONTROLLER_PROFILE_ID)
        .map(|info| info.button_set.clone())
}

fn main() {
    env_logger::init();

    // Offline mode: decode a captured access-unit stream. No networking, no
    // streamer — used to iterate on the decoder without the box, and as the
    // regression-fixture runner for a real captured bitstream.
    if let Ok(path) = std::env::var("FRAMESIM_DECODE_FILE") {
        exit(decode_file(Path::new(&path)));
    }

    // A Steam Frame-ish capability set per ADR-0008: HEVC-capable, foveated
    // encoding on; no AV1 and no 10-bit, because the Frame kernel decodes
    // neither. The streamer negotiates down from here.
    //
    // The refresh-rate list is what the streamer paces the encoder to (it picks
    // the closest advertised rate to its own `preferred_fps`). Under qemu-user
    // software decode we cannot keep up with 72 Hz, and a client that drops 90%
    // of frames cannot reconstruct P-frames at all — it decodes noise. So the
    // rate list is overridable, to run the emulator at a rate it can actually
    // decode. On real hardware this list reflects the panel.
    let refresh_rates = std::env::var("FRAMESIM_REFRESH_RATES")
        .ok()
        .map(|s| {
            s.split(',')
                .filter_map(|p| p.trim().parse::<f32>().ok())
                .collect::<Vec<f32>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![60.0, 72.0, 80.0, 90.0, 120.0]);

    // The client owns its render resolution — that is the design (ADR per the
    // north star: the device decides and the streamer obeys). So when the
    // emulator needs a cheaper stream it advertises a smaller view rather than
    // reaching over and editing the streamer's transcoding setting. The server
    // clamps its own choice to this maximum, so this is the honest lever.
    // "512" or "512x480"; default is the Steam Frame's panel.
    let view_resolution = std::env::var("FRAMESIM_VIEW_RESOLUTION")
        .ok()
        .and_then(|s| {
            let s = s.trim().to_ascii_lowercase();
            let (w, h) = match s.split_once('x') {
                Some((w, h)) => (w.parse::<u32>().ok()?, h.parse::<u32>().ok()?),
                None => {
                    let n = s.parse::<u32>().ok()?;
                    (n, n)
                }
            };
            Some(UVec2::new(w, h))
        })
        .unwrap_or_else(|| UVec2::new(2160, 2160));

    let capabilities = ClientCapabilities {
        platform: alvr_system_info::platform(None, None),
        default_view_resolution: view_resolution,
        max_view_resolution: view_resolution,
        refresh_rates: refresh_rates.clone(),
        // The streamer only enables foveated encoding if the client advertises
        // the capability (or the setting forces it), so this is a one-word way to
        // run the same rig with FFR on or off — no settings edit, and therefore no
        // invalidated restart hash and no wasted connection cycle.
        foveated_encoding: std::env::var("FRAMESIM_FOVEATED").map_or(true, |v| v != "0"),
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
    println!("[framesim] advertising refresh rates {refresh_rates:?} at view {view_resolution}");
    println!(
        "[framesim] controllers: {}",
        if controllers_enabled() {
            "on (Steam Frame profile, synthetic hands + moving buttons)"
        } else {
            "OFF (FRAMESIM_CONTROLLERS=0)"
        }
    );

    let ctx = Arc::new(ClientCoreContext::new(capabilities, vec![]));

    // Emulated-prototype decode. Absent unless FRAMESIM_DECODE=1, so the
    // transport harness behaves exactly as before.
    let decode = Arc::new(DecodeState::new(&DecodeCfg::from_env()));

    // A null decoder: accept every frame and record it. Returning `false` is
    // read as decoder saturation and makes the client spam RequestIdr, so always
    // accept. This is what lets the sim prove real video arrived with no
    // hardware decoder in the loop.
    let frames = Arc::new(AtomicUsize::new(0));
    let bytes = Arc::new(AtomicUsize::new(0));
    let samples: Arc<Mutex<Vec<Sample>>> = Arc::new(Mutex::new(Vec::new()));
    let first = Arc::new(AtomicBool::new(true));
    let warned_not_ready = Arc::new(AtomicBool::new(false));
    // Foveation centres the streamer sent with each frame, via the real metadata
    // API the compositor uses (report_compositor_start).
    let fov: Arc<Mutex<Vec<[[f32; 2]; 2]>>> = Arc::new(Mutex::new(Vec::new()));
    let fov_seen = Arc::clone(&fov);
    let ctx_for_cb = Arc::clone(&ctx);
    let decode_cb = Arc::clone(&decode);

    // Decode runs on its own thread, **not** in the receive loop.
    //
    // The receive loop calls the callback synchronously, so a callback that decodes stops the loop
    // reading for the whole decode. On a machine that cannot decode in real time — this rig, a
    // 2-core host running an aarch64 client under qemu-user — the media socket then backs up and
    // the kernel discards datagrams, which presents as network loss and is nothing of the kind.
    // Measured before this change: the media socket's drop counter climbing ~4000/s while the
    // callback was busy, and the plane reporting `0 repaired by FEC` because ~50 % loss is past
    // what any code can cover. The real client (`client_openxr`) has always enqueued to its decoder
    // thread for exactly this reason; the harness now does the same.
    //
    // The queue is small and bounded, and a full queue **drops the unit and counts it**: the
    // alternative — blocking — backs the socket up and loses every frame's fragments rather than
    // one frame.
    let (au_tx, au_rx) = std::sync::mpsc::sync_channel::<(Duration, Vec<u8>)>(8);
    let decode_for_worker = Arc::clone(&decode);
    let ctx_for_worker = Arc::clone(&ctx);
    thread::spawn(move || {
        while let Ok((header_ts, nal)) = au_rx.recv() {
            decode_for_worker.on_access_unit(Some(&ctx_for_worker), header_ts, &nal);
        }
    });
    let au_dropped = Arc::new(AtomicUsize::new(0));

    let frames_seen = Arc::clone(&frames);
    let bytes_seen = Arc::clone(&bytes);
    let samples_seen = Arc::clone(&samples);
    let first_frame = Arc::clone(&first);
    ctx.set_decoder_input_callback(Box::new(move |header_ts, nal| {
        let n = frames_seen.fetch_add(1, Ordering::SeqCst) + 1;
        bytes_seen.fetch_add(nal.len(), Ordering::SeqCst);
        // ADR-0011's frame identity, read synchronously from the receive loop, which set it
        // immediately before calling us. Without this the capture has no key to join against
        // the server's own record of what it transmitted, and the two candidate causes of a
        // missing frame — the server discarding it, and the network losing it — are
        // indistinguishable.
        let frame_id = ctx_for_cb.current_video_frame();
        let arrived = Instant::now();
        if let Some(meta) = ctx_for_cb.report_compositor_start(header_ts)
            && let Some(shifts) = meta.foveation_center_shifts
            && let Ok(mut v) = fov_seen.lock()
        {
            v.push(shifts);
        }

        // Real decode: this is what turns "packets arrived" into "the client can
        // actually show this". Timed, because a client that cannot decode inside a frame
        // interval is the leading hypothesis for the loss: the receive loop calls us
        // synchronously, so a slow decode backs the socket up and the kernel starts
        // discarding — which looks exactly like the network losing frames.
        // Hand the access unit to the decode thread and return at once — see the thread above for
        // why the loop must not decode. What remains here is only what has to be read
        // *synchronously*: the frame identity, the compositor metadata and the counters.
        if au_tx.try_send((header_ts, nal.to_vec())).is_err() {
            au_dropped.fetch_add(1, Ordering::SeqCst);
        }
        let client_ms = arrived.elapsed().as_secs_f64() * 1e3;

        if let Ok(mut s) = samples_seen.lock() {
            s.push(Sample {
                at: arrived,
                size: nal.len(),
                header_ts,
                frame_index: frame_id.map(|f| f.frame_index),
                missed_frames: frame_id.map_or(0, |f| f.missed_frames),
                had_datagram_loss: frame_id.is_some_and(|f| f.had_datagram_loss),
                client_ms,
            });
        }

        if first_frame.swap(false, Ordering::SeqCst) || n % 60 == 0 {
            println!(
                "[framesim] video frame #{n} ts={header_ts:?} bytes={} au_dropped={}",
                nal.len(),
                au_dropped.load(Ordering::SeqCst),
            );
        }

        // Reporting "not yet" is what makes client_core ask for a keyframe, which
        // is what makes the streamer send the parameter sets. See ready().
        let ready = decode_cb.ready();
        if !ready && !warned_not_ready.swap(true, Ordering::SeqCst) {
            println!(
                "[framesim] decoder not ready (no parameter sets yet) — requesting a keyframe"
            );
        }
        ready
    }));

    let want_frames = frame_target();

    // Tracking must start as soon as there is a connection: the streamer cannot
    // identify a single frame without a pose history to match against.
    let streaming = Arc::new(AtomicBool::new(false));
    let tracking_origin = Instant::now();
    {
        let ctx_for_tracking = Arc::clone(&ctx);
        let streaming_for_tracking = Arc::clone(&streaming);
        thread::spawn(move || {
            tracking_thread(ctx_for_tracking, streaming_for_tracking, tracking_origin)
        });
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
                    // The active interaction profile only means anything once
                    // there is a control socket to carry it, which is why it is
                    // sent here and not alongside the tracking thread's start.
                    if controllers_enabled() {
                        match frame_button_set() {
                            Some(input_ids) => {
                                for device_id in [*HAND_LEFT_ID, *HAND_RIGHT_ID] {
                                    ctx.send_active_interaction_profile(
                                        device_id,
                                        *FRAME_CONTROLLER_PROFILE_ID,
                                        input_ids.clone(),
                                    );
                                }
                                println!(
                                    "[framesim] controllers: Steam Frame profile advertised ({} inputs) for both hands",
                                    input_ids.len()
                                );
                            }
                            None => eprintln!(
                                "[framesim] controllers: Steam Frame profile missing from CONTROLLER_PROFILE_INFO"
                            ),
                        }
                    }
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
                    if let Ok(mut csd) = decode.csd.lock() {
                        *csd = Some(config_nal.clone());
                    }
                    if let Ok(dump) = std::env::var("FRAMESIM_DUMP_NALS") {
                        let _ = std::fs::write(format!("{dump}.csd"), &config_nal);
                    }
                    if let Some(decoder) = &decode.decoder
                        && let Ok(mut d) = decoder.lock()
                    {
                        d.push_config(&config_nal);
                    }
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
            let code = report(&samples, &bytes, &fov, &decode, start);
            decode.print_stats();
            exit(code);
        }

        if start.elapsed() > limit {
            eprintln!(
                "[framesim] TIMEOUT after {:?} (frames={}); last hud: {last_hud}",
                start.elapsed(),
                frames.load(Ordering::SeqCst)
            );
            // A timeout is the *normal* end of a fixed-duration capture — the box script
            // streams for 100 s and then stops — so it must still produce the CSV and the
            // loss report. It used to print decode stats and exit, which silently threw the
            // whole capture away: the first instrumented run reported nothing at all.
            report(&samples, &bytes, &fov, &decode, start);
            decode.print_stats();
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
    decode: &Arc<DecodeState>,
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
    //
    // The interval is **derived, not assumed**. It used to be `FRAME_INTERVAL_US`, a
    // hardcoded 90 Hz — and the emulated runs are pace-driven by whatever display rate the
    // session negotiated, which was 30-45 Hz in every capture taken so far. With the wrong
    // interval the deviation accumulates at (real - assumed) per frame and every number in
    // the report is wrong by an amount proportional to the run length. Deriving it: loss only
    // ever *multiplies* the true inter-arrival, so the smallest common observed gap is the
    // source interval.
    let interval = {
        let mut gaps: Vec<f64> = (1..n)
            .map(|i| s[i].header_ts.as_micros() as f64 - s[i - 1].header_ts.as_micros() as f64)
            .filter(|g| *g > 0.0)
            .collect();
        if gaps.is_empty() {
            FRAME_INTERVAL_US as f64 / 1000.0
        } else {
            gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
            // The median of the smallest decile: robust to loss, and biased high if more
            // than half the frames were lost, which makes the loss figure below a floor.
            gaps[gaps.len() / 20] / 1000.0
        }
    };
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

    // ---- ADR-0011's frame identity: the loss, exactly, and who caused it ----------------
    //
    // The server allocates `frame_index` once per frame at the earliest point the frame
    // exists, and it reaches here. So the loss is a *set difference*, not an estimate from
    // timestamps: frames the source produced, minus frames we decoded, and the shortfall is
    // attributed by which ones never arrived.
    {
        let ids: Vec<u64> = s.iter().filter_map(|x| x.frame_index).collect();
        if ids.len() >= 2 {
            let first = ids[0];
            let last = ids[ids.len() - 1];
            let produced = last - first + 1;
            let received = ids.len() as u64;
            let lost = produced.saturating_sub(received);
            println!(
                "[framesim] LOSS (by frame identity, exact): source frames {first}..={last} = \
                 {produced}, decoded {received}, missing {lost} ({:.1}%)",
                100.0 * lost as f64 / produced.max(1) as f64
            );

            let with_loss = s.iter().filter(|x| x.had_datagram_loss).count();
            println!(
                "[framesim]   of the frames that did arrive: {with_loss} had datagrams lost \
                 inside them (partial), {} arrived whole",
                received as usize - with_loss
            );
            println!(
                "[framesim]   server-reported cumulative discards at the last frame: {}",
                s[n - 1].missed_frames
            );

            // What the *socket layer itself* threw away. This is the counter that decides
            // between "the wire lost it" and "we did": a datagram discarded here was read out of
            // the socket successfully, so the kernel's own drop counter stays at zero and the
            // loss is indistinguishable from the network's.
            let discarded = alvr_sockets::datagrams_discarded_no_buffer();
            let read = alvr_sockets::datagrams_read();
            println!(
                "[framesim]   socket layer: {read} datagrams read, {discarded} discarded for want \
                 of a free buffer ({:.2}% of reads)",
                100.0 * discarded as f64 / read.max(1) as f64
            );

            // The client's own per-frame cost. The receive loop calls the decode callback
            // synchronously, so this is the time the receive path spends per frame — and if
            // it exceeds the frame interval, the socket backs up and the *kernel* starts
            // discarding, which is indistinguishable from the network losing frames unless
            // you measure it.
            let mut cost: Vec<f64> = s.iter().map(|x| x.client_ms).collect();
            cost.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let over = cost.iter().filter(|c| **c > interval).count();
            println!(
                "[framesim] CLIENT COST: decode-callback p50 {:.2} ms, p90 {:.2} ms, p99 {:.2} \
                 ms, max {:.2} ms; frame interval derived at {:.2} ms ({:.1} Hz); {over}/{} \
                 frames ({:.1}%) exceeded it",
                percentile(&cost, 0.5),
                percentile(&cost, 0.9),
                percentile(&cost, 0.99),
                cost[cost.len() - 1],
                interval,
                1000.0 / interval,
                n,
                100.0 * over as f64 / n as f64,
            );
        } else {
            println!(
                "[framesim] LOSS: no frame identity reached the client — `frame_index` is \
                 absent or not advancing, so the loss cannot be attributed. Fix that first; \
                 every other number here is uninterpretable without it."
            );
        }
    }
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
            if s[i].size > median_size * 2 {
                "yes"
            } else {
                ""
            },
            s[i].header_ts.as_micros()
        );
    }

    // Is the tail explained by big frames (IDRs) rather than by the transport?
    let outlier_idx: Vec<usize> = (0..n)
        .filter(|&i| centred[i].abs() > DEADLINES_MS[0].0)
        .collect();
    let big = outlier_idx
        .iter()
        .filter(|&&i| s[i].size > median_size * 2)
        .count();
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
                let content = decode.content_stats();
                let warns = decode.warnings();
                let _ = writeln!(
                    f,
                    "idx,frame_index,header_ts_us,size_bytes,dev_ms,gap_ms,client_ms,missed_frames,datagram_loss,mean_luma,luma_std,tile_std,class,de265_warning"
                );
                let mut counts: std::collections::HashMap<&str, usize> =
                    std::collections::HashMap::new();
                let mut missing = 0usize;
                for i in 0..n {
                    let gap = if i == 0 {
                        0.0
                    } else {
                        s[i].at.duration_since(s[i - 1].at).as_secs_f64() * 1e3
                    };
                    let key = s[i].header_ts.as_nanos() as i64;
                    let (mean, std, tile, class) = match content.get(&key) {
                        Some(st) => (
                            format!("{:.2}", st.mean),
                            format!("{:.2}", st.std),
                            format!("{:.2}", st.tile_std),
                            DecodeState::classify(st),
                        ),
                        None => {
                            missing += 1;
                            ("".into(), "".into(), "".into(), "undecoded")
                        }
                    };
                    *counts.entry(class).or_default() += 1;
                    let warn = warns
                        .get(&key)
                        .map(|w| w.replace(',', ";"))
                        .unwrap_or_default();
                    let _ = writeln!(
                        f,
                        "{},{},{},{},{:.3},{:.3},{:.3},{},{},{},{},{},{},{}",
                        i,
                        s[i].frame_index.map_or(-1i64, |v| v as i64),
                        s[i].header_ts.as_micros(),
                        s[i].size,
                        centred[i],
                        gap,
                        s[i].client_ms,
                        s[i].missed_frames,
                        s[i].had_datagram_loss as u8,
                        mean,
                        std,
                        tile,
                        class,
                        warn
                    );
                }
                let total = counts.values().sum::<usize>().max(1);
                let mut sorted: Vec<_> = counts.into_iter().collect();
                sorted.sort_by(|a, b| b.1.cmp(&a.1));
                println!("[framesim] per-frame CSV written to {path}");
                println!(
                    "[framesim] EXACT content over all {total} received frames (not sampled):"
                );
                for (class, count) in &sorted {
                    println!(
                        "[framesim]   {class:<10} {count:>6}  ({:.1}%)",
                        100.0 * *count as f64 / total as f64
                    );
                }
                if missing > 0 {
                    println!(
                        "[framesim]   ({missing} frames had no decoded picture to fingerprint)"
                    );
                }
            }
            Err(e) => eprintln!("[framesim] could not write CSV {path}: {e}"),
        }
    }

    let _ = start;
    if gate_ok { 0 } else { 3 }
}
