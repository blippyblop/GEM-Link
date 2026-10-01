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

use alvr_client_core::{ClientCapabilities, ClientCoreContext, ClientCoreEvent};
use alvr_common::glam::UVec2;
use std::{
    process::exit,
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
    ctx.resume();
    println!("[framesim] resume() called; announcing + listening for the streamer");

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
                    println!("[framesim] M1 OK: negotiation completed");
                    exit(0);
                }
                ClientCoreEvent::StreamingStopped => {
                    println!("[framesim] stream stopped");
                }
                _ => println!("[framesim] event (unhandled variant)"),
            }
        }

        if start.elapsed() > limit {
            eprintln!(
                "[framesim] TIMEOUT after {:?}; last hud: {last_hud}",
                start.elapsed()
            );
            exit(2);
        }

        thread::sleep(Duration::from_millis(50));
    }
}
