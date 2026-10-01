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

use alvr_common::{ViewParams, glam::UVec2};
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
    time::{Duration, Instant},
};

#[cfg(windows)]
mod nvenc;

const TIMEOUT: Duration = Duration::from_secs(10);
/// 90 Hz.
const FRAME_INTERVAL: Duration = Duration::from_micros(11_111);

/// Where video frames come from.
enum Source {
    /// Padding bytes: exercises the transport and framing only.
    Synthetic(Vec<u8>),
    /// Real NVENC bitstream (Windows).
    #[cfg(windows)]
    Nvenc(nvenc::Feeder),
}

impl Source {
    fn next_frame(&mut self) -> Result<Vec<u8>, String> {
        match self {
            Source::Synthetic(p) => Ok(p.clone()),
            #[cfg(windows)]
            Source::Nvenc(f) => f.encode_next(),
        }
    }
}

/// Real NVENC when it is available and asked for, else a synthetic payload.
/// The encoder is fed VR-resolution frames (never the desktop's), because that
/// is what the headset consumes and the resolution must stay negotiable.
fn make_source(view_w: u32, view_h: u32, payload_len: usize) -> Source {
    #[cfg(windows)]
    {
        let want = env::var("PCSIM_NVENC").map(|v| v != "0").unwrap_or(true);
        if want {
            match nvenc::Feeder::new(view_w, view_h, 90) {
                Ok(f) => {
                    println!("[pcsim] video source: NVENC {view_w}x{view_h} (8-bit)");
                    return Source::Nvenc(f);
                }
                Err(e) => eprintln!("[pcsim] NVENC unavailable ({e}); using synthetic payload"),
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (view_w, view_h);
    println!("[pcsim] video source: synthetic {payload_len}B payload");
    Source::Synthetic(vec![0u8; payload_len])
}

fn main() {
    env_logger::init();

    let args: Vec<String> = env::args().collect();
    let ip: IpAddr = args
        .get(1)
        .expect("usage: pcsim <client-ip> [pcsim-frames] [stream-port]")
        .parse()
        .expect("bad client ip");
    let frames: usize = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(90);
    let payload_len: usize = env::var("PCSIM_PAYLOAD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    // Negotiable: render resolution may change, so it is a parameter, not a
    // constant. Defaults to the Frame's per-eye resolution.
    let view_w: u32 = env::var("PCSIM_VIEW_W").ok().and_then(|s| s.parse().ok()).unwrap_or(2160);
    let view_h: u32 = env::var("PCSIM_VIEW_H").ok().and_then(|s| s.parse().ok()).unwrap_or(2160);

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
            foveated_encoding: None,
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
    let mut video_sender: StreamSender<VideoPacketHeader> =
        socket.request_unreliable_stream(VIDEO);
    let mut control_sender: ControlSocketSender<ServerControlPacket> =
        match socket.request_reliable_stream() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[pcsim] could not open the reliable control stream: {e}");
                exit(1);
            }
        };

    let mut source = make_source(view_w, view_h, payload_len);
    let t0 = Instant::now();
    println!(
        "[pcsim] streaming {frames} frames of {payload_len}B over VIDEO ({:?}/frame)",
        FRAME_INTERVAL
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

        let header = VideoPacketHeader {
            timestamp: t0.elapsed(),
            global_view_params: [ViewParams::DUMMY; 2],
            foveation_center_shifts: None,
            // The client starts every session stream_corrupted; without a first
            // IDR it would drop frames until one arrives.
            is_idr: i == 0 || i % 120 == 0,
        };

        let payload = match source.next_frame() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[pcsim] frame {i} encode failed: {e}");
                exit(1);
            }
        };
        if i == 0 || i % 90 == 0 {
            println!("[pcsim] frame {i}: {} bytes", payload.len());
        }

        if let Err(e) = video_sender.send_header_with_payload(&header, &payload) {
            eprintln!("[pcsim] video send failed at frame {i}: {e}");
            exit(1);
        }

        thread::sleep(FRAME_INTERVAL);
    }

    let elapsed = t0.elapsed();
    println!(
        "[pcsim] VIDEO OK: sent {frames} frames in {elapsed:?} ({:.1} fps)",
        frames as f64 / elapsed.as_secs_f64()
    );
    exit(0);
}
