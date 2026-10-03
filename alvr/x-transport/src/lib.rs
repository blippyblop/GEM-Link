//! x-transport — GemLink's media plane.
//!
//! Milestone **M4 — transport v2** (`ROADMAP.md`), and the machinery behind
//! [ADR-0011](../../docs/adr/ADR-0011-never-show-an-untrusted-frame.md): *never show a
//! frame the client cannot trust*.
//!
//! ## What this is, given that VD's transport is unrecoverable
//!
//! Virtual Desktop's media plane is not in any artifact we possess and no module we have
//! could send it (`VD_RE/24-vd-link-qos.md` §8.1 — six negative checks). So this is **not
//! a port**. It is our design, holding the properties VD is *known* to have and this
//! project's tree does not:
//!
//! | Property | Where it comes from |
//! |---|---|
//! | Datagrams carry a monotonic per-frame index | ADR-0011's required signal; VD's media port and per-session `Aes`/`ConnectionID` at pairing are the only facts we have about its shape |
//! | A lost datagram is repaired rather than ignored | the grey-frame defect: a dropped frame breaks the HEVC reference chain and the following P-frames decode to flat grey. VD does not have that class of bug |
//! | The receiver knows a frame is unreconstructable, and **cannot present it** | ADR-0011. This is a *type-level* guarantee here, not a convention: see [`Receiver::release`] |
//! | Per-datagram AEAD with derived nonces | ADR-0006 / DoD #5, and M1's design trap: the media plane needs per-packet nonces without a shared lock, which the control plane's `NoiseSocket`-behind-a-`Mutex` cannot give |
//! | Pacing that backpressures instead of discarding | the framing of the defect: today the send path is pace → encode → discover the queue is full → discard, so the encoder and the reference chain are already spent |
//!
//! ## What is deliberately *not* here
//!
//! * **No sockets.** This is the plane, not the transport binding; `alvr_sockets` owns the
//!   datagram plumbing and the wiring step decides how they meet.
//! * **No congestion control.** The pacer is rate-limited, not adaptive: the rate comes in
//!   from the caller, because deciding what the rate *should* be is the bitrate
//!   controller's job and mixing the two is how you get a controller that fights itself.
//! * **No retransmit *policy*.** The receiver can name exactly which fragments are missing
//!   ([`Receiver::missing_for`]) and the sender can mark a datagram as a retransmit
//!   ([`Flags::RETRANSMIT`]), but whether and how often to ask is a decision for the layer
//!   with the RTT, so it is left to the caller.
//! * **No measurements.** The bench drives this crate through the impairment profiles and
//!   asserts the gate (`x-bench -- transport`); nothing in this crate claims a number.
//!
//! ## The invariant, structurally
//!
//! The single most important thing here is a type: [`Receiver::release`] returns a
//! [`DeliveredFrame`], and a [`DeliveredFrame`] only ever carries payload bytes for a
//! frame that was either complete or repaired. A frame that could not be reconstructed is
//! reported as [`FrameOutcome::Unreconstructable`] with **no payload field to read**. The
//! display path cannot show a bogus frame because it has nothing to show — which is the
//! failure the old code had, where "the decoder produced a picture" was treated as "we have
//! a frame".

#![forbid(unsafe_code)]

pub mod adaptive;
pub mod crypto;
pub mod fec;
pub mod pacer;
pub mod packetizer;
pub mod receiver;
pub mod trust;
pub mod wire;

pub use adaptive::{AdaptiveConfig, AdaptiveParity, RatioChange};
pub use crypto::{KEY_LEN, MediaCipher};
pub use fec::{FecError, decode_striped as fec_decode_striped, encode as fec_encode};
pub use pacer::{Pacer, PacerConfig};
pub use packetizer::{FrameMeta, Packetizer};
pub use receiver::{DeliveredFrame, FrameOutcome, Receiver, ReceiverStats, ReleasePolicy};
pub use trust::{FrameTrust, SendDecision, SendGate, SuppressReason, TrustGate, UntrustedReason};
pub use wire::{Flags, FragmentHeader, HEADER_LEN};

/// The largest payload a single datagram may carry, given an MTU.
///
/// Kept as a function rather than a constant because the MTU is a property of the link, and
/// getting this wrong by a few bytes is how a "1400-byte" stream becomes fragmented at IP
/// and doubles its packet rate.
pub const fn payload_budget(mtu: usize) -> usize {
    mtu.saturating_sub(HEADER_LEN)
}
