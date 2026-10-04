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
    con_bail, dbg_connection, debug, error, info,
    parking_lot::{Condvar, Mutex, RwLock},
    wait_rwlock, warn,
};
use alvr_packets::{
    AUDIO, ClientConnectionResult, ClientControlPacket, ClientStatistics, ConnectionAcceptedInfo,
    HAPTICS, Haptics, STATISTICS, ServerControlPacket, StreamConfigPacket, TRACKING, TrackingData,
    VideoPacketHeader, VideoStreamingCapabilities, VideoStreamingCapabilitiesExt,
};
use alvr_session::{SocketProtocol, settings_schema::Switch};
use alvr_sockets::media::MediaSocket;
use alvr_sockets::media_key::{MediaKeyExchange, MediaKeyRole};
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
use x_crypto::fingerprint_of;
use x_transport::DatagramSink;
use x_transport::{FeedbackSender, KeySchedule};

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

    // -----------------------------------------------------------------------------------------
    // The media key, agreed before a single media datagram exists.
    //
    // A Noise-XX exchange on the control socket — reliable, ordered, and already carrying the
    // stream configuration — from which both ends derive the keys the media plane is built on. The
    // keys are never transmitted: an eavesdropper on this socket (which is still plaintext today)
    // cannot read or forge a video datagram or a repair request.
    //
    // What it does not do is authenticate the peer. The identities are ephemeral and the peer's key
    // is not pinned, so an *active* man in the middle can complete the exchange with each end
    // separately. Closing that is pairing — `MediaKeyExchange::with_pinned_peer` is the whole of
    // the change — and it is a product decision rather than a cryptographic one.
    // -----------------------------------------------------------------------------------------
    let mut media_key = MediaKeyExchange::new(MediaKeyRole::Client).to_con()?;
    // The client answers; the server opens. `start` returning nothing is not a failure, it is which
    // end of an XX handshake this is.
    if let Some(opening) = media_key.start().to_con()? {
        proto_control_socket
            .send(&ClientControlPacket::MediaKeyHandshake(opening))
            .to_con()?;
    }

    loop {
        let message =
            match proto_control_socket.recv::<ServerControlPacket>(HANDSHAKE_ACTION_TIMEOUT)? {
                ServerControlPacket::MediaKeyHandshake(message) => message,
                other => {
                    // Named rather than dumped: `ServerControlPacket` is not `Debug`, and which variant
                    // arrived is the whole of what matters.
                    let name = match &other {
                        ServerControlPacket::StartStream => "StartStream",
                        ServerControlPacket::DecoderConfig(_) => "DecoderConfig",
                        ServerControlPacket::Restarting => "Restarting",
                        ServerControlPacket::KeepAlive => "KeepAlive",
                        ServerControlPacket::RealTimeConfig(_) => "RealTimeConfig",
                        ServerControlPacket::MediaKeyHandshake(_) => "MediaKeyHandshake",
                        _ => "a reserved packet",
                    };
                    con_bail!(
                        "expected a media key handshake message before streaming, got {name}; the \
                     server and this client disagree about the session's shape"
                    );
                }
            };

        match media_key.advance(&message).to_con()? {
            Some(reply) => proto_control_socket
                .send(&ClientControlPacket::MediaKeyHandshake(reply))
                .to_con()?,
            None => break,
        }
    }

    if !media_key.is_finished() {
        con_bail!("the media key exchange ended without a key");
    }
    let media_key_schedule = KeySchedule::new(media_key.session_secret().to_con()?);
    let feedback_cipher = media_key_schedule.cipher_for_feedback();
    // The fingerprint is what a user would compare against the other screen when pairing exists.
    // Until then it is a log line: an active man in the middle would show up here as a change, and
    // nothing acts on that yet.
    info!(
        "Media plane keyed from the session handshake (peer {})",
        fingerprint_of(&media_key.remote_static().to_con()?)
    );

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

    // Video is not a stream on this socket; it is the media plane, further down.
    let mut game_audio_receiver = stream_socket.subscribe_to_stream(AUDIO, MAX_UNREAD_PACKETS);
    let tracking_sender = stream_socket.request_stream(TRACKING);
    let mut haptics_receiver =
        stream_socket.subscribe_to_stream::<Haptics>(HAPTICS, MAX_UNREAD_PACKETS);
    let statistics_sender = stream_socket.request_stream(STATISTICS);

    ctx.video_frame_metadata_queue.lock().clear();

    // -------------------------------------------------------------------------------------------
    // Video arrives on the media plane, and there is no alternative path.
    //
    // It used to arrive as stream `VIDEO` on the multiplexed socket, and that path had no error
    // correction, no retransmission, no frame identity on the wire, and **no counter for the
    // datagrams it discarded** — measured at 1.5 % of everything read, thrown away silently, and
    // indistinguishable from the network losing them (`doc 50 §A10`). A path that drops frames
    // without recording it is not a fallback; it is the defect this project spent two sessions
    // chasing.
    //
    // The per-frame header comes with the frame: the server serialises `VideoPacketHeader` in front
    // of the NAL, so view parameters, foveation centres and the keyframe flag travel on the same
    // datagrams, under the same FEC, in the same order. Metadata on another channel can arrive out
    // of step with the frame it describes, which is a wrong pose for one frame — subtle, and exactly
    // the kind of thing nobody attributes to the transport.
    // -------------------------------------------------------------------------------------------
    let media_port = alvr_packets::media_port(settings.connection.stream_port);
    let media_socket = MediaSocket::bind_to(media_port, server_ip, settings.connection.dscp)?;
    let mut media_feedback_socket = media_socket.try_clone()?;
    // Sealed, and the same session secret the media datagrams use — with different labels, so a
    // NACK cannot be opened as a video shard or the reverse.
    let mut feedback_sender = FeedbackSender::new(feedback_cipher);

    // The client's release policy, written out rather than derived, because the numbers are a
    // statement about this device and not about a profile in a bench.
    //
    // **It holds no frame longer than one frame period.** A reorder or repair window longer than
    // that means the frame is released after the display has already moved on, which is late by
    // definition — the bench measured this link class and the repair round trip alone is 12 ms
    // against an 11.1 ms period at 90 Hz. So the FEC does the work and the re-send is the safety net
    // for the frames that fit; a frame that neither can save is held by the display path
    // (ADR-0011) and the client asks for a keyframe.
    let frame_interval =
        Duration::from_secs_f32(1.0 / negotiated_config.refresh_rate_hint.max(1.0));
    let link_slack = frame_interval / 10;
    // **These windows are short, and they are correct only for a client that drains at wire speed.**
    //
    // Measured on the rig, with the numbers this policy produced: a frame that could not be rebuilt
    // had **5.4 of its 14.9 shards** when it was declared failed, 21 ms after its first one arrived,
    // and 22 000 datagrams over the run arrived *after* the frame they belonged to had been
    // released. The client reads ~480 datagrams/s, so one frame's own ~15 shards take ~30 ms to
    // read — longer than every window here. So each frame is released a third-read, the parity
    // needed to repair it is still queued, `decode_striped` fails with enough parity on paper
    // (`4.5 erasures vs 5.0 parity`), the frame becomes a hole, and the hole blocks the trust gate
    // until the next clean keyframe — ~140 frames of black per hole.
    //
    // Widening the windows was tried and did not help (`repair_delay` at a full frame interval moved
    // the failure from 21 ms to 28.6 ms and left presented slightly *worse*): the read of a frame's
    // own shards is itself slower than any window a display path could tolerate. **The fix is the
    // drain, not these constants** — see the note on the read loop, and SteamLink's answer to the
    // same problem, which is to bound the sender's rate against a queueing-latency target rather
    // than to make the receiver more patient.
    let media_release_policy = x_transport::ReleasePolicy {
        straggler_delay: Duration::from_millis(3) + link_slack,
        repair_delay: (Duration::from_millis(3) + Duration::from_millis(6) + link_slack)
            .min(frame_interval),
        deadline: frame_interval * 2,
        jitter_frames: 0,
    };

    let video_receive_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        let mut plane = crate::media_plane::MediaPlaneReceiver::new(
            media_socket,
            media_release_policy,
            Some(media_key_schedule),
            Instant::now(),
        );
        move || {
            // The latency markers, marked at the boundaries this thread owns. The decode and submit
            // spans are marked by the render path on the same trace, joined by frame index.
            let mut trace = crate::latency::LatencyTrace::new(4096);
            let mut feedback_peer: Option<std::net::SocketAddr> = None;
            let mut frames_decoded = 0u64;
            let mut requests = 0u64;
            // A periodic line, unconditionally. ADR-0014: diagnostics are never gated by a user
            // setting, and this one is the only place the *rate* the media plane actually works at
            // is visible while it is running. Without it a client that reads 84 datagrams/s on a
            // link offering 280 looks identical to a link offering 84.
            let mut last_report = Instant::now();

            while is_streaming(&ctx) {
                let (actions, open) =
                    plane.poll(&mut trace, Instant::now(), Duration::from_millis(20));
                if !open {
                    warn!("The video media socket can no longer be read from; ending the stream");
                    return;
                }

                if last_report.elapsed() >= Duration::from_secs(2) {
                    last_report = Instant::now();
                    info!("{}", plane.stats().summary());
                    info!("{}", plane.receiver_account());
                    info!("{}", plane.failed_frame_account());
                }

                for action in actions {
                    match action {
                        crate::media_plane::MediaPlaneAction::Decode {
                            frame_index,
                            target_timestamp_us,
                            payload,
                        } => {
                            // `decode_from_slice` returns how many bytes it consumed, which is
                            // exactly the split between the header and the NAL behind it.
                            let Ok((header, header_len)) =
                                bincode::serde::decode_from_slice::<VideoPacketHeader, _>(
                                    &payload,
                                    bincode::config::standard(),
                                )
                            else {
                                warn!(
                                    "Video frame {frame_index} arrived with a header that would \
                                     not decode; it cannot be shown"
                                );
                                continue;
                            };

                            if let Some(stats) = &mut *ctx.statistics_manager.lock() {
                                stats.report_video_packet_received(header.timestamp);
                            }

                            // Metadata must be available before the decoder returns the frame.
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

                            // Name the frame before handing it over, so anything the callback does
                            // can join what it got against what the server sent.
                            *ctx.current_video_frame.write() = Some(CurrentVideoFrame {
                                frame_index: header.frame_index,
                                missed_frames: plane.stats().frames_abandoned,
                                // The media plane hands over a frame it could rebuild whole. A
                                // datagram that went missing inside it was repaired, and if it
                                // could not be, the frame was never released.
                                had_datagram_loss: false,
                            });

                            let submitted =
                                ctx.decoder_callback
                                    .lock()
                                    .as_mut()
                                    .is_some_and(|callback| {
                                        callback(header.timestamp, &payload[header_len..])
                                    });

                            if !submitted {
                                // UNCONDITIONAL. Gating this behind a setting is what turned a hold
                                // into a permanent black screen once already.
                                if let Some(sender) = &mut *ctx.control_sender.lock() {
                                    sender.send(&ClientControlPacket::RequestIdr).ok();
                                }
                                warn!("Dropped video packet. Reason: Decoder saturation")
                            } else {
                                frames_decoded += 1;
                                let _ = target_timestamp_us;
                            }
                        }
                        crate::media_plane::MediaPlaneAction::AskForKeyframe { stalled_for } => {
                            requests += 1;
                            if let Some(sender) = &mut *ctx.control_sender.lock() {
                                sender.send(&ClientControlPacket::RequestIdr).ok();
                            }
                            warn!(
                                "Holding video: no progress for {:.0} ms — asked the sender for a \
                                 keyframe",
                                stalled_for.as_secs_f64() * 1e3
                            );
                        }
                        crate::media_plane::MediaPlaneAction::Reset { stalled_for } => {
                            error!(
                                "Video stalled for {:.0} ms with no keyframe; the stream needs \
                                 rebuilding",
                                stalled_for.as_secs_f64() * 1e3
                            );
                            if let Some(sender) = &mut *ctx.control_sender.lock() {
                                sender.send(&ClientControlPacket::RequestIdr).ok();
                            }
                        }
                        crate::media_plane::MediaPlaneAction::Nack {
                            frame_index,
                            fragments,
                        } => {
                            // Once per session, learn where "back" is: the server sends from an
                            // ephemeral port and this is the only place it is visible.
                            if feedback_peer.is_none() {
                                feedback_peer = plane.last_sender();
                                if let Some(peer) = feedback_peer {
                                    media_feedback_socket.accept_only_from(peer);
                                }
                            }
                            if feedback_peer.is_none() {
                                continue;
                            }

                            // Sealed under a key that never crossed the wire, carrying a monotonic
                            // sequence only this end uses — so a captured NACK replayed at the
                            // server is refused rather than answered, which is what stops a
                            // retransmit storm being driven from a single packet.
                            let feedback = x_transport::Feedback::Nack {
                                frame_index,
                                fragments,
                            };
                            let mut sealed = [0u8; x_transport::MAX_FEEDBACK_LEN];
                            let Ok(len) = feedback_sender.seal(&feedback, &mut sealed) else {
                                continue;
                            };
                            if media_feedback_socket.send(&sealed[..len]).is_err() {
                                // Counted by the socket; the frame is lost and the FEC is what is
                                // left, which the next release decides.
                            }
                        }
                        crate::media_plane::MediaPlaneAction::QueueDelay {
                            micros,
                            late_per_mille,
                        } => {
                            // The client's own queueing delay, carried back so the sender can send
                            // *less*. Every other message either end sends is about a frame; this is
                            // the only one that is about the receiver, and without it the sender has
                            // no way to know it is outrunning the client — which is the actual
                            // fault. See `Feedback::QueueDelay`.
                            if feedback_peer.is_none() {
                                feedback_peer = plane.last_sender();
                                if let Some(peer) = feedback_peer {
                                    media_feedback_socket.accept_only_from(peer);
                                }
                            }
                            if feedback_peer.is_none() {
                                continue;
                            }

                            let feedback = x_transport::Feedback::QueueDelay {
                                micros,
                                late_per_mille,
                            };
                            let mut sealed = [0u8; x_transport::MAX_FEEDBACK_LEN];
                            let Ok(len) = feedback_sender.seal(&feedback, &mut sealed) else {
                                continue;
                            };
                            let _ = media_feedback_socket.send(&sealed[..len]);
                        }
                    }
                }
            }

            info!(
                "{}; {} frame(s) decoded, {} keyframe request(s)",
                plane.stats().summary(),
                frames_decoded,
                requests
            );
            info!("{}", trace.summary());
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
                    // The key exchange happens once, before streaming starts; a second one is either
                    // a server that has lost track or a replay, and neither is worth acting on.
                    Ok(ServerControlPacket::MediaKeyHandshake(_)) => {
                        warn!("Ignoring a media key handshake after the session was keyed");
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
