use crate::{
    ConnectionContext, FILESYSTEM_LAYOUT, SESSION_MANAGER, ServerCoreEvent,
    ServerNegotiatedStreamingConfig,
    bitrate::BitrateManager,
    input_mapping::ButtonMappingManager,
    sockets::WelcomeSocket,
    statistics::StatisticsManager,
    tracking::{self, TrackingManager},
};
use alvr_adb::{WiredConnection, WiredConnectionStatus};
use alvr_common::{
    AlvrFoveatedEncodingParams, AnyhowToCon, BUTTON_INFO, CONTROLLER_PROFILE_INFO, ConResult,
    ConnectionError, ConnectionState, LifecycleState, QUEST_CONTROLLER_PROFILE_PATH, con_bail,
    dbg_connection, debug, error,
    glam::{UVec2, Vec2},
    info,
    parking_lot::{Condvar, Mutex, RwLock},
    settings_schema::Switch,
    warn,
};
use alvr_events::{AdbEvent, ButtonEvent, EventType};
use alvr_packets::{
    AUDIO, ClientConnectionResult, ClientConnectionsAction, ClientControlPacket,
    ClientNegotiatedStreamingConfig, ClientStatistics, HAPTICS, NegotiatedStreamingConfigExt,
    RealTimeConfig, STATISTICS, ServerControlPacket, StreamConfigPacket, TRACKING, TrackingData,
    VideoPacketHeader,
};
use alvr_session::BitrateMode;
use alvr_session::{
    BodyTrackingSinkConfig, CodecType, ControllersEmulationMode, FrameSize, H264Profile, Settings,
    SocketProtocol, SteamvrHmdInitConfig,
};
use alvr_sockets::{
    CONTROL_PORT, KEEPALIVE_INTERVAL, KEEPALIVE_TIMEOUT, ProtoControlSocket, SocketConnection,
    StreamSocketConfig, WIRED_CLIENT_HOSTNAME,
};
use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    net::{IpAddr, Ipv4Addr},
    process::Command,
    sync::{Arc, mpsc::RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};
use x_transport::{
    DatagramSource, FeedbackOutcome, FrameMeta, MediaSender, PacerConfig, ParityPolicy,
    SenderConfig, SourceEvent, TimebaseOffset,
};

const RETRY_CONNECT_MIN_INTERVAL: Duration = Duration::from_secs(1);
const HANDSHAKE_ACTION_TIMEOUT: Duration = Duration::from_secs(2);
pub const STREAMING_RECV_TIMEOUT: Duration = Duration::from_millis(500);
/// How often the send loop services the client's repair requests while it waits for the next frame.
///
/// A repair is useful only if it arrives inside the **client's** repair window, which is a few
/// milliseconds — so the sender has to look at its feedback socket far more often than once per
/// frame. See the note on the loop in `connection_pipeline`.
const FEEDBACK_POLL_INTERVAL: Duration = Duration::from_millis(1);
const REAL_TIME_UPDATE_INTERVAL: Duration = Duration::from_secs(1);

const MAX_UNREAD_PACKETS: usize = 10; // Applies per stream

/// Where the media plane's FEC ratio starts before it has measured the link.
///
/// A *starting point*, not a setting. `MediaSender` measures the loss from the repairs the client
/// asks for and sizes the code to it, so the only job of this number is to be wrong in the safe
/// direction on the first frame: loss is unrecoverable and bandwidth is not.
const MEDIA_STARTING_FEC_FRACTION: f32 = 0.25;

/// What the sender assumes a repair round costs, until a measurement replaces it.
///
/// Consumed in exactly one place — refusing a repair that could not arrive before the client gives
/// up on the frame — so an over-estimate costs a repair that would have worked and an
/// under-estimate costs bandwidth on a frame that cannot be shown.
const MEDIA_ROUND_TRIP: Duration = Duration::from_millis(6);

/// The client's release policy for a session at `fps`.
///
/// The sender needs the same numbers the client will use, because a repair is only worth sending if
/// it arrives before the client stops waiting. Written down once and used on both ends: the client's
/// own is built from the same function.
fn media_release_policy(fps: f32) -> x_transport::ReleasePolicy {
    let frame_interval = Duration::from_secs_f32(1.0 / fps.max(1.0));
    // The link's jitter and its stalls are the client's to measure; what the *server* needs from
    // this policy is the deadline, and the dominating term in it is the round trip. A generous
    // straggler window here costs nothing — it only makes the server willing to repair for slightly
    // longer than the client will wait, and the `WouldArriveLate` check is what actually bounds it.
    x_transport::ReleasePolicy::for_link(
        Duration::from_millis(2),
        Duration::ZERO,
        MEDIA_ROUND_TRIP,
        frame_interval,
    )
}

/// The bitrate the pacer is sized for.
///
/// The pacer does not decide the rate — that is the bitrate controller's job, and a pacer that also
/// adapted would be two controllers fighting over one actuator. This is the rate the *settings* ask
/// for, and the frame-level `over_budget` warning is how the settings find out they are wrong.
fn nominal_bitrate_bps(settings: &alvr_session::Settings) -> u64 {
    // Adaptive mode with no ceiling asks the *controller* to find the rate, which the pacer cannot
    // do — it is deliberately not adaptive. The safe direction is a pacer sized *above* the
    // controller rather than below it: too high and the controller is the only rate authority (its
    // own `max_throughput_mbps` limiter still applies); too low and the pacer throttles the stream
    // beneath the rate the controller chose, which shows up as `over_budget` on every frame. The
    // top of the setting's own range is the honest ceiling to fall back to.
    const UNBOUNDED_ADAPTIVE_BPS: u64 = 1_000_000_000;

    match &settings.video.bitrate.mode {
        BitrateMode::ConstantMbps(mbps) => *mbps * 1_000_000,
        BitrateMode::Adaptive {
            max_throughput_mbps,
            ..
        } => match max_throughput_mbps {
            Switch::Enabled(mbps) => *mbps * 1_000_000,
            Switch::Disabled => UNBOUNDED_ADAPTIVE_BPS,
        },
    }
}

pub struct VideoPacket {
    pub header: VideoPacketHeader,
    pub payload: Vec<u8>,
}

fn align32(value: f32) -> u32 {
    ((value / 32.).floor() * 32.) as u32
}

fn is_streaming(client_hostname: &str) -> bool {
    SESSION_MANAGER
        .read()
        .client_list()
        .get(client_hostname)
        .is_some_and(|c| c.connection_state == ConnectionState::Streaming)
}

use crate::align_foveation_center_shift;

// Compute a hash over all steamvr-restart settings and client-negotiated values.
// The small SteamvrHmdInitConfig carries the negotiated resolution/fps; everything else comes from
// Settings directly, using the same derivation as the old full SteamvrHmdInitConfig did.
pub fn compute_restart_settings_hash(
    steamvr_hmd_init_config: &SteamvrHmdInitConfig,
    settings: &Settings,
) -> u64 {
    let mut controller_is_tracker = false;
    let mut controller_profile: i32 = 0;
    let mut use_separate_hand_trackers = false;
    let controllers_enabled = if let Switch::Enabled(config) = &settings.headset.controllers {
        controller_is_tracker =
            matches!(config.emulation_mode, ControllersEmulationMode::ViveTracker);
        controller_profile = match config.emulation_mode {
            ControllersEmulationMode::RiftSTouch => 0,
            ControllersEmulationMode::Quest1Touch => 1,
            ControllersEmulationMode::Quest2Touch => 2,
            ControllersEmulationMode::Quest3Plus => 3,
            ControllersEmulationMode::QuestPro => 4,
            ControllersEmulationMode::Pico4 => 10,
            ControllersEmulationMode::ValveIndex => 20,
            ControllersEmulationMode::SteamFrame => 70,
            ControllersEmulationMode::ViveWand => 40,
            ControllersEmulationMode::ViveTracker => 41,
            ControllersEmulationMode::PSVR2Sense => 60,
            ControllersEmulationMode::Custom { .. } => 500,
        };
        use_separate_hand_trackers = config
            .hand_skeleton
            .as_option()
            .is_some_and(|c| c.steamvr_input_2_0);
        true
    } else {
        false
    };

    let body_tracking_vive_enabled =
        if let Switch::Enabled(config) = &settings.headset.body_tracking {
            matches!(config.sink, BodyTrackingSinkConfig::FakeViveTracker)
        } else if let Switch::Enabled(config) = &settings.headset.multimodal_tracking {
            config.detached_controllers_steamvr_sink
        } else {
            false
        };

    let body_tracking_has_legs = settings
        .headset
        .body_tracking
        .as_option()
        .map(|c| c.sources.meta.prefer_full_body)
        .unwrap_or(false);

    let mut foveation_eye_tracking = false;
    let mut foveation_center_size_x = 0.0_f32;
    let mut foveation_center_size_y = 0.0_f32;
    let mut foveation_center_shift_x = 0.0_f32;
    let mut foveation_center_shift_y = 0.0_f32;
    let mut foveation_edge_ratio_x = 0.0_f32;
    let mut foveation_edge_ratio_y = 0.0_f32;
    let enable_foveated_encoding =
        if let Switch::Enabled(config) = &settings.video.foveated_encoding {
            foveation_eye_tracking = settings
                .headset
                .face_tracking
                .as_option()
                .is_some_and(|config| config.sink.eye_tracked_foveated_encoding);
            [foveation_center_size_x, foveation_center_size_y] = config.center_size;
            [foveation_center_shift_x, foveation_center_shift_y] = config.center_shift;
            [foveation_edge_ratio_x, foveation_edge_ratio_y] = config.edge_ratio;
            true
        } else {
            false
        };

    let mut brightness = 0.0_f32;
    let mut contrast = 0.0_f32;
    let mut saturation = 0.0_f32;
    let mut gamma = 0.0_f32;
    let mut sharpening = 0.0_f32;
    let enable_color_correction = if let Switch::Enabled(config) = &settings.video.color_correction
    {
        brightness = config.brightness;
        contrast = config.contrast;
        saturation = config.saturation;
        gamma = config.gamma;
        sharpening = config.sharpening;
        true
    } else {
        false
    };

    let nvenc = &settings.video.encoder_config.nvenc;
    let amf = &settings.video.encoder_config.amf;
    let hdr = &settings.video.encoder_config.hdr;
    let enc = &settings.video.encoder_config;
    let dbg = &settings.extra.logging.debug_groups;

    let mut h = DefaultHasher::new();

    // Negotiated fields (persisted in SteamvrHmdInitConfig)
    steamvr_hmd_init_config.eye_resolution_width.hash(&mut h);
    steamvr_hmd_init_config.eye_resolution_height.hash(&mut h);
    steamvr_hmd_init_config
        .target_eye_resolution_width
        .hash(&mut h);
    steamvr_hmd_init_config
        .target_eye_resolution_height
        .hash(&mut h);
    steamvr_hmd_init_config.refresh_rate.hash(&mut h);
    // Pre-init settings fields (read directly from settings)
    settings.video.adapter_index.hash(&mut h);
    settings.headset.tracking_ref_only.hash(&mut h);
    settings.headset.enable_vive_tracker_proxy.hash(&mut h);
    settings.extra.patches.linux_async_compute.hash(&mut h);
    settings.extra.patches.linux_async_reprojection.hash(&mut h);
    // Encoder / codec
    (settings.video.preferred_codec as u8).hash(&mut h);
    (enc.h264_profile as u32).hash(&mut h);
    (enc.rate_control_mode as u32).hash(&mut h);
    enc.filler_data.hash(&mut h);
    (enc.entropy_coding as u32).hash(&mut h);
    (enc.quality_preset as u32).hash(&mut h);
    enc.enable_vbaq.hash(&mut h);
    enc.use_10bit.hash(&mut h);
    enc.encoding_gamma.map(f32::to_bits).hash(&mut h);
    enc.software.force_software_encoding.hash(&mut h);
    enc.software.thread_count.hash(&mut h);
    // HDR
    hdr.enable.hash(&mut h);
    hdr.force_hdr_srgb_correction.hash(&mut h);
    hdr.clamp_hdr_extended_range.hash(&mut h);
    // AMF
    amf.enable_pre_analysis.hash(&mut h);
    amf.enable_hmqb.hash(&mut h);
    amf.use_preproc.hash(&mut h);
    amf.preproc_sigma.hash(&mut h);
    amf.preproc_tor.hash(&mut h);
    // NVENC
    (nvenc.quality_preset as u32).hash(&mut h);
    (nvenc.tuning_preset as u32).hash(&mut h);
    (nvenc.multi_pass as u32).hash(&mut h);
    (nvenc.adaptive_quantization_mode as u32).hash(&mut h);
    nvenc.low_delay_key_frame_scale.hash(&mut h);
    nvenc.refresh_rate.hash(&mut h);
    nvenc.enable_intra_refresh.hash(&mut h);
    nvenc.intra_refresh_period.hash(&mut h);
    nvenc.intra_refresh_count.hash(&mut h);
    nvenc.max_num_ref_frames.hash(&mut h);
    nvenc.gop_length.hash(&mut h);
    nvenc.p_frame_strategy.hash(&mut h);
    nvenc.rate_control_mode.hash(&mut h);
    nvenc.rc_buffer_size.hash(&mut h);
    nvenc.rc_initial_delay.hash(&mut h);
    nvenc.rc_max_bitrate.hash(&mut h);
    nvenc.rc_average_bitrate.hash(&mut h);
    nvenc.enable_weighted_prediction.hash(&mut h);
    // Foveated encoding
    enable_foveated_encoding.hash(&mut h);
    foveation_eye_tracking.hash(&mut h);
    foveation_center_size_x.to_bits().hash(&mut h);
    foveation_center_size_y.to_bits().hash(&mut h);
    foveation_center_shift_x.to_bits().hash(&mut h);
    foveation_center_shift_y.to_bits().hash(&mut h);
    foveation_edge_ratio_x.to_bits().hash(&mut h);
    foveation_edge_ratio_y.to_bits().hash(&mut h);
    // Color correction
    enable_color_correction.hash(&mut h);
    brightness.to_bits().hash(&mut h);
    contrast.to_bits().hash(&mut h);
    saturation.to_bits().hash(&mut h);
    gamma.to_bits().hash(&mut h);
    sharpening.to_bits().hash(&mut h);
    // Controllers
    controllers_enabled.hash(&mut h);
    controller_is_tracker.hash(&mut h);
    controller_profile.hash(&mut h);
    use_separate_hand_trackers.hash(&mut h);
    // Body tracking
    body_tracking_vive_enabled.hash(&mut h);
    body_tracking_has_legs.hash(&mut h);
    // Misc
    settings.connection.minimum_idr_interval_ms.hash(&mut h);
    settings.extra.capture.capture_frame_dir.hash(&mut h);
    settings.video.bitrate.image_corruption_fix.hash(&mut h);
    // Debug groups
    dbg.server_impl.hash(&mut h);
    dbg.client_impl.hash(&mut h);
    dbg.server_core.hash(&mut h);
    dbg.client_core.hash(&mut h);
    dbg.connection.hash(&mut h);
    dbg.sockets.hash(&mut h);
    dbg.server_gfx.hash(&mut h);
    dbg.client_gfx.hash(&mut h);
    dbg.encoder.hash(&mut h);
    dbg.decoder.hash(&mut h);

    h.finish()
}

// Alternate connection trials with manual IPs and clients discovered on the local network
pub fn handshake_loop(ctx: Arc<ConnectionContext>, lifecycle_state: Arc<RwLock<LifecycleState>>) {
    dbg_connection!("handshake_loop: Begin");

    let welcome_socket = match WelcomeSocket::new() {
        Ok(socket) => socket,
        Err(e) => {
            error!("Failed to create discovery socket: {e:?}");
            return;
        }
    };

    let mut wired_connection = None;

    while *lifecycle_state.read() != LifecycleState::ShuttingDown {
        dbg_connection!("handshake_loop: Try connect to wired device");

        let mut wired_client_ips = HashMap::new();
        if SESSION_MANAGER
            .read()
            .client_list()
            .iter()
            .any(|(hostname, info)| {
                info.connection_state == ConnectionState::Disconnected
                    && hostname.as_str() == WIRED_CLIENT_HOSTNAME
            })
        {
            // Make sure the wired connection is created once and kept alive
            let wired_connection = if let Some(connection) = &wired_connection {
                connection
            } else {
                let connection = match WiredConnection::new(
                    FILESYSTEM_LAYOUT.get().unwrap(),
                    |downloaded, maybe_total| {
                        if let Some(total) = maybe_total {
                            alvr_events::send_event(EventType::Adb(AdbEvent {
                                download_progress: downloaded as f32 / total as f32,
                            }));
                        };
                    },
                ) {
                    Ok(connection) => connection,
                    Err(e) => {
                        error!("{e:?}");
                        thread::sleep(RETRY_CONNECT_MIN_INTERVAL);
                        continue;
                    }
                };

                wired_connection = Some(connection);

                wired_connection.as_ref().unwrap()
            };

            let stream_port;
            let client_type;
            let client_autolaunch;
            {
                let session_manager_lock = SESSION_MANAGER.read();
                let connection = &session_manager_lock.settings().connection;
                stream_port = connection.stream_port;
                client_type = connection.wired_client_type.clone();
                client_autolaunch = connection.wired_client_autolaunch.as_option().cloned();
            }

            let status = match wired_connection.setup(
                CONTROL_PORT,
                stream_port,
                &client_type,
                client_autolaunch,
            ) {
                Ok(status) => status,
                Err(e) => {
                    error!("{e:?}");
                    thread::sleep(RETRY_CONNECT_MIN_INTERVAL);
                    continue;
                }
            };

            #[cfg_attr(not(debug_assertions), expect(unused_variables))]
            if let WiredConnectionStatus::NotReady(s) = status {
                dbg_connection!("handshake_loop: Wired connection not ready: {s}");
                thread::sleep(RETRY_CONNECT_MIN_INTERVAL);
                continue;
            }

            let client_ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
            wired_client_ips.insert(client_ip, WIRED_CLIENT_HOSTNAME.to_owned());
        }

        if !wired_client_ips.is_empty()
            && try_connect(
                Arc::clone(&ctx),
                Arc::clone(&lifecycle_state),
                wired_client_ips,
            )
            .is_ok()
        {
            thread::sleep(RETRY_CONNECT_MIN_INTERVAL);
            continue;
        }

        dbg_connection!("handshake_loop: Try connect to manual IPs");

        let available_manual_client_ips = {
            let mut manual_client_ips = HashMap::new();
            for (hostname, connection_info) in
                SESSION_MANAGER
                    .read()
                    .client_list()
                    .iter()
                    .filter(|(hostname, info)| {
                        info.connection_state == ConnectionState::Disconnected
                            && hostname.as_str() != WIRED_CLIENT_HOSTNAME
                    })
            {
                for ip in &connection_info.manual_ips {
                    manual_client_ips.insert(*ip, hostname.clone());
                }
            }
            manual_client_ips
        };

        if !available_manual_client_ips.is_empty()
            && try_connect(
                Arc::clone(&ctx),
                Arc::clone(&lifecycle_state),
                available_manual_client_ips,
            )
            .is_ok()
        {
            thread::sleep(RETRY_CONNECT_MIN_INTERVAL);
            continue;
        }

        let discovery_config = SESSION_MANAGER
            .read()
            .settings()
            .connection
            .client_discovery
            .clone();
        if let Switch::Enabled(config) = discovery_config {
            dbg_connection!("handshake_loop: Discovering clients");

            let clients = match welcome_socket.recv_all() {
                Ok(clients) => clients,
                Err(e) => {
                    warn!("mDNS listening error: {e:?}");

                    thread::sleep(RETRY_CONNECT_MIN_INTERVAL);
                    continue;
                }
            };

            if clients.is_empty() {
                thread::sleep(RETRY_CONNECT_MIN_INTERVAL);
                continue;
            }

            for (client_hostname, client_ip) in clients {
                let trusted = {
                    let mut session_manager = SESSION_MANAGER.write();

                    session_manager.update_client_connections(
                        client_hostname.clone(),
                        ClientConnectionsAction::AddIfMissing {
                            trusted: false,
                            manual_ips: vec![],
                        },
                    );

                    if config.auto_trust_clients {
                        session_manager.update_client_connections(
                            client_hostname.clone(),
                            ClientConnectionsAction::Trust,
                        );
                    }

                    session_manager
                        .client_list()
                        .get(&client_hostname)
                        .is_some_and(|c| c.trusted)
                };

                // do not attempt connection if the client is already connected
                if trusted
                    && SESSION_MANAGER
                        .read()
                        .client_list()
                        .get(&client_hostname)
                        .is_some_and(|c| c.connection_state == ConnectionState::Disconnected)
                    && let Err(e) = try_connect(
                        Arc::clone(&ctx),
                        Arc::clone(&lifecycle_state),
                        [(client_ip, client_hostname.clone())].into_iter().collect(),
                    )
                {
                    error!("Could not initiate connection for {client_hostname}: {e}");
                }

                thread::sleep(RETRY_CONNECT_MIN_INTERVAL);
            }
        } else {
            thread::sleep(RETRY_CONNECT_MIN_INTERVAL);
        }
    }

    alvr_common::dbg_connection!("handshake_loop: Joining connection threads");

    // At this point, LIFECYCLE_STATE == ShuttingDown, so all threads are already terminating
    for thread in ctx.connection_threads.lock().drain(..) {
        thread.join().ok();
    }

    alvr_common::dbg_connection!("handshake_loop: End");
}

fn try_connect(
    ctx: Arc<ConnectionContext>,
    lifecycle_state: Arc<RwLock<LifecycleState>>,
    mut client_ips: HashMap<IpAddr, String>,
) -> ConResult {
    dbg_connection!("try_connect: Finding client and creating control socket");

    let (socket, client_ip, connection_result) = alvr_sockets::connect_to_client(
        client_ips.keys().cloned().collect(),
        Duration::from_secs(1),
    )?;

    let Some(client_hostname) = client_ips.remove(&client_ip) else {
        con_bail!("unreachable");
    };

    dbg_connection!("try_connect: Pushing new client connection thread");

    ctx.connection_threads.lock().push(thread::spawn({
        let ctx = Arc::clone(&ctx);
        move || {
            if let Err(e) = connection_pipeline(
                Arc::clone(&ctx),
                lifecycle_state,
                socket,
                connection_result,
                client_hostname.clone(),
                client_ip,
            ) {
                error!("Handshake error for {client_hostname}: {e}");
            }

            let mut clients_to_be_removed = ctx.clients_to_be_removed.lock();

            let action = if clients_to_be_removed.contains(&client_hostname) {
                clients_to_be_removed.remove(&client_hostname);

                ClientConnectionsAction::RemoveEntry
            } else {
                ClientConnectionsAction::SetConnectionState(ConnectionState::Disconnected)
            };
            SESSION_MANAGER
                .write()
                .update_client_connections(client_hostname, action);
        }
    }));

    Ok(())
}

fn connection_pipeline(
    ctx: Arc<ConnectionContext>,
    lifecycle_state: Arc<RwLock<LifecycleState>>,
    socket: ProtoControlSocket,
    connection_result: ClientConnectionResult,
    client_hostname: String,
    client_ip: IpAddr,
) -> ConResult {
    dbg_connection!("connection_pipeline: Begin");

    // This session lock will make sure settings and client list cannot be changed while connecting
    // to thos client, no other client can connect until handshake is finished. It will then be
    // temporarily relocked while shutting down the threads.
    let mut session_manager_lock = SESSION_MANAGER.write();

    dbg_connection!("connection_pipeline: Setting client state in session");
    session_manager_lock.update_client_connections(
        client_hostname.clone(),
        ClientConnectionsAction::SetConnectionState(ConnectionState::Connecting),
    );
    session_manager_lock.update_client_connections(
        client_hostname.clone(),
        ClientConnectionsAction::UpdateCurrentIp(Some(client_ip)),
    );

    let maybe_streaming_caps =
        if let ClientConnectionResult::ConnectionAccepted(info) = connection_result {
            session_manager_lock.update_client_connections(
                client_hostname.clone(),
                ClientConnectionsAction::SetDisplayName(info.platform_string),
            );

            if info.client_protocol_id != alvr_common::protocol_id_u64() {
                warn!(
                    "Trusted client is incompatible! Expected protocol ID: {}, found: {}",
                    alvr_common::protocol_id_u64(),
                    info.client_protocol_id,
                );

                return Ok(());
            }

            info.streaming_capabilities
        } else {
            debug!("Found client in standby. Retrying");
            return Ok(());
        };

    let Some(streaming_caps) = maybe_streaming_caps else {
        con_bail!("Only streaming clients are supported for now");
    };

    let initial_settings = session_manager_lock.settings().clone();

    // Fresh instrumentation per session: counters that accumulate across connections are
    // worse than none, because they look like a rate.
    crate::reset_send_gate();

    // Resolve what this link actually is, once, and drive everything link-shaped from it:
    // the QoS posture, the WLAN optimizer, and host scheduling. The resolution is logged
    // in full because it is its own instrument — if the posture looks wrong later, this
    // line says what we thought the link was and why.
    let link = x_link::resolve(client_ip);
    let link_profile = x_link::profile_for_resolution(&link);
    info!(
        "Link resolved: {} -> wlan posture media_streaming={} background_scan={}, \
         planned {} Mbps, dscp {:?}, jitter buffer {} frames",
        link.summary(),
        link_profile.wlan.media_streaming,
        link_profile.wlan.background_scan,
        link_profile.expected_throughput_mbps,
        link_profile.dscp,
        link_profile.jitter_buffer_frames,
    );

    // Hold the PC's Wi-Fi adapter in streaming posture for exactly as long as this
    // connection lives: the guard restores the adapter when it drops, and it is dropped on
    // every return path out of this function, including a failed handshake.
    //
    // This is the one link-layer action that has no upstream equivalent; it is ported from
    // Virtual Desktop, whose `OptimizeWLAN` drives `wlan_intf_opcode_media_streaming_mode`
    // and `wlan_intf_opcode_background_scan_enabled` on every connected WLAN interface
    // (VD_RE/24-vd-link-qos.md).
    //
    // Whether to do it at all is *the resolved link's* decision, not the negotiated class's:
    // the honest condition is "the interface that reaches this client is wireless".
    let _wlan_session = if initial_settings.connection.wlan_optimizer && !link_profile.wlan.is_off()
    {
        match x_link::WlanSession::start(link_profile.wlan) {
            Ok(session) => Some(session),
            Err(e) => {
                // Not an error worth failing a session over: a wired-only machine has no
                // WLAN API to find, and "no connected WLAN interface" is reported by the
                // session's own log line, not by this branch.
                info!("WLAN optimizer not started: {e}");
                None
            }
        }
    } else {
        None
    };

    // Scheduling is process-wide and is restored on drop for the same reason.
    let _scheduler = initial_settings
        .connection
        .host_scheduling
        .then(x_link::HostScheduler::start);

    fn get_view_res(config: FrameSize, default_res: UVec2) -> UVec2 {
        let res = match config {
            FrameSize::Scale(scale) => default_res.as_vec2() * scale,
            FrameSize::Absolute { width, height } => {
                let width = width as f32;
                Vec2::new(
                    width,
                    height.map_or_else(
                        || {
                            let default_res = default_res.as_vec2();
                            width * default_res.y / default_res.x
                        },
                        |h| h as f32,
                    ),
                )
            }
        };

        UVec2::new(align32(res.x), align32(res.y))
    }

    let mut transcoding_view_resolution = get_view_res(
        initial_settings.video.transcoding_view_resolution.clone(),
        streaming_caps.default_view_resolution,
    );
    if transcoding_view_resolution.x > streaming_caps.max_view_resolution.x
        || transcoding_view_resolution.y > streaming_caps.max_view_resolution.y
    {
        warn!(
            "Chosen resolution {}x{} exceeds client maximum supported resolution of {}x{}. \
            Using maximum supported resolution at same aspect ratio.",
            transcoding_view_resolution.x,
            transcoding_view_resolution.y,
            streaming_caps.max_view_resolution.x,
            streaming_caps.max_view_resolution.y,
        );

        let transcoding_ratio =
            transcoding_view_resolution.x as f32 / transcoding_view_resolution.y as f32;

        if transcoding_ratio
            > streaming_caps.max_view_resolution.x as f32
                / streaming_caps.max_view_resolution.y as f32
        {
            transcoding_view_resolution = UVec2::new(
                align32(streaming_caps.max_view_resolution.x as f32),
                align32(streaming_caps.max_view_resolution.x as f32 / transcoding_ratio),
            );
        } else {
            transcoding_view_resolution = UVec2::new(
                align32(streaming_caps.max_view_resolution.y as f32 * transcoding_ratio),
                align32(streaming_caps.max_view_resolution.y as f32),
            );
        }
    }

    let emulated_headset_view_resolution = get_view_res(
        initial_settings
            .video
            .emulated_headset_view_resolution
            .clone(),
        streaming_caps.default_view_resolution,
    );

    let fps = {
        let mut best_match = 0_f32;
        let mut min_diff = f32::MAX;
        for rate in &streaming_caps.refresh_rates {
            let diff = (*rate - initial_settings.video.preferred_fps).abs();
            if diff < min_diff {
                best_match = *rate;
                min_diff = diff;
            }
        }
        best_match
    };

    if !streaming_caps
        .refresh_rates
        .contains(&initial_settings.video.preferred_fps)
    {
        warn!("Chosen refresh rate not supported. Using {fps}Hz");
    }

    // One frame at the chosen rate. The media plane is built on this number — the pacer's burst
    // credit, the sender's own `frame_interval` and the release policy all derive from it — so it is
    // computed once, from the negotiated rate, rather than re-derived from `fps` at each use.
    let frame_interval = Duration::from_secs_f32(1.0 / fps.max(1.0));

    let foveated_encoding = if let Switch::Enabled(config) =
        &initial_settings.video.foveated_encoding
    {
        if streaming_caps.foveated_encoding || config.force_enable {
            let mut params = AlvrFoveatedEncodingParams {
                edge_ratio: config.edge_ratio,
                ..Default::default()
            };

            for (axis, resolution) in transcoding_view_resolution
                .to_array()
                .into_iter()
                .enumerate()
            {
                let resolution = resolution as f32;
                // # Safety: `axis` comes from a two-element resolution array, and each
                // configuration array also contains exactly two axes.
                let (center_size, center_shift, edge_ratio) = (
                    config.center_size[axis],
                    config.center_shift[axis],
                    config.edge_ratio[axis],
                );
                let edge_size = resolution - center_size * resolution;

                // Preserve the existing C++ encoder's operation order and double intermediates.
                let center_size = (1.0
                    - (edge_size as f64 / (edge_ratio as f64 * 2.0)).ceil()
                        * (edge_ratio as f64 * 2.0)
                        / resolution as f64) as f32;
                let edge_size = resolution - center_size * resolution;
                let center_shift =
                    align_foveation_center_shift(center_shift, edge_size, edge_ratio);

                let scale =
                    (center_size as f64 + (1.0 - center_size as f64) / edge_ratio as f64) as f32;
                let optimized_resolution = scale * resolution;
                let encoded_resolution = (optimized_resolution / 32.0).ceil() as u32 * 32;

                // # Safety: `axis` is 0 or 1; all output axis arrays and the eye array have length 2.
                (
                    params.encoded_view_resolution[axis],
                    params.view_ratio[axis],
                    params.center_size[axis],
                    params.center_shifts[0][axis],
                    params.center_shifts[1][axis],
                ) = (
                    encoded_resolution,
                    optimized_resolution / encoded_resolution as f32,
                    center_size,
                    center_shift,
                    center_shift,
                );
            }

            Some(params)
        } else {
            warn!("Foveated encoding is not supported by the client.");

            None
        }
    } else {
        None
    };

    let encoder_profile = if initial_settings.video.encoder_config.h264_profile == H264Profile::High
    {
        let profile = if streaming_caps.encoder_high_profile {
            H264Profile::High
        } else {
            H264Profile::Main
        };

        if profile != H264Profile::High {
            warn!("High profile encoding is not supported by the client.");
        }

        profile
    } else {
        initial_settings.video.encoder_config.h264_profile
    };

    let mut enable_10_bits_encoding = initial_settings
        .video
        .encoder_config
        .use_10bit
        .unwrap_or(streaming_caps.prefer_10bit);

    if enable_10_bits_encoding && !streaming_caps.encoder_10_bits {
        warn!("10 bits encoding is not supported by the client.");
        enable_10_bits_encoding = false
    }

    let enable_hdr = initial_settings
        .video
        .encoder_config
        .hdr
        .enable
        .unwrap_or(streaming_caps.prefer_hdr);

    let encoding_gamma = initial_settings
        .video
        .encoder_config
        .encoding_gamma
        .unwrap_or(streaming_caps.preferred_encoding_gamma);

    let codec = if initial_settings.video.preferred_codec == CodecType::AV1 {
        let codec = if streaming_caps.encoder_av1 {
            CodecType::AV1
        } else {
            CodecType::Hevc
        };

        if codec != CodecType::AV1 {
            warn!("AV1 encoding is not supported by the client.");
        }

        codec
    } else {
        initial_settings.video.preferred_codec
    };

    #[cfg(not(target_os = "windows"))]
    let game_audio_sample_rate = 44100;

    #[cfg(target_os = "windows")]
    let game_audio_sample_rate =
        if let Switch::Enabled(game_audio_config) = &initial_settings.audio.game_audio {
            let game_audio_device =
                alvr_audio::new_output(game_audio_config.device.as_ref()).to_con()?;

            if let Switch::Enabled(microphone_config) = &initial_settings.audio.microphone
                && matches!(
                    microphone_config.devices,
                    alvr_session::MicrophoneDevicesConfig::VAC
                        | alvr_session::MicrophoneDevicesConfig::VBCable
                )
            {
                let (sink, _) =
                    alvr_audio::new_virtual_microphone_pair(microphone_config.devices.clone())
                        .to_con()?;

                // VoiceMeeter and Custom devices may have arbitrary internal routing.
                // Therefore, we cannot detect the loopback issue without knowing the routing.
                if alvr_audio::is_same_device(&game_audio_device, &sink) {
                    con_bail!("Game audio and microphone cannot point to the same device!");
                }
            }

            alvr_audio::input_sample_rate(&game_audio_device).to_con()?
        } else {
            0
        };

    let wired = client_ip.is_loopback();

    dbg_connection!("connection_pipeline: send streaming config");
    let stream_config_packet = StreamConfigPacket::new(
        session_manager_lock.session(),
        ClientNegotiatedStreamingConfig {
            view_resolution: transcoding_view_resolution,
            refresh_rate_hint: fps,
            game_audio_sample_rate,
            foveated_encoding,
            encoding_gamma,
            enable_hdr,
            wired,
            ext_str: String::new(),
        }
        .with_ext(NegotiatedStreamingConfigExt {}),
    )
    .to_con()?;

    let new_steamvr_hmd_init_config = SteamvrHmdInitConfig {
        eye_resolution_width: transcoding_view_resolution.x,
        eye_resolution_height: transcoding_view_resolution.y,
        target_eye_resolution_width: emulated_headset_view_resolution.x,
        target_eye_resolution_height: emulated_headset_view_resolution.y,
        refresh_rate: fps as _,
    };
    let new_hash = compute_restart_settings_hash(&new_steamvr_hmd_init_config, &initial_settings);
    if session_manager_lock.session().restart_settings_hash != new_hash {
        let mut session = session_manager_lock.session_mut();
        session.steamvr_hmd_init_config = new_steamvr_hmd_init_config;
        session.restart_settings_hash = new_hash;

        alvr_sockets::send_restart_signal(socket, stream_config_packet)?;

        crate::notify_restart_driver();

        *lifecycle_state.write() = LifecycleState::ShuttingDown;

        return Ok(());
    }

    let stream_protocol = if wired {
        SocketProtocol::Tcp
    } else {
        initial_settings.connection.stream_protocol
    };

    dbg_connection!("connection_pipeline: Finishing handshake");
    let mut socket = SocketConnection::from_client_connection(
        socket,
        HANDSHAKE_ACTION_TIMEOUT,
        stream_config_packet,
        StreamSocketConfig {
            protocol: stream_protocol,
            port: initial_settings.connection.stream_port,
            buffer_config: initial_settings.connection.server_buffer_config,
            max_packet_size: initial_settings.connection.packet_size as _,
            dscp: initial_settings.connection.dscp,
        },
    )?;

    dbg_connection!("connection_pipeline: Handshake successful, spawning threads");

    let disconnect_notif = Arc::new(Condvar::new());

    *ctx.statistics_manager.write() = Some(StatisticsManager::new(
        initial_settings.connection.statistics_history_size,
        Duration::from_secs_f32(1.0 / fps),
        if let Switch::Enabled(config) = &initial_settings.headset.controllers {
            config.steamvr_pipeline_frames
        } else {
            0.0
        },
    ));
    *ctx.bitrate_manager.lock() =
        BitrateManager::new(initial_settings.video.bitrate.history_size, fps);
    *ctx.tracking_manager.write() =
        TrackingManager::new(initial_settings.connection.statistics_history_size);

    let control_sender = Arc::new(Mutex::new(socket.request_reliable_stream()?));

    // ---------------------------------------------------------------------------------------
    // Video goes on the media plane, and there is no alternative path.
    //
    // It used to go through the multiplexed stream socket as stream `VIDEO`, and that path had no
    // error correction, no retransmission, no frame identity on the wire and **no counter for the
    // datagrams it discarded**. Measured: the client's own reader threw away 1.5 % of everything it
    // read, silently, and it looked like the network's fault for two days (`doc 50 §A10`). A path
    // that drops frames without recording it is not a fallback, it is a defect with a working-
    // looking surface.
    //
    // The per-frame metadata is *not* lost with the old header: `VideoPacketHeader` — view
    // parameters, foveation centres, the keyframe flag — is serialised in front of the frame's
    // bytes, so it travels on the same datagrams, under the same FEC, in the same order. Metadata
    // on any other channel can arrive out of step with the frame it describes.
    // ---------------------------------------------------------------------------------------
    let media_port = alvr_packets::media_port(initial_settings.connection.stream_port);
    let media_socket = alvr_sockets::media::MediaSocket::connect_to(
        std::net::SocketAddr::new(client_ip, media_port),
        initial_settings.connection.dscp,
    )?;
    let mut media_receive_socket = media_socket.try_clone()?;

    // The key the media plane is built on, agreed over the control socket during the handshake and
    // **never transmitted** — so a NACK is authenticated and a video datagram is sealed, and an
    // eavesdropper on the control socket cannot forge either. It is the same session secret the
    // client derived, under a different label for each purpose.
    let media_keys = socket.media_keys().clone();
    let media_cipher = media_keys.cipher_for_feedback();
    let mut feedback_receiver = x_transport::FeedbackReceiver::new(media_cipher);
    let mut video_sender = MediaSender::new(
        SenderConfig::matching_policy(
            initial_settings.connection.packet_size as usize,
            ParityPolicy::Ratio {
                fraction: MEDIA_STARTING_FEC_FRACTION,
            },
            &media_release_policy(fps),
            frame_interval,
        ),
        PacerConfig::for_rate(nominal_bitrate_bps(&initial_settings), frame_interval),
        frame_interval,
        // Sealed with the same session key the client derives, under the media label. This is what
        // the client demands: a media datagram that is not sealed under the negotiated epoch key is
        // refused, so a server that sent in the clear would deliver exactly zero frames.
        Some(media_keys),
    );
    video_sender.set_rtt(MEDIA_ROUND_TRIP);

    let game_audio_sender: alvr_sockets::StreamSender<()> = socket.request_unreliable_stream(AUDIO);
    let haptics_sender = socket.request_unreliable_stream(HAPTICS);

    let mut control_receiver = socket.subscribe_to_reliable_stream()?;
    let mut microphone_receiver: alvr_sockets::StreamReceiver<()> =
        socket.subscribe_to_unreliable_stream(AUDIO, MAX_UNREAD_PACKETS);
    let tracking_receiver =
        socket.subscribe_to_unreliable_stream::<TrackingData>(TRACKING, MAX_UNREAD_PACKETS);
    let mut statics_receiver =
        socket.subscribe_to_unreliable_stream::<ClientStatistics>(STATISTICS, MAX_UNREAD_PACKETS);

    let (video_channel_sender, video_channel_receiver) =
        std::sync::mpsc::sync_channel(initial_settings.connection.max_queued_server_video_frames);
    *ctx.video_channel_sender.lock() = Some(video_channel_sender);
    *ctx.haptics_sender.lock() = Some(haptics_sender);

    // The sender's clock, and what joins it to the stream's. A frame's target time is in the
    // driver's epoch; `Instant::elapsed` is in ours, and the two are unrelated numbers.
    let session_start = Instant::now();
    let mut timebase = TimebaseOffset::new(Duration::from_secs(1));
    let mut over_budget_frames = 0u64;
    let mut frames_this_run = 0u64;
    let mut bytes_this_run = 0u64;

    let video_send_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        let client_hostname = client_hostname.clone();
        move || {
            let mut media_socket = media_socket;
            let mut feedback_buffer = Vec::with_capacity(2048);

            while is_streaming(&client_hostname) {
                // The client's repair requests, **serviced on a millisecond cadence rather than a
                // frame cadence**.
                //
                // This used to drain the feedback socket once per iteration and then block for a
                // whole `STREAMING_RECV_TIMEOUT` on the next frame, on the reasoning that the loop
                // "runs at the frame rate, which is the rate a repair is useful at". It does not.
                // A repair is useful only if it arrives inside the **client's** repair window, which
                // is a few milliseconds: at 30 Hz a NACK arriving just after the drain waited 33 ms
                // for the next iteration, so every retransmit landed after the client had already
                // released the frame. Measured on the rig: the client reading 525 datagrams/s, the
                // link loss ~15 %, and 12 % of frames presented against 2771 held — with ~13 000
                // repair requests answered too late to matter. The repair path existed and was
                // decorative.
                //
                // The wait below is therefore short, and this drain runs between every chunk of it.
                //
                // Sealed under the session key, and replay-protected by the sequence the client
                // puts in every message — so a captured NACK replayed here is refused rather than
                // answered, which is what stops a retransmit storm being driven from one packet.
                loop {
                    match media_receive_socket.recv(&mut feedback_buffer, Duration::ZERO) {
                        SourceEvent::Datagram => {
                            let Ok(Some(feedback)) = feedback_receiver.open(&feedback_buffer)
                            else {
                                // Authentic-but-replayed, or not authentic at all. Either way there
                                // is nothing to act on and the counters hold the record.
                                continue;
                            };
                            let now = timebase.from_local(session_start.elapsed());
                            match video_sender.on_feedback(&mut media_socket, &feedback, now) {
                                FeedbackOutcome::KeyframeRequired { .. }
                                | FeedbackOutcome::Resume { .. } => {
                                    ctx.events_sender.send(ServerCoreEvent::RequestIDR).ok();
                                }
                                FeedbackOutcome::QueueDelay {
                                    micros,
                                    missing_per_mille,
                                    read_per_sec,
                                } => {
                                    // The client is behind and has said so. The lever is the
                                    // encoder's bitrate — fewer bits is fewer datagrams, which is
                                    // the only thing that actually drains the queue — and the
                                    // bitrate manager is what owns it.
                                    let mut manager = ctx.bitrate_manager.lock();
                                    manager.report_client_queue_delay(micros);
                                    manager.report_client_missing(missing_per_mille);
                                    // **The budget's source.** Everything the sender does about rate
                                    // is solved from this one number, and it is the only one that
                                    // survives a client which completes nothing.
                                    manager.report_client_read_rate(read_per_sec);
                                }
                                FeedbackOutcome::Acknowledged { newest, mask, .. } => {
                                    // **The one thing the encoder cannot work out for itself.** A
                                    // frame the client decoded may be referenced by the frames that
                                    // follow; a frame it did not, may not — and that is what keeps a
                                    // lost frame from poisoning everything behind it, and what lets
                                    // the client present across a hole instead of holding until a
                                    // keyframe. See `Feedback::Ack`.
                                    //
                                    // The bitmap travels with the cursor so one lost acknowledgement
                                    // is repaired by the next: the client's decoded frames are what
                                    // the encoder may build on, and a gap in the reports must not read
                                    // as a client that decoded nothing.
                                    debug!(
                                        "client acknowledged frame {newest} (mask {mask:#018b})"
                                    );
                                    ctx.bitrate_manager.lock().report_client_ack(newest);
                                }
                                _ => {}
                            }
                        }
                        SourceEvent::Timeout => break,
                        SourceEvent::Closed => return,
                    }
                }

                let VideoPacket {
                    mut header,
                    payload,
                } = match video_channel_receiver.recv_timeout(FEEDBACK_POLL_INTERVAL) {
                    Ok(packet) => packet,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                };

                // What came out of the encoder, before the per-frame header is prepended. This is
                // the number the rate control had to work with, and the one to compare against what
                // the encoder was asked for.
                let encoded_len = payload.len() as u64;
                crate::send_probe::dequeued();

                ctx.tracking_manager
                    .read()
                    .unrecenter_view_params(&mut header.global_view_params);

                // The header travels with the frame. bincode rather than a hand-rolled layout
                // because this struct has a dozen fields and will grow, and it is serialised once
                // per frame rather than once per datagram — ninety a second is not a hot path.
                let mut frame_bytes =
                    match bincode::serde::encode_to_vec(&header, bincode::config::standard()) {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            warn!("Could not serialise a video frame header: {e}");
                            continue;
                        }
                    };
                frame_bytes.extend_from_slice(&payload);

                let target = Duration::from_nanos(header.timestamp.as_nanos() as u64);
                let arrived = session_start.elapsed();
                timebase.observe(arrived, target);

                let meta = FrameMeta {
                    frame_index: header.frame_index,
                    target_timestamp_us: target.as_micros() as u64,
                    is_keyframe: header.is_idr,
                    key_epoch: 0,
                    reference_frame: header.reference_frame,
                };

                // The second rung of the degradation ladder, read per frame because it is a fact about
                // the client rather than about the encoder: skip frames rather than spend them. Safe
                // only because the encoder references only acknowledged frames — a frame that is not
                // sent is a frame the client never confirms, and the chain routes around it.
                //
                // **And the bootstrap, which is the state before any of that works.** While nothing
                // has ever been acknowledged, exactly one frame is in flight: it is the only frame
                // under pressure, so no later frame can take its slot, and what it is sized to is a
                // frame the client can actually read. Measured, on this rig: 2038 frames completed, 2
                // presented and 0 acknowledgements, because a 64-datagram keyframe needs 220 ms of
                // reading against release windows of 6, 12 and 66 ms with the stream still sending.
                let acknowledged = video_sender.client_acked_frame().is_some();
                let observed_parity = video_sender.stats().overhead_fraction() as f32;
                video_sender.set_stop_and_wait(!acknowledged);
                video_sender.set_frame_divisor({
                    let mut manager = ctx.bitrate_manager.lock();
                    manager.set_media_shape(
                        (initial_settings.connection.packet_size as usize)
                            .saturating_sub(x_transport::HEADER_LEN),
                        observed_parity,
                    );
                    manager.set_bootstrap(!acknowledged);
                    manager.ladder_frame_divisor()
                });

                // A bootstrap frame that did not arrive in the time its own size implied was too big
                // for this client: ask for a smaller one, and for a keyframe, because the next frame
                // the encoder produces will otherwise be a P-frame referencing the frame that was lost.
                if video_sender.take_bootstrap_timeout() {
                    let bytes = ctx.bitrate_manager.lock().shrink_bootstrap();
                    info!(
                        "the bootstrap frame did not arrive; asking for one of {bytes} bytes instead"
                    );
                    ctx.events_sender.send(ServerCoreEvent::RequestIDR).ok();
                }

                // Timed because the socket write is the *only* place the server can block: the
                // kernel send buffer fills when the receiver stops draining, and that is the
                // difference between the network losing a frame and the receiver being unable to
                // take one.
                let write_started = Instant::now();
                let sent = video_sender.send_frame(
                    &mut media_socket,
                    meta,
                    &frame_bytes,
                    timebase.from_local(arrived),
                );
                crate::send_probe::sent(write_started.elapsed());

                if sent.over_budget {
                    over_budget_frames += 1;
                    if over_budget_frames.is_multiple_of(90) {
                        warn!(
                            "Video pacing is {:.1} ms behind after {} frame(s): the configured \
                             bitrate cannot carry this stream, and the bitrate controller is the \
                             thing that has to hear about it",
                            sent.paced_wait.as_secs_f64() * 1e3,
                            over_budget_frames,
                        );
                    }
                }

                // The sender's own account of the repair path, every few seconds. Without it the
                // only way to see what the sender did with a client's requests is to stop the
                // session — and a repair path that answers the same question repeatedly, or answers
                // none of them, looks identical from the client's side.
                frames_this_run += 1;
                bytes_this_run += encoded_len;
                if frames_this_run.is_multiple_of(150) {
                    let stats = video_sender.stats();
                    // **The bisect: bytes per frame out of the encoder, next to the number the
                    // encoder was given.** Bytes unchanged means the value is not reaching it or it
                    // is ignoring it; bytes changed while datagrams per frame stay the same means
                    // packetisation, not rate control. Without both numbers on one line the two are
                    // indistinguishable, which is how a cap can look like it is working.
                    let mean_bytes = bytes_this_run as f64 / frames_this_run as f64;
                    let mean_mbps = mean_bytes * 8.0 * fps as f64 / 1e6;
                    let asked_mbps = ctx
                        .bitrate_manager
                        .lock()
                        .effective_bitrate_bps()
                        .unwrap_or(0.0)
                        as f64
                        / 1e6;
                    let (divisor, read_ceiling, budget) = {
                        let manager = ctx.bitrate_manager.lock();
                        (
                            manager.ladder_frame_divisor(),
                            manager.read_ceiling_per_sec(),
                            manager.delivery_budget_per_sec(),
                        )
                    };
                    info!(
                        "encoder: {mean_bytes:.0} B/frame ({mean_mbps:.2} Mbps at {fps:.0} fps), asked \
                         for {asked_mbps:.2} Mbps, sending 1 frame in {divisor}; client reads \
                         {read_ceiling} datagram(s)/s, budget {} datagram(s)/s",
                        budget.unwrap_or(0.0).round() as u64
                    );
                    info!(
                        "media sender: {} frame(s) sent, {} datagram(s) ({} parity), {} skipped for \
                         rate, {} requested, {} retransmitted, {} coalesced, refusals {}/{}/{}/{}",
                        stats.frames_sent,
                        stats.datagrams_sent,
                        stats.parity_datagrams,
                        stats.frames_skipped_for_rate,
                        stats.retransmit_requests,
                        stats.retransmitted_datagrams,
                        stats.repairs_coalesced,
                        stats.repairs_refused_expired,
                        stats.repairs_refused_late,
                        stats.repairs_refused_evicted,
                        stats.repairs_refused_unknown,
                    );
                    if stats.queue_delay_reports > 0 {
                        info!(
                            "media sender: client reports {} us of queueing delay ({} reports)",
                            stats.reported_queue_delay_us, stats.queue_delay_reports,
                        );
                    }
                }
            }
        }
    });

    #[cfg_attr(target_os = "linux", expect(unused_variables))]
    let game_audio_thread = if let Switch::Enabled(config) =
        initial_settings.audio.game_audio.clone()
    {
        #[cfg(windows)]
        let ctx = Arc::clone(&ctx);

        let client_hostname = client_hostname.clone();
        thread::spawn(move || {
            #[cfg(not(target_os = "linux"))]
            while is_streaming(&client_hostname) {
                {
                    let device = match alvr_audio::new_output(config.device.as_ref()) {
                        Ok(data) => data,
                        Err(e) => {
                            warn!("New audio device failed: {e:?}");
                            thread::sleep(RETRY_CONNECT_MIN_INTERVAL);
                            continue;
                        }
                    };

                    #[cfg(windows)]
                    if let Ok(id) = alvr_audio::get_windows_device_id(&device) {
                        let prop = alvr_session::OpenvrProperty {
                            key: alvr_session::OpenvrPropKey::AudioDefaultPlaybackDeviceIdString,
                            value: id,
                        };
                        ctx.events_sender
                            .send(ServerCoreEvent::SetOpenvrProperty {
                                device_id: *alvr_common::HEAD_ID,
                                prop,
                            })
                            .ok();
                    } else {
                        continue;
                    };

                    if let Err(e) = alvr_audio::record_audio_blocking(
                        Arc::new({
                            let client_hostname = client_hostname.clone();
                            move || is_streaming(&client_hostname)
                        }),
                        game_audio_sender.clone(),
                        &device,
                        2,
                        config.mute_when_streaming,
                    ) {
                        error!("Audio record error: {e:?}");
                    }

                    #[cfg(windows)]
                    if let Ok(id) = alvr_audio::new_output(None)
                        .and_then(|d| alvr_audio::get_windows_device_id(&d))
                    {
                        let prop = alvr_session::OpenvrProperty {
                            key: alvr_session::OpenvrPropKey::AudioDefaultPlaybackDeviceIdString,
                            value: id,
                        };
                        ctx.events_sender
                            .send(ServerCoreEvent::SetOpenvrProperty {
                                device_id: *alvr_common::HEAD_ID,
                                prop,
                            })
                            .ok();
                    }
                }
            }
        })
    } else {
        thread::spawn(|| ())
    };

    #[cfg(not(target_os = "linux"))]
    let microphone_thread = if let Switch::Enabled(config) =
        initial_settings.audio.microphone.clone()
    {
        #[allow(unused_variables)]
        let (sink, source) = alvr_audio::new_virtual_microphone_pair(config.devices).to_con()?;

        #[cfg(windows)]
        if let Ok(id) = alvr_audio::get_windows_device_id(&source) {
            ctx.events_sender
                .send(ServerCoreEvent::SetOpenvrProperty {
                    device_id: *alvr_common::HEAD_ID,
                    prop: alvr_session::OpenvrProperty {
                        key: alvr_session::OpenvrPropKey::AudioDefaultRecordingDeviceIdString,
                        value: id,
                    },
                })
                .ok();
        }

        let client_hostname = client_hostname.clone();
        thread::spawn(move || {
            alvr_common::show_err(alvr_audio::play_audio_loop(
                {
                    let client_hostname = client_hostname.clone();
                    move || is_streaming(&client_hostname)
                },
                &sink,
                1,
                streaming_caps.microphone_sample_rate,
                config.buffering,
                &mut microphone_receiver,
            ));
        })
    } else {
        thread::spawn(|| ())
    };

    #[cfg(target_os = "linux")]
    let microphone_thread = {
        use alvr_audio::linux::{self, AudioInfo};
        let mic = if let Switch::Enabled(config) = initial_settings.audio.microphone.clone() {
            Some((
                AudioInfo {
                    sample_rate: streaming_caps.microphone_sample_rate,
                    channel_count: 1,
                },
                config.buffering,
            ))
        } else {
            None
        };

        let audio_info = initial_settings
            .audio
            .game_audio
            .enabled()
            .then_some(AudioInfo {
                sample_rate: game_audio_sample_rate,
                channel_count: 2,
            });

        if mic.is_some() || audio_info.is_some() {
            let client_hostname = client_hostname.clone();
            thread::spawn(move || {
                linux::audio_loop(
                    {
                        let client_hostname = client_hostname.clone();
                        move || is_streaming(&client_hostname)
                    },
                    game_audio_sender,
                    audio_info,
                    &mut microphone_receiver,
                    mic,
                );
            })
        } else {
            thread::spawn(|| ())
        }
    };

    let tracking_receive_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        let initial_settings = initial_settings.clone();
        let client_hostname = client_hostname.clone();
        move || {
            tracking::tracking_loop(&ctx, initial_settings, tracking_receiver, || {
                is_streaming(&client_hostname)
            });
        }
    });

    let statistics_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        let client_hostname = client_hostname.clone();
        move || {
            while is_streaming(&client_hostname) {
                let data = match statics_receiver.recv(STREAMING_RECV_TIMEOUT) {
                    Ok(stats) => stats,
                    Err(ConnectionError::TryAgain(_)) => continue,
                    Err(ConnectionError::Other(_)) => return,
                };
                let Ok(client_stats) = data.get_header() else {
                    return;
                };

                if let Some(stats) = &mut *ctx.statistics_manager.write() {
                    let timestamp = client_stats.target_timestamp;
                    let decoder_latency = client_stats.video_decode;
                    let (network_latency, game_latency) = stats.report_statistics(client_stats);

                    ctx.events_sender
                        .send(ServerCoreEvent::GameRenderLatencyFeedback(game_latency))
                        .ok();

                    let session_manager_lock = SESSION_MANAGER.read();
                    ctx.bitrate_manager.lock().report_frame_latencies(
                        &session_manager_lock.settings().video.bitrate.mode,
                        timestamp,
                        network_latency,
                        decoder_latency,
                    );
                }
            }
        }
    });

    let real_time_update_thread = thread::spawn({
        let control_sender = Arc::clone(&control_sender);
        let client_hostname = client_hostname.clone();
        move || {
            let mut previous_config = None;
            while is_streaming(&client_hostname) {
                let config = {
                    let session_manager_lock = SESSION_MANAGER.read();
                    let settings = session_manager_lock.settings();

                    RealTimeConfig::from_settings(settings)
                };

                let same_config = previous_config.as_ref().is_some_and(|prev| config == *prev);
                if !same_config {
                    previous_config = Some(config.clone());

                    control_sender
                        .lock()
                        .send(&ServerControlPacket::RealTimeConfig(config))
                        .ok();
                }

                thread::sleep(REAL_TIME_UPDATE_INTERVAL);
            }
        }
    });

    let keepalive_thread = thread::spawn({
        let control_sender = Arc::clone(&control_sender);
        let disconnect_notif = Arc::clone(&disconnect_notif);
        let client_hostname = client_hostname.clone();
        move || {
            while is_streaming(&client_hostname) {
                if let Err(e) = control_sender.lock().send(&ServerControlPacket::KeepAlive) {
                    info!("Client disconnected. Cause: {e:?}");

                    disconnect_notif.notify_one();

                    return;
                }

                thread::sleep(KEEPALIVE_INTERVAL);
            }
        }
    });

    let control_receive_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);

        let controllers_config = session_manager_lock
            .settings()
            .headset
            .controllers
            .as_option();
        let mut controller_button_mapping_manager = controllers_config.map(|config| {
            if let Some(mappings) = &config.button_mappings {
                ButtonMappingManager::new_manual(mappings)
            } else {
                ButtonMappingManager::new_automatic(
                    &CONTROLLER_PROFILE_INFO
                        .get(&alvr_common::hash_string(QUEST_CONTROLLER_PROFILE_PATH))
                        .unwrap()
                        .button_set,
                    &config.emulation_mode,
                    &config.button_mapping_config,
                )
            }
        });
        let controllers_emulation_mode =
            controllers_config.map(|config| config.emulation_mode.clone());

        let disconnect_notif = Arc::clone(&disconnect_notif);
        let control_sender = Arc::clone(&control_sender);
        let client_hostname = client_hostname.clone();
        move || {
            let mut disconnection_deadline = Instant::now() + KEEPALIVE_TIMEOUT;
            while is_streaming(&client_hostname) {
                let packet = match control_receiver.recv(STREAMING_RECV_TIMEOUT) {
                    Ok(packet) => packet,
                    Err(ConnectionError::TryAgain(_)) => {
                        if Instant::now() > disconnection_deadline {
                            info!("Client disconnected. Timeout");
                            break;
                        } else {
                            continue;
                        }
                    }
                    Err(e) => {
                        info!("Client disconnected. Cause: {e}");
                        break;
                    }
                };

                match packet {
                    ClientControlPacket::PlayspaceSync(packet) => {
                        if !initial_settings.headset.tracking_ref_only {
                            let session_manager_lock = SESSION_MANAGER.read();
                            let config = &session_manager_lock.settings().headset;
                            ctx.tracking_manager
                                .write()
                                .recenter(&config.recentering_mode);

                            let area = packet.unwrap_or(Vec2::new(2.0, 2.0));
                            let wh = area.x * area.y;
                            if wh.is_finite() && wh > 0.0 {
                                info!("Received new playspace with size: {}", area);
                                ctx.events_sender
                                    .send(ServerCoreEvent::PlayspaceSync(area))
                                    .ok();
                            } else {
                                warn!("Received invalid playspace size: {}", area);
                                ctx.events_sender
                                    .send(ServerCoreEvent::PlayspaceSync(Vec2::new(2.0, 2.0)))
                                    .ok();
                            }
                        }
                    }
                    ClientControlPacket::RequestIdr => {
                        if let Some(config) = ctx.decoder_config.lock().clone() {
                            control_sender
                                .lock()
                                .send(&ServerControlPacket::DecoderConfig(config))
                                .ok();
                        }
                        ctx.events_sender.send(ServerCoreEvent::RequestIDR).ok();
                    }
                    ClientControlPacket::MediaKeyHandshake(_) => {
                        // The media key is agreed during the socket connection's own handshake,
                        // before a single media datagram exists. A handshake message arriving here
                        // is out of place, and re-keying a live plane mid-stream would black it out
                        // rather than repair it, so it is refused rather than obeyed.
                        warn!("Ignoring a media key handshake after the session was keyed");
                    }
                    ClientControlPacket::LocalViewParams(params) => {
                        ctx.events_sender
                            .send(ServerCoreEvent::LocalViewParams(params))
                            .ok();
                    }
                    ClientControlPacket::Battery(packet) => {
                        ctx.events_sender
                            .send(ServerCoreEvent::Battery(packet.clone()))
                            .ok();

                        if let Some(stats) = &mut *ctx.statistics_manager.write() {
                            stats.report_battery(
                                packet.device_id,
                                packet.gauge_value,
                                packet.is_plugged,
                            );
                        }
                    }
                    ClientControlPacket::Buttons(entries) => {
                        {
                            let session_manager_lock = SESSION_MANAGER.read();
                            if session_manager_lock
                                .settings()
                                .extra
                                .logging
                                .log_button_presses
                            {
                                alvr_events::send_event(EventType::Buttons(
                                    entries
                                        .iter()
                                        .map(|e| ButtonEvent {
                                            path: BUTTON_INFO.get(&e.path_id).map_or_else(
                                                || format!("Unknown (ID: {:#16x})", e.path_id),
                                                |info| info.path.to_owned(),
                                            ),
                                            value: e.value,
                                        })
                                        .collect(),
                                ));
                            }
                        }

                        if let Some(manager) = &mut controller_button_mapping_manager {
                            let button_entries = entries
                                .iter()
                                .flat_map(|entry| manager.map_button(entry))
                                .collect::<Vec<_>>();

                            if !button_entries.is_empty() {
                                ctx.events_sender
                                    .send(ServerCoreEvent::Buttons(button_entries))
                                    .ok();
                            }
                        };
                    }
                    ClientControlPacket::ActiveInteractionProfile { input_ids, .. } => {
                        controller_button_mapping_manager = if let Switch::Enabled(config) =
                            &SESSION_MANAGER.read().settings().headset.controllers
                        {
                            if let Some(mappings) = &config.button_mappings {
                                Some(ButtonMappingManager::new_manual(mappings))
                            } else {
                                controllers_emulation_mode.as_ref().map(|emulation_mode| {
                                    ButtonMappingManager::new_automatic(
                                        &input_ids,
                                        emulation_mode,
                                        &config.button_mapping_config,
                                    )
                                })
                            }
                        } else {
                            None
                        };
                    }
                    ClientControlPacket::Log { level, message } => {
                        info!("Client {client_hostname}: [{level:?}] {message}")
                    }
                    ClientControlPacket::KeepAlive | ClientControlPacket::StreamReady => (),
                    ClientControlPacket::ProximityState(headset_is_worn) => {
                        ctx.events_sender
                            .send(ServerCoreEvent::ProximityState(headset_is_worn))
                            .ok();
                    }
                    ClientControlPacket::Reserved(_) | ClientControlPacket::ReservedBuffer(_) => (),
                }

                disconnection_deadline = Instant::now() + KEEPALIVE_TIMEOUT;
            }

            disconnect_notif.notify_one()
        }
    });

    let stream_receive_thread = thread::spawn({
        let disconnect_notif = Arc::clone(&disconnect_notif);
        let client_hostname = client_hostname.clone();
        move || {
            while is_streaming(&client_hostname) {
                match socket.recv_poll() {
                    Ok(()) => (),
                    Err(ConnectionError::TryAgain(_)) => continue,
                    Err(e) => {
                        info!("Client disconnected. Cause: {e}");

                        disconnect_notif.notify_one();

                        return;
                    }
                }
            }
        }
    });

    let lifecycle_check_thread = thread::spawn({
        let disconnect_notif = Arc::clone(&disconnect_notif);
        let client_hostname = client_hostname.clone();
        move || {
            while SESSION_MANAGER
                .read()
                .client_list()
                .get(&client_hostname)
                .is_some_and(|c| c.connection_state == ConnectionState::Streaming)
                && *lifecycle_state.read() == LifecycleState::Resumed
            {
                thread::sleep(STREAMING_RECV_TIMEOUT);
            }

            disconnect_notif.notify_one()
        }
    });

    if initial_settings.connection.enable_on_connect_script {
        let on_connect_script = FILESYSTEM_LAYOUT.get().map(|l| l.connect_script()).unwrap();
        info!(
            "Running on connect script (connect): {}",
            on_connect_script.display()
        );
        if let Err(e) = Command::new(&on_connect_script)
            .env("ACTION", "connect")
            .spawn()
        {
            warn!("Failed to run connect script: {e}");
        }
    }
    if initial_settings.extra.capture.startup_video_recording {
        info!("Creating recording file");
        crate::create_recording_file(&ctx, session_manager_lock.settings());
    }

    session_manager_lock.update_client_connections(
        client_hostname.clone(),
        ClientConnectionsAction::SetConnectionState(ConnectionState::Streaming),
    );

    ctx.events_sender
        .send(ServerCoreEvent::ClientConnected(
            ServerNegotiatedStreamingConfig {
                transcoding_view_resolution,
                emulated_headset_view_resolution: transcoding_view_resolution,
                refresh_rate: fps as _,
                foveated_encoding,
                codec,
                h264_profile: encoder_profile,
                use_10bit_encoder: enable_10_bits_encoding,
                encoding_gamma,
                enable_hdr,
            },
        ))
        .ok();

    dbg_connection!("connection_pipeline: Threads initialized; unlocking streams");
    alvr_common::wait_rwlock(&disconnect_notif, &mut session_manager_lock);
    dbg_connection!("connection_pipeline: Begin connection shutdown");

    // The session's own instrumentation, printed before the threads come down so it is in the
    // log even if the shutdown itself is what breaks. This is the block that answers "why did
    // a frame go missing": the two counters at the end are what separate a stalled receiver
    // from a lossy link, and nothing else can.
    {
        let probe = crate::send_probe::snapshot();
        let gate = crate::send_gate_snapshot();
        info!(
            "Video send summary: {} frames sent, {} discarded (queue full), {} suppressed \
             (reference chain), max queue depth {}/{}, send blocked {} ms total / {} ms longest",
            probe.frames_sent,
            gate.discarded,
            gate.suppressed,
            probe.max_queue_depth,
            initial_settings.connection.max_queued_server_video_frames,
            probe.send_blocked_ms,
            probe.longest_send_blocked_ms,
        );
    }

    // This requests shutdown from threads
    *ctx.video_channel_sender.lock() = None;
    *ctx.haptics_sender.lock() = None;

    *ctx.video_recording_file.lock() = None;

    session_manager_lock.update_client_connections(
        client_hostname,
        ClientConnectionsAction::SetConnectionState(ConnectionState::Disconnecting),
    );

    let enable_on_disconnect_script = session_manager_lock
        .settings()
        .connection
        .enable_on_disconnect_script;
    if enable_on_disconnect_script {
        let on_disconnect_script = FILESYSTEM_LAYOUT
            .get()
            .map(|l| l.disconnect_script())
            .unwrap();
        info!(
            "Running on disconnect script (disconnect): {}",
            on_disconnect_script.display()
        );
        if let Err(e) = Command::new(&on_disconnect_script)
            .env("ACTION", "disconnect")
            .spawn()
        {
            warn!("Failed to run disconnect script: {e}");
        }
    }

    // Allow threads to shutdown correctly
    drop(session_manager_lock);

    // Ensure shutdown of threads
    dbg_connection!("connection_pipeline: Shutdown threads");
    video_send_thread.join().ok();
    game_audio_thread.join().ok();
    microphone_thread.join().ok();
    tracking_receive_thread.join().ok();
    statistics_thread.join().ok();
    real_time_update_thread.join().ok();
    control_receive_thread.join().ok();
    stream_receive_thread.join().ok();
    keepalive_thread.join().ok();
    lifecycle_check_thread.join().ok();

    ctx.events_sender
        .send(ServerCoreEvent::ClientDisconnected)
        .ok();

    dbg_connection!("connection_pipeline: End");

    Ok(())
}
