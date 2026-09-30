//! x-protocol — GemLink's versioned wire-protocol types and capability negotiation.
//!
//! Phase 0 deliverable (ADR-0003, ADR-0004): a standalone crate with
//! **zero dependencies on upstream `alvr_*` crates**, so it can become the protocol
//! layer without rebase friction. Serde-only; transport framing stays
//! in `sockets`/transport-v2. All roadmap negotiation policy lives here and is
//! bench-tested (`x-bench`), per the "measured, not vibes" charter value.
//!
//! Device tiers (ADR-0004): Steam Frame gates releases; Quest Pro / Quest 3 are
//! best-effort; everything else rides the policy tables (`docs/PRESETS.md`).

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Wire protocol version of this build.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0, 0);

/// Semantic wire version. Peers are wire-compatible when **major and minor match**;
/// patch drifts freely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl ProtocolVersion {
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    pub const fn is_wire_compatible(self, other: Self) -> bool {
        self.major == other.major && self.minor == other.minor
    }
}

/// Client operating environment. `steamos_vr_aarch64` is the Steam Frame
/// (ADR-0003); the rest keep Tier-2/3 devices first-class in the protocol.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClientOs {
    SteamosVrAarch64,
    Android,
    LinuxPc,
    WindowsPc,
    Unknown(String),
}

/// Video codecs, declared in **ascending preference order** (`Ord`): when both
/// sides support several, negotiation picks the greatest common element.
/// Codec preference policy: AV1 (10-bit) > AV1 > HEVC 10-bit > HEVC > H.264.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    Hevc,
    Hevc10Bit,
    Av1,
    Av110Bit,
}

/// Foveation capability of the *client* device.
/// `EyeGaze`: head-mounted eye trackers (Steam Frame, Quest Pro).
/// `Fixed`: no gaze source; center-biased foveation still valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FoveationHw {
    EyeGaze,
    Fixed,
    None,
}

/// Physical/logical link the session may run over, in **ascending latency
/// preference** via [`LinkClass::latency_preference`] (wired first —
/// definition-of-done #5: NCM wired is the lowest-latency profile).
/// `Wifi7SoftAp`: the headset's own 6 GHz AP (Frame topology, ADR-0003).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum LinkClass {
    UsbNcm,
    Wifi7SoftAp,
    Wifi7Lan,
    Wifi6Lan,
    Other(String),
}

impl LinkClass {
    pub fn latency_preference(&self) -> u8 {
        match self {
            LinkClass::UsbNcm => 0,
            LinkClass::Wifi7SoftAp => 1,
            LinkClass::Wifi7Lan => 2,
            LinkClass::Wifi6Lan => 3,
            LinkClass::Other(_) => 4,
        }
    }
}

/// Display characteristics as advertised by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayCaps {
    pub per_eye_width: u32,
    pub per_eye_height: u32,
    /// Supported refresh rates, ascending (Frame: 72/80/90/120, 144 experimental).
    pub refresh_rates: Vec<u16>,
}

/// Everything a client advertises at session setup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientCapabilities {
    pub protocol_version: ProtocolVersion,
    pub hostname: String,
    pub client_os: ClientOs,
    pub codecs: BTreeSet<VideoCodec>,
    pub foveation_hw: FoveationHw,
    /// Hard client ceiling (decode/compositor budget), fps.
    pub max_fps: u16,
    pub display: DisplayCaps,
    pub link_classes: BTreeSet<LinkClass>,
    /// The device's bandwidth envelope for this session class (GemLink primary
    /// scenario: 300 Mbps *with foveated encoding*, ADR-0003).
    pub bitrate_envelope_mbps: u32,
    /// True iff this build ships the insecure-debug transport. Release
    /// builds hard-code `false`.
    pub insecure_debug: bool,
}

/// What the server can do, at session setup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub protocol_version: ProtocolVersion,
    pub codecs: BTreeSet<VideoCodec>,
    pub foveation_supported: bool,
    pub insecure_debug: bool,
    pub max_fps: u16,
    pub max_bitrate_mbps: u32,
    pub link_classes: BTreeSet<LinkClass>,
    /// Requested encode resolution, per eye.
    pub per_eye_width: u32,
    pub per_eye_height: u32,
}

/// Foveation mode for the session. **Synthesis-once** applies regardless of
/// mode: the PC never interpolates frames (ADR-0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FoveationMode {
    /// Gaze-driven per-eye centers, predicted one frame ahead (flagship).
    EyeGazeDriven,
    /// Fixed center-biased foveation (fallback ladder).
    Fixed,
    Disabled,
}

/// The negotiated session, computed once at connect and re-negotiated on
/// hot-switch (M4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionPlan {
    pub codec: VideoCodec,
    pub fps: u16,
    pub bitrate_mbps: u32,
    pub foveation: FoveationMode,
    pub link_class: LinkClass,
    pub encryption: EncryptionMode,
    pub per_eye_width: u32,
    pub per_eye_height: u32,
}

/// Transport encryption mode. `NoiseXx` is the default and the only mode
/// release builds ship; `InsecureDebugOnly` exists solely for wire debugging
/// and is only negotiable when BOTH sides are built with the
/// `insecure-debug-transport` feature (compile-time, default OFF; ADR-0006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EncryptionMode {
    NoiseXx,
    InsecureDebugOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NegotiationError {
    VersionMismatch {
        client: ProtocolVersion,
        server: ProtocolVersion,
    },
    NoCommonCodec,
    NoCommonLink,
    NoCommonRefresh,
}

impl std::fmt::Display for NegotiationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NegotiationError::VersionMismatch { client, server } => write!(
                f,
                "protocol version mismatch: client {client:?} vs server {server:?}"
            ),
            NegotiationError::NoCommonCodec => write!(f, "no codec supported by both peers"),
            NegotiationError::NoCommonLink => write!(f, "no link class available to both peers"),
            NegotiationError::NoCommonRefresh => {
                write!(f, "no client refresh rate at or below the fps clamp")
            }
        }
    }
}

impl std::error::Error for NegotiationError {}

/// Negotiate a [`SessionPlan`] from both sides' capability sets.
///
/// Policy summary:
/// - codec: highest common element of the preference-ordered [`VideoCodec`]
/// - link: common class with the best (lowest) latency preference — wired wins
/// - fps: highest client refresh rate at or below `min(client, server) max_fps`
/// - bitrate: `min(client envelope, server max)`
/// - foveation: client hardware ∩ server support, per the fallback ladder
///   (EyeGaze → Fixed → Disabled)
/// - resolution: server request clamped to the client's per-eye display caps
pub fn negotiate(
    client: &ClientCapabilities,
    server: &ServerCapabilities,
) -> Result<SessionPlan, NegotiationError> {
    if !client
        .protocol_version
        .is_wire_compatible(server.protocol_version)
    {
        return Err(NegotiationError::VersionMismatch {
            client: client.protocol_version,
            server: server.protocol_version,
        });
    }

    let codec = client
        .codecs
        .intersection(&server.codecs)
        .copied()
        .max()
        .ok_or(NegotiationError::NoCommonCodec)?;

    let link_class = client
        .link_classes
        .intersection(&server.link_classes)
        .min_by_key(|link| link.latency_preference())
        .ok_or(NegotiationError::NoCommonLink)?
        .clone();

    let fps_clamp = client.max_fps.min(server.max_fps);
    let fps = *client
        .display
        .refresh_rates
        .iter()
        .filter(|rate| **rate <= fps_clamp)
        .max()
        .ok_or(NegotiationError::NoCommonRefresh)?;

    let bitrate_mbps = client.bitrate_envelope_mbps.min(server.max_bitrate_mbps);

    let foveation = match (client.foveation_hw, server.foveation_supported) {
        (FoveationHw::EyeGaze, true) => FoveationMode::EyeGazeDriven,
        (FoveationHw::Fixed, true) => FoveationMode::Fixed,
        _ => FoveationMode::Disabled,
    };

    Ok(SessionPlan {
        codec,
        fps,
        bitrate_mbps,
        foveation,
        link_class,
        encryption: if client.insecure_debug && server.insecure_debug {
            EncryptionMode::InsecureDebugOnly
        } else {
            EncryptionMode::NoiseXx
        },
        per_eye_width: client.display.per_eye_width.min(server.per_eye_width),
        per_eye_height: client.display.per_eye_height.min(server.per_eye_height),
    })
}

/// Sample capability sets for the Tier-1/Tier-2 devices (ADR-0004). These are
/// the seeds the fake-headset (`x-bench`) and the dashboard's preset UI will
/// reuse; values trace to `docs/PRESETS.md` provenance.
pub mod samples {
    use super::*;

    /// Steam Frame (deckard): SM8650 iris decode, eye tracking, Wi-Fi 7 SoftAp
    /// + USB-C NCM. AV1 10-bit deliberately **not** advertised yet — iris caps
    ///   unverified — pending device capability validation.
    pub fn steam_frame() -> ClientCapabilities {
        ClientCapabilities {
            protocol_version: PROTOCOL_VERSION,
            hostname: "steam-frame".into(),
            client_os: ClientOs::SteamosVrAarch64,
            codecs: BTreeSet::from([
                VideoCodec::H264,
                VideoCodec::Hevc,
                VideoCodec::Hevc10Bit,
                VideoCodec::Av1,
            ]),
            foveation_hw: FoveationHw::EyeGaze,
            max_fps: 144,
            display: DisplayCaps {
                per_eye_width: 2160,
                per_eye_height: 2160,
                refresh_rates: vec![72, 80, 90, 120, 144],
            },
            link_classes: BTreeSet::from([LinkClass::Wifi7SoftAp, LinkClass::UsbNcm]),
            insecure_debug: false,
            bitrate_envelope_mbps: 300,
        }
    }

    /// Quest Pro: HEVC 10-bit tier, eye tracking, home-LAN Wi-Fi 6.
    pub fn quest_pro() -> ClientCapabilities {
        ClientCapabilities {
            protocol_version: PROTOCOL_VERSION,
            hostname: "quest-pro".into(),
            client_os: ClientOs::Android,
            codecs: BTreeSet::from([VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Hevc10Bit]),
            foveation_hw: FoveationHw::EyeGaze,
            max_fps: 90,
            display: DisplayCaps {
                per_eye_width: 1832,
                per_eye_height: 1920,
                refresh_rates: vec![72, 90],
            },
            link_classes: BTreeSet::from([LinkClass::Wifi6Lan]),
            insecure_debug: false,
            bitrate_envelope_mbps: 200,
        }
    }

    /// Quest 3: AV1 tier, no eye tracking (fixed foveation), 120 Hz.
    pub fn quest_3() -> ClientCapabilities {
        ClientCapabilities {
            protocol_version: PROTOCOL_VERSION,
            hostname: "quest-3".into(),
            client_os: ClientOs::Android,
            codecs: BTreeSet::from([
                VideoCodec::H264,
                VideoCodec::Hevc,
                VideoCodec::Hevc10Bit,
                VideoCodec::Av1,
            ]),
            foveation_hw: FoveationHw::Fixed,
            max_fps: 120,
            display: DisplayCaps {
                per_eye_width: 2064,
                per_eye_height: 2208,
                refresh_rates: vec![72, 80, 90, 120],
            },
            link_classes: BTreeSet::from([LinkClass::Wifi6Lan]),
            insecure_debug: false,
            bitrate_envelope_mbps: 200,
        }
    }

    /// A reasonably capable GemLink server (NVENC-class).
    pub fn server() -> ServerCapabilities {
        ServerCapabilities {
            insecure_debug: false,
            protocol_version: PROTOCOL_VERSION,
            codecs: BTreeSet::from([
                VideoCodec::H264,
                VideoCodec::Hevc,
                VideoCodec::Hevc10Bit,
                VideoCodec::Av1,
            ]),
            foveation_supported: true,
            max_fps: 120,
            max_bitrate_mbps: 800,
            link_classes: BTreeSet::from([
                LinkClass::UsbNcm,
                LinkClass::Wifi7SoftAp,
                LinkClass::Wifi7Lan,
                LinkClass::Wifi6Lan,
            ]),
            per_eye_width: 2880,
            per_eye_height: 2880,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use samples::*;

    #[test]
    fn steam_frame_negotiates_av1_gaze_300mbps_wired_first() {
        let plan = negotiate(&steam_frame(), &server()).expect("steam frame negotiates");
        assert_eq!(plan.codec, VideoCodec::Av1);
        assert_eq!(plan.foveation, FoveationMode::EyeGazeDriven);
        // wired beats Wi-Fi when both exist (definition-of-done #5)
        assert_eq!(plan.link_class, LinkClass::UsbNcm);
        // fps clamp 120 (server) → highest client refresh at or below
        assert_eq!(plan.fps, 120);
        // envelope wins over server max
        assert_eq!(plan.bitrate_mbps, 300);
        // server request 2880² clamped to client panel 2160²
        assert_eq!((plan.per_eye_width, plan.per_eye_height), (2160, 2160));
    }

    #[test]
    fn wireless_only_frame_uses_softap() {
        let mut client = steam_frame();
        client.link_classes = BTreeSet::from([LinkClass::Wifi7SoftAp]);
        let plan = negotiate(&client, &server()).expect("negotiates");
        assert_eq!(plan.link_class, LinkClass::Wifi7SoftAp);
    }

    #[test]
    fn quest_pro_gets_hevc10_and_gaze() {
        let plan = negotiate(&quest_pro(), &server()).expect("quest pro negotiates");
        assert_eq!(plan.codec, VideoCodec::Hevc10Bit);
        assert_eq!(plan.foveation, FoveationMode::EyeGazeDriven);
        assert_eq!(plan.fps, 90);
        assert_eq!(plan.bitrate_mbps, 200);
    }

    #[test]
    fn quest_3_gets_av1_fixed_foveation() {
        let plan = negotiate(&quest_3(), &server()).expect("quest 3 negotiates");
        assert_eq!(plan.codec, VideoCodec::Av1);
        assert_eq!(plan.foveation, FoveationMode::Fixed);
        assert_eq!(plan.fps, 120);
    }

    #[test]
    fn no_gaze_source_disables_foveation() {
        let mut client = steam_frame();
        client.foveation_hw = FoveationHw::None;
        let plan = negotiate(&client, &server()).unwrap();
        assert_eq!(plan.foveation, FoveationMode::Disabled);
    }

    #[test]
    fn fixed_hw_without_server_support_disables() {
        let mut client = quest_3();
        let mut srv = server();
        client.link_classes = BTreeSet::from([LinkClass::Wifi6Lan]);
        srv.link_classes = BTreeSet::from([LinkClass::Wifi6Lan]);
        srv.foveation_supported = false;
        let plan = negotiate(&client, &srv).unwrap();
        assert_eq!(plan.foveation, FoveationMode::Disabled);
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let mut client = steam_frame();
        client.protocol_version = ProtocolVersion::new(2, 0, 0);
        assert_eq!(
            negotiate(&client, &server()),
            Err(NegotiationError::VersionMismatch {
                client: ProtocolVersion::new(2, 0, 0),
                server: PROTOCOL_VERSION,
            })
        );
    }

    #[test]
    fn patch_drift_is_wire_compatible() {
        let mut client = steam_frame();
        client.protocol_version = ProtocolVersion::new(1, 0, 7);
        assert!(negotiate(&client, &server()).is_ok());
    }

    #[test]
    fn no_common_codec_is_rejected() {
        let mut client = steam_frame();
        client.codecs = BTreeSet::from([VideoCodec::Av110Bit]);
        assert_eq!(
            negotiate(&client, &server()),
            Err(NegotiationError::NoCommonCodec)
        );
    }

    #[test]
    fn capabilities_roundtrip_through_json() {
        let client = steam_frame();
        let json = serde_json::to_string(&client).expect("serialize");
        let back: ClientCapabilities = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, client);
    }
}
