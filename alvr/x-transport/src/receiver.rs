//! The receiver: reassemble, repair, and **refuse to hand out what it cannot rebuild**.
//!
//! This is where ADR-0011 stops being a policy and becomes a property.
//!
//! ## The type is the guarantee
//!
//! [`Receiver::release`] yields [`DeliveredFrame`] values. A `DeliveredFrame` carries payload
//! bytes **only** for a frame that arrived complete or was repaired — [`DeliveredFrame::payload`]
//! is `Option<&[u8]>` and is `None` for [`FrameOutcome::Unreconstructable`]. There is no field
//! to read, no "and then check the flag" step to forget, and no way to reach the display path
//! with garbage.
//!
//! That matters because of how the defect actually happened. The old path treated "the
//! decoder produced a picture" as "we have a frame", so a P-frame whose reference had been
//! dropped upstream arrived as flat mid-grey and was displayed — 41.3 % of frames, with the
//! telemetry reporting zero errors (`VD_RE/50-grey-frame-experiments.md`). A convention
//! ("check the flag") would have been forgotten in the same way. A missing payload cannot be.
//!
//! Note what this does *not* cover, and be honest about the boundary: this receiver knows a
//! frame is unreconstructable **when its own datagrams are missing**. A frame whose reference
//! chain was broken *upstream* — because the sender chose not to transmit it — arrives here
//! complete and authentic, and no media plane can tell. That case is the sender's half of
//! ADR-0011 and it is why the send side must not emit an undecodable frame in the first
//! place.
//!
//! ## The jitter buffer
//!
//! Frames are released on the later of two conditions:
//!
//! * **Reorder cover** ([`ReleasePolicy::jitter_frames`]): once a frame `n + jitter_frames`
//!   has completed, frame `n` is released whether or not its stragglers arrived. Waiting
//!   longer only helps if the stragglers are still coming.
//! * **Deadline** ([`ReleasePolicy::deadline`]): a hard bound from the frame's first arriving
//!   datagram, so an incomplete frame cannot be held forever. This is also what bounds the
//!   receiver's memory: nothing survives its deadline.
//!
//! A deadline that is too tight wastes repair that was available; too loose and it adds
//! latency. The bench is where that trade is measured.

use std::{collections::BTreeMap, time::Duration};

use crate::{
    crypto::{CryptoError, MediaKeys},
    fec,
    wire::{FragmentHeader, WireError},
};

/// What happened to a frame that has been released from the jitter buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// Every data shard arrived; no repair was needed.
    Complete,
    /// Data shards were missing and the FEC rebuilt them.
    Recovered { repaired: u16 },
    /// Shards are missing and cannot be rebuilt. **The caller must not display this frame.**
    /// Holding the previous frame and reprojecting it is the response ADR-0011 permits.
    Unreconstructable { missing: u16 },
}

impl FrameOutcome {
    pub const fn is_usable(self) -> bool {
        matches!(
            self,
            FrameOutcome::Complete | FrameOutcome::Recovered { .. }
        )
    }
}

/// A released frame. The payload exists only if the frame is usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveredFrame {
    pub frame_index: u64,
    pub target_timestamp_us: u64,
    /// How late the release was relative to the frame's own target time, when the sender's
    /// clock and this one share an epoch. Zero when they do not.
    pub released_at: Duration,
    pub outcome: FrameOutcome,
    /// Whether the sender marked this frame a keyframe.
    ///
    /// The display gate needs it and nothing else on the wire could supply it: a hold is released
    /// by a keyframe, and "keyframe" is a property of the encoder's output, not of the transport.
    pub is_keyframe: bool,
    payload: Option<Vec<u8>>,
}

impl DeliveredFrame {
    /// The frame's bytes, or `None` if it could not be reconstructed.
    ///
    /// Returning `Option` rather than an empty slice is the entire point: a caller cannot
    /// accidentally display a frame that was not rebuilt, because there is nothing to
    /// display. An empty slice would be a valid, decodable, wrong picture.
    pub fn payload(&self) -> Option<&[u8]> {
        self.payload.as_deref()
    }

    /// Take the frame's bytes.
    ///
    /// The receive path has already built this buffer and nothing else needs it, so handing it over
    /// rather than copying it is the difference between a memcpy per frame and none. At 90 Hz with a
    /// 48 KB frame that is 4.3 MB/s of pure copying, spent to leave the original in a struct that is
    /// about to be dropped.
    pub fn into_payload(self) -> Option<Vec<u8>> {
        self.payload
    }

    /// Whether this frame may be presented. Equivalent to `payload().is_some()`, and the
    /// only question the display path needs to ask.
    pub const fn is_displayable(&self) -> bool {
        self.outcome.is_usable()
    }
}

/// When to release a frame.
///
/// Three thresholds, because there are three different things a hole can be waiting for, and only
/// the frame in that state should pay for it:
///
/// | state | waits | why |
/// |---|---|---|
/// | complete | **nothing** | there is no hole |
/// | holed, and the FEC can rebuild it from what has arrived | [`Self::straggler_delay`] | the data is here; the stragglers just need to stop arriving |
/// | holed, and the FEC cannot | [`Self::repair_delay`] | only a re-send can help, and that costs a round trip |
///
/// Collapsing those into one number is what the bench caught twice over: first a count of frames,
/// which made *every* frame wait a whole display period; then a single time, which made every
/// FEC-recoverable frame pay for a round trip it did not need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasePolicy {
    /// How long to wait for a frame's own stragglers to finish arriving before believing a hole.
    /// The spread of a frame's datagrams is a property of the link, so this is a time: expressing
    /// it as a count of frames made it a property of the *display rate*, which is how a wired link
    /// with 0.3 ms of jitter came to be charged 11.1 ms.
    pub straggler_delay: Duration,
    /// Straggler delay **plus a round trip**: how long a hole the FEC cannot fill is given for a
    /// re-send to arrive. Only a frame in that state pays it, and the round trip is unavoidable —
    /// it is the cost of asking.
    pub repair_delay: Duration,
    /// Hard bound from the first arriving datagram: after this, whatever is still missing is lost.
    pub deadline: Duration,
    /// Kept for callers that size the cover in whole frames. [`Self::for_link`] does not use it.
    pub jitter_frames: u16,
}

impl ReleasePolicy {
    /// A policy expressed in frames, for a given frame interval.
    ///
    /// The coarse form, and the right one for a test that wants a specific window rather than a
    /// modelled one. [`Self::for_link`] is what a session should use.
    pub fn new(jitter_frames: u16, frame_interval: Duration) -> Self {
        let window = frame_interval * jitter_frames as u32;
        Self {
            straggler_delay: window,
            repair_delay: window,
            deadline: window + frame_interval,
            jitter_frames,
        }
    }

    /// A policy sized from what the link actually does.
    ///
    /// `jitter` is how far apart a frame's own datagrams can be spread; `stall` is how long the link
    /// can go quiet without dropping them (a channel hop delays rather than loses, so it belongs in
    /// the spread); `rtt` is what a re-send costs. The tenth-of-a-frame added to each is scheduling
    /// slack, not a modelling term — without it a repair lands exactly on the threshold that would
    /// discard it.
    pub fn for_link(
        jitter: Duration,
        stall: Duration,
        rtt: Duration,
        frame_interval: Duration,
    ) -> Self {
        let slack = frame_interval / 10;
        let straggler_delay = jitter + stall + slack;
        let repair_delay = straggler_delay + rtt + jitter + slack;
        Self {
            straggler_delay,
            // The round trip, plus the jitter the *repair itself* will suffer on the way back. The
            // repair is a datagram like any other and it crosses the same link; leaving its jitter
            // out made the window exactly the round trip, which the bench showed as repairs landing
            // a hundredth of a millisecond after the frame that was waiting for them.
            repair_delay,
            // The hard bound is one frame interval *past the repair window*, not past the straggler
            // window. Being derived from the wrong one made the deadline fire before the window it
            // was supposed to be a backstop for, so a frame was given up on while the repair it had
            // asked for was still in flight.
            deadline: repair_delay + frame_interval,
            jitter_frames: 0,
        }
    }

    /// How many whole frames of cover this policy would be, for a log line or a budget.
    pub fn cover_in_frames(&self, frame_interval: Duration) -> f64 {
        if frame_interval.is_zero() {
            return 0.0;
        }
        self.straggler_delay.as_secs_f64() / frame_interval.as_secs_f64()
    }
}

/// Counters. These exist to be read by the bench and by the session log — ADR-0011's lesson
/// was that the *absence* of a counter is what let a 41 % defect hide behind "0 errors".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiverStats {
    pub datagrams_received: u64,
    pub datagrams_duplicate: u64,
    pub datagrams_late: u64,
    pub datagrams_malformed: u64,
    pub datagrams_unauthenticated: u64,
    pub datagrams_retransmit: u64,
    pub payload_bytes_delivered: u64,
    pub frames_complete: u64,
    pub frames_recovered: u64,
    pub frames_unreconstructable: u64,
    pub fragments_repaired: u64,
}

impl ReceiverStats {
    pub fn frames_released(&self) -> u64 {
        self.frames_complete + self.frames_recovered + self.frames_unreconstructable
    }

    /// The fraction of released frames that could not be reconstructed. This is the number
    /// that was missing: the old path had no way to express "a frame I could not rebuild",
    /// so it could not report that 41 % of them were unusable.
    pub fn unreconstructable_fraction(&self) -> f64 {
        let released = self.frames_released();
        if released == 0 {
            0.0
        } else {
            self.frames_unreconstructable as f64 / released as f64
        }
    }

    /// Frames released with a payload. The display path can only ever have seen this many.
    pub fn frames_deliverable(&self) -> u64 {
        self.frames_complete + self.frames_recovered
    }
}

/// What the receiver made of one datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvEvent {
    /// Stored. `completed` is true if this was the last shard the frame was waiting for.
    Accepted {
        frame_index: u64,
        fragment_index: u16,
        completed: bool,
    },
    /// Already have this fragment. Counted rather than ignored — see the note on replay in
    /// [`crate::crypto`].
    Duplicate,
    /// For a frame that has already been released; too late to matter.
    Late,
    /// Rejected.
    Rejected(RecvError),
}

/// Why a datagram was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    Wire(WireError),
    Crypto(CryptoError),
}

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecvError::Wire(e) => write!(f, "{e}"),
            RecvError::Crypto(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RecvError {}

#[derive(Debug)]
struct PartialFrame {
    first_arrival: Duration,
    target_timestamp_us: u64,
    /// Learned from the first shard that arrives, whichever kind it is.
    is_keyframe: bool,
    frame_len: u32,
    data_count: u16,
    parity_count: u16,
    shards: Vec<Option<Vec<u8>>>,
    received: usize,
}

impl PartialFrame {
    /// Whether every **data** shard is present.
    ///
    /// Not `received >= data_count`: parity shards count towards `received`, so that
    /// comparison would call a frame complete while one of its data shards was still
    /// missing — and then hand out a frame assembled with a hole in it. The distinction
    /// between "enough datagrams" and "the right datagrams" is the whole of this method.
    fn has_all_data(&self) -> bool {
        self.shards[..self.data_count as usize]
            .iter()
            .all(Option::is_some)
    }

    fn missing_data(&self) -> Vec<u16> {
        (0..self.data_count)
            .filter(|i| self.shards[*i as usize].is_none())
            .collect()
    }

    /// Whether the FEC could rebuild this frame from what has arrived, without waiting for anything.
    ///
    /// Asked *before* releasing rather than discovered at release, because it decides how long the
    /// frame waits: a hole the code can fill needs only the stragglers to stop arriving, and one it
    /// cannot needs a round trip. Per block, because the code is striped — a frame can be repairable
    /// in three blocks and hopeless in the fourth.
    fn fec_repairable(&self) -> bool {
        let data_count = self.data_count as usize;
        if data_count == 0 {
            return false;
        }
        let blocks = fec::blocks_for(data_count);
        let parity_per_block = self.parity_count as usize / blocks;

        let mut missing_per_block = vec![0usize; blocks];
        for (index, slot) in self.shards[..data_count].iter().enumerate() {
            if slot.is_none() {
                missing_per_block[fec::block_of(index, blocks)] += 1;
            }
        }

        missing_per_block
            .iter()
            .all(|missing| *missing <= parity_per_block)
    }
}

/// Reassembles frames from datagrams.
pub struct Receiver {
    policy: ReleasePolicy,
    keys: Option<MediaKeys>,
    /// Frames still being assembled, ordered so release is a single pass.
    partial: BTreeMap<u64, PartialFrame>,
    /// The highest frame index that has been handed out. A datagram at or below this is
    /// late by definition: the display path has already moved past it.
    max_released: Option<u64>,
    stats: ReceiverStats,
    /// Bounded so a sender bug (a frame index leaping into the future) cannot exhaust
    /// memory. Entries are dropped oldest-first.
    max_frames_in_flight: usize,
}

impl Receiver {
    pub fn new(policy: ReleasePolicy, keys: Option<MediaKeys>) -> Self {
        Self {
            policy,
            keys,
            partial: BTreeMap::new(),
            max_released: None,
            stats: ReceiverStats::default(),
            max_frames_in_flight: 256,
        }
    }

    /// Cap on simultaneously incomplete frames.
    pub fn with_max_in_flight(mut self, max: usize) -> Self {
        self.max_frames_in_flight = max.max(1);
        self
    }

    pub fn stats(&self) -> &ReceiverStats {
        &self.stats
    }

    /// Take the counters, resetting them. For a session logger that reports deltas.
    pub fn take_stats(&mut self) -> ReceiverStats {
        std::mem::take(&mut self.stats)
    }

    /// Frames currently being assembled.
    pub fn in_flight(&self) -> usize {
        self.partial.len()
    }

    /// Take one datagram.
    pub fn on_datagram(&mut self, datagram: &[u8], now: Duration) -> RecvEvent {
        self.stats.datagrams_received += 1;

        let (header, body) = match FragmentHeader::decode(datagram) {
            Ok(parsed) => parsed,
            Err(e) => {
                self.stats.datagrams_malformed += 1;
                return RecvEvent::Rejected(RecvError::Wire(e));
            }
        };

        if header.flags.is_retransmit() {
            self.stats.datagrams_retransmit += 1;
        }

        // Decrypt before anything else: the plaintext's length is what the shard layout was
        // built from, and an unauthenticated datagram must not influence state at all.
        let shard = match &mut self.keys {
            Some(keys) => match keys.open(
                header.key_epoch,
                header.frame_index,
                header.fragment_index,
                &datagram[..crate::wire::HEADER_LEN],
                body,
            ) {
                Ok(plaintext) => plaintext,
                Err(e) => {
                    self.stats.datagrams_unauthenticated += 1;
                    return RecvEvent::Rejected(RecvError::Crypto(e));
                }
            },
            None => body.to_vec(),
        };

        // A frame that has already been released is late: keeping it would mean holding a
        // frame the display path has moved past, so it is counted and dropped.
        if self
            .max_released
            .is_some_and(|released| header.frame_index <= released)
        {
            self.stats.datagrams_late += 1;
            return RecvEvent::Late;
        }

        if self.partial.len() >= self.max_frames_in_flight
            && !self.partial.contains_key(&header.frame_index)
        {
            self.evict_oldest();
        }

        let fragment_index = header.fragment_index;
        let entry = self
            .partial
            .entry(header.frame_index)
            .or_insert_with(|| PartialFrame {
                first_arrival: now,
                target_timestamp_us: header.target_timestamp_us,
                is_keyframe: header.flags.is_keyframe(),
                frame_len: header.frame_len,
                data_count: header.data_count,
                parity_count: header.parity_count,
                shards: vec![None; header.data_count as usize + header.parity_count as usize],
                received: 0,
            });

        // Every shard carries the keyframe bit, so a frame whose first shard was lost still
        // learns the truth from any later one.
        entry.is_keyframe |= header.flags.is_keyframe();

        // A datagram that disagrees with the frame's established shape is a bug or an
        // attack; the frame it belongs to is not trustworthy, so the whole frame is.
        if entry.data_count != header.data_count
            || entry.parity_count != header.parity_count
            || entry.frame_len != header.frame_len
        {
            self.stats.datagrams_malformed += 1;
            return RecvEvent::Rejected(RecvError::Wire(WireError::Malformed(
                "frame shape changed mid-frame",
            )));
        }

        let slot = &mut entry.shards[fragment_index as usize];
        if slot.is_some() {
            // A retransmit that crossed its own repair, or a replay.
            self.stats.datagrams_duplicate += 1;
            return RecvEvent::Duplicate;
        }
        *slot = Some(shard);
        entry.received += 1;

        let completed = entry.has_all_data();
        let frame_index = header.frame_index;

        RecvEvent::Accepted {
            frame_index,
            fragment_index,
            completed,
        }
    }

    /// The data fragments still missing for a frame, for the caller to request again.
    ///
    /// Empty when the frame is complete, unknown, or already released. The *policy* — when to
    /// ask, how often, whether to ask at all — is the caller's: it is the layer that knows
    /// the round-trip time, and a receiver that nagged on its own timer would double the
    /// traffic on exactly the link that is already struggling.
    pub fn nack(&self, frame_index: u64) -> Vec<u16> {
        self.partial
            .get(&frame_index)
            .map(PartialFrame::missing_data)
            .unwrap_or_default()
    }

    /// Frames still incomplete, oldest first — the candidates for a NACK.
    pub fn incomplete_frames(&self) -> Vec<u64> {
        self.partial
            .iter()
            .filter(|(_, f)| !f.has_all_data())
            .map(|(index, _)| *index)
            .collect()
    }

    /// Frames a repair request is **about**: still incomplete, but past the point where the
    /// stragglers were expected to finish arriving.
    ///
    /// Not the same set as [`Self::incomplete_frames`], and the difference is the whole point of
    /// having two. A frame that arrived a moment ago is incomplete because its own datagrams are
    /// still in flight; asking for them is useless, and at the millisecond cadence a prompt sweep
    /// needs, asking for every such frame is the bulk of the traffic — measured as `nack(s)`
    /// climbing by tens of thousands against a handful of frames that were ever really missing
    /// anything.
    pub fn repairable_frames(&self, now: Duration) -> Vec<u64> {
        self.partial
            .iter()
            .filter(|(_, f)| {
                !f.has_all_data()
                    && now.saturating_sub(f.first_arrival) >= self.policy.straggler_delay
            })
            .map(|(index, _)| *index)
            .collect()
    }

    /// Release everything that is ready, oldest frame first, **stopping at the first frame that is
    /// not** — so what comes out is always in frame order.
    ///
    /// Three ways a frame becomes ready, and the first is the one that matters:
    ///
    /// 1. **It is complete.** Nothing is missing, so there is nothing to wait for. This is the
    ///    change that removed a whole frame interval of latency from every frame: the cover exists
    ///    to give a *hole* time to be filled by a straggler or a repair, and a frame without one has
    ///    no use for it. The old rule held every frame until a later one completed, which on a clean
    ///    stream released each frame at the very end of its own period — late by definition.
    /// 2. Its straggler window has elapsed **and** the FEC can rebuild it from what has arrived —
    ///    the data is here, so stop waiting. See [`ReleasePolicy::straggler_delay`].
    /// 3. Its repair window has elapsed: the code could not fill the hole, a re-send was asked for,
    ///    and this is how long it was given. See [`ReleasePolicy::repair_delay`].
    /// 4. Its deadline has passed. The hole is permanent and the frame goes out with whatever the
    ///    FEC could make of it, or without a payload at all.
    ///
    /// The stopping is what keeps ordering: frame `n + 1` may not overtake a held frame `n`, or the
    /// display path would show the newer picture and then the older one.
    pub fn release(&mut self, now: Duration) -> Vec<DeliveredFrame> {
        let mut ready: Vec<u64> = Vec::new();

        for (index, frame) in self.partial.iter() {
            let waited = now.saturating_sub(frame.first_arrival);
            let stragglers_over = waited >= self.policy.straggler_delay;
            let repair_window_over = waited >= self.policy.repair_delay;
            let deadline_passed = waited >= self.policy.deadline;

            // Complete: nothing to wait for.
            // FEC can rebuild it: the data is here, so stop waiting for the stragglers.
            // Otherwise the repair window, then the deadline.
            let due = frame.has_all_data()
                || (stragglers_over && frame.fec_repairable())
                || repair_window_over
                || deadline_passed;

            if due {
                ready.push(*index);
            } else {
                // Holding a hole holds everything behind it — for at most one of the windows above.
                break;
            }
        }

        let mut out = Vec::with_capacity(ready.len());
        for index in ready {
            let Some(mut frame) = self.partial.remove(&index) else {
                continue;
            };
            self.max_released = Some(self.max_released.map_or(index, |r| r.max(index)));
            out.push(self.finish(index, &mut frame, now));
        }
        out
    }

    fn finish(&mut self, index: u64, frame: &mut PartialFrame, now: Duration) -> DeliveredFrame {
        let missing = frame.missing_data();

        let outcome = if missing.is_empty() {
            FrameOutcome::Complete
        } else {
            match fec::decode_striped(
                &mut frame.shards,
                frame.data_count as usize,
                frame.parity_count as usize,
            ) {
                Ok(repaired) => {
                    self.stats.fragments_repaired += repaired as u64;
                    FrameOutcome::Recovered {
                        repaired: repaired as u16,
                    }
                }
                Err(_) => FrameOutcome::Unreconstructable {
                    missing: missing.len() as u16,
                },
            }
        };

        let payload = if outcome.is_usable() {
            // Concatenate the data shards and trim the padding with the frame's own length.
            // The length comes from the header, which the AEAD authenticated, so a sender
            // cannot make us trim to a length it did not commit to.
            let mut bytes = Vec::with_capacity(frame.data_count as usize * 1400);
            for shard in frame.shards[..frame.data_count as usize].iter().flatten() {
                bytes.extend_from_slice(shard);
            }
            bytes.truncate(frame.frame_len as usize);
            Some(bytes)
        } else {
            // No payload is constructed at all. This is the invariant: a frame that could
            // not be rebuilt does not exist as bytes anywhere in this process.
            None
        };

        match outcome {
            FrameOutcome::Complete => self.stats.frames_complete += 1,
            FrameOutcome::Recovered { .. } => self.stats.frames_recovered += 1,
            FrameOutcome::Unreconstructable { .. } => self.stats.frames_unreconstructable += 1,
        }
        self.stats.payload_bytes_delivered += payload.as_ref().map_or(0, Vec::len) as u64;
        let _ = now;

        DeliveredFrame {
            frame_index: index,
            target_timestamp_us: frame.target_timestamp_us,
            released_at: now,
            outcome,
            is_keyframe: frame.is_keyframe,
            payload,
        }
    }

    fn evict_oldest(&mut self) {
        if let Some(&oldest) = self.partial.keys().next()
            && let Some(mut frame) = self.partial.remove(&oldest)
        {
            // Evicted rather than released: it counts as unreconstructable, because that
            // is exactly what it is, and pretending otherwise would hide loss.
            let at = frame.first_arrival;
            let _ = self.finish(oldest, &mut frame, at);
        }
    }
}

impl std::fmt::Debug for Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver")
            .field("policy", &self.policy)
            .field("keys", &self.keys)
            .field("in_flight", &self.partial.len())
            .field("stats", &self.stats)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        packetizer::{FrameMeta, Packetizer, ParityPolicy},
        wire::Flags,
    };

    const MTU: usize = 1400;
    const SHARD: usize = MTU - crate::wire::HEADER_LEN;

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn policy() -> ReleasePolicy {
        ReleasePolicy::new(1, Duration::from_millis(11))
    }

    /// No cover at all: release the moment a frame is complete. For the tests that are about
    /// *what* comes out rather than *when*.
    fn immediate_policy() -> ReleasePolicy {
        ReleasePolicy {
            straggler_delay: Duration::ZERO,
            repair_delay: Duration::ZERO,
            deadline: Duration::from_millis(11),
            jitter_frames: 0,
        }
    }

    fn receiver() -> Receiver {
        Receiver::new(policy(), None)
    }

    /// Send one frame through, dropping the fragment indices in `drop_set`.
    fn send(
        receiver: &mut Receiver,
        packetizer: &Packetizer,
        frame_index: u64,
        bytes: &[u8],
        drop_set: &[u16],
        now: Duration,
        seq: &mut u32,
    ) -> usize {
        let (layout, datagrams) = packetizer
            .fragment(
                FrameMeta {
                    frame_index,
                    target_timestamp_us: 11_111 * frame_index,
                    is_keyframe: false,
                    key_epoch: 0,
                },
                bytes,
                seq,
                None,
            )
            .unwrap();
        let mut dropped = 0;
        for (i, datagram) in datagrams.iter().enumerate() {
            if drop_set.contains(&(i as u16)) {
                dropped += 1;
                continue;
            }
            receiver.on_datagram(datagram, now);
        }
        assert_eq!(
            layout.data_count as usize + layout.parity_count as usize,
            datagrams.len()
        );
        dropped
    }

    #[test]
    fn a_complete_frame_arrives_intact() {
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let mut receiver = Receiver::new(immediate_policy(), None);
        let bytes = payload(SHARD * 3 + 5);
        let mut seq = 0;
        send(
            &mut receiver,
            &packetizer,
            1,
            &bytes,
            &[],
            Duration::ZERO,
            &mut seq,
        );
        // A second frame arrives behind it, so the release path is exercised with more than one
        // frame in flight.
        send(
            &mut receiver,
            &packetizer,
            2,
            &payload(100),
            &[],
            Duration::ZERO,
            &mut seq,
        );

        let released = receiver.release(Duration::from_millis(1));
        assert_eq!(released.len(), 2);
        assert_eq!(released[0].frame_index, 1);
        assert_eq!(released[0].outcome, FrameOutcome::Complete);
        assert_eq!(released[0].payload().unwrap(), &bytes[..]);
    }

    #[test]
    fn a_lost_data_fragment_is_repaired() {
        let packetizer = Packetizer::new(MTU, ParityPolicy::Fixed(2));
        let mut receiver = receiver();
        let bytes = payload(SHARD * 5);
        let mut seq = 0;
        // Drop two data shards.
        send(
            &mut receiver,
            &packetizer,
            1,
            &bytes,
            &[1, 3],
            Duration::ZERO,
            &mut seq,
        );
        let released = receiver.release(Duration::from_millis(50));
        assert_eq!(released.len(), 1);
        assert_eq!(
            released[0].outcome,
            FrameOutcome::Recovered { repaired: 2 },
            "two erasures with two parity shards must be repaired"
        );
        assert_eq!(
            released[0].payload().unwrap(),
            &bytes[..],
            "the repaired frame must be byte-identical to what was sent"
        );
        assert_eq!(receiver.stats().fragments_repaired, 2);
    }

    #[test]
    fn more_loss_than_parity_yields_a_frame_with_no_payload() {
        // The invariant, in one test: the receiver does not hand over a frame it could not
        // rebuild, and the caller has no way to display one.
        let packetizer = Packetizer::new(MTU, ParityPolicy::Fixed(1));
        let mut receiver = receiver();
        let bytes = payload(SHARD * 4);
        let mut seq = 0;
        send(
            &mut receiver,
            &packetizer,
            1,
            &bytes,
            &[0, 2, 3],
            Duration::ZERO,
            &mut seq,
        );

        let released = receiver.release(Duration::from_millis(50));
        assert_eq!(released.len(), 1);
        assert!(matches!(
            released[0].outcome,
            FrameOutcome::Unreconstructable { missing: 3 }
        ));
        assert!(released[0].payload().is_none());
        assert!(!released[0].is_displayable());
        assert_eq!(receiver.stats().frames_unreconstructable, 1);
        assert_eq!(receiver.stats().frames_deliverable(), 0);
        assert_eq!(receiver.stats().payload_bytes_delivered, 0);
    }

    #[test]
    fn every_released_frame_has_a_payload_if_and_only_if_it_is_usable() {
        // The property the type promises, checked over a mixed traffic pattern including
        // loss, reorder, duplicates and a late arrival.
        let packetizer = Packetizer::new(MTU, ParityPolicy::Fixed(2));
        let mut receiver = receiver();
        let mut seq = 0;
        let mut now = Duration::ZERO;

        for frame_index in 1..=20u64 {
            let bytes = payload(SHARD * 4 + (frame_index as usize % 3) * 100);
            let drops: &[u16] = match frame_index % 5 {
                0 => &[1, 2, 3], // beyond repair
                1 => &[0],       // repairable
                2 => &[0, 1],    // repairable
                _ => &[],        // clean
            };
            send(
                &mut receiver,
                &packetizer,
                frame_index,
                &bytes,
                drops,
                now,
                &mut seq,
            );
            now += Duration::from_millis(11);
            for frame in receiver.release(now) {
                assert_eq!(
                    frame.payload().is_some(),
                    frame.outcome.is_usable(),
                    "frame {}: payload presence disagrees with the outcome",
                    frame.frame_index
                );
            }
        }
        // Drain the tail.
        now += Duration::from_secs(1);
        for frame in receiver.release(now) {
            assert_eq!(frame.payload().is_some(), frame.outcome.is_usable());
        }
        assert!(
            receiver.stats().frames_unreconstructable > 0,
            "the test should exercise loss"
        );
        assert!(receiver.stats().frames_complete > 0);
        assert!(receiver.stats().frames_recovered > 0);
    }

    #[test]
    fn a_keyframe_is_recognised_even_when_its_first_shard_is_lost() {
        // The bits travel on *every* shard. If only the first carried it, the client would lose
        // the one signal that releases its hold (ADR-0011) exactly when the link was bad enough
        // to drop that shard — which is when it needs it. So: drop the first one and check that a
        // later shard still tells the truth.
        let packetizer = Packetizer::new(MTU, ParityPolicy::Ratio { fraction: 0.2 });
        let mut receiver = receiver();
        let bytes = payload(SHARD * 4);
        let mut seq = 0;
        let (_, datagrams) = packetizer
            .fragment(
                FrameMeta {
                    frame_index: 1,
                    target_timestamp_us: 1,
                    is_keyframe: true,
                    key_epoch: 0,
                },
                &bytes,
                &mut seq,
                None,
            )
            .unwrap();
        assert!(datagrams.len() > 2, "need several shards for this test");

        for datagram in datagrams.iter().skip(1) {
            let _ = receiver.on_datagram(datagram, Duration::ZERO);
        }

        // Past the release deadline: `jitter_frames` is 1, so a lone frame is released when its
        // deadline passes rather than when a later one completes.
        let released = receiver.release(Duration::from_millis(50));
        assert_eq!(released.len(), 1, "the frame did not complete");
        assert!(
            released[0].is_keyframe,
            "a keyframe whose first shard was lost stopped being a keyframe"
        );
        assert!(released[0].payload().is_some());
    }

    #[test]
    fn an_ordinary_frame_does_not_claim_to_be_a_keyframe() {
        // The flag must mean something. A false positive would release a hold onto a frame whose
        // reference chain is still broken.
        let mut receiver = receiver();
        let mut seq = 0;
        let bytes = payload(SHARD * 2);
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        // `send` returns how many datagrams it was told to drop; nothing is dropped here.
        let dropped = send(
            &mut receiver,
            &packetizer,
            1,
            &bytes,
            &[],
            Duration::ZERO,
            &mut seq,
        );
        assert_eq!(dropped, 0);
        let released = receiver.release(Duration::from_millis(50));
        assert_eq!(released.len(), 1);
        assert!(!released[0].is_keyframe);
    }

    #[test]
    fn duplicates_are_counted_not_stored() {
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let mut receiver = receiver();
        let bytes = payload(SHARD * 2);
        let mut seq = 0;
        let (_, datagrams) = packetizer
            .fragment(
                FrameMeta {
                    frame_index: 1,
                    target_timestamp_us: 1,
                    is_keyframe: true,
                    key_epoch: 0,
                },
                &bytes,
                &mut seq,
                None,
            )
            .unwrap();

        for datagram in &datagrams {
            assert!(matches!(
                receiver.on_datagram(datagram, Duration::ZERO),
                RecvEvent::Accepted { .. }
            ));
        }
        // The replay: same bytes, same frame, same fragment.
        assert_eq!(
            receiver.on_datagram(&datagrams[0], Duration::ZERO),
            RecvEvent::Duplicate
        );
        assert_eq!(receiver.stats().datagrams_duplicate, 1);
        assert_eq!(receiver.stats().datagrams_received, 3);
    }

    #[test]
    fn a_retransmitted_fragment_fills_the_gap() {
        let packetizer = Packetizer::new(MTU, ParityPolicy::Fixed(1));
        let mut receiver = receiver();
        let bytes = payload(SHARD * 3);
        let mut seq = 0;
        let (_, datagrams) = packetizer
            .fragment(
                FrameMeta {
                    frame_index: 7,
                    target_timestamp_us: 1,
                    is_keyframe: true,
                    key_epoch: 0,
                },
                &bytes,
                &mut seq,
                None,
            )
            .unwrap();

        // Fragment 1 is lost, the rest arrive; parity cannot cover it (only one shard lost,
        // parity *can* cover it) — so instead send it as a retransmit and check the frame
        // completes from the retransmit rather than from parity.
        for (i, datagram) in datagrams.iter().enumerate() {
            if i == 1 {
                continue;
            }
            receiver.on_datagram(datagram, Duration::ZERO);
        }
        assert_eq!(
            receiver.nack(7),
            vec![1],
            "the receiver must name the missing fragment"
        );
        assert_eq!(receiver.incomplete_frames(), vec![7]);

        // Now the retransmit: same shard, flagged.
        let mut retransmit = datagrams[1].clone();
        let mut header = FragmentHeader::decode(&retransmit).unwrap().0;
        header.flags = header.flags.with_retransmit();
        retransmit[..crate::wire::HEADER_LEN].copy_from_slice(&header.encode());
        assert!(matches!(
            receiver.on_datagram(&retransmit, Duration::from_millis(1)),
            RecvEvent::Accepted {
                completed: true,
                ..
            }
        ));
        assert_eq!(receiver.stats().datagrams_retransmit, 1);
        assert_eq!(receiver.nack(7), Vec::<u16>::new());
    }

    #[test]
    fn a_late_datagram_for_a_released_frame_is_counted_and_dropped() {
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let mut receiver = receiver();
        let mut seq = 0;

        send(
            &mut receiver,
            &packetizer,
            1,
            &payload(100),
            &[],
            Duration::ZERO,
            &mut seq,
        );
        // Let the deadline pass so frame 1 is gone.
        let _ = receiver.release(Duration::from_millis(100));
        assert_eq!(receiver.in_flight(), 0);

        // A straggler for frame 1 now arrives. It must not create a new partial frame.
        let (_, datagrams) = packetizer
            .fragment(
                FrameMeta {
                    frame_index: 1,
                    target_timestamp_us: 1,
                    is_keyframe: true,
                    key_epoch: 0,
                },
                &payload(100),
                &mut seq,
                None,
            )
            .unwrap();
        assert_eq!(
            receiver.on_datagram(&datagrams[0], Duration::from_millis(200)),
            RecvEvent::Late
        );
        assert_eq!(receiver.stats().datagrams_late, 1);
        assert_eq!(receiver.in_flight(), 0);
    }

    /// The cover is a **time**, it is only paid by frames that have a **hole**, and a frame without
    /// one is released the instant it is whole.
    ///
    /// All three parts are corrections the bench forced. The cover used to be a count of frames —
    /// "release frame 1 once frame 3 has completed" — which held *every* frame for a whole display
    /// period regardless of the link, so every frame of every scenario came out at the end of its
    /// own period. That is the definition of late.
    #[test]
    fn the_reorder_cover_is_a_time_paid_only_by_frames_with_a_hole() {
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        // 22 ms of cover, expressed as two frame intervals.
        let mut receiver = Receiver::new(ReleasePolicy::new(2, Duration::from_millis(11)), None);
        let mut seq = 0;

        // Two frames, each missing its first shard — `payload` takes bytes, so this is three
        // shards with one of them gone.
        send(
            &mut receiver,
            &packetizer,
            1,
            &payload(SHARD * 3),
            &[0],
            Duration::ZERO,
            &mut seq,
        );
        send(
            &mut receiver,
            &packetizer,
            2,
            &payload(SHARD * 3),
            &[0],
            Duration::ZERO,
            &mut seq,
        );

        assert!(
            receiver.release(Duration::from_millis(21)).is_empty(),
            "the cover had not elapsed, so waiting is exactly the point"
        );

        let released = receiver.release(Duration::from_millis(22));
        assert_eq!(released.len(), 2, "the cover elapsed, so both are due");
        assert_eq!(
            released[0].frame_index, 1,
            "release must stay in frame order"
        );
        assert_eq!(released[1].frame_index, 2);
    }

    /// The improvement, stated as a test: a whole frame pays **no** latency for order cover.
    #[test]
    fn a_frame_with_no_hole_is_released_at_once() {
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let mut receiver = Receiver::new(ReleasePolicy::new(4, Duration::from_millis(11)), None);
        let mut seq = 0;

        send(
            &mut receiver,
            &packetizer,
            1,
            &payload(SHARD),
            &[],
            Duration::ZERO,
            &mut seq,
        );

        let released = receiver.release(Duration::ZERO);
        assert_eq!(
            released.len(),
            1,
            "a complete frame has nothing to wait for, and holding it is latency the picture pays"
        );
        assert_eq!(released[0].outcome, FrameOutcome::Complete);
    }

    /// And the reason the cover exists at all: a hole held a little longer is a hole the repair can
    /// still fill. This is the property that makes NACKing possible — with the cover at the jitter
    /// alone, the frame was gone before a request could be made and the whole path was dead code.
    #[test]
    fn a_hole_is_still_open_while_the_repair_round_trip_is_in_flight() {
        let packetizer = Packetizer::new(MTU, ParityPolicy::Fixed(1));
        let mut receiver = Receiver::new(
            ReleasePolicy::for_link(
                Duration::from_micros(300),
                Duration::ZERO,
                Duration::from_millis(4),
                Duration::from_millis(11),
            ),
            None,
        );
        let bytes = payload(SHARD * 3);
        let mut seq = 0;

        let (_, datagrams) = packetizer
            .fragment(
                FrameMeta {
                    frame_index: 1,
                    target_timestamp_us: 0,
                    is_keyframe: true,
                    key_epoch: 0,
                },
                &bytes,
                &mut seq,
                None,
            )
            .unwrap();

        // **Two** data shards are lost, against one parity shard: the code cannot rebuild this, so
        // the frame is one of the ones that genuinely has to wait for a re-send. (Losing one shard
        // would be repairable, and a repairable frame is released as soon as the stragglers stop
        // arriving rather than paying for a round trip.)
        for (index, datagram) in datagrams.iter().enumerate() {
            if index == 1 || index == 2 {
                continue;
            }
            receiver.on_datagram(datagram, Duration::ZERO);
        }

        assert_eq!(
            receiver.nack(1),
            vec![1, 2],
            "the client must be able to name what is missing while the frame is still held"
        );
        assert!(
            receiver.release(Duration::from_millis(2)).is_empty(),
            "the FEC cannot rebuild this, so waiting for the repair is the only way to keep the \
             frame — releasing now throws away a frame that a round trip would have saved"
        );

        // The repair comes back.
        receiver.on_datagram(&datagrams[1], Duration::from_millis(3));
        receiver.on_datagram(&datagrams[2], Duration::from_millis(3));
        let released = receiver.release(Duration::from_millis(3));
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].outcome, FrameOutcome::Complete);
    }

    /// Ordering is not a coincidence here: a hole holds everything behind it, so a later frame can
    /// never overtake an earlier one.
    #[test]
    fn a_held_hole_holds_the_frames_behind_it() {
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let mut receiver = Receiver::new(ReleasePolicy::new(4, Duration::from_millis(11)), None);
        let mut seq = 0;

        send(
            &mut receiver,
            &packetizer,
            1,
            &payload(SHARD * 3),
            &[0],
            Duration::ZERO,
            &mut seq,
        );
        send(
            &mut receiver,
            &packetizer,
            2,
            &payload(SHARD),
            &[],
            Duration::ZERO,
            &mut seq,
        );

        assert!(
            receiver.release(Duration::from_millis(5)).is_empty(),
            "frame 2 is whole, but it may not overtake the hole in frame 1"
        );

        let released = receiver.release(Duration::from_millis(44));
        assert_eq!(released.len(), 2);
        assert_eq!(released[0].frame_index, 1);
        assert_eq!(released[1].frame_index, 2);
    }

    #[test]
    fn the_deadline_bounds_memory() {
        // A frame whose fragments simply stop arriving must be released by the deadline, or
        // the receiver would hold it forever.
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let mut receiver = receiver();
        let bytes = payload(SHARD * 5);
        let mut seq = 0;
        // Only the first fragment of frame 1 arrives, ever.
        let (_, datagrams) = packetizer
            .fragment(
                FrameMeta {
                    frame_index: 1,
                    target_timestamp_us: 1,
                    is_keyframe: true,
                    key_epoch: 0,
                },
                &bytes,
                &mut seq,
                None,
            )
            .unwrap();
        receiver.on_datagram(&datagrams[0], Duration::ZERO);
        assert_eq!(receiver.in_flight(), 1);

        let released = receiver.release(Duration::from_millis(100));
        assert_eq!(released.len(), 1);
        assert!(matches!(
            released[0].outcome,
            FrameOutcome::Unreconstructable { missing: 4 }
        ));
        assert_eq!(receiver.in_flight(), 0);
    }

    /// A mid-stream key rotation, through the real receiver.
    ///
    /// The point is not that two keys decrypt two frames — it is that a rotation the client did not
    /// negotiate, arriving as a header field, works: the frame sealed under the *next* epoch is
    /// accepted without any signalling, and the straggler sealed under the previous one still is too.
    /// That is what makes rotation something the sender can decide alone, which is what the reference
    /// client treats it as.
    #[test]
    fn a_frame_sealed_under_the_next_epoch_is_accepted_without_any_signalling() {
        use crate::crypto::{KEY_LEN, KeySchedule, MediaKeys};

        // Rotate every 10 frames, so frame 5 is epoch 0 and frame 15 is epoch 1.
        let schedule = KeySchedule::with_frames_per_key([5u8; KEY_LEN], 10);
        let packetizer = Packetizer::new(MTU, ParityPolicy::Fixed(1));
        let bytes = payload(SHARD);
        let mut receiver = Receiver::new(policy(), Some(MediaKeys::rotating(schedule.clone())));
        let mut seq = 0;

        for frame_index in [5u64, 15] {
            let epoch = schedule.epoch_for(frame_index);
            assert_eq!(epoch, if frame_index == 5 { 0 } else { 1 });

            let cipher = schedule.cipher_for(epoch);
            let (_, datagrams) = packetizer
                .fragment(
                    FrameMeta {
                        frame_index,
                        target_timestamp_us: frame_index * 11_111,
                        is_keyframe: frame_index == 5,
                        key_epoch: epoch,
                    },
                    &bytes,
                    &mut seq,
                    Some(&cipher),
                )
                .unwrap();

            for datagram in &datagrams {
                assert!(
                    !matches!(
                        receiver.on_datagram(datagram, Duration::ZERO),
                        RecvEvent::Rejected(_)
                    ),
                    "frame {frame_index} (epoch {epoch}) was rejected by the receiver"
                );
            }
        }

        let released = receiver.release(Duration::from_millis(50));
        assert_eq!(released.len(), 2, "a rotation lost a frame");
        assert!(
            released.iter().all(|frame| frame.is_displayable()),
            "a frame that rotated underneath the receiver came out unusable"
        );
        assert_eq!(receiver.stats().datagrams_unauthenticated, 0);
    }

    #[test]
    fn an_unauthenticated_datagram_changes_nothing() {
        use crate::crypto::{KEY_LEN, MediaCipher};

        let cipher = MediaCipher::new(&[3u8; KEY_LEN]);
        let packetizer = Packetizer::new(MTU, ParityPolicy::Fixed(1));
        let mut receiver = Receiver::new(policy(), Some(MediaCipher::new(&[3u8; KEY_LEN]).into()));
        let bytes = payload(SHARD * 2);
        let mut seq = 0;
        let (_, datagrams) = packetizer
            .fragment(
                FrameMeta {
                    frame_index: 1,
                    target_timestamp_us: 1,
                    is_keyframe: true,
                    key_epoch: 0,
                },
                &bytes,
                &mut seq,
                Some(&cipher),
            )
            .unwrap();

        // A tampered datagram is rejected and does not occupy a slot.
        let mut tampered = datagrams[0].clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(matches!(
            receiver.on_datagram(&tampered, Duration::ZERO),
            RecvEvent::Rejected(RecvError::Crypto(_))
        ));
        assert_eq!(receiver.stats().datagrams_unauthenticated, 1);
        assert_eq!(
            receiver.in_flight(),
            0,
            "a rejected datagram must not create state"
        );

        // The genuine article still completes the frame, and the payload is the plaintext.
        for datagram in datagrams.iter().take(2) {
            receiver.on_datagram(datagram, Duration::ZERO);
        }
        let released = receiver.release(Duration::from_millis(100));
        let frame = released
            .iter()
            .find(|f| f.outcome.is_usable())
            .expect("the honest datagrams should rebuild the frame");
        assert_eq!(frame.payload().unwrap(), &bytes[..]);
    }

    #[test]
    fn a_frame_whose_shape_changes_mid_flight_is_rejected() {
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let mut receiver = receiver();
        let bytes = payload(SHARD * 3);
        let mut seq = 0;
        let (_, datagrams) = packetizer
            .fragment(
                FrameMeta {
                    frame_index: 1,
                    target_timestamp_us: 1,
                    is_keyframe: true,
                    key_epoch: 0,
                },
                &bytes,
                &mut seq,
                None,
            )
            .unwrap();

        receiver.on_datagram(&datagrams[0], Duration::ZERO);
        // Forge a second fragment claiming a different data_count.
        let mut forged = datagrams[1].clone();
        let mut header = FragmentHeader::decode(&forged).unwrap().0;
        header.data_count += 1;
        forged[..crate::wire::HEADER_LEN].copy_from_slice(&header.encode());
        assert!(matches!(
            receiver.on_datagram(&forged, Duration::ZERO),
            RecvEvent::Rejected(RecvError::Wire(_))
        ));
    }

    #[test]
    fn malformed_datagrams_are_counted() {
        let mut receiver = receiver();
        assert!(matches!(
            receiver.on_datagram(&[0u8; 4], Duration::ZERO),
            RecvEvent::Rejected(RecvError::Wire(_))
        ));
        assert_eq!(receiver.stats().datagrams_malformed, 1);
        assert_eq!(receiver.stats().datagrams_received, 1);
    }

    #[test]
    fn the_field_parity_flag_survives_the_round_trip() {
        // Guards the packetizer/receiver agreement on which shards are repair shards.
        let packetizer = Packetizer::new(MTU, ParityPolicy::Fixed(2));
        let mut seq = 0;
        let (layout, datagrams) = packetizer
            .fragment(
                FrameMeta {
                    frame_index: 1,
                    target_timestamp_us: 1,
                    is_keyframe: true,
                    key_epoch: 0,
                },
                &payload(SHARD * 3),
                &mut seq,
                None,
            )
            .unwrap();
        for (i, datagram) in datagrams.iter().enumerate() {
            let (header, _) = FragmentHeader::decode(datagram).unwrap();
            assert_eq!(
                header.flags.0 & Flags::PARITY != 0,
                layout.is_parity(i as u16),
                "datagram {i}"
            );
        }
    }

    #[test]
    fn the_catch_all_statistic_is_the_one_that_was_missing() {
        // 41 % of frames were unusable and the telemetry said zero errors, because nothing
        // counted "a frame I could not rebuild". This is that counter.
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let mut receiver = receiver();
        let mut seq = 0;
        let mut now = Duration::ZERO;
        for frame_index in 1..=10u64 {
            let drops: &[u16] = if frame_index % 2 == 0 { &[1] } else { &[] };
            send(
                &mut receiver,
                &packetizer,
                frame_index,
                &payload(SHARD * 3),
                drops,
                now,
                &mut seq,
            );
            now += Duration::from_millis(11);
            let _ = receiver.release(now);
        }
        now += Duration::from_secs(1);
        let _ = receiver.release(now);

        let stats = receiver.stats();
        assert_eq!(stats.frames_unreconstructable, 5);
        assert_eq!(stats.frames_deliverable(), 5);
        assert!((stats.unreconstructable_fraction() - 0.5).abs() < 1e-9);
    }
}
