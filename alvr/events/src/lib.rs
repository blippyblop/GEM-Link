use alvr_common::{DeviceMotion, LogEntry, LogSeverity, Pose, info};
use alvr_packets::{ButtonValue, FaceData};
use alvr_session::SessionConfig;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct StatisticsSummary {
    pub video_packets_total: usize,
    pub video_packets_per_sec: usize,
    pub video_mbytes_total: usize,
    pub video_mbits_per_sec: f32,
    pub total_latency_ms: f32,
    pub network_latency_ms: f32,
    pub encode_latency_ms: f32,
    pub decode_latency_ms: f32,
    pub client_fps: u32,
    pub server_fps: u32,
    pub battery_hmd: u32,
    pub hmd_plugged: bool,
}

// Bitrate statistics minus the empirical output value
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct BitrateDirectives {
    pub scaled_calculated_throughput_bps: Option<f32>,
    pub decoder_latency_limiter_bps: Option<f32>,
    pub network_latency_limiter_bps: Option<f32>,
    pub encoder_latency_limiter_bps: Option<f32>,
    pub manual_max_throughput_bps: Option<f32>,
    pub manual_min_throughput_bps: Option<f32>,
    /// The cap a **client-reported queueing delay** imposed, if any.
    ///
    /// This is the one limiter driven by the receiver's own measurement rather than by the server's
    /// view of the link, and it exists because every other signal is ambiguous from the client's
    /// side: a shard that never arrived and a shard still queued behind it are the same fact
    /// locally. The delay is not ambiguous — it says the sender is outrunning the receiver.
    pub client_queue_limiter_bps: Option<f32>,
    /// How many frames the degradation ladder is dropping for every one it sends, if it is above 1.
    ///
    /// Reported so the ladder is visible in the throughput statistics rather than only in the frame
    /// rate someone notices: the second rung is the one that costs smoothness, and it should be
    /// possible to see it being spent.
    pub degrade_frame_divisor: Option<u32>,
    /// Whether the degradation ladder is engaged: the client is not receiving what is being sent.
    ///
    /// Reported because it is the one fact that changes what the *encoder* should do rather than what
    /// it should be told: while it is set, a keyframe is a burst the client cannot take, so the
    /// recovery is suppressed rather than requested.
    pub degrade_starving: Option<bool>,
    pub requested_bitrate_bps: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct GraphStatistics {
    pub total_pipeline_latency_s: f32,
    pub game_time_s: f32,
    pub server_compositor_s: f32,
    pub encoder_s: f32,
    pub network_s: f32,
    pub decoder_s: f32,
    pub decoder_queue_s: f32,
    pub client_compositor_s: f32,
    pub vsync_queue_s: f32,
    pub client_fps: f32,
    pub server_fps: f32,
    pub bitrate_directives: BitrateDirectives,
    pub throughput_bps: f32,
    pub bitrate_bps: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TrackingEvent {
    pub device_motions: Vec<(String, DeviceMotion)>,
    pub hand_skeletons: [Option<[Pose; 26]>; 2],
    pub face: FaceData,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ButtonEvent {
    pub path: String,
    pub value: ButtonValue,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HapticsEvent {
    pub path: String,
    pub duration: Duration,
    pub frequency: f32,
    pub amplitude: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AdbEvent {
    pub download_progress: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "id", content = "data")]
pub enum EventType {
    Log(LogEntry),
    DebugGroup { group: String, message: String },
    Session(Box<SessionConfig>),
    StatisticsSummary(StatisticsSummary),
    GraphStatistics(GraphStatistics),
    Tracking(Box<TrackingEvent>),
    Buttons(Vec<ButtonEvent>),
    Haptics(HapticsEvent),
    DriversList(Vec<PathBuf>),
    ServerRequestsSelfRestart,
    Adb(AdbEvent),
    NewVersionFound { version: String, message: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Event {
    pub timestamp: String,
    pub event_type: EventType,
}

impl Event {
    pub fn event_type_string(&self) -> String {
        match &self.event_type {
            EventType::Log(entry) => match entry.severity {
                LogSeverity::Error => "ERROR".into(),
                LogSeverity::Warning => "WARNING".into(),
                LogSeverity::Info => "INFO".into(),
                LogSeverity::Debug => "DEBUG".into(),
            },
            EventType::DebugGroup { group, .. } => group.clone(),
            EventType::Session(_) => "SESSION".to_string(),
            EventType::StatisticsSummary(_) => "STATS".to_string(),
            EventType::GraphStatistics(_) => "GRAPH".to_string(),
            EventType::Tracking(_) => "TRACKING".to_string(),
            EventType::Buttons(_) => "BUTTONS".to_string(),
            EventType::Haptics(_) => "HAPTICS".to_string(),
            EventType::DriversList(_) => "DRV LIST".to_string(),
            EventType::ServerRequestsSelfRestart => "RESTART".to_string(),
            EventType::Adb(_) => "ADB".to_string(),
            EventType::NewVersionFound { .. } => "NEW VER".to_string(),
        }
    }

    pub fn message(&self) -> String {
        match &self.event_type {
            EventType::Log(log_entry) => log_entry.content.clone(),
            EventType::DebugGroup { message, .. } => message.clone(),
            EventType::Session(_) => "Updated".into(),
            EventType::StatisticsSummary(_) | EventType::GraphStatistics(_) => "".into(),
            EventType::Tracking(tracking) => serde_json::to_string(tracking).unwrap(),
            EventType::Buttons(buttons) => serde_json::to_string(buttons).unwrap(),
            EventType::Haptics(haptics) => serde_json::to_string(haptics).unwrap(),
            EventType::DriversList(drivers) => serde_json::to_string(drivers).unwrap(),
            EventType::ServerRequestsSelfRestart => "Request for server restart".into(),
            EventType::Adb(adb) => serde_json::to_string(adb).unwrap(),
            EventType::NewVersionFound { version, .. } => version.clone(),
        }
    }
}

pub fn send_event(event_type: EventType) {
    info!("{}", serde_json::to_string(&event_type).unwrap());
}
