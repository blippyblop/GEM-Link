//! The audio path end-to-end, minus the OS device boundary: the shipped encoder settings go
//! through a real stream-socket pair into the production receive/decode loop, and a consumer
//! pulls batches exactly the way the speaker callback does (`get_next_frame_batch`, in real
//! time). The device boundary — a real sink or capture device — is the part only hardware can
//! prove (A1, doc 54).
//!
//! This is what "audio works" can mean before the headset exists: every function between the
//! codec and the speaker buffer is the one production runs, with the parameters production is
//! configured to use.

use alvr_audio::{
    Application, AudioDecoding, OpusEncoder, get_next_frame_batch, receive_samples_loop,
};
use alvr_common::parking_lot::Mutex;
use alvr_packets::AUDIO;
use alvr_session::{SocketBufferConfig, SocketProtocol};
use alvr_sockets::{StreamReceiver, StreamSender, StreamSocket, StreamSocketBuilder};
use std::{
    collections::VecDeque,
    f32::consts::TAU,
    net::Ipv4Addr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PACKET: usize = 1400;
const BATCH_FRAMES: usize = 480; // 10 ms at 48 kHz, the sink's callback cadence

/// A connected sender/receiver pair over loopback. TCP, so the framing and multiplexing
/// machinery is real while the test stays free of the UDP peer-port exchange.
fn pair(port: u16) -> (StreamSocket, StreamSocket) {
    /// `ConnectionError` is not `Debug`, so an assertion helper rather than `expect`.
    fn ok<T>(result: alvr_common::ConResult<T>, what: &str) -> T {
        match result {
            Ok(value) => value,
            Err(e) => panic!("{what}: {e}"),
        }
    }

    let listener = StreamSocketBuilder::listen_for_server(
        TIMEOUT,
        port,
        SocketProtocol::Tcp,
        None,
        SocketBufferConfig::default(),
    )
    .expect("bind receiver");

    let sender = ok(
        StreamSocketBuilder::connect_to_client(
            TIMEOUT,
            Ipv4Addr::LOCALHOST.into(),
            port,
            SocketProtocol::Tcp,
            None,
            SocketBufferConfig::default(),
            MAX_PACKET,
        ),
        "connect sender",
    );

    let receiver = ok(
        listener.accept_from_server(Ipv4Addr::LOCALHOST.into(), 0, MAX_PACKET, TIMEOUT),
        "accept",
    );
    (receiver, sender)
}

/// Sine frames, so the decoded output must be non-silent.
fn sine_frame(frame_samples: usize, channels: usize, start_sample: usize) -> Vec<i16> {
    (0..frame_samples * channels)
        .map(|i| {
            let t = (start_sample + i / channels) as f32 / 48_000.0;
            ((t * 440.0 * TAU).sin() * 12_000.0) as i16
        })
        .collect()
}

/// Send `frames` encoded frames through `sender`. A send failure fails the test: the raw path
/// swallows send errors with `.ok()`, and a silent sender is indistinguishable from a dead one.
fn send_frames(mut sender: StreamSender<()>, settings: (u32, u32, u32, u16)) {
    let (rate, frame_ms, bitrate, channels) = settings;
    let mut encoder = OpusEncoder::new(
        rate,
        channels as usize,
        Application::LowDelay,
        frame_ms,
        bitrate,
        false,
        false,
        0,
    )
    .expect("encoder");
    let frame_samples = encoder.frame_samples();

    for frame in 0..200 {
        let pcm = sine_frame(frame_samples, channels as usize, frame * frame_samples);
        let packet = encoder.encode(&pcm).expect("encode").to_vec();
        if let Err(e) = sender.send_header_with_payload(&(), &packet) {
            panic!("send {frame} failed: {e}");
        }
    }
}

/// Run the link the way production does: a socket pump feeding the stream queues, the
/// receive/decode loop filling the playout buffer, and the "speaker" pulling batches in real
/// time. Returns the count of non-silent samples the speaker actually rendered.
fn run_link(
    mut receiver_socket: StreamSocket,
    receiver: StreamReceiver<()>,
    decoding: AudioDecoding,
    channels: usize,
    render_target_samples: usize,
    sender_finished: Arc<AtomicBool>,
) -> usize {
    let running = Arc::new(AtomicBool::new(true));

    // The socket pump — production's socket-read thread.
    let pump_running = running.clone();
    let pump_other = Arc::new(AtomicUsize::new(0));
    {
        let pump_running = pump_running.clone();
        let pump_other = pump_other.clone();
        thread::spawn(move || {
            loop {
                if !pump_running.load(Ordering::Relaxed) {
                    break;
                }
                match receiver_socket.recv() {
                    Ok(()) => {}
                    Err(alvr_common::ConnectionError::TryAgain(_)) => {}
                    Err(_) => {
                        pump_other.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });
    }

    // The receive/decode loop — production's play_audio_loop inner half.
    let buffer: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
    let rx_running = running.clone();
    let rx_buffer = buffer.clone();
    thread::spawn(move || {
        let mut receiver = receiver;
        let _ = receive_samples_loop(
            || rx_running.load(Ordering::Relaxed),
            &mut receiver,
            rx_buffer,
            channels,
            BATCH_FRAMES,
            BATCH_FRAMES, // average buffering: 10 ms
            decoding,
        );
    });

    // The speaker: pull one 10 ms batch every 10 ms, exactly like the mixer callback, counting
    // what was actually rendered. Without a consumer the playout buffer just trims to its
    // average level — which is the designed behavior, not a signal.
    let rendered = Arc::new(AtomicUsize::new(0));
    let speaker_running = running.clone();
    let sp_buffer = buffer.clone();
    let sp_rendered = rendered.clone();
    thread::spawn(move || {
        loop {
            if !speaker_running.load(Ordering::Relaxed) {
                break;
            }
            let batch = get_next_frame_batch(&mut sp_buffer.lock(), channels, BATCH_FRAMES);
            sp_rendered.fetch_add(
                batch.iter().filter(|&&s| s != 0.0).count(),
                Ordering::Relaxed,
            );
            thread::sleep(Duration::from_millis(10));
        }
    });

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let now = rendered.load(Ordering::Relaxed);
        if now >= render_target_samples {
            break;
        }
        if Instant::now() >= deadline {
            running.store(false, Ordering::Relaxed);
            panic!(
                "rendered only {now} of {render_target_samples} samples after 10 s \
                 (buffer {}, pump errors {})",
                buffer.lock().len(),
                pump_other.load(Ordering::Relaxed),
            );
        }
        if !sender_finished.load(Ordering::Relaxed) {
            // Keep waiting: the sender is still producing.
        }
        thread::sleep(Duration::from_millis(10));
    }

    running.store(false, Ordering::Relaxed);
    rendered.load(Ordering::Relaxed)
}

#[test]
fn downlink_game_audio_at_shipped_settings_survives_the_real_loop() {
    let (mut receiver_socket, sender_socket) = pair(47654);
    let game_receiver = receiver_socket.subscribe_to_stream::<()>(AUDIO, 64);
    let game_sender = sender_socket.request_stream::<()>(AUDIO);

    // The shipped downlink: LOWDELAY, 10 ms, 192 kbps, 48 kHz stereo (192 = the slider default).
    let sender_thread = thread::spawn(move || send_frames(game_sender, (48_000, 10, 192_000, 2)));
    let sender_finished = Arc::new(AtomicBool::new(false));
    let sf = sender_finished.clone();
    thread::spawn(move || {
        let _ = sender_thread.join();
        sf.store(true, Ordering::Relaxed);
    });

    // 0.25 s of stereo actually rendered by the "speaker".
    let rendered = run_link(
        receiver_socket,
        game_receiver,
        AudioDecoding::Opus {
            sample_rate: 48_000,
            frame_ms: 10,
        },
        2,
        24_000,
        sender_finished,
    );

    assert!(
        rendered >= 24_000,
        "decoded audio is silent or starved: {rendered} nonzero samples rendered"
    );
}

#[test]
fn uplink_microphone_at_shipped_settings_survives_the_real_loop() {
    let (mut receiver_socket, sender_socket) = pair(47655);
    let mic_receiver = receiver_socket.subscribe_to_stream::<()>(AUDIO, 64);
    let mut mic_sender = sender_socket.request_stream::<()>(AUDIO);

    // The shipped uplink shape: VOIP-mode frames of 20 ms, mono, 72 kbps (the slider default).
    // FEC/DTX off here so every frame is a real packet; the codec-level tests cover FEC and DTX.
    let sender_thread = thread::spawn(move || {
        let mut encoder =
            OpusEncoder::new(48_000, 1, Application::Voip, 20, 72_000, false, false, 0)
                .expect("encoder");
        let frame_samples = encoder.frame_samples();

        for frame in 0..100 {
            let pcm = sine_frame(frame_samples, 1, frame * frame_samples);
            let packet = encoder.encode(&pcm).expect("encode").to_vec();
            if let Err(e) = mic_sender.send_header_with_payload(&(), &packet) {
                panic!("send {frame} failed: {e}");
            }
        }
    });
    let sender_finished = Arc::new(AtomicBool::new(false));
    let sf = sender_finished.clone();
    thread::spawn(move || {
        let _ = sender_thread.join();
        sf.store(true, Ordering::Relaxed);
    });

    // 0.25 s of mono actually rendered.
    let rendered = run_link(
        receiver_socket,
        mic_receiver,
        AudioDecoding::Opus {
            sample_rate: 48_000,
            frame_ms: 20,
        },
        1,
        12_000,
        sender_finished,
    );

    assert!(
        rendered >= 12_000,
        "decoded mic audio is silent or starved: {rendered} nonzero samples rendered"
    );
}
