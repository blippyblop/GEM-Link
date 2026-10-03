#![allow(clippy::if_same_then_else)]

use crate::{
    ClientCapabilities, ClientCoreEvent, VideoFrameMetadata,
    logging_backend::{LOG_CHANNEL_SENDER, LogMirrorData},
    sockets::AnnouncerSocket,
    statistics::StatisticsManager,
    storage::Config,
};
use alvr_common::{
    ALVR_VERSION, AnyhowToCon, ConResult, ConnectionError, ConnectionState, LifecycleState,
    dbg_connection, debug, error, info,
    parking_lot::{Condvar, Mutex, RwLock},
    wait_rwlock, warn,
};
use alvr_packets::{
    AUDIO, ClientConnectionResult, ClientControlPacket, ClientStatistics, ConnectionAcceptedInfo,
    HAPTICS, Haptics, STATISTICS, ServerControlPacket, StreamConfigPacket, TRACKING, TrackingData,
    VIDEO, VideoPacketHeader, VideoStreamingCapabilities, VideoStreamingCapabilitiesExt,
};
use alvr_session::{SocketProtocol, settings_schema::Switch};
use alvr_sockets::{
    ControlSocketSender, KEEPALIVE_INTERVAL, KEEPALIVE_TIMEOUT, PeerType, ProtoControlSocket,
    StreamSender, StreamSocketBuilder,
};
use std::{
    collections::VecDeque,
    net::TcpListener,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "android")]
use crate::audio;
#[cfg(not(target_os = "android"))]
use alvr_audio as audio;

const INITIAL_MESSAGE: &str = concat!(
    "Searching for streamer...\n",
    "Open ALVR on your PC then click \"Trust\"\n",
    "next to the device entry",
);
const SUCCESS_CONNECT_MESSAGE: &str = "Successful connection!\nPlease wait...";
const STREAM_STARTING_MESSAGE: &str = "The stream will begin soon\nPlease wait...";
const SERVER_RESTART_MESSAGE: &str = "The streamer is restarting\nPlease wait...";
const SERVER_DISCONNECTED_MESSAGE: &str = "The streamer has disconnected.";
const CONNECTION_TIMEOUT_MESSAGE: &str = "Connection timeout.";

const SOCKET_INIT_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const CONNECTION_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const HANDSHAKE_ACTION_TIMEOUT: Duration = Duration::from_secs(2);
const STREAMING_RECV_TIMEOUT: Duration = Duration::from_millis(500);

const MAX_UNREAD_PACKETS: usize = 10; // Applies per stream
const VIDEO_FRAME_METADATA_HISTORY_SIZE: usize = 128;

pub type DecoderCallback = dyn FnMut(Duration, &[u8]) -> bool + Send;

/// What the media plane knows about the frame currently being handed to the decoder.
///
/// Exists so a caller can *name* a frame. Without it, everything downstream — the
/// harness CSV, the statistics, the display path — sees only a timestamp and a local
/// index, and cannot join what it decoded against what the server transmitted. That join
/// is the only way to tell "the server discarded this frame" from "the network lost it",
/// which is the question the grey-frame investigation has never been able to answer.
///
/// Set immediately before the decoder callback is invoked, from the receive thread, so a
/// callback reading it synchronously is reading its own frame. It is deliberately *not* a
/// parameter of [`DecoderCallback`]: that signature is shared with the Android JNI bridge
/// (`c_api.rs`), and widening it would change a C ABI the Android app is compiled against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrentVideoFrame {
    /// ADR-0011's monotonic frame index, allocated by the server at `Present`.
    pub frame_index: u64,
    /// Frames the server discarded, cumulative for this session, as seen from here.
    pub missed_frames: u64,
    /// Whether datagrams were lost *inside* this frame, as opposed to whole frames going
    /// missing. The two have different causes and the distinction is the point.
    pub had_datagram_loss: bool,
}

#[derive(Default)]
pub struct ConnectionContext {
    pub state: RwLock<ConnectionState>,
    pub disconnected_notif: Condvar,
    pub control_sender: Mutex<Option<ControlSocketSender<ClientControlPacket>>>,
    pub tracking_sender: Mutex<Option<StreamSender<TrackingData>>>,
    pub statistics_sender: Mutex<Option<StreamSender<ClientStatistics>>>,
    pub statistics_manager: Mutex<Option<StatisticsManager>>,
    pub decoder_callback: Mutex<Option<Box<DecoderCallback>>>,
    /// The frame the receive loop is about to hand to the decoder. See
    /// [`CurrentVideoFrame`].
    pub current_video_frame: RwLock<Option<CurrentVideoFrame>>,
    pub video_frame_metadata_queue: Mutex<VecDeque<(Duration, VideoFrameMetadata)>>,
    pub max_prediction: RwLock<Duration>,
}

fn set_hud_message(event_queue: &Mutex<VecDeque<ClientCoreEvent>>, message: &str) {
    let message = format!(
        "ALVR v{}\nhostname: {}\nIP: {}\n\n{message}",
        *ALVR_VERSION,
        Config::load().hostname,
        alvr_system_info::local_ip(),
    );

    event_queue
        .lock()
        .push_back(ClientCoreEvent::UpdateHudMessage(message));
}

fn is_streaming(ctx: &ConnectionContext) -> bool {
    *ctx.state.read() == ConnectionState::Streaming
}

pub fn connection_lifecycle_loop(
    capabilities: ClientCapabilities,
    ctx: Arc<ConnectionContext>,
    lifecycle_state: Arc<RwLock<LifecycleState>>,
    event_queue: Arc<Mutex<VecDeque<ClientCoreEvent>>>,
) {
    dbg_connection!("connection_lifecycle_loop: Begin");

    set_hud_message(&event_queue, INITIAL_MESSAGE);

    // Hold the well-known control port for as long as the user wants to stream,
    // instead of re-binding it on every retry. The control connection is TCP, so
    // the socket a dropped session leaves behind sits in TIME_WAIT *on that
    // port*; re-binding into it makes the next reconnect start with an RST
    // (10054/10057) even with SO_REUSEADDR, which turns a fast reconnect into a
    // stumble. Keeping one listener makes a reconnect just another accept().
    // Released again when streaming stops, so we do not squat on the port idle.
    let mut listener = None;

    while *lifecycle_state.read() != LifecycleState::ShuttingDown {
        if *lifecycle_state.read() == LifecycleState::Resumed {
            if listener.is_none() {
                match alvr_sockets::get_server_listener(HANDSHAKE_ACTION_TIMEOUT) {
                    Ok(socket) => listener = Some(socket),
                    Err(e) => {
                        let message =
                            format!("Connection error:\n{e}\nCheck the PC for more details");
                        set_hud_message(&event_queue, &message);
                        error!("Failed to bind the control port: {e}");
                    }
                }
            }

            if let Some(listener_socket) = listener.as_ref()
                && let Err(e) = connection_pipeline(
                    capabilities.clone(),
                    Arc::clone(&ctx),
                    Arc::clone(&lifecycle_state),
                    Arc::clone(&event_queue),
                    listener_socket,
                )
            {
                let message = format!("Connection error:\n{e}\nCheck the PC for more details");
                set_hud_message(&event_queue, &message);
                error!("Connection error: {e}");
            }
        } else {
            // Not streaming: give the port back.
            listener = None;
            debug!("Skip try connection because the device is sleeping");
        }

        *ctx.state.write() = ConnectionState::Disconnected;
        ctx.disconnected_notif.notify_all();

        thread::sleep(CONNECTION_RETRY_INTERVAL);
    }

    dbg_connection!("connection_lifecycle_loop: End");
}

fn connection_pipeline(
    capabilities: ClientCapabilities,
    ctx: Arc<ConnectionContext>,
    lifecycle_state: Arc<RwLock<LifecycleState>>,
    event_queue: Arc<Mutex<VecDeque<ClientCoreEvent>>>,
    // Bound once by the caller and held across retries — see the TIME_WAIT note
    // in connection_lifecycle_loop.
    listener_socket: &TcpListener,
) -> ConResult {
    dbg_connection!("connection_pipeline: Begin");

    let (mut proto_control_socket, server_ip) = {
        let config = Config::load();
        let announcer_socket = AnnouncerSocket::new(&config.hostname).to_con()?;

        loop {
            if *lifecycle_state.write() != LifecycleState::Resumed {
                return Ok(());
            }

            announcer_socket.announce().ok();

            if let Ok(pair) = ProtoControlSocket::connect_to(
                SOCKET_INIT_RETRY_INTERVAL,
                PeerType::Server(listener_socket),
            ) {
                set_hud_message(&event_queue, SUCCESS_CONNECT_MESSAGE);
                break pair;
            }
        }
    };

    let mut connection_state_lock = ctx.state.write();
    let disconnect_notif = Arc::new(Condvar::new());

    *connection_state_lock = ConnectionState::Connecting;

    // TODO: Don't fetch cpal sample rate, get directly from AAudio
    let microphone_sample_rate =
        alvr_audio::input_sample_rate(&alvr_audio::new_input(None).to_con()?).to_con()?;

    dbg_connection!("connection_pipeline: Send stream capabilities");
    proto_control_socket
        .send(&ClientConnectionResult::ConnectionAccepted(Box::new(
            ConnectionAcceptedInfo {
                client_protocol_id: alvr_common::protocol_id_u64(),
                platform_string: capabilities.platform.to_string(),
                server_ip,
                streaming_capabilities: Some(
                    VideoStreamingCapabilities {
                        default_view_resolution: capabilities.default_view_resolution,
                        max_view_resolution: capabilities.max_view_resolution,
                        refresh_rates: capabilities.refresh_rates,
                        microphone_sample_rate,
                        foveated_encoding: capabilities.foveated_encoding,
                        encoder_high_profile: capabilities.encoder_high_profile,
                        encoder_10_bits: capabilities.encoder_10_bits,
                        encoder_av1: capabilities.encoder_av1,
                        prefer_10bit: capabilities.prefer_10bit,
                        preferred_encoding_gamma: capabilities.preferred_encoding_gamma,
                        prefer_hdr: capabilities.prefer_hdr,
                        ext_str: String::new(),
                    }
                    .with_ext(VideoStreamingCapabilitiesExt {}),
                ),
            },
        )))
        .to_con()?;
    let config_packet =
        proto_control_socket.recv::<StreamConfigPacket>(HANDSHAKE_ACTION_TIMEOUT)?;
    dbg_connection!("connection_pipeline: stream config received");

    let stream_config = config_packet.to_stream_config().to_con()?;

    let streaming_start_event = ClientCoreEvent::StreamingStarted(Box::new(stream_config.clone()));

    let settings = stream_config.settings;
    let negotiated_config = stream_config.negotiated_config;

    *ctx.max_prediction.write() = Duration::from_millis(settings.headset.max_prediction_ms);

    *ctx.statistics_manager.lock() = Some(StatisticsManager::new(
        settings.connection.statistics_history_size,
    ));

    let (mut control_sender, mut control_receiver) = proto_control_socket
        .split(STREAMING_RECV_TIMEOUT)
        .to_con()?;

    match control_receiver.recv(HANDSHAKE_ACTION_TIMEOUT) {
        Ok(ServerControlPacket::StartStream) => {
            info!("Stream starting");
            set_hud_message(&event_queue, STREAM_STARTING_MESSAGE);
        }
        Ok(ServerControlPacket::Restarting) => {
            info!("Server restarting");
            set_hud_message(&event_queue, SERVER_RESTART_MESSAGE);
            return Ok(());
        }
        Err(e) => {
            info!("Server disconnected. Cause: {e}");
            set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);
            return Ok(());
        }
        _ => {
            info!("Unexpected packet");
            set_hud_message(&event_queue, "Unexpected packet");
            return Ok(());
        }
    }

    let stream_protocol = if negotiated_config.wired {
        SocketProtocol::Tcp
    } else {
        settings.connection.stream_protocol
    };

    dbg_connection!("connection_pipeline: create StreamSocket");
    let stream_socket_builder = StreamSocketBuilder::listen_for_server(
        Duration::from_secs(1),
        settings.connection.stream_port,
        stream_protocol,
        settings.connection.dscp,
        settings.connection.client_buffer_config,
    )
    .to_con()?;

    dbg_connection!("connection_pipeline: Send StreamReady");
    if let Err(e) = control_sender.send(&ClientControlPacket::StreamReady) {
        info!("Server disconnected. Cause: {e:?}");
        set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);
        return Ok(());
    }

    dbg_connection!("connection_pipeline: accept connection");
    let mut stream_socket = stream_socket_builder.accept_from_server(
        server_ip,
        settings.connection.stream_port,
        settings.connection.packet_size as _,
        HANDSHAKE_ACTION_TIMEOUT,
    )?;

    info!("Connected to server");

    let mut video_receiver =
        stream_socket.subscribe_to_stream::<VideoPacketHeader>(VIDEO, MAX_UNREAD_PACKETS);
    let mut game_audio_receiver = stream_socket.subscribe_to_stream(AUDIO, MAX_UNREAD_PACKETS);
    let tracking_sender = stream_socket.request_stream(TRACKING);
    let mut haptics_receiver =
        stream_socket.subscribe_to_stream::<Haptics>(HAPTICS, MAX_UNREAD_PACKETS);
    let statistics_sender = stream_socket.request_stream(STATISTICS);

    ctx.video_frame_metadata_queue.lock().clear();

    let video_receive_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        move || {
            // ADR-0011's display half. A frame the client cannot reconstruct is never
            // submitted to the decoder: it keeps showing the last good frame, reprojected by
            // the compositor. That is the permitted response; showing the grey is not.
            //
            // The rule lives in `x_transport::TrustGate` because it was previously two bare
            // booleans in two places and both were wrong the same way — armed on *datagram*
            // loss, which a server-side discarded frame does not produce. See the module docs.
            let mut trust = x_transport::TrustGate::new();
            // ADR-0011's hold, bounded. The gate says *whether* a frame may be shown; this says
            // how long we are willing to show nothing before asking, and how long before giving
            // up on asking and rebuilding. Both rungs come from the reference client for this
            // device (see `crate::stall`); GemLink had only the first, which is exactly how a
            // hold with no keyframe behind it became a black screen.
            let mut stall = crate::stall::StuckDetector::new(Instant::now());
            let mut frames_seen: u64 = 0;
            let mut first_report = true;
            while is_streaming(&ctx) {
                let data = match video_receiver.recv(STREAMING_RECV_TIMEOUT) {
                    Ok(data) => data,
                    Err(ConnectionError::TryAgain(_)) => continue,
                    Err(ConnectionError::Other(_)) => return,
                };
                let Ok((header, nal)) = data.get() else {
                    return;
                };

                if let Some(stats) = &mut *ctx.statistics_manager.lock() {
                    stats.report_video_packet_received(header.timestamp);
                }

                let had_datagram_loss = data.had_packet_loss();
                let decision =
                    trust.may_present(header.frame_index, header.is_idr, had_datagram_loss);

                // Prove the sequence is actually advancing. Without this, a
                // frame_index pinned at 0 is indistinguishable from a perfectly
                // contiguous stream by the gap check alone.
                if first_report || header.frame_index % 300 == 0 {
                    first_report = false;
                    info!(
                        "video frame_index={} (frames seen {}, missed {})",
                        header.frame_index,
                        frames_seen,
                        trust.missed_frames()
                    );
                }
                frames_seen += 1;

                match decision {
                    x_transport::FrameTrust::Trusted => {
                        stall.progress(Instant::now());
                        // Name the frame before handing it over, so anything the callback
                        // does — decoding, logging, writing a CSV — can join what it got
                        // against what the server sent.
                        *ctx.current_video_frame.write() = Some(CurrentVideoFrame {
                            frame_index: header.frame_index,
                            missed_frames: trust.missed_frames(),
                            had_datagram_loss,
                        });

                        // Metadata must be available before the decoder can return this frame.
                        {
                            let queue_mut = &mut *ctx.video_frame_metadata_queue.lock();
                            queue_mut.push_back((
                                header.timestamp,
                                VideoFrameMetadata {
                                    view_params: header.global_view_params,
                                    foveation_center_shifts: header.foveation_center_shifts,
                                },
                            ));

                            while queue_mut.len() > VIDEO_FRAME_METADATA_HISTORY_SIZE {
                                queue_mut.pop_front();
                            }
                        }

                        let submitted = ctx
                            .decoder_callback
                            .lock()
                            .as_mut()
                            .is_some_and(|callback| callback(header.timestamp, nal));

                        trust.on_decoder_result(submitted);
                        if !submitted {
                            // The decoder refused it, so we have no frame and the next one's
                            // reference is broken. Same response as a lost frame: hold, and
                            // ask for a keyframe.
                            //
                            // UNCONDITIONAL. Gating this behind a setting is what turned the
                            // hold into a permanent black screen: `avoid_video_glitching` is
                            // stored `false` on the box, so the client held every frame and
                            // never asked for the one thing that could release the hold. The
                            // encoder inserts an IDR only when asked.
                            if let Some(sender) = &mut *ctx.control_sender.lock() {
                                sender.send(&ClientControlPacket::RequestIdr).ok();
                            }
                            warn!("Dropped video packet. Reason: Decoder saturation")
                        }
                    }
                    x_transport::FrameTrust::Untrusted { reason, first } => {
                        // Ask for a keyframe once per recovery, not once per frame. During a
                        // recovery window this branch runs for every frame that arrives, and a
                        // reliable control packet per frame — each answered by the sender with a
                        // keyframe — is a flood with a bitrate spike attached.
                        //
                        // The request is UNCONDITIONAL — no setting can disable it — and the
                        // clock decides when to repeat it, not a frame count: 300 ms of no
                        // progress asks, 800 ms rebuilds. `avoid_video_glitching` (stored
                        // `false` on the box) used to gate this, which meant no request at all;
                        // and `first` used to mean "first time ever" rather than "first of this
                        // recovery", which meant the second hold never asked. Either one blacks
                        // the screen for good, because the encoder inserts an IDR only when asked.
                        let now = Instant::now();
                        match stall.poll(now) {
                            crate::stall::StuckAction::AskForKeyframe => {
                                if let Some(sender) = &mut *ctx.control_sender.lock() {
                                    sender.send(&ClientControlPacket::RequestIdr).ok();
                                }
                                warn!(
                                    "Holding video: no progress for {:.0} ms ({} frames held) — \
                                     asked the sender for a keyframe",
                                    stall.stalled_for(now).as_secs_f64() * 1e3,
                                    trust.untrusted_run(),
                                );
                            }
                            crate::stall::StuckAction::Reset => {
                                // Rung two, and the one GemLink never had. Asking has failed for
                                // 800 ms; the reference client calls `HardReset` here. We do not
                                // yet own the decoder's handle from this thread, so this is a
                                // counted, loud escalation rather than a silent freeze — which is
                                // the whole difference from the black screen we shipped.
                                error!(
                                    "Video stalled for {:.0} ms with no keyframe; the stream needs \
                                     rebuilding ({} frame(s) held)",
                                    stall.stalled_for(now).as_secs_f64() * 1e3,
                                    trust.untrusted_run(),
                                );
                            }
                            crate::stall::StuckAction::Progress => {}
                        }
                        if first {
                            warn!(
                                "Holding video: frame_index={} is not trustworthy ({reason:?}); \
                                 {} frame(s) missed so far",
                                header.frame_index,
                                trust.missed_frames()
                            );
                        } else {
                            debug!(
                                "Holding video: frame_index={} ({reason:?})",
                                header.frame_index
                            );
                        }
                    }
                }
            }
        }
    });

    let game_audio_thread = if let Switch::Enabled(config) = settings.audio.game_audio {
        let device = alvr_audio::new_output(None).to_con()?;
        thread::spawn({
            let ctx = Arc::clone(&ctx);
            move || {
                while is_streaming(&ctx) {
                    alvr_common::show_err(audio::play_audio_loop(
                        || is_streaming(&ctx),
                        &device,
                        2,
                        negotiated_config.game_audio_sample_rate,
                        config.buffering.clone(),
                        &mut game_audio_receiver,
                    ));
                }
            }
        })
    } else {
        thread::spawn(|| ())
    };

    let microphone_thread = if matches!(settings.audio.microphone, Switch::Enabled(_)) {
        let device = alvr_audio::new_input(None).to_con()?;

        let microphone_sender = stream_socket.request_stream(AUDIO);

        thread::spawn({
            let ctx = Arc::clone(&ctx);
            move || {
                while is_streaming(&ctx) {
                    let ctx = Arc::clone(&ctx);
                    match audio::record_audio_blocking(
                        Arc::new(move || is_streaming(&ctx)),
                        microphone_sender.clone(),
                        &device,
                        1,
                        false,
                    ) {
                        Ok(()) => break,
                        Err(e) => {
                            error!("Audio record error: {e}");

                            continue;
                        }
                    }
                }
            }
        })
    } else {
        thread::spawn(|| ())
    };

    let haptics_receive_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        let event_queue = Arc::clone(&event_queue);
        move || {
            while is_streaming(&ctx) {
                let data = match haptics_receiver.recv(STREAMING_RECV_TIMEOUT) {
                    Ok(packet) => packet,
                    Err(ConnectionError::TryAgain(_)) => continue,
                    Err(ConnectionError::Other(_)) => return,
                };
                let Ok(haptics) = data.get_header() else {
                    return;
                };

                event_queue.lock().push_back(ClientCoreEvent::Haptics {
                    device_id: haptics.device_id,
                    duration: haptics.duration,
                    frequency: haptics.frequency,
                    amplitude: haptics.amplitude,
                });
            }
        }
    });

    let (log_channel_sender, log_channel_receiver) = mpsc::channel();

    let control_send_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        let event_queue = Arc::clone(&event_queue);
        let disconnect_notif = Arc::clone(&disconnect_notif);
        move || {
            let mut keepalive_deadline = Instant::now();

            #[cfg(target_os = "android")]
            let mut battery_deadline = Instant::now();

            while is_streaming(&ctx) && *lifecycle_state.read() == LifecycleState::Resumed {
                if let Ok(packet) = log_channel_receiver.recv_timeout(STREAMING_RECV_TIMEOUT)
                    && let Some(sender) = &mut *ctx.control_sender.lock()
                    && let Err(e) = sender.send(&packet)
                {
                    info!("Server disconnected. Cause: {e:?}");
                    set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);

                    break;
                }

                if Instant::now() > keepalive_deadline
                    && let Some(sender) = &mut *ctx.control_sender.lock()
                {
                    sender.send(&ClientControlPacket::KeepAlive).ok();

                    keepalive_deadline = Instant::now() + KEEPALIVE_INTERVAL;
                }

                #[cfg(target_os = "android")]
                if Instant::now() > battery_deadline {
                    let (gauge_value, is_plugged) = alvr_system_info::get_battery_status();
                    if let Some(sender) = &mut *ctx.control_sender.lock() {
                        sender
                            .send(&ClientControlPacket::Battery(crate::BatteryInfo {
                                device_id: *alvr_common::HEAD_ID,
                                gauge_value,
                                is_plugged,
                            }))
                            .ok();
                    }

                    battery_deadline = Instant::now() + Duration::from_secs(5);
                }
            }

            disconnect_notif.notify_one();
        }
    });

    let control_receive_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        let event_queue = Arc::clone(&event_queue);
        let disconnect_notif = Arc::clone(&disconnect_notif);
        move || {
            let mut disconnection_deadline = Instant::now() + KEEPALIVE_TIMEOUT;
            while is_streaming(&ctx) {
                let maybe_packet = control_receiver.recv(STREAMING_RECV_TIMEOUT);

                match maybe_packet {
                    Ok(ServerControlPacket::DecoderConfig(config)) => {
                        event_queue
                            .lock()
                            .push_back(ClientCoreEvent::DecoderConfig {
                                codec: config.codec,
                                config_nal: config.config_buffer,
                            });
                    }
                    Ok(ServerControlPacket::Restarting) => {
                        info!("{SERVER_RESTART_MESSAGE}");
                        set_hud_message(&event_queue, SERVER_RESTART_MESSAGE);
                        disconnect_notif.notify_one();
                    }
                    Ok(ServerControlPacket::RealTimeConfig(config)) => {
                        event_queue
                            .lock()
                            .push_back(ClientCoreEvent::RealTimeConfig(config));
                    }
                    Ok(ServerControlPacket::StartStream) => {
                        error!("Unexpected StartStream paceket");
                    }
                    Ok(ServerControlPacket::KeepAlive) => (),
                    Ok(
                        ServerControlPacket::Reserved(_) | ServerControlPacket::ReservedBuffer(_),
                    ) => {}
                    Err(ConnectionError::TryAgain(_)) => {
                        if Instant::now() > disconnection_deadline {
                            info!("{CONNECTION_TIMEOUT_MESSAGE}");
                            set_hud_message(&event_queue, CONNECTION_TIMEOUT_MESSAGE);
                            disconnect_notif.notify_one();
                        } else {
                            continue;
                        }
                    }
                    Err(e) => {
                        info!("{SERVER_DISCONNECTED_MESSAGE} Cause: {e}");
                        set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);
                        disconnect_notif.notify_one();
                    }
                }

                disconnection_deadline = Instant::now() + KEEPALIVE_TIMEOUT;
            }
        }
    });

    let stream_receive_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        let event_queue = Arc::clone(&event_queue);
        let disconnect_notif = Arc::clone(&disconnect_notif);
        move || {
            while is_streaming(&ctx) {
                match stream_socket.recv() {
                    Ok(()) => (),
                    Err(ConnectionError::TryAgain(_)) => continue,
                    Err(e) => {
                        info!("Client disconnected. Cause: {e}");
                        set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);
                        disconnect_notif.notify_one();
                    }
                }
            }
        }
    });

    *ctx.control_sender.lock() = Some(control_sender);
    *ctx.tracking_sender.lock() = Some(tracking_sender);
    *ctx.statistics_sender.lock() = Some(statistics_sender);
    if let Switch::Enabled(filter_level) = settings.extra.logging.client_log_report_level {
        *LOG_CHANNEL_SENDER.lock() = Some(LogMirrorData {
            sender: log_channel_sender,
            filter_level,
            debug_groups_config: settings.extra.logging.debug_groups,
        });
    }
    event_queue.lock().push_back(streaming_start_event);

    *connection_state_lock = ConnectionState::Streaming;

    dbg_connection!("connection_pipeline: Unlock streams");

    // Unlock CONNECTION_STATE and block thread
    wait_rwlock(&disconnect_notif, &mut connection_state_lock);

    *connection_state_lock = ConnectionState::Disconnecting;

    *ctx.control_sender.lock() = None;
    *ctx.tracking_sender.lock() = None;
    *ctx.statistics_sender.lock() = None;
    *LOG_CHANNEL_SENDER.lock() = None;

    event_queue
        .lock()
        .push_back(ClientCoreEvent::StreamingStopped);

    // Remove lock to allow threads to properly exit:
    drop(connection_state_lock);

    dbg_connection!("connection_pipeline: Destroying streams");

    video_receive_thread.join().ok();
    game_audio_thread.join().ok();
    microphone_thread.join().ok();
    haptics_receive_thread.join().ok();
    control_send_thread.join().ok();
    control_receive_thread.join().ok();
    stream_receive_thread.join().ok();

    dbg_connection!("connection_pipeline: End");

    Ok(())
}
