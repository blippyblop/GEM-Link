//! The WLAN optimizer: put the PC's Wi-Fi adapter into streaming posture for the
//! duration of a session, and put it back afterwards.
//!
//! This is a port of Virtual Desktop's `libVirtualDesktopNet.dll`
//! (`VD_RE/24-vd-link-qos.md` §2). No ALVR version of
//! this exists; there is no setting, no socket option and no driver flag that does it.
//!
//! ## The mechanism
//!
//! For every **connected** WLAN interface, two `WLAN_INTF_OPCODE`s are driven through a
//! query → compare → set → **re-query → verify** cycle with a 4-byte boolean payload:
//!
//! | Opcode | Value | Streaming | Restored |
//! |---|---|---|---|
//! | [`OPCODE_BACKGROUND_SCAN_ENABLED`] | `wlan_intf_opcode_background_scan_enabled` = 2 | **off** | on |
//! | [`OPCODE_MEDIA_STREAMING_MODE`] | `wlan_intf_opcode_media_streaming_mode` = 3 | **on** | off |
//!
//! Background scanning is the important half: a scanning station periodically tunes
//! *off-channel* to listen for other APs, and every scan is a latency spike on the very
//! AP the stream is running over. Media streaming mode is the miniport's documented hook
//! for "service this queue as real-time media".
//!
//! ## Two things copied deliberately from VD
//!
//! 1. **Re-assert every [`REASSERT_PERIOD`] (11.0 s).** The value is transient — it is
//!    not written to a profile — so anything that resets the adapter (a driver reload, a
//!    reconnection, Windows re-applying a stored profile) can silently drop it.
//!    Re-asserting is free because the cycle above is idempotent: when the value is
//!    already correct the set is skipped entirely.
//! 2. **Stop on a hard failure.** VD's thread exits when `OptimizeWLAN` returns anything
//!    above 1 (access denied, set failed, verify failed), and keeps going on 1 ("no
//!    connected WLAN interface"). Retrying a refused API every 11 s forever would fill
//!    the log with the same line; stopping leaves one honest line instead.

use std::{
    fmt,
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::Duration,
};

/// How often the posture is re-asserted. VD's literal: `0x28fa6ae00` ns = 11.0 s.
///
/// The period is exact in VD's binary; its *intent* is not recoverable from there. It is
/// long enough not to be noise in a log and short enough that a dropped posture is
/// corrected within a fraction of a session.
pub const REASSERT_PERIOD: Duration = Duration::from_secs(11);

/// `wlan_intf_opcode_background_scan_enabled`.
pub const OPCODE_BACKGROUND_SCAN_ENABLED: u32 = 2;
/// `wlan_intf_opcode_media_streaming_mode`.
pub const OPCODE_MEDIA_STREAMING_MODE: u32 = 3;

/// What the optimizer should hold the adapter at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WlanPosture {
    /// `wlan_intf_opcode_media_streaming_mode` — true while streaming.
    pub media_streaming: bool,
    /// `wlan_intf_opcode_background_scan_enabled` — true is the OS default,
    /// false is what we want while streaming.
    pub background_scan: bool,
}

impl WlanPosture {
    /// The streaming posture: media streaming on, background scanning off.
    /// VD's `OptimizeWLAN(1)`.
    pub const fn streaming() -> Self {
        Self {
            media_streaming: true,
            background_scan: false,
        }
    }

    /// The resting posture: media streaming off, background scanning on.
    /// VD's `OptimizeWLAN(0)` — which VD never calls.
    pub const fn off() -> Self {
        Self {
            media_streaming: false,
            background_scan: true,
        }
    }

    /// The desired value for one of our two opcodes, or `None` if the opcode is not ours
    /// to drive. The single place the opcode→value mapping lives, so it can be tested
    /// without a radio.
    pub const fn desired_value(&self, opcode: u32) -> Option<bool> {
        match opcode {
            OPCODE_MEDIA_STREAMING_MODE => Some(self.media_streaming),
            OPCODE_BACKGROUND_SCAN_ENABLED => Some(self.background_scan),
            _ => None,
        }
    }

    /// True when this posture asks for nothing (i.e. it is the resting state). Used to
    /// skip a pointless restore on a session that never got to apply anything.
    pub const fn is_off(&self) -> bool {
        !self.media_streaming && self.background_scan
    }

    /// The opcodes this posture drives, in application order. Media streaming first, as
    /// VD does — the first failure wins in VD's status folding, and we keep the order
    /// observable rather than reordering it for aesthetics.
    pub const fn opcodes(&self) -> [u32; 2] {
        [OPCODE_MEDIA_STREAMING_MODE, OPCODE_BACKGROUND_SCAN_ENABLED]
    }
}

/// The result of driving one opcode through one cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpcodeOutcome {
    /// Already at the desired value — no write was issued.
    Unchanged,
    /// Written and read back as the desired value.
    Applied,
    /// The API refused with `ERROR_ACCESS_DENIED` (5).
    AccessDenied,
    /// The write failed with this Win32 error.
    SetFailed(u32),
    /// The query failed with this Win32 error.
    QueryFailed(u32),
    /// The API returned a size, value or value-type that cannot be trusted (VD's poisoned
    /// out-parameter guard: `wlan_opcode_value_type_invalid` left untouched).
    InvalidValueType,
    /// Written, but the read-back did not agree.
    VerifyFailed,
}

impl OpcodeOutcome {
    /// Anything other than "we know the value is what we wanted".
    pub const fn is_error(self) -> bool {
        !matches!(self, OpcodeOutcome::Unchanged | OpcodeOutcome::Applied)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            OpcodeOutcome::Unchanged => "already correct",
            OpcodeOutcome::Applied => "applied + verified",
            OpcodeOutcome::AccessDenied => "access denied",
            OpcodeOutcome::SetFailed(_) => "set failed",
            OpcodeOutcome::QueryFailed(_) => "query failed",
            OpcodeOutcome::InvalidValueType => "invalid value type",
            OpcodeOutcome::VerifyFailed => "verify failed",
        }
    }
}

impl fmt::Display for OpcodeOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpcodeOutcome::SetFailed(code) => write!(f, "set failed (win32 {code})"),
            OpcodeOutcome::QueryFailed(code) => write!(f, "query failed (win32 {code})"),
            other => f.write_str(other.as_str()),
        }
    }
}

/// What happened to one interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceOutcome {
    /// The interface description as Windows reports it.
    pub description: String,
    /// The interface GUID, formatted — the only stable identity Windows gives us.
    pub guid: String,
    pub media_streaming: OpcodeOutcome,
    pub background_scan: OpcodeOutcome,
}

impl InterfaceOutcome {
    pub const fn is_error(&self) -> bool {
        self.media_streaming.is_error() || self.background_scan.is_error()
    }
}

/// The result of one pass of the optimizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptimizeReport {
    /// The posture was walked over every connected WLAN interface.
    Applied { interfaces: Vec<InterfaceOutcome> },
    /// There is no connected WLAN interface. Correct and expected on a wired-only
    /// machine, and **not** a failure: VD's `OptimizeWLAN` returns 1 here and its thread
    /// keeps running, because a Wi-Fi adapter can appear mid-session.
    NoInterface,
    /// Nobody to ask: the platform has no WLAN API (we are not on Windows).
    Unsupported,
    /// The API refused us.
    AccessDenied,
    /// Opening or enumerating failed.
    Failed(String),
}

impl OptimizeReport {
    /// Whether the periodic thread should stop.
    ///
    /// Mirrors VD's `status > 1` rule: a refusal or a failure stops the loop; "no
    /// interface" and "applied" do not.
    pub fn is_failure(&self) -> bool {
        match self {
            OptimizeReport::Applied { interfaces } => {
                interfaces.iter().any(InterfaceOutcome::is_error)
            }
            OptimizeReport::NoInterface => false,
            OptimizeReport::Unsupported
            | OptimizeReport::AccessDenied
            | OptimizeReport::Failed(_) => true,
        }
    }

    /// One line for the log. Says what was walked and, if anything went wrong, which
    /// interface and which opcode — the discriminating evidence if this is ever suspected
    /// of doing nothing.
    pub fn summary(&self) -> String {
        match self {
            OptimizeReport::Applied { interfaces } if interfaces.is_empty() => {
                "no connected WLAN interface".into()
            }
            OptimizeReport::Applied { interfaces } => {
                let mut s = format!("{} connected WLAN interface(s):", interfaces.len());
                for iface in interfaces {
                    s.push_str(&format!(
                        " [{} media={} bgscan={}]",
                        iface.description, iface.media_streaming, iface.background_scan
                    ));
                }
                s
            }
            OptimizeReport::NoInterface => "no connected WLAN interface".into(),
            OptimizeReport::Unsupported => "platform has no WLAN API".into(),
            OptimizeReport::AccessDenied => "access denied by the WLAN API".into(),
            OptimizeReport::Failed(e) => format!("failed: {e}"),
        }
    }
}

/// Why the optimizer could not be started at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// This platform has no WLAN control API. Not an error worth reporting to a user on
    /// a wired-only machine, but a real "we are doing nothing" signal.
    Unsupported,
    /// `WlanOpenHandle` failed with this Win32 error.
    Open(u32),
    /// A WLAN query failed with this Win32 error.
    Query(u32),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::Unsupported => f.write_str("WLAN control is not available on this platform"),
            LinkError::Open(code) => write!(f, "WlanOpenHandle failed (win32 {code})"),
            LinkError::Query(code) => write!(f, "WLAN query failed (win32 {code})"),
        }
    }
}

impl std::error::Error for LinkError {}

/// A thing that can apply a [`WlanPosture`]. Exists so the session's orchestration —
/// the period, the stop-on-failure rule, the restore — is testable without a radio.
pub trait WlanBackend: Send + Sync + 'static {
    /// For logs.
    fn name(&self) -> &'static str;
    /// Apply the posture to every interface worth touching. Must be idempotent.
    fn apply(&self, posture: WlanPosture) -> OptimizeReport;
}

struct SessionInner {
    backend: Arc<dyn WlanBackend>,
    posture: WlanPosture,
    period: Duration,
    /// `true` once a stop has been requested. Guarded by the condvar's mutex.
    stop: Mutex<bool>,
    cv: Condvar,
    reports: Mutex<Vec<OptimizeReport>>,
}

impl SessionInner {
    /// Wait up to `period`, returning true if a stop was requested.
    fn wait(&self, period: Duration) -> bool {
        let stop = self.stop.lock().unwrap_or_else(|e| e.into_inner());
        if *stop {
            return true;
        }
        let (stop, _timeout) = self
            .cv
            .wait_timeout(stop, period)
            .unwrap_or_else(|e| e.into_inner());
        *stop
    }

    fn request_stop(&self) {
        let mut stop = self.stop.lock().unwrap_or_else(|e| e.into_inner());
        *stop = true;
        self.cv.notify_all();
    }

    fn record(&self, report: OptimizeReport) {
        self.reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(report);
    }
}

/// Holds the adapter in [`WlanPosture::streaming()`] for as long as it lives, and puts it
/// back when it is dropped.
///
/// The restore is the one deliberate improvement on Virtual Desktop: VD never calls its
/// own off-path, so it leaves media-streaming-mode on and background-scan off after the
/// session ends (`VD_RE/24-vd-link-qos.md` §2.3). The
/// value is transient so this is untidy rather than harmful, but there is no reason to
/// leave a user's adapter in a streaming posture when nothing is streaming.
///
/// Dropping the session stops the thread and restores. There is no way to construct one
/// that does not self-thread; that is deliberate.
pub struct WlanSession {
    inner: Arc<SessionInner>,
    thread: Option<JoinHandle<()>>,
}

impl WlanSession {
    /// Start the optimizer with the posture this link class calls for.
    ///
    /// Returns [`LinkError::Unsupported`] on a platform with no WLAN API, which callers
    /// should treat as "nothing to do here", not as a session failure.
    pub fn start(posture: WlanPosture) -> Result<Self, LinkError> {
        let backend = platform::open_backend()?;
        Ok(Self::start_with(backend, posture, REASSERT_PERIOD))
    }

    /// Start the optimizer with an explicit backend and period. Used by the tests, and
    /// by anything that needs to drive a different platform implementation.
    pub fn start_with(
        backend: Arc<dyn WlanBackend>,
        posture: WlanPosture,
        period: Duration,
    ) -> Self {
        let name = backend.name();
        let inner = Arc::new(SessionInner {
            backend,
            posture,
            period,
            stop: Mutex::new(false),
            cv: Condvar::new(),
            reports: Mutex::new(Vec::new()),
        });

        // First application happens on the thread so that `start` never blocks the
        // caller on a Win32 call — the session is live immediately.
        let thread = {
            let inner = Arc::clone(&inner);
            std::thread::Builder::new()
                .name("gemlink-wlan-optimizer".into())
                .spawn(move || run(Arc::clone(&inner)))
                .map_err(|e| log::warn!("x-link: could not spawn the WLAN optimizer thread: {e}"))
                .ok()
        };

        log::info!(
            "x-link: WLAN optimizer started on {name} (media_streaming={}, background_scan={}, \
             re-assert every {:?})",
            posture.media_streaming,
            posture.background_scan,
            period
        );

        Self { inner, thread }
    }

    /// Every report so far, oldest first. The first entry is the state at session start:
    /// if that entry is not `Applied`/`NoInterface`, nothing was ever changed.
    pub fn reports(&self) -> Vec<OptimizeReport> {
        self.inner
            .reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Stop the thread and restore the adapter. Equivalent to dropping; explicit for
    /// callers that want to observe the restoration point in a log.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for WlanSession {
    fn drop(&mut self) {
        self.inner.request_stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }

        let resting = WlanPosture::off();
        if self.inner.posture != resting {
            let report = self.inner.backend.apply(resting);
            log::info!(
                "x-link: WLAN optimizer restored the adapter ({}) — {}",
                self.inner.backend.name(),
                report.summary()
            );
        }
    }
}

fn run(inner: Arc<SessionInner>) {
    loop {
        let report = inner.backend.apply(inner.posture);
        let failed = report.is_failure();
        log::info!(
            "x-link: WLAN posture on {} — {}",
            inner.backend.name(),
            report.summary()
        );
        inner.record(report);

        if failed {
            // VD's rule: a refusal stops the loop rather than repeating forever.
            log::warn!(
                "x-link: WLAN optimizer stopping — the adapter refused the streaming posture"
            );
            break;
        }

        if inner.wait(inner.period) {
            break;
        }
    }
}

/// The 802.11 PHY a connection is using, as `wlan_intf_opcode_current_connection` reports
/// it. Only the members that map to a Wi-Fi generation are named; anything else is
/// [`WlanPhy::Other`] and maps to no generation rather than to a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WlanPhy {
    /// 802.11n — Wi-Fi 4.
    Ht,
    /// 802.11ac — Wi-Fi 5.
    Vht,
    /// 802.11ax — Wi-Fi 6.
    He,
    /// 802.11be — Wi-Fi 7.
    Eht,
    /// Anything else, carrying the raw `DOT11_PHY_TYPE`.
    Other(i32),
}

impl WlanPhy {
    /// The generation number, or `None` when the PHY does not correspond to one we name.
    ///
    /// Returning `None` rather than a default is the whole point: an unknown PHY must not
    /// be silently promoted to Wi-Fi 7 (which would earn a session the least conservative
    /// posture) nor demoted to Wi-Fi 4 (which would waste a good link).
    pub const fn wifi_generation(self) -> Option<u8> {
        match self {
            WlanPhy::Ht => Some(4),
            WlanPhy::Vht => Some(5),
            WlanPhy::He => Some(6),
            WlanPhy::Eht => Some(7),
            WlanPhy::Other(_) => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            WlanPhy::Ht => "802.11n (Wi-Fi 4)",
            WlanPhy::Vht => "802.11ac (Wi-Fi 5)",
            WlanPhy::He => "802.11ax (Wi-Fi 6)",
            WlanPhy::Eht => "802.11be (Wi-Fi 7)",
            WlanPhy::Other(_) => "unknown PHY",
        }
    }

    /// Map a raw `DOT11_PHY_TYPE`.
    ///
    /// The numeric values are the SDK's and are stable (`dot11_phy_type_ht` = 7,
    /// `_vht` = 8, `_he` = 10, `_eht` = 11). They are written out here rather than taken
    /// from the Windows bindings so that this mapping is testable everywhere, including
    /// where there is no Windows.
    pub const fn from_raw(raw: i32) -> Self {
        match raw {
            7 => WlanPhy::Ht,
            8 => WlanPhy::Vht,
            10 => WlanPhy::He,
            11 => WlanPhy::Eht,
            other => WlanPhy::Other(other),
        }
    }

    pub const fn raw(self) -> i32 {
        match self {
            WlanPhy::Ht => 7,
            WlanPhy::Vht => 8,
            WlanPhy::He => 10,
            WlanPhy::Eht => 11,
            WlanPhy::Other(raw) => raw,
        }
    }
}

/// The live state of a connected WLAN interface.
///
/// VD does not read any of this — its optimizer uses exactly two opcodes (§2 of
/// `VD_RE/24-vd-link-qos.md`) — but the information is what our classifier needs and it
/// costs one query. In particular `phy` is the only way to learn the radio generation from
/// the host side, and `rx_kbps`/`tx_kbps` are a real, current rate rather than a nominal
/// link speed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WlanConnection {
    /// Empty when the network is hidden.
    pub ssid: String,
    pub bssid: [u8; 6],
    pub phy: WlanPhy,
    /// Current receive rate, kbps, as the driver reports it.
    pub rx_kbps: u32,
    /// Current transmit rate, kbps.
    pub tx_kbps: u32,
    /// 0–100.
    pub signal_quality: u32,
    /// The connected profile's name.
    pub profile_name: String,
}

/// Read the current connection of the first connected WLAN interface.
///
/// `Ok(None)` means "there is a WLAN API and nothing is connected" — the ordinary answer
/// on a wired machine, and not an error. An `Err` means we could not ask.
pub fn current_connection() -> Result<Option<WlanConnection>, LinkError> {
    platform::current_connection()
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::NetworkManagement::WiFi::{
        WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO, WLAN_INTERFACE_INFO_LIST,
        WLAN_INTF_OPCODE, WLAN_OPCODE_VALUE_TYPE, WlanCloseHandle, WlanEnumInterfaces,
        WlanFreeMemory, WlanOpenHandle, WlanQueryInterface, WlanSetInterface,
        wlan_interface_state_connected, wlan_opcode_value_type_invalid,
    };
    use windows::core::GUID;

    const ERROR_SUCCESS: u32 = 0;
    const ERROR_ACCESS_DENIED: u32 = 5;
    /// `WLAN_API_VERSION_2_0`, what VD asks for.
    const API_VERSION_2_0: u32 = 2;

    pub(super) fn open_backend() -> Result<Arc<dyn WlanBackend>, LinkError> {
        Ok(Arc::new(Win32Wlan::open()?))
    }

    /// Opcode constants are looked up by value here so the numbers in the tests and the
    /// numbers VD uses stay obviously the same thing.
    fn opcode(value: u32) -> WLAN_INTF_OPCODE {
        WLAN_INTF_OPCODE(value as i32)
    }

    /// `wlan_intf_opcode_current_connection`.
    const OPCODE_CURRENT_CONNECTION: u32 = 7;

    pub(super) fn current_connection() -> Result<Option<WlanConnection>, LinkError> {
        let wlan = Win32Wlan::open()?;
        let interfaces = wlan.connected_interfaces().map_err(LinkError::Query)?;
        for info in interfaces {
            // `connection_attributes` is `None` when the interface is not associated.
            if let Some(conn) = wlan.connection_attributes(&info.InterfaceGuid) {
                return Ok(Some(conn));
            }
        }
        Ok(None)
    }

    struct Win32Wlan {
        handle: HANDLE,
    }

    // SAFETY: an open WLAN client handle is a process-wide resource the API documents as
    // usable from any thread, and every method here takes `&self` and performs no
    // unsynchronised mutation of its own state.
    unsafe impl Send for Win32Wlan {}
    unsafe impl Sync for Win32Wlan {}

    impl Win32Wlan {
        fn open() -> Result<Self, LinkError> {
            let mut negotiated = 0u32;
            let mut handle = HANDLE::default();
            let err =
                unsafe { WlanOpenHandle(API_VERSION_2_0, None, &mut negotiated, &mut handle) };
            if err != ERROR_SUCCESS {
                return Err(LinkError::Open(err));
            }
            Ok(Self { handle })
        }

        /// The connected WLAN interfaces, copied out of the API's buffer before it is
        /// freed. The buffer's `InterfaceInfo` field is a one-element trailing array, so
        /// the walk is by pointer, not by index.
        fn connected_interfaces(&self) -> Result<Vec<WLAN_INTERFACE_INFO>, u32> {
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            let err = unsafe { WlanEnumInterfaces(self.handle, None, &mut list) };
            if err != ERROR_SUCCESS {
                return Err(err);
            }
            if list.is_null() {
                return Ok(Vec::new());
            }

            let out = unsafe {
                let count = (*list).dwNumberOfItems as usize;
                let first = std::ptr::addr_of!((*list).InterfaceInfo) as *const WLAN_INTERFACE_INFO;
                (0..count)
                    .map(|i| *first.add(i))
                    .filter(|info| info.isState == wlan_interface_state_connected)
                    .collect::<Vec<_>>()
            };

            unsafe { WlanFreeMemory(list as *const c_void) };
            Ok(out)
        }

        /// `WlanQueryInterface` for a 4-byte boolean, with VD's poisoned-out-parameter
        /// guard: the value type is pre-set to `invalid` and a still-invalid read is
        /// rejected, which catches "the API did not fill the field in".
        fn query_bool(&self, guid: &GUID, opcode_value: u32) -> Result<bool, QueryError> {
            let mut size: u32 = 0;
            let mut data: *mut c_void = std::ptr::null_mut();
            let mut value_type = wlan_opcode_value_type_invalid;

            let err = unsafe {
                WlanQueryInterface(
                    self.handle,
                    guid,
                    opcode(opcode_value),
                    None,
                    &mut size,
                    &mut data,
                    Some(&mut value_type),
                )
            };
            if err != ERROR_SUCCESS {
                return Err(QueryError::Api(err));
            }

            let result = unsafe {
                if data.is_null() || size < 1 {
                    Err(QueryError::Untrustworthy)
                } else if value_type == wlan_opcode_value_type_invalid {
                    // VD's poisoned out-parameter guard: a value type still `invalid`
                    // after the call means the field was never filled in.
                    Err(QueryError::Untrustworthy)
                } else {
                    Ok(*(data as *const u32) != 0)
                }
            };

            if !data.is_null() {
                unsafe { WlanFreeMemory(data as *const c_void) };
            }
            result
        }

        /// `WlanSetInterface` for a 4-byte boolean. The payload is `u32`, matching VD's
        /// `dwDataSize = 4` and the API's documented `BOOL`.
        fn set_bool(&self, guid: &GUID, opcode_value: u32, value: bool) -> u32 {
            let payload: u32 = u32::from(value);
            unsafe {
                WlanSetInterface(
                    self.handle,
                    guid,
                    opcode(opcode_value),
                    size_of::<u32>() as u32,
                    &payload as *const u32 as *const c_void,
                    None,
                )
            }
        }

        /// `WlanQueryInterface` for the full connection attributes.
        ///
        /// `None` covers every "there is nothing to report" case — not associated, a
        /// short buffer, an API error — because the caller's only sensible response to any
        /// of them is the same: carry on without radio detail.
        fn connection_attributes(&self, guid: &GUID) -> Option<WlanConnection> {
            let mut size: u32 = 0;
            let mut data: *mut c_void = std::ptr::null_mut();
            let mut value_type = wlan_opcode_value_type_invalid;

            let err = unsafe {
                WlanQueryInterface(
                    self.handle,
                    guid,
                    opcode(OPCODE_CURRENT_CONNECTION),
                    None,
                    &mut size,
                    &mut data,
                    Some(&mut value_type),
                )
            };
            if err != ERROR_SUCCESS || data.is_null() {
                if !data.is_null() {
                    unsafe { WlanFreeMemory(data as *const c_void) };
                }
                return None;
            }

            let attributes = unsafe {
                if (size as usize) < size_of::<WLAN_CONNECTION_ATTRIBUTES>() {
                    None
                } else {
                    let attrs = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
                    let assoc = &attrs.wlanAssociationAttributes;
                    let ssid_len = (assoc.dot11Ssid.uSSIDLength as usize).min(32);

                    Some(WlanConnection {
                        ssid: String::from_utf8_lossy(&assoc.dot11Ssid.ucSSID[..ssid_len])
                            .into_owned(),
                        bssid: assoc.dot11Bssid,
                        phy: WlanPhy::from_raw(assoc.dot11PhyType.0),
                        rx_kbps: assoc.ulRxRate,
                        tx_kbps: assoc.ulTxRate,
                        signal_quality: assoc.wlanSignalQuality,
                        profile_name: wide_to_string(&attrs.strProfileName),
                    })
                }
            };

            unsafe { WlanFreeMemory(data as *const c_void) };
            attributes
        }

        fn apply_opcode(&self, guid: &GUID, opcode_value: u32, desired: bool) -> OpcodeOutcome {
            match self.query_bool(guid, opcode_value) {
                Err(QueryError::Api(code)) => OpcodeOutcome::QueryFailed(code),
                Err(QueryError::Untrustworthy) => OpcodeOutcome::InvalidValueType,
                Ok(current) if current == desired => OpcodeOutcome::Unchanged,
                Ok(_) => {
                    let err = self.set_bool(guid, opcode_value, desired);
                    if err == ERROR_ACCESS_DENIED {
                        return OpcodeOutcome::AccessDenied;
                    }
                    if err != ERROR_SUCCESS {
                        return OpcodeOutcome::SetFailed(err);
                    }

                    // Read it back. `WlanSetInterface` returning success is not evidence
                    // that the miniport took the value; VD does not trust it either.
                    match self.query_bool(guid, opcode_value) {
                        Ok(new) if new == desired => OpcodeOutcome::Applied,
                        Ok(_) => OpcodeOutcome::VerifyFailed,
                        Err(QueryError::Api(code)) => OpcodeOutcome::QueryFailed(code),
                        Err(QueryError::Untrustworthy) => OpcodeOutcome::InvalidValueType,
                    }
                }
            }
        }
    }

    impl WlanBackend for Win32Wlan {
        fn name(&self) -> &'static str {
            "Win32 wlanapi"
        }

        fn apply(&self, posture: WlanPosture) -> OptimizeReport {
            let interfaces = match self.connected_interfaces() {
                Ok(interfaces) => interfaces,
                Err(code) if code == ERROR_ACCESS_DENIED => return OptimizeReport::AccessDenied,
                Err(code) => {
                    return OptimizeReport::Failed(format!(
                        "WlanEnumInterfaces failed (win32 {code})"
                    ));
                }
            };

            if interfaces.is_empty() {
                return OptimizeReport::NoInterface;
            }

            let mut outcomes = Vec::with_capacity(interfaces.len());
            for info in interfaces {
                let media_streaming = posture
                    .desired_value(OPCODE_MEDIA_STREAMING_MODE)
                    .map(|v| self.apply_opcode(&info.InterfaceGuid, OPCODE_MEDIA_STREAMING_MODE, v))
                    .unwrap_or(OpcodeOutcome::Unchanged);
                let background_scan = posture
                    .desired_value(OPCODE_BACKGROUND_SCAN_ENABLED)
                    .map(|v| {
                        self.apply_opcode(&info.InterfaceGuid, OPCODE_BACKGROUND_SCAN_ENABLED, v)
                    })
                    .unwrap_or(OpcodeOutcome::Unchanged);

                outcomes.push(InterfaceOutcome {
                    description: wide_to_string(&info.strInterfaceDescription),
                    guid: format!("{:?}", info.InterfaceGuid),
                    media_streaming,
                    background_scan,
                });
            }

            OptimizeReport::Applied {
                interfaces: outcomes,
            }
        }
    }

    impl Drop for Win32Wlan {
        fn drop(&mut self) {
            unsafe { WlanCloseHandle(self.handle, None) };
        }
    }

    enum QueryError {
        /// The API returned a Win32 error.
        Api(u32),
        /// The API claimed success but the out-parameters cannot be believed.
        Untrustworthy,
    }

    /// Windows interface descriptions are NUL-terminated UTF-16 in a fixed 256-element
    /// array; the array is not guaranteed to be NUL-terminated if it fills the buffer.
    fn wide_to_string(raw: &[u16; 256]) -> String {
        let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        String::from_utf16_lossy(&raw[..len])
    }

    // The type is only named to keep the import list honest about what the walk uses.
    #[allow(dead_code)]
    fn _assert_trailing_array(list: &WLAN_INTERFACE_INFO_LIST) -> &[WLAN_INTERFACE_INFO; 1] {
        &list.InterfaceInfo
    }

    // Keep the value-type import used on platforms where the comparison is optimised out.
    #[allow(dead_code)]
    fn _assert_invalid(value_type: WLAN_OPCODE_VALUE_TYPE) -> bool {
        value_type == wlan_opcode_value_type_invalid
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub(super) fn open_backend() -> Result<Arc<dyn WlanBackend>, LinkError> {
        // Deliberately *not* a silent no-op backend: a caller asking for the optimizer on
        // a platform that cannot provide it should be told, so the log says "we are not
        // doing this" rather than nothing at all.
        Err(LinkError::Unsupported)
    }

    pub(super) fn current_connection() -> Result<Option<WlanConnection>, LinkError> {
        Err(LinkError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records every posture it is asked for and answers with a canned report.
    struct FakeBackend {
        seen: Mutex<Vec<WlanPosture>>,
        calls: AtomicUsize,
        report: OptimizeReport,
    }

    impl FakeBackend {
        fn new(report: OptimizeReport) -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
                report,
            }
        }

        fn postures(&self) -> Vec<WlanPosture> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl WlanBackend for FakeBackend {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn apply(&self, posture: WlanPosture) -> OptimizeReport {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(posture);
            self.report.clone()
        }
    }

    fn applied() -> OptimizeReport {
        OptimizeReport::Applied {
            interfaces: vec![InterfaceOutcome {
                description: "Intel(R) Wi-Fi 7".into(),
                guid: "{00000000-0000-0000-0000-000000000000}".into(),
                media_streaming: OpcodeOutcome::Applied,
                background_scan: OpcodeOutcome::Applied,
            }],
        }
    }

    #[test]
    fn streaming_posture_matches_vds_optimize_wlan_of_one() {
        // VD's OptimizeWLAN(1): media_streaming_mode = true, background_scan_enabled =
        // false. If this ever flips, we are doing the opposite of the thing we ported.
        let streaming = WlanPosture::streaming();
        assert_eq!(
            streaming.desired_value(OPCODE_MEDIA_STREAMING_MODE),
            Some(true)
        );
        assert_eq!(
            streaming.desired_value(OPCODE_BACKGROUND_SCAN_ENABLED),
            Some(false)
        );
    }

    #[test]
    fn resting_posture_is_the_exact_inverse() {
        let off = WlanPosture::off();
        assert_eq!(off.desired_value(OPCODE_MEDIA_STREAMING_MODE), Some(false));
        assert_eq!(
            off.desired_value(OPCODE_BACKGROUND_SCAN_ENABLED),
            Some(true)
        );
        assert!(off.is_off());
        assert!(!WlanPosture::streaming().is_off());
    }

    #[test]
    fn opcodes_are_the_documented_ones() {
        assert_eq!(OPCODE_BACKGROUND_SCAN_ENABLED, 2);
        assert_eq!(OPCODE_MEDIA_STREAMING_MODE, 3);
        assert_eq!(
            WlanPosture::streaming().desired_value(1),
            None,
            "autoconf is not ours"
        );
    }

    #[test]
    fn phy_types_map_to_the_right_generations() {
        // The SDK's DOT11_PHY_TYPE values. Getting these wrong would silently mislabel the
        // client's radio, which is worse than not classifying it at all.
        assert_eq!(WlanPhy::from_raw(7).wifi_generation(), Some(4)); // ht  = 802.11n
        assert_eq!(WlanPhy::from_raw(8).wifi_generation(), Some(5)); // vht = 802.11ac
        assert_eq!(WlanPhy::from_raw(10).wifi_generation(), Some(6)); // he  = 802.11ax
        assert_eq!(WlanPhy::from_raw(11).wifi_generation(), Some(7)); // eht = 802.11be
    }

    #[test]
    fn an_unknown_phy_has_no_generation() {
        // Neither promoted nor demoted: `None` means the classifier falls back to its own
        // conservative path, rather than this layer picking a posture for us.
        assert_eq!(WlanPhy::from_raw(4).wifi_generation(), None); // ofdm = 802.11a
        assert_eq!(WlanPhy::from_raw(0).wifi_generation(), None); // unknown
        assert_eq!(WlanPhy::Other(99).wifi_generation(), None);
    }

    #[test]
    fn phy_round_trips() {
        for raw in [0, 4, 7, 8, 10, 11, 42] {
            assert_eq!(WlanPhy::from_raw(raw).raw(), raw);
        }
    }

    #[test]
    fn session_applies_on_start_and_repeats_on_the_period() {
        let backend = Arc::new(FakeBackend::new(applied()));
        let session = WlanSession::start_with(
            Arc::clone(&backend) as Arc<dyn WlanBackend>,
            WlanPosture::streaming(),
            Duration::from_millis(20),
        );

        std::thread::sleep(Duration::from_millis(120));
        let postures = backend.postures();
        assert!(
            postures.len() >= 3,
            "expected the posture to be re-asserted, saw {} application(s)",
            postures.len()
        );
        assert!(postures.iter().all(|p| *p == WlanPosture::streaming()));

        drop(session);
    }

    #[test]
    fn dropping_restores_the_adapter() {
        // The improvement on VD: the off-path is taken on the way out.
        let backend = Arc::new(FakeBackend::new(applied()));
        let session = WlanSession::start_with(
            Arc::clone(&backend) as Arc<dyn WlanBackend>,
            WlanPosture::streaming(),
            Duration::from_millis(10),
        );
        std::thread::sleep(Duration::from_millis(30));
        drop(session);

        let postures = backend.postures();
        assert_eq!(
            postures.last(),
            Some(&WlanPosture::off()),
            "the last thing the backend is asked for must be the resting posture"
        );
    }

    #[test]
    fn a_session_that_never_asserted_anything_does_not_restore() {
        // If the requested posture *is* the resting posture there is nothing to undo.
        let backend = Arc::new(FakeBackend::new(OptimizeReport::NoInterface));
        let session = WlanSession::start_with(
            Arc::clone(&backend) as Arc<dyn WlanBackend>,
            WlanPosture::off(),
            Duration::from_millis(10),
        );
        drop(session);
        assert_eq!(backend.postures(), vec![WlanPosture::off()]);
    }

    #[test]
    fn no_interface_is_not_a_failure_and_the_thread_keeps_polling() {
        // A wired-only machine must not stop the optimizer: a Wi-Fi adapter can appear
        // mid-session. This mirrors VD returning 1 and continuing.
        let backend = Arc::new(FakeBackend::new(OptimizeReport::NoInterface));
        let session = WlanSession::start_with(
            Arc::clone(&backend) as Arc<dyn WlanBackend>,
            WlanPosture::streaming(),
            Duration::from_millis(10),
        );
        std::thread::sleep(Duration::from_millis(60));
        assert!(
            backend.postures().len() >= 3,
            "no-interface must not stop the loop"
        );
        drop(session);
    }

    #[test]
    fn a_refusal_stops_the_loop_after_one_attempt() {
        // VD's `status > 1` rule. One honest log line beats an 11-second heartbeat of
        // the same warning.
        let backend = Arc::new(FakeBackend::new(OptimizeReport::AccessDenied));
        let session = WlanSession::start_with(
            Arc::clone(&backend) as Arc<dyn WlanBackend>,
            WlanPosture::streaming(),
            Duration::from_millis(10),
        );
        std::thread::sleep(Duration::from_millis(60));

        let attempts = backend
            .postures()
            .iter()
            .filter(|p| **p == WlanPosture::streaming())
            .count();
        assert_eq!(
            attempts, 1,
            "a refused posture must be attempted exactly once"
        );
        drop(session);
    }

    #[test]
    fn a_per_interface_error_is_a_failure() {
        let report = OptimizeReport::Applied {
            interfaces: vec![InterfaceOutcome {
                description: "x".into(),
                guid: "y".into(),
                media_streaming: OpcodeOutcome::Applied,
                background_scan: OpcodeOutcome::VerifyFailed,
            }],
        };
        assert!(
            report.is_failure(),
            "a write that did not read back must stop the loop"
        );
    }

    #[test]
    fn outcome_error_classification() {
        assert!(!OpcodeOutcome::Unchanged.is_error());
        assert!(!OpcodeOutcome::Applied.is_error());
        for outcome in [
            OpcodeOutcome::AccessDenied,
            OpcodeOutcome::SetFailed(6),
            OpcodeOutcome::QueryFailed(87),
            OpcodeOutcome::InvalidValueType,
            OpcodeOutcome::VerifyFailed,
        ] {
            assert!(outcome.is_error(), "{outcome:?} must count as an error");
        }
    }

    #[test]
    fn summary_names_the_interface_and_both_opcodes() {
        let report = applied();
        let summary = report.summary();
        assert!(summary.contains("Intel(R) Wi-Fi 7"), "{summary}");
        assert!(summary.contains("media=applied + verified"), "{summary}");
        assert!(summary.contains("bgscan=applied + verified"), "{summary}");
    }

    #[test]
    fn unsupported_platform_is_reported_not_swallowed() {
        #[cfg(not(windows))]
        {
            assert_eq!(
                WlanSession::start(WlanPosture::streaming()).err(),
                Some(LinkError::Unsupported)
            );
        }
    }
}
