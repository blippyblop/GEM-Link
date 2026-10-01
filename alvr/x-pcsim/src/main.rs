//! Minimal streamer-side ("PC sim") for Phase 3 M1.
//!
//! Dials a headset sim's control port and performs the **real** server half of
//! the ALVR handshake, using alvr_sockets' own helper
//! (`SocketConnection::from_client_connection`) — the same code path the
//! production server uses. This is the counterpart to `x_framesim` running
//! under qemu on the sim host.
//!
//! Usage: pcsim <client-ip> [stream-port]
//! Exits 0 once the control-plane handshake completes.

use alvr_common::glam::UVec2;
use alvr_packets::{
    ClientConnectionResult, ClientNegotiatedStreamingConfig, NegotiatedStreamingConfigExt,
    StreamConfigPacket,
};
use alvr_session::{SessionConfig, SocketProtocol};
use alvr_sockets::{SocketConnection, StreamSocketConfig, connect_to_client};
use std::{env, net::IpAddr, process::exit, time::Duration};

const TIMEOUT: Duration = Duration::from_secs(10);

fn main() {
    env_logger::init();

    let args: Vec<String> = env::args().collect();
    let ip: IpAddr = args
        .get(1)
        .expect("usage: pcsim <client-ip> [stream-port]")
        .parse()
        .expect("bad client ip");
    let stream_port: u16 = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(9947);

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
            println!("[pcsim] client accepted: platform={} protocol_id={}", info.platform_string, info.client_protocol_id);
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
    let stream_config_packet = match StreamConfigPacket::new(
        &SessionConfig::default(),
        ClientNegotiatedStreamingConfig {
            view_resolution: UVec2::new(2160, 2160),
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
    let connection = SocketConnection::from_client_connection(
        control_socket,
        TIMEOUT,
        stream_config_packet,
        StreamSocketConfig {
            protocol: SocketProtocol::Udp,
            port: stream_port,
            buffer_config: Default::default(),
            max_packet_size: 1400,
            dscp: None,
        },
    );

    match connection {
        Ok(_) => {
            println!("[pcsim] M1 OK: control-plane handshake completed with {client_ip}");
            exit(0);
        }
        Err(e) => {
            eprintln!("[pcsim] handshake failed: {e}");
            exit(1);
        }
    }
}
