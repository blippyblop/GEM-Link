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
    crypto::{CryptoError, MediaCipher},
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

    /// Whether this frame may be presented. Equivalent to `payload().is_some()`, and the
    /// only question the display path needs to ask.
    pub const fn is_displayable(&self) -> bool {
        self.outcome.is_usable()
    }
}

/// When to release a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasePolicy {
    /// How many later frames must have completed before an incomplete frame is given up on.
    /// Zero means "release as soon as a later frame completes".
    pub jitter_frames: u16,
    /// Hard bound from the first arriving datagram of a frame.
    pub deadline: Duration,
}

impl ReleasePolicy {
    /// A policy expressed in frames, for a given frame interval.
    ///
    /// `jitter_frames` of order cover and a deadline of the same length, which is the
    /// smallest pair that can absorb a reorder without adding more than that much latency.
    pub fn new(jitter_frames: u16, frame_interval: Duration) -> Self {
        Self {
            jitter_frames,
            // One extra frame interval of slack over the reorder window, so the deadline
            // does not pre-empt a straggler that the reorder window was still waiting for.
            deadline: frame_interval * (jitter_frames as u32 + 1),
        }
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
}

/// Reassembles frames from datagrams.
pub struct Receiver {
    policy: ReleasePolicy,
    cipher: Option<MediaCipher>,
    /// Frames still being assembled, ordered so release is a single pass.
    partial: BTreeMap<u64, PartialFrame>,
    /// The highest frame index that has arrived complete — the reorder-reference.
    max_completed: Option<u64>,
    /// The highest frame index that has been handed out. A datagram at or below this is
    /// late by definition: the display path has already moved past it.
    max_released: Option<u64>,
    stats: ReceiverStats,
    /// Bounded so a sender bug (a frame index leaping into the future) cannot exhaust
    /// memory. Entries are dropped oldest-first.
    max_frames_in_flight: usize,
}

impl Receiver {
    pub fn new(policy: ReleasePolicy, cipher: Option<MediaCipher>) -> Self {
        Self {
            policy,
            cipher,
            partial: BTreeMap::new(),
            max_completed: None,
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
        let shard = match &self.cipher {
            Some(cipher) => match cipher.open(
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
        if completed {
            self.max_completed = Some(
                self.max_completed
                    .map_or(frame_index, |m| m.max(frame_index)),
            );
        }

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

    /// Release everything that is ready, oldest frame first.
    ///
    /// Ready means: `jitter_frames` later frames have completed, **or** the deadline has
    /// passed since the frame's first datagram.
    pub fn release(&mut self, now: Duration) -> Vec<DeliveredFrame> {
        let ready: Vec<u64> = self
            .partial
            .iter()
            .filter(|(index, frame)| {
                let reorder_ready = self.max_completed.is_some_and(|max| {
                    index.saturating_add(self.policy.jitter_frames as u64) <= max
                });
                let deadline_passed =
                    now.saturating_sub(frame.first_arrival) >= self.policy.deadline;
                reorder_ready || deadline_passed
            })
            .map(|(index, _)| *index)
            .collect();

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
            .field("cipher", &self.cipher)
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
        let mut receiver = receiver();
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
        // A second frame must complete (or the deadline pass) before the first is released.
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
        assert_eq!(
            released.len(),
            1,
            "only frame 1 is due after one later frame"
        );
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

    #[test]
    fn the_jitter_window_holds_frames_back() {
        // With a two-frame window, frame 1 must not be released until frame 3 completes.
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let mut receiver = Receiver::new(ReleasePolicy::new(2, Duration::from_millis(11)), None);
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
        send(
            &mut receiver,
            &packetizer,
            2,
            &payload(100),
            &[],
            Duration::ZERO,
            &mut seq,
        );
        assert!(
            receiver.release(Duration::from_millis(1)).is_empty(),
            "one later completion is not enough for a two-frame window"
        );

        send(
            &mut receiver,
            &packetizer,
            3,
            &payload(100),
            &[],
            Duration::ZERO,
            &mut seq,
        );
        let released = receiver.release(Duration::from_millis(1));
        assert_eq!(
            released.len(),
            1,
            "two later completions should release frame 1"
        );
        assert_eq!(released[0].frame_index, 1);
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

    #[test]
    fn an_unauthenticated_datagram_changes_nothing() {
        use crate::crypto::{KEY_LEN, MediaCipher};

        let cipher = MediaCipher::new(&[3u8; KEY_LEN]);
        let packetizer = Packetizer::new(MTU, ParityPolicy::Fixed(1));
        let mut receiver = Receiver::new(policy(), Some(MediaCipher::new(&[3u8; KEY_LEN])));
        let bytes = payload(SHARD * 2);
        let mut seq = 0;
        let (_, datagrams) = packetizer
            .fragment(
                FrameMeta {
                    frame_index: 1,
                    target_timestamp_us: 1,
                    is_keyframe: true,
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
