//! x-link — GemLink's link layer: classification, per-class QoS posture, and the
//! VD-parity WLAN optimizer.
//!
//! The project's stream quality is decided by the *wire* as much as by the codec, and
//! until now nothing in GemLink ever touched the wire's own controls. This crate is the
//! link half of milestone **M4 — transport v2** ([`ROADMAP.md`](../../../ROADMAP.md)):
//!
//! * [`classify`] — turn a description of an interface (kind, speed, Wi-Fi generation,
//!   whether it is the headset's own SoftAP) into the negotiated [`LinkClass`].
//! * [`qos`] — the QoS posture for each class: what the server should ask of the link
//!   while a session runs, and what it should not.
//! * [`wlan`] — the mechanism that has no upstream equivalent: put the PC's Wi-Fi
//!   adapter into *media streaming mode* with background scanning off for the duration
//!   of a session, re-asserted periodically, restored on exit.
//!
//! ## Where this comes from
//!
//! [`wlan`] is a port of behaviour read out of Virtual Desktop's
//! `libVirtualDesktopNet.dll` — reverse-engineering notes, including the disassembly
//! this was written against, are in
//! `VD_RE/24-vd-link-qos.md`. The short version:
//!
//! > VD exports `OptimizeWLAN` / `StartWLANOptimizerThread` / `StopWLANOptimizerThread`
//! > and imports exactly five functions from `wlanapi.dll`. Its thread calls
//! > `OptimizeWLAN(1)` every 11.0 s, which for every *connected* WLAN interface sets
//! > `wlan_intf_opcode_media_streaming_mode` true and
//! > `wlan_intf_opcode_background_scan_enabled` false, idempotently and with
//! > read-back verification. It never turns them back off — not on stop, not on exit.
//!
//! We keep the mechanism and fix the wart: the posture is symmetric and
//! [`wlan::WlanSession`] restores the adapter when it drops.
//!
//! ## What this crate deliberately does not do
//!
//! Nothing here is a *measurement*. Every claim in this crate is "this is what the API
//! does", never "this made the stream better" — the latter needs the rig
//! ([`docs/BENCH.md`](../../../docs/BENCH.md)) and a real link. See the handoff's §8b:
//! a mechanism read out of a binary is a hypothesis until the rig says otherwise.
//!
//! FEC, retransmit and jitter buffering in the media plane are **not** here: VD's are
//! unrecovered (its transport is not in the extracted payload — see
//! `VD_RE/24-vd-link-qos.md` §7), and guessing at
//! them is exactly the failure mode this project keeps having to write down.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod classify;
pub mod host;
pub mod qos;
pub mod sched;
pub mod wlan;

pub use classify::{LinkDescriptor, LinkKind, classify};
pub use host::{HostAdapter, HostLink, LinkResolution, classify_host, is_soft_ap_ssid, resolve};
pub use qos::{Dscp, LinkQosProfile, profile_for, profile_for_resolution};
pub use sched::{HostScheduler, SchedError, SchedReport};
pub use wlan::{
    InterfaceOutcome, LinkError, OpcodeOutcome, OptimizeReport, REASSERT_PERIOD, WlanBackend,
    WlanConnection, WlanPhy, WlanPosture, WlanSession,
};

/// Re-exported so callers do not have to depend on `x-protocol` for the one type this
/// crate's whole surface is keyed on.
pub use x_protocol::LinkClass;
