mod bitrate;
mod c_api;
mod connection;
mod hand_gestures;
mod haptics;
mod input_mapping;
mod logging_backend;
mod sockets;
mod statistics;
mod tracking;
mod web_server;

pub use c_api::*;
pub use logging_backend::init_logging;
pub use tracking::HandType;
pub use x_foveation::{EyeTrackedFoveation, align_foveation_center_shift};

use crate::connection::VideoPacket;
use alvr_common::{
    AlvrFoveatedEncodingParams, ConnectionState, DEVICE_ID_TO_PATH, DeviceMotion, LifecycleState,
    Pose, ViewParams, dbg_server_core, debug, error,
    glam::{Quat, UVec2, Vec2},
    parking_lot::{Mutex, RwLock},
    settings_schema::Switch,
    warn,
};
use alvr_events::{EventType, HapticsEvent};
use alvr_filesystem as afs;
use alvr_packets::{
    BatteryInfo, ButtonEntry, ClientConnectionsAction, DecoderInitializationConfig, Haptics,
    VideoPacketHeader,
};
use alvr_server_io::ServerSessionManager;
use alvr_session::{CodecType, H264Profile, OpenvrProperty, Settings, SteamvrHmdInitConfig};
use alvr_sockets::StreamSender;
use bitrate::{BitrateManager, DynamicEncoderParams};
use statistics::StatisticsManager;
use std::{
    collections::HashSet,
    env,
    ffi::OsStr,
    fs::File,
    io::Write,
    sync::{
        Arc, LazyLock, OnceLock,
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::broadcast};
use tracking::TrackingManager;

static FILESYSTEM_LAYOUT: OnceLock<afs::Layout> = OnceLock::new();

// This is lazily initialized when initializing logging or ServerCoreContext. So FILESYSTEM_LAYOUT
// needs to be initialized first using initialize_environment().
// NB: this must remain a global because only one instance should exist for the whole application
// execution time.
/// ADR-0011's send gate for the video path, at module scope so the session summary can read
/// its counters. One gate, because there is one video stream.
pub(crate) static SEND_GATE: Mutex<x_transport::SendGate> =
    Mutex::new(x_transport::SendGate::new());

/// A snapshot of ADR-0011's send gate, for the session summary.
pub fn send_gate_snapshot() -> SendGateSnapshot {
    let gate = SEND_GATE.lock();
    SendGateSnapshot {
        discarded: gate.discarded(),
        suppressed: gate.suppressed_frames(),
    }
}

/// Start a session with the gate and the send probe at zero rather than accumulating across
/// connections.
pub fn reset_send_gate() {
    *SEND_GATE.lock() = x_transport::SendGate::new();
    send_probe::reset();
}

pub struct SendGateSnapshot {
    pub discarded: u64,
    pub suppressed: u64,
}

/// Instrumentation for the video send path, and the counters that make the grey-frame
/// question answerable.
///
/// The question is *why* a frame goes missing, and there are three candidate mechanisms that
/// look identical from the client:
///
/// 1. **The server discarded it** — the bounded channel was full. Already logged, and counted
///    by [`crate::SendGate`].
/// 2. **The network lost it** — every datagram vanished. Invisible on this side.
/// 3. **The receiver stopped draining** — the client's kernel buffer filled, then this side's
///    send buffer filled, and the send thread sat blocked inside a UDP `send()`. The frame is
///    then discarded here (mechanism 1) *because* of mechanism 3.
///
/// 1 and 2 need the client's frame indices to separate, and every frame the server transmits
/// carries one, so the client's capture and [`SEND_GATE`]'s counters are enough. Separating 3
/// from 2 needs the two numbers here: **how deep the queue ever got**, and **how long the send
/// thread spent blocked**. If the queue never went deep and nothing ever blocked, then
/// mechanism 1 never fired, and a missing frame was the network's doing. If the thread spent
/// seconds blocked, the drop is a symptom of a stalled receiver and the fix is not in the
/// transport at all.
pub(crate) mod send_probe {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    /// Frames currently queued for the send thread.
    static QUEUE_DEPTH: AtomicUsize = AtomicUsize::new(0);
    /// The high-water mark, which is the number that says whether the channel ever came close
    /// to the capacity that `try_send` fails at.
    static MAX_QUEUE_DEPTH: AtomicUsize = AtomicUsize::new(0);
    /// Nanoseconds spent inside the blocking UDP send.
    static SEND_BLOCKED_NS: AtomicU64 = AtomicU64::new(0);
    /// The longest single blocking send, which distinguishes "steady slight backpressure"
    /// from "one multi-second stall".
    static LONGEST_SEND_BLOCKED_NS: AtomicU64 = AtomicU64::new(0);
    static FRAMES_SENT: AtomicU64 = AtomicU64::new(0);

    pub fn reset() {
        QUEUE_DEPTH.store(0, Ordering::SeqCst);
        MAX_QUEUE_DEPTH.store(0, Ordering::SeqCst);
        SEND_BLOCKED_NS.store(0, Ordering::SeqCst);
        LONGEST_SEND_BLOCKED_NS.store(0, Ordering::SeqCst);
        FRAMES_SENT.store(0, Ordering::SeqCst);
    }

    /// A frame was handed to the send thread.
    pub fn enqueued() {
        let depth = QUEUE_DEPTH.fetch_add(1, Ordering::SeqCst) + 1;
        MAX_QUEUE_DEPTH.fetch_max(depth, Ordering::SeqCst);
    }

    /// The send thread took a frame off the queue.
    pub fn dequeued() {
        QUEUE_DEPTH.fetch_sub(1, Ordering::SeqCst);
    }

    /// The send thread finished writing a frame to the socket, after this long.
    pub fn sent(blocked: std::time::Duration) {
        FRAMES_SENT.fetch_add(1, Ordering::SeqCst);
        let ns = blocked.as_nanos() as u64;
        SEND_BLOCKED_NS.fetch_add(ns, Ordering::SeqCst);
        LONGEST_SEND_BLOCKED_NS.fetch_max(ns, Ordering::SeqCst);
    }

    pub struct Snapshot {
        pub frames_sent: u64,
        pub max_queue_depth: usize,
        pub send_blocked_ms: f64,
        pub longest_send_blocked_ms: f64,
    }

    pub fn snapshot() -> Snapshot {
        Snapshot {
            frames_sent: FRAMES_SENT.load(Ordering::SeqCst),
            max_queue_depth: MAX_QUEUE_DEPTH.load(Ordering::SeqCst),
            send_blocked_ms: SEND_BLOCKED_NS.load(Ordering::SeqCst) as f64 / 1e6,
            longest_send_blocked_ms: LONGEST_SEND_BLOCKED_NS.load(Ordering::SeqCst) as f64 / 1e6,
        }
    }
}

static SESSION_MANAGER: LazyLock<RwLock<ServerSessionManager>> = LazyLock::new(|| {
    RwLock::new(ServerSessionManager::new(
        FILESYSTEM_LAYOUT.get().map(|l| l.session()),
    ))
});

pub fn initialize_environment(layout: afs::Layout) {
    FILESYSTEM_LAYOUT.set(layout).unwrap();

    // This ensures that the session is written to disk
    SESSION_MANAGER.write().session_mut();
}

pub struct ServerNegotiatedStreamingConfig {
    pub transcoding_view_resolution: UVec2,
    pub emulated_headset_view_resolution: UVec2,
    pub refresh_rate: f32,
    pub foveated_encoding: Option<AlvrFoveatedEncodingParams>,
    pub codec: CodecType,
    pub h264_profile: H264Profile,
    pub use_10bit_encoder: bool,
    pub encoding_gamma: f32,
    pub enable_hdr: bool,
}

pub enum ServerCoreEvent {
    SetOpenvrProperty {
        device_id: u64,
        prop: OpenvrProperty,
    },
    ClientConnected(ServerNegotiatedStreamingConfig),
    ClientDisconnected,
    Battery(BatteryInfo),
    PlayspaceSync(Vec2),
    LocalViewParams([ViewParams; 2]), // In relation to head
    Tracking {
        poll_timestamp: Duration,
    },
    Buttons(Vec<ButtonEntry>), // Note: this is after mapping
    RequestIDR,
    CaptureFrame,
    GameRenderLatencyFeedback(Duration), // only used for SteamVR
    ShutdownPending,
    RestartPending,
    ProximityState(bool),
}

pub struct ConnectionContext {
    events_sender: mpsc::Sender<ServerCoreEvent>,
    statistics_manager: RwLock<Option<StatisticsManager>>,
    bitrate_manager: Mutex<BitrateManager>,
    tracking_manager: RwLock<TrackingManager>,
    decoder_config: Mutex<Option<DecoderInitializationConfig>>,
    video_mirror_sender: Mutex<Option<broadcast::Sender<Vec<u8>>>>,
    video_recording_file: Mutex<Option<File>>,
    connection_threads: Mutex<Vec<JoinHandle<()>>>,
    clients_to_be_removed: Mutex<HashSet<String>>,
    video_channel_sender: Mutex<Option<SyncSender<VideoPacket>>>,
    haptics_sender: Mutex<Option<StreamSender<Haptics>>>,
}

pub fn create_recording_file(connection_context: &ConnectionContext, settings: &Settings) {
    let codec = settings.video.preferred_codec;
    let ext = match codec {
        CodecType::H264 => "h264",
        CodecType::Hevc => "h265",
        CodecType::AV1 => "av1",
    };

    let path = FILESYSTEM_LAYOUT.get().unwrap().log_dir.join(format!(
        "recording.{}.{ext}",
        chrono::Local::now().format("%F.%H-%M-%S")
    ));

    match File::create(path) {
        Ok(mut file) => {
            if let Some(config) = &*connection_context.decoder_config.lock() {
                file.write_all(&config.config_buffer).ok();
            }

            *connection_context.video_recording_file.lock() = Some(file);

            connection_context
                .events_sender
                .send(ServerCoreEvent::RequestIDR)
                .ok();
        }
        Err(e) => {
            error!("Failed to record video on disk: {e}");
        }
    }
}

pub fn notify_restart_driver() {
    if sysinfo::System::new_all()
        .processes_by_name(OsStr::new(&afs::dashboard_fname()))
        .next()
        .is_some()
    {
        alvr_events::send_event(EventType::ServerRequestsSelfRestart);
    } else {
        error!("Cannot restart SteamVR. No dashboard process found on local device.");
    }
}

pub fn settings() -> Settings {
    SESSION_MANAGER.read().settings().clone()
}

pub fn steamvr_hmd_init_config() -> SteamvrHmdInitConfig {
    SESSION_MANAGER
        .read()
        .session()
        .steamvr_hmd_init_config
        .clone()
}

pub fn registered_button_set() -> HashSet<u64> {
    let session_manager = SESSION_MANAGER.read();
    if let Switch::Enabled(input_mapping) = &session_manager.settings().headset.controllers {
        input_mapping::registered_button_set(&input_mapping.emulation_mode)
    } else {
        HashSet::new()
    }
}

pub struct ServerCoreContext {
    lifecycle_state: Arc<RwLock<LifecycleState>>,
    connection_context: Arc<ConnectionContext>,
    connection_thread: Arc<RwLock<Option<JoinHandle<()>>>>,
    webserver_runtime: Option<Runtime>,
}

impl ServerCoreContext {
    pub fn new() -> (Self, mpsc::Receiver<ServerCoreEvent>) {
        dbg_server_core!("Creating");

        if SESSION_MANAGER
            .read()
            .settings()
            .extra
            .logging
            .prefer_backtrace
        {
            unsafe { env::set_var("RUST_BACKTRACE", "1") };
        }

        SESSION_MANAGER.write().clean_client_list();

        let (events_sender, events_receiver) = mpsc::channel();

        // Create a temporary StatisticsManager until a headset connects
        let initial_settings = SESSION_MANAGER.read().settings().clone();
        let stats = StatisticsManager::new(
            initial_settings.connection.statistics_history_size,
            Duration::from_secs_f32(1.0 / 90.0),
            if let Switch::Enabled(config) = &initial_settings.headset.controllers {
                config.steamvr_pipeline_frames
            } else {
                0.0
            },
        );

        let connection_context = Arc::new(ConnectionContext {
            events_sender,
            statistics_manager: RwLock::new(Some(stats)),
            bitrate_manager: Mutex::new(BitrateManager::new(256, 60.0)),
            tracking_manager: RwLock::new(TrackingManager::new(
                initial_settings.connection.statistics_history_size,
            )),
            decoder_config: Mutex::new(None),
            video_mirror_sender: Mutex::new(None),
            video_recording_file: Mutex::new(None),
            connection_threads: Mutex::new(Vec::new()),
            clients_to_be_removed: Mutex::new(HashSet::new()),
            video_channel_sender: Mutex::new(None),
            haptics_sender: Mutex::new(None),
        });

        let webserver_runtime = Runtime::new().unwrap();
        webserver_runtime.spawn({
            let connection_context = Arc::clone(&connection_context);
            async move { alvr_common::show_err(web_server::web_server(connection_context).await) }
        });

        (
            Self {
                lifecycle_state: Arc::new(RwLock::new(LifecycleState::StartingUp)),
                connection_context,
                connection_thread: Arc::new(RwLock::new(None)),
                webserver_runtime: Some(webserver_runtime),
            },
            events_receiver,
        )
    }

    pub fn start_connection(&self) {
        dbg_server_core!("start_connection");

        // Note: Idle state is not used on the server side
        *self.lifecycle_state.write() = LifecycleState::Resumed;

        let connection_context = Arc::clone(&self.connection_context);
        let lifecycle_state = Arc::clone(&self.lifecycle_state);
        *self.connection_thread.write() = Some(thread::spawn(move || {
            connection::handshake_loop(connection_context, lifecycle_state);
        }));
    }

    pub fn get_device_motion(
        &self,
        device_id: u64,
        sample_timestamp: Duration,
    ) -> Option<DeviceMotion> {
        dbg_server_core!("get_device_motion: dev={device_id} sample_ts={sample_timestamp:?}");

        self.connection_context
            .tracking_manager
            .read()
            .get_device_motion(device_id, sample_timestamp)
    }

    pub fn get_hand_skeleton(
        &self,
        hand_type: HandType,
        timestamp: Duration,
    ) -> Option<[Pose; 26]> {
        dbg_server_core!("get_hand_skeleton: hand={hand_type:?} ts={timestamp:?}");

        self.connection_context
            .tracking_manager
            .read()
            .get_hand_skeleton(hand_type, timestamp)
            .copied()
    }

    /// Return head-local gaze for an exact retained tracking timestamp, with -Z along the gaze.
    /// Returns None if the sample has no gaze or is no longer in the bounded history.
    pub fn get_combined_eye_gaze(&self, sample_timestamp: Duration) -> Option<Quat> {
        dbg_server_core!("get_combined_eye_gaze: sample_ts={sample_timestamp:?}");

        self.connection_context
            .tracking_manager
            .read()
            .get_combined_eye_gaze(sample_timestamp)
    }

    pub fn get_motion_to_photon_latency(&self) -> Duration {
        dbg_server_core!("get_motion_to_photon_latency");

        let latency = self
            .connection_context
            .statistics_manager
            .read()
            .as_ref()
            .map(|stats| stats.motion_to_photon_latency_average())
            .unwrap_or_default();

        let max_prediction =
            Duration::from_millis(SESSION_MANAGER.read().settings().headset.max_prediction_ms);

        if latency > max_prediction {
            warn!("Latency is too high. Clamping prediction");

            max_prediction
        } else {
            latency
        }
    }

    pub fn get_tracker_pose_time_offset(&self) -> Duration {
        dbg_server_core!("get_tracker_pose_time_offset");

        self.connection_context
            .statistics_manager
            .read()
            .as_ref()
            .map(|stats| stats.tracker_pose_time_offset())
            .unwrap_or_default()
    }

    pub fn send_haptics(&self, haptics: Haptics) {
        dbg_server_core!("send_haptics");

        let haptics_config = {
            let session_manager_lock = SESSION_MANAGER.read();

            if session_manager_lock.settings().extra.logging.log_haptics {
                alvr_events::send_event(EventType::Haptics(HapticsEvent {
                    path: DEVICE_ID_TO_PATH.get(&haptics.device_id).map_or_else(
                        || format!("Unknown (ID: {:#16x})", haptics.device_id),
                        |p| (*p).to_owned(),
                    ),
                    duration: haptics.duration,
                    frequency: haptics.frequency,
                    amplitude: haptics.amplitude,
                }))
            }

            session_manager_lock
                .settings()
                .headset
                .controllers
                .as_option()
                .and_then(|c| c.haptics.as_option().cloned())
        };

        if let (Some(config), Some(sender)) = (
            haptics_config,
            &mut *self.connection_context.haptics_sender.lock(),
        ) {
            sender
                .send_header(&haptics::map_haptics(&config, haptics))
                .ok();
        }
    }

    pub fn set_video_config_nals(&self, config_buffer: Vec<u8>, codec: CodecType) {
        dbg_server_core!("set_video_config_nals");

        if let Some(sender) = &*self.connection_context.video_mirror_sender.lock() {
            sender.send(config_buffer.clone()).ok();
        }

        if let Some(file) = &mut *self.connection_context.video_recording_file.lock() {
            file.write_all(&config_buffer).ok();
        }

        *self.connection_context.decoder_config.lock() = Some(DecoderInitializationConfig {
            codec,
            config_buffer,
            ext_str: String::new(),
        });
    }

    /// `reference_frame` is the frame index the encoder was told it may reference for this picture,
    /// or `0` when it did not say. It travels to the client, where it is the difference between one
    /// lost frame and a hold that lasts until the next keyframe: a frame chained to a confirmed
    /// reference decodes across a hole.
    pub fn send_video_nal(
        &self,
        frame_index: u64,
        timestamp: Duration,
        global_view_params: [ViewParams; 2],
        foveation_center_shifts: Option<[[f32; 2]; 2]>,
        is_idr: bool,
        reference_frame: u64,
        nal_buffer: Vec<u8>,
    ) {
        dbg_server_core!("send_video_nal");

        // ADR-0011's send half, at module scope so the session summary can read its counters.
        // It used to be a bare `AtomicBool` here, plus a bypass when `avoid_video_glitching`
        // was off — and it defaults off — which is how a frame that broke the decoder's
        // reference chain got transmitted anyway.
        static LAST_IDR_INSTANT: LazyLock<Mutex<Instant>> =
            LazyLock::new(|| Mutex::new(Instant::now()));

        if let Some(sender) = &*self.connection_context.video_channel_sender.lock() {
            let buffer_size = nal_buffer.len();

            if let Switch::Enabled(config) = &SESSION_MANAGER
                .read()
                .settings()
                .extra
                .capture
                .rolling_video_files
                && Instant::now()
                    > *LAST_IDR_INSTANT.lock() + Duration::from_secs(config.duration_s)
            {
                self.connection_context
                    .events_sender
                    .send(ServerCoreEvent::RequestIDR)
                    .ok();

                if is_idr {
                    create_recording_file(
                        &self.connection_context,
                        SESSION_MANAGER.read().settings(),
                    );
                    *LAST_IDR_INSTANT.lock() = Instant::now();
                }
            }

            // ADR-0011, phase one, with the information the gate needed and never had: what this
            // frame was encoded against. The encoder references only frames the client has
            // acknowledged, so a frame that *states* its reference is decodable however the frames
            // around it fared, and the stream no longer has to stop for a discard. See `SendGate`.
            //
            // And the invariant is checked here rather than assumed: the encoder may only name a
            // frame the client confirmed, and a mismatch between what the C++ side thinks it was told
            // and what this side knows would silently reintroduce the poisoning this whole path
            // exists to remove. One warning, then silence — it is an invariant, not a rate.
            if reference_frame != 0 && !is_idr {
                static CHECKED: LazyLock<Mutex<bool>> = LazyLock::new(|| Mutex::new(false));
                let mut checked = CHECKED.lock();
                let ack = self.connection_context.bitrate_manager.lock().client_ack();
                // Only a claim made *while the client is talking to us* can be wrong. With no
                // acknowledgements at all the encoder is in its fallback branch — it reports the
                // frame it actually built on, which is unconfirmed by definition, and the client
                // holds on it. That is the designed behaviour, not a disagreement.
                if !*checked && ack.valid && !ack.contains(reference_frame) {
                    *checked = true;
                    warn!(
                        "frame {frame_index} says it was encoded against {reference_frame}, which the \
                         client has never confirmed decoded. The encoder's reference decision and this \
                         side's acknowledgements disagree — a lost frame will poison the chain again"
                    );
                }
            }

            let mut gate = SEND_GATE.lock();
            match gate.may_transmit(is_idr, reference_frame) {
                x_transport::SendDecision::Transmit => {
                    // The mirror and the rolling-file recording are records of what the
                    // client received, so they live inside the gate, as they did before.
                    if let Some(sender) = &*self.connection_context.video_mirror_sender.lock() {
                        sender.send(nal_buffer.clone()).ok();
                    }

                    if let Some(file) = &mut *self.connection_context.video_recording_file.lock() {
                        file.write_all(&nal_buffer).ok();
                    }

                    let admitted = sender
                        .try_send(VideoPacket {
                            header: VideoPacketHeader {
                                frame_index,
                                timestamp,
                                global_view_params,
                                foveation_center_shifts,
                                is_idr,
                                reference_frame,
                            },
                            payload: nal_buffer,
                        })
                        .is_ok();

                    gate.on_send_result(admitted);
                    if admitted {
                        send_probe::enqueued();
                    } else {
                        self.connection_context
                            .events_sender
                            .send(ServerCoreEvent::RequestIDR)
                            .ok();
                        // Name the frame. Without the sequence number a dropped frame was
                        // anonymous, so the server could not tell whether the keyframe it had
                        // just asked the encoder for was the frame it had just thrown away —
                        // and would loop: drop, request a keyframe, drop the keyframe, request
                        // again.
                        warn!(
                            "Dropped video frame_index={frame_index} idr={is_idr} (reason: can't \
                             push to network). Total dropped: {}. Suppressing until a keyframe \
                             goes out",
                            gate.discarded()
                        );
                    }
                }
                x_transport::SendDecision::Suppress(reason) => {
                    // Deliberately not silent: the count of these is the cost of the
                    // invariant, and it is the number that was missing when 41 % of frames
                    // were garbage behind a "0 errors" telemetry line.
                    debug!(
                        "Suppressing video frame_index={frame_index} idr={is_idr} ({reason:?}); \
                         {} suppressed so far this session",
                        gate.suppressed_frames()
                    );
                }
            }
            drop(gate);

            // The encoder did this work whether or not the frame could be transmitted, so the
            // statistics and the bitrate controller see it either way: the cost is real even
            // when the output is suppressed, and hiding it would make the controller believe
            // the link is better than it is.
            if let Some(stats) = &mut *self.connection_context.statistics_manager.write() {
                let encoder_latency = stats.report_frame_encoded(timestamp, buffer_size);

                self.connection_context
                    .bitrate_manager
                    .lock()
                    .report_frame_encoded(timestamp, encoder_latency, buffer_size);
            }
        } else {
            // No video channel at all: the socket has not been set up yet, which is a
            // connection-stage condition rather than a frame decision.
            debug!("No video channel; dropping frame_index={frame_index} idr={is_idr}");
        }
    }
    pub fn get_dynamic_encoder_params(&self) -> Option<DynamicEncoderParams> {
        dbg_server_core!("get_dynamic_encoder_params");

        let pair = {
            let session_manager_lock = SESSION_MANAGER.read();
            self.connection_context
                .bitrate_manager
                .lock()
                .get_encoder_params(&session_manager_lock.settings().video.bitrate)
        };

        pair.map(|(params, stats)| {
            if let Some(stats_manager) = &mut *self.connection_context.statistics_manager.write() {
                stats_manager.report_throughput_stats(stats);
            }
            params
        })
    }

    /// What the encoder may reference: which frames the client has confirmed it decoded.
    ///
    /// Deliberately a separate call from [`Self::get_dynamic_encoder_params`], and deliberately not
    /// gated on anything. Encoder parameters are a *change* — `None` means "nothing to do" — whereas
    /// this is a fact about the link that the encoder needs on **every** frame, whether or not any
    /// parameter changed. Folding it into the parameter struct would have tied it to `updated`, which
    /// is set once a second at best: the encoder would then reference frames the client lost for as
    /// long as the bitrate stayed still.
    /// Whether the stream is in its bootstrap. See `BitrateManager::is_bootstrap`.
    pub fn client_bootstrapping(&self) -> bool {
        self.connection_context.bitrate_manager.lock().is_bootstrap()
    }

    /// Whether the degradation ladder is engaged. See `BitrateManager::is_degrading`.
    pub fn client_degrading(&self) -> bool {
        self.connection_context.bitrate_manager.lock().is_degrading()
    }

    pub fn client_ack_state(&self) -> (bool, u64, u64) {
        let ack = self.connection_context.bitrate_manager.lock().client_ack();
        (ack.valid, ack.newest, ack.recent_mask)
    }

    pub fn report_composed(&self, target_timestamp: Duration, offset: Duration) {
        dbg_server_core!("report_composed");

        if let Some(stats) = &mut *self.connection_context.statistics_manager.write() {
            stats.report_frame_composed(target_timestamp, offset);
        }
    }

    pub fn report_present(&self, target_timestamp: Duration, offset: Duration) {
        dbg_server_core!("report_present");

        if let Some(stats) = &mut *self.connection_context.statistics_manager.write() {
            stats.report_frame_present(target_timestamp, offset);
        }

        let session_manager_lock = SESSION_MANAGER.read();
        self.connection_context
            .bitrate_manager
            .lock()
            .report_frame_present(
                &session_manager_lock
                    .settings()
                    .video
                    .bitrate
                    .adapt_to_framerate,
            );
    }

    pub fn duration_until_next_vsync(&self) -> Option<Duration> {
        dbg_server_core!("duration_until_next_vsync");

        self.connection_context
            .statistics_manager
            .write()
            .as_mut()
            .map(|stats| stats.duration_until_next_vsync())
    }
}

impl Drop for ServerCoreContext {
    fn drop(&mut self) {
        dbg_server_core!("Drop");

        // Invoke connection runtimes shutdown
        *self.lifecycle_state.write() = LifecycleState::ShuttingDown;

        dbg_server_core!("Setting clients as Disconnecting");
        {
            let mut session_manager_lock = SESSION_MANAGER.write();

            let hostnames = session_manager_lock
                .client_list()
                .iter()
                .filter(|&(_, info)| {
                    !matches!(
                        info.connection_state,
                        ConnectionState::Disconnected | ConnectionState::Disconnecting
                    )
                })
                .map(|(hostname, _)| hostname.clone())
                .collect::<Vec<_>>();

            for hostname in hostnames {
                session_manager_lock.update_client_connections(
                    hostname,
                    ClientConnectionsAction::SetConnectionState(ConnectionState::Disconnecting),
                );
            }
        }

        dbg_server_core!("Joining connection thread");
        if let Some(thread) = self.connection_thread.write().take() {
            thread.join().ok();
        }

        // apply openvr config for the next launch
        dbg_server_core!("Setting restart settings cache");
        {
            let mut session_manager_lock = SESSION_MANAGER.write();
            let new_steamvr_hmd_init_config = session_manager_lock
                .session()
                .steamvr_hmd_init_config
                .clone();
            let settings = session_manager_lock.session().to_settings();
            let new_hash =
                connection::compute_restart_settings_hash(&new_steamvr_hmd_init_config, &settings);
            let mut session = session_manager_lock.session_mut();
            session.steamvr_hmd_init_config = new_steamvr_hmd_init_config;
            session.restart_settings_hash = new_hash;
        }

        // todo: check if this is still needed
        while SESSION_MANAGER
            .read()
            .client_list()
            .iter()
            .any(|(_, info)| info.connection_state != ConnectionState::Disconnected)
        {
            thread::sleep(Duration::from_millis(100));
        }

        self.webserver_runtime.take();
    }
}
