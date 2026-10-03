//! Minimal streamer-side ("PC sim") for Phase 3 M1.
//!
//! Dials a headset sim's control port and performs the **real** server half of
//! the ALVR handshake, using alvr_sockets' own helper
//! (`SocketConnection::from_client_connection`) — the same code path the
//! production server uses. This is the counterpart to `x_framesim` running
//! under qemu on the sim host.
//!
//! After the handshake it pushes a video stream over the **real** stream socket
//! (unreliable/VIDEO, the same channel the production server uses) so the whole
//! client-side video path can be exercised: framing, fragmentation, timestamps,
//! IDR/RequestIdr handling and the decoder-input callback.
//!
//! Usage: pcsim <client-ip> [pcsim-frames] [stream-port]
//!   PCSIM_FRAMES=N    frames to send (default 90; 0 = handshake only)
//!   PCSIM_PAYLOAD=N   bytes per frame payload (default 20000)
//! Exits 0 on success.

use alvr_common::{
    AlvrFoveatedEncodingParams, ViewParams,
    glam::{Quat, UVec2},
};
use alvr_packets::{
    ClientConnectionResult, ClientNegotiatedStreamingConfig, NegotiatedStreamingConfigExt,
    ServerControlPacket, StreamConfigPacket, VIDEO, VideoPacketHeader,
};
use alvr_session::SessionConfig;
use alvr_sockets::{
    ControlSocketSender, SocketConnection, StreamSender, StreamSocketConfig, connect_to_client,
};
use std::{
    env,
    net::IpAddr,
    process::exit,
    thread,
    time::Instant as StdInstant,
    time::{Duration, Instant},
};

#[cfg(windows)]
mod foveation;
#[cfg(windows)]
mod nvenc;

const TIMEOUT: Duration = Duration::from_secs(10);
/// Foveation: fraction of each axis in the sharp centre region.
const PCSIM_FOV_CENTER: f32 = 0.35;
/// Foveation: periphery degradation ratio.
const PCSIM_FOV_EDGE_RATIO: f32 = 2.0;
/// Synthetic gaze amplitude, radians. PCSIM_GAZE=0 holds the centre still,
/// which is the honest way to measure what foveation buys in compression: with a
/// moving centre the blurred region moves too, and on a low-motion source that
/// motion dominates the frame difference and swamps the effect.
fn gaze_rad() -> f32 {
    std::env::var("PCSIM_GAZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.25)
}

/// Per-frame trace path, for joining against the client's CSV.
const PCSIM_CSV: &str = "pcsim_frames.csv";
/// 90 Hz.
const FRAME_INTERVAL: Duration = Duration::from_micros(11_111);
const FRAME_INTERVAL_US: u64 = 11_111;

/// Where video frames come from.
enum Source {
    /// Padding bytes: exercises the transport and framing only.
    Synthetic(Vec<u8>),
    /// Real NVENC bitstream from synthetic patterned textures (Windows).
    #[cfg(windows)]
    Nvenc(nvenc::Feeder),
    /// Real NVENC bitstream from Desktop Duplication (Windows).
    #[cfg(windows)]
    Dda(nvenc::DdaFeeder),
}

impl Source {
    /// (count, p50_ms, p99_ms, mean_ms) when the source is NVENC.
    fn encode_stats(&self) -> Option<(usize, f64, f64, f64)> {
        #[cfg(windows)]
        match self {
            Source::Nvenc(f) => return Some(f.encode_stats()),
            Source::Dda(f) => return Some(f.encode_stats()),
            _ => {}
        }
        None
    }

    /// Capture-side stats when the source has them: (desktop frames, empty polls).
    fn capture_stats(&self) -> Option<(u64, u64)> {
        #[cfg(windows)]
        if let Source::Dda(f) = self {
            return Some(f.capture_stats());
        }
        None
    }

    /// Returns (bitstream, encode_ms). Synthetic has no encode step.
    /// `center`/`center_size` are the foveation region applied before encoding.
    fn next_frame(&mut self, center: [f32; 2], center_size: f32) -> Result<(Vec<u8>, f64), String> {
        match self {
            Source::Synthetic(p) => Ok((p.clone(), 0.0)),
            #[cfg(windows)]
            Source::Nvenc(f) => {
                let b = f.encode_next(center, center_size)?;
                Ok((b, f.last_encode_ms()))
            }
            #[cfg(windows)]
            Source::Dda(f) => {
                let b = f.encode_next(center, center_size)?;
                Ok((b, f.last_encode_ms()))
            }
        }
    }
}

/// Real NVENC when it is available and asked for, else a synthetic payload.
/// The encoder is fed VR-resolution frames (never the desktop's), because that
/// is what the headset consumes and the resolution must stay negotiable.
fn make_source(view_w: u32, view_h: u32, payload_len: usize) -> (Source, u32, u32) {
    #[cfg(windows)]
    {
        let which = env::var("PCSIM_SOURCE").unwrap_or_else(|_| "pattern".to_string());
        // Desktop Duplication: capture arrives at the DESKTOP's resolution and we
        // negotiate exactly that (no scaling here; the correct VR-resolution
        // answer is a virtual display, see VD_RE/23 section 8.2).
        if which == "dda" {
            let adapter: u32 = env::var("PCSIM_DDA_ADAPTER")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let output: u32 = env::var("PCSIM_DDA_OUTPUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            match nvenc::DdaFeeder::new(adapter, output, 90) {
                Ok((f, w, h)) => {
                    println!(
                        "[pcsim] video source: DDA {w}x{h} on adapter {adapter} output {output}"
                    );
                    println!(
                        "[pcsim]   note: a static desktop yields near-empty P-frames -- not a valid bitrate sample"
                    );
                    return (Source::Dda(f), w, h);
                }
                Err(e) => {
                    eprintln!(
                        "[pcsim] DDA unavailable ({e}); falling back to the patterned NVENC source"
                    );
                }
            }
        }
        let want = which != "synthetic";
        if want {
            match nvenc::Feeder::new(view_w, view_h, 90) {
                Ok(f) => {
                    println!("[pcsim] video source: NVENC {view_w}x{view_h} (8-bit)");
                    return (Source::Nvenc(f), view_w, view_h);
                }
                Err(e) => eprintln!("[pcsim] NVENC unavailable ({e}); using synthetic payload"),
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (view_w, view_h);
    println!("[pcsim] video source: synthetic {payload_len}B payload");
    (Source::Synthetic(vec![0u8; payload_len]), view_w, view_h)
}

fn main() {
    env_logger::init();

    let args: Vec<String> = env::args().collect();
    let ip: IpAddr = args
        .get(1)
        .expect("usage: pcsim <client-ip> [pcsim-frames] [stream-port]")
        .parse()
        .expect("bad client ip");
    let frames: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(90);
    let payload_len: usize = env::var("PCSIM_PAYLOAD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    // Negotiable: render resolution may change, so it is a parameter, not a
    // constant. Defaults to the Frame's per-eye resolution.
    let view_w: u32 = env::var("PCSIM_VIEW_W")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2160);
    let view_h: u32 = env::var("PCSIM_VIEW_H")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2160);

    println!("[pcsim] dialling headset sim at {ip} (control port 9943)");

    // 1. Connect and read the client's capabilities.
    let (control_socket, client_ip, result): (_, _, ClientConnectionResult) =
        match connect_to_client(vec![ip], TIMEOUT) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[pcsim] FAILED to connect/read capabilities: {e}");
                exit(1);
            }
        };

    match &result {
        ClientConnectionResult::ConnectionAccepted(info) => {
            println!(
                "[pcsim] client accepted: platform={} protocol_id={}",
                info.platform_string, info.client_protocol_id
            );
            if let Some(caps) = &info.streaming_capabilities {
                println!(
                    "[pcsim]   caps: view={}x{} refresh={:?} foveated={} av1={} 10bit={}",
                    caps.default_view_resolution.x,
                    caps.default_view_resolution.y,
                    caps.refresh_rates,
                    caps.foveated_encoding,
                    caps.encoder_av1,
                    caps.encoder_10_bits,
                );
            }
            println!("[pcsim] M1 STEP 1 OK: client capabilities received");
        }
        ClientConnectionResult::ClientStandby => {
            eprintln!("[pcsim] client is in standby, not accepting");
            exit(1);
        }
    }

    // 2. Offer the stream config and run the rest of the server-side handshake.
    //    SessionConfig::default() is a valid session for a control-plane test;
    //    a real server would use its negotiated session here.
    // Build the session once: the client will listen for the video socket on
    // THIS session's stream_port, so the server must dial the very same value.
    // Pick the video source first, then negotiate ITS resolution. Taking the
    // resolution from the source guarantees the two cannot drift -- hard-coded
    // port/protocol is exactly what silently broke the stream earlier while the
    // handshake still reported success.
    let (mut source, view_w, view_h) = make_source(view_w, view_h, payload_len);

    // Foveation: the 300 Mbps target is WITH foveation -- the periphery is what
    // we spend the savings on. Negotiate the params and drive the per-frame
    // centres from the REAL x-foveation code path (not a hard-coded value), so
    // this exercises the same math the product uses.
    let foveation_params = AlvrFoveatedEncodingParams {
        encoded_view_resolution: [view_w, view_h],
        view_ratio: [1.0, 1.0],
        // Fraction of each axis in the sharp centre region (ALVR's centre_size).
        center_size: [PCSIM_FOV_CENTER, PCSIM_FOV_CENTER],
        center_shifts: [[0.0, 0.0]; 2],
        edge_ratio: [PCSIM_FOV_EDGE_RATIO, PCSIM_FOV_EDGE_RATIO],
    };
    println!(
        "[pcsim] foveation: centre {:.0}% of axis, edge_ratio {:.1}, encoded {}x{}",
        foveation_params.center_size[0] * 100.0,
        foveation_params.edge_ratio[0],
        view_w,
        view_h
    );
    // x-foveation drives the per-frame centres; see the params above.
    let mut foveation =
        x_foveation::EyeTrackedFoveation::new(foveation_params.clone(), UVec2::new(view_w, view_h));
    // The streamer needs the client's eye poses/FOV before it will compute any
    // centres -- update() no-ops until view_params is populated ("wait for this
    // client's real eye poses and FOV, including after a reconnect"). In the
    // product that arrives from the headset; here we supply it directly.
    // ViewParams::DUMMY carries a well-formed FOV (-1..1 on both axes).
    foveation.view_params = Some([ViewParams::DUMMY; 2]);
    let session_config = SessionConfig::default();
    let session_settings = session_config.to_settings();
    let stream_port: u16 = session_settings.connection.stream_port;
    let stream_protocol = session_settings.connection.stream_protocol;
    println!("[pcsim] session video socket port = {stream_port}");

    let stream_config_packet = match StreamConfigPacket::new(
        &session_config,
        ClientNegotiatedStreamingConfig {
            view_resolution: UVec2::new(view_w, view_h),
            refresh_rate_hint: 90.0,
            game_audio_sample_rate: 48000,
            foveated_encoding: Some(foveation_params.clone()),
            encoding_gamma: 1.0,
            enable_hdr: false,
            wired: false,
            ext_str: String::new(),
        }
        .with_ext(NegotiatedStreamingConfigExt {}),
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[pcsim] FAILED to build StreamConfigPacket: {e}");
            exit(1);
        }
    };

    println!("[pcsim] sending StreamConfig + StartStream, awaiting StreamReady...");
    let socket = match SocketConnection::from_client_connection(
        control_socket,
        TIMEOUT,
        stream_config_packet,
        StreamSocketConfig {
            protocol: stream_protocol,
            port: stream_port,
            buffer_config: Default::default(),
            max_packet_size: 1400,
            dscp: None,
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[pcsim] handshake failed: {e}");
            exit(1);
        }
    };
    println!("[pcsim] M1 OK: control-plane handshake completed with {client_ip}");

    if frames == 0 {
        exit(0);
    }

    // 3. Video path: real unreliable VIDEO stream, same as the production server.
    let mut video_sender: StreamSender<VideoPacketHeader> = socket.request_unreliable_stream(VIDEO);
    let mut control_sender: ControlSocketSender<ServerControlPacket> =
        match socket.request_reliable_stream() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[pcsim] could not open the reliable control stream: {e}");
                exit(1);
            }
        };

    let t0 = Instant::now();
    let mut base = t0;
    let mut total_bytes: u64 = 0;
    let mut max_frame = 0usize;
    let mut send_late_ms: Vec<f64> = Vec::new();
    // Per-frame trace, joinable with the client's on the header timestamp.
    let mut trace: Vec<(u64, usize, f64, f64)> = Vec::new();
    let _ = FRAME_INTERVAL;
    println!(
        "[pcsim] streaming {frames} frames over VIDEO (target {} fps)",
        1_000_000 / FRAME_INTERVAL_US
    );

    for i in 0..frames {
        // The client drops the session if the control stream goes quiet
        // (KEEPALIVE_TIMEOUT = 2s), so keep it fed.
        if i % 45 == 0
            && let Err(e) = control_sender.send(&ServerControlPacket::KeepAlive)
        {
            eprintln!("[pcsim] keepalive failed at frame {i}: {e}");
            exit(1);
        }

        // Synthetic moving gaze until real eye tracking is wired in. Oscillates
        // a few degrees so the centres actually move and the client sees them
        // change frame to frame.
        let ts = t0.elapsed();
        let secs = ts.as_secs_f64();
        let amp = gaze_rad();
        let gaze = Quat::from_rotation_y((secs * 0.8).sin() as f32 * amp)
            * Quat::from_rotation_x((secs * 0.5).cos() as f32 * amp);
        foveation.update(ts, Some(gaze), StdInstant::now());
        let shifts = foveation.centers(ts);
        if i == 0 || i % 90 == 0 {
            println!("[pcsim] foveation centres: {shifts:?}");
        }

        let header = VideoPacketHeader {
            frame_index: i as u64,
            timestamp: ts,
            global_view_params: [ViewParams::DUMMY; 2],
            foveation_center_shifts: shifts,
            // The client starts every session stream_corrupted; without a first
            // IDR it would drop frames until one arrives.
            is_idr: i == 0 || i % 120 == 0,
        };

        let fov_center = shifts.map(|s| s[0]).unwrap_or([0.0, 0.0]);
        let (payload, encode_ms) = match source.next_frame(fov_center, PCSIM_FOV_CENTER) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[pcsim] frame {i} encode failed: {e}");
                exit(1);
            }
        };
        total_bytes += payload.len() as u64;
        max_frame = max_frame.max(payload.len());
        if i == 0 || i % 90 == 0 {
            println!("[pcsim] frame {i}: {} bytes", payload.len());
        }

        if let Err(e) = video_sender.send_header_with_payload(&header, &payload) {
            eprintln!("[pcsim] video send failed at frame {i}: {e}");
            exit(1);
        }
        trace.push((
            header.timestamp.as_micros() as u64,
            payload.len(),
            encode_ms,
            send_late_ms.last().copied().unwrap_or(0.0),
        ));

        // Pace against absolute deadlines. Sleeping the interval AFTER encoding
        // adds the encode time to the frame period (we measured 11.1 + 3.1 =
        // 14.6 ms -> 68 fps instead of 90). A deadline schedule hides the work
        // inside the budget, which is what the real server does.
        //
        // Re-anchor when we fall more than a frame behind. Without this the
        // shortfall accumulates without bound (we measured p50 2385 ms of
        // "lateness" over 900 frames simply because we were ~2.6 ms/frame
        // short), which measures the backlog since frame 0 rather than the
        // current shortfall. A real streamer drops frames to catch up; this is
        // the harness equivalent, so the lateness number stays meaningful.
        let mut deadline = base + Duration::from_micros((i as u64 + 1) * FRAME_INTERVAL_US);
        if let Some(rest) = deadline.checked_duration_since(Instant::now()) {
            thread::sleep(rest);
        } else if Instant::now().saturating_duration_since(deadline)
            > Duration::from_micros(FRAME_INTERVAL_US)
        {
            base = Instant::now() - Duration::from_micros(i as u64 * FRAME_INTERVAL_US);
            deadline = base + Duration::from_micros((i as u64 + 1) * FRAME_INTERVAL_US);
        }
        send_late_ms.push(
            Instant::now()
                .saturating_duration_since(deadline)
                .as_secs_f64()
                * 1e3,
        );
    }

    let elapsed = t0.elapsed();
    let secs = elapsed.as_secs_f64();
    println!(
        "[pcsim] VIDEO OK: sent {frames} frames in {elapsed:?} ({:.1} fps)",
        frames as f64 / secs
    );
    println!(
        "[pcsim]   bitrate {:.1} Mbps, {} bytes total, max frame {} B, mean {:.0} B",
        total_bytes as f64 * 8.0 / secs / 1e6,
        total_bytes,
        max_frame,
        total_bytes as f64 / frames as f64
    );
    // Same tail convention as the client: how late did each frame leave,
    // against the 90 Hz and 120 Hz frame budgets?
    send_late_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let m = send_late_ms.len();
    if m > 0 {
        let over90 = send_late_ms.iter().filter(|x| **x > 1000.0 / 90.0).count();
        let over120 = send_late_ms.iter().filter(|x| **x > 1000.0 / 120.0).count();
        println!(
            "[pcsim]   send lateness: p50 {:.3} ms, p99 {:.3} ms, max {:.3} ms",
            send_late_ms[m / 2],
            send_late_ms[(m * 99 / 100).min(m - 1)],
            send_late_ms[m - 1]
        );
        println!("[pcsim]   over 90Hz budget: {over90}/{m} | over 120Hz budget: {over120}/{m}");
    }
    if !trace.is_empty() {
        let mut enc: Vec<f64> = trace.iter().map(|r| r.2).collect();
        enc.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let m = enc.len();
        println!(
            "[pcsim]   worst encode: max {:.2} ms at frame {}",
            enc[m - 1],
            trace.iter().position(|r| r.2 == enc[m - 1]).unwrap_or(0)
        );
        let mut by_size: Vec<&(u64, usize, f64, f64)> = trace.iter().collect();
        by_size.sort_by_key(|r| std::cmp::Reverse(r.1));
        println!(
            "[pcsim]   biggest frames (idx by size): {:?}",
            by_size
                .iter()
                .take(5)
                .map(|r| (r.1, r.2 as u64))
                .collect::<Vec<_>>()
        );
        match std::fs::File::create(PCSIM_CSV) {
            Ok(mut f) => {
                use std::io::Write as _;
                let _ = writeln!(f, "header_ts_us,size_bytes,encode_ms,send_late_ms");
                for r in &trace {
                    let _ = writeln!(f, "{},{},{:.3},{:.3}", r.0, r.1, r.2, r.3);
                }
                println!("[pcsim] per-frame CSV written to {PCSIM_CSV}");
            }
            Err(e) => eprintln!("[pcsim] could not write CSV: {e}"),
        }
    }
    if let Some((observed, empty)) = source.capture_stats() {
        println!(
            "[pcsim]   dda: {observed} desktop frames observed, {empty} empty polls ({:.1}% idle)",
            empty as f64 * 100.0 / (observed + empty).max(1) as f64
        );
    }
    if let Some((n, p50, p99, mean)) = source.encode_stats() {
        println!(
            "[pcsim]   nvenc encode over {n} frames: p50 {p50:.2} ms, p99 {p99:.2} ms, mean {mean:.2} ms ({:.0} fps encodable)",
            1000.0 / mean
        );
    }
    exit(0);
}
