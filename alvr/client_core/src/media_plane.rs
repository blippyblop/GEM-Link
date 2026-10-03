//! The client's video receive path, on the media plane.
//!
//! `x-transport` has had a complete receiver since ADR-0013 — FEC, derived-nonce AEAD, a release
//! policy, `nack()` — and **nothing on the client used it**. The live path ran `alvr_sockets`, and
//! that is where the frames were being lost: a fixed 10-buffer pool that discarded datagrams
//! silently when it ran dry (doc 50 §A10). This module is the client half of the wiring.
//!
//! It puts the four pieces that must agree into one type, because they are only correct together
//! and every one of them has been wrong on its own at some point in this project:
//!
//! | piece | what it decides | what happens when it is missing |
//! |---|---|---|
//! | `x_transport::Receiver` | can this frame be rebuilt at all | a hole is displayed as a picture |
//! | `x_transport::TrustGate` | may this frame be shown | a broken reference chain is shown as grey |
//! | [`crate::stall::StuckDetector`] | how long to wait, and when to stop waiting | a hold never ends: a black screen |
//! | this module's counters | whether any of the above happened | two days lost to a silent discard |
//!
//! ## Where the datagrams come from
//!
//! [`DatagramSource`], so that the same receive path can be driven by a socket, by a recorded
//! stream, or by a lossy wrapper around either. The reference client does exactly this — its
//! transport is file-backed behind `-f <FILENAME>` and it can inject loss (`Fail this % of packets,
//! incoming on this side`) — and it is the reason its behaviour could be characterised without a
//! network. Ours could not, which is how a 1.5 % client-side discard came to look like a network
//! problem for two days.

use std::time::{Duration, Instant};

use x_transport::{
    DeliveredFrame, FrameTrust, RecvEvent, Receiver, ReleasePolicy, UntrustedReason,
};

use crate::stall::{StuckAction, StuckDetector};

/// One read from wherever datagrams come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceEvent {
    /// `out` holds one datagram.
    Datagram,
    /// Nothing was ready within the timeout.
    Timeout,
    /// The source is finished. A replay that has run out, or a socket that will never deliver.
    Closed,
}

/// Where datagrams come from. Implemented for a socket, a recording, and a lossy wrapper.
pub trait DatagramSource {
    fn recv(&mut self, out: &mut Vec<u8>, timeout: Duration) -> SourceEvent;
}

/// A recorded stream, replayed. Deterministic by construction: no clock, no network, no loss
/// except the loss that is in the recording.
#[derive(Debug, Clone, Default)]
pub struct ReplaySource {
    datagrams: Vec<Vec<u8>>,
    next: usize,
}

impl ReplaySource {
    pub fn new(datagrams: Vec<Vec<u8>>) -> Self {
        Self { datagrams, next: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.datagrams.len().saturating_sub(self.next)
    }
}

impl DatagramSource for ReplaySource {
    fn recv(&mut self, out: &mut Vec<u8>, _timeout: Duration) -> SourceEvent {
        let Some(datagram) = self.datagrams.get(self.next) else {
            return SourceEvent::Closed;
        };
        self.next += 1;
        out.clear();
        out.extend_from_slice(datagram);
        SourceEvent::Datagram
    }
}

/// Drops a deterministic fraction of datagrams.
///
/// The reference client has this built into the shipping binary; we did not, which meant the only
/// way to see how the client behaves on a bad link was to be on a bad link. A seeded generator
/// makes a run reproducible: the same seed and the same input give the same losses, so a
/// regression is a diff rather than a shrug.
#[derive(Debug)]
pub struct LossySource<S> {
    inner: S,
    /// xorshift64*. No dependency, and reproducible across machines — which a hash map's order is
    /// not, and neither is a system RNG.
    state: u64,
    /// Loss rate in parts per thousand, so 15 is 1.5 % — the figure this client was actually
    /// discarding before anyone could see it.
    permille: u32,
    consumed: u64,
    dropped: u64,
}

impl<S: DatagramSource> LossySource<S> {
    pub fn new(inner: S, seed: u64, permille: u32) -> Self {
        Self {
            inner,
            state: seed | 1,
            permille: permille.min(1000),
            consumed: 0,
            dropped: 0,
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    fn next_random(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

impl<S: DatagramSource> DatagramSource for LossySource<S> {
    fn recv(&mut self, out: &mut Vec<u8>, timeout: Duration) -> SourceEvent {
        loop {
            match self.inner.recv(out, timeout) {
                SourceEvent::Datagram => {
                    self.consumed += 1;
                    if (self.next_random() % 1000) < self.permille as u64 {
                        self.dropped += 1;
                        continue;
                    }
                    return SourceEvent::Datagram;
                }
                other => return other,
            }
        }
    }
}

/// What the receive path decided. One of these per frame, plus the ladder's escalations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaneEvent {
    /// Rebuilt and trustworthy: show it.
    Present { frame_index: u64, len: usize },
    /// Not trustworthy: **hold the last good frame**. ADR-0011 — this is not a failure to show a
    /// frame, it is the permitted response to one that cannot be trusted.
    Held {
        frame_index: u64,
        reason: UntrustedReason,
    },
    /// Ask the sender to re-send these fragments of this frame. The repair that costs one round
    /// trip instead of one frame.
    Nack {
        frame_index: u64,
        fragments: Vec<u16>,
    },
    /// No progress for the ask threshold: ask the sender for a keyframe.
    AskForKeyframe { stalled_for: Duration },
    /// No progress for the reset threshold. Asking has failed.
    Reset { stalled_for: Duration },
}

/// Counters, because a receive path whose behaviour is only visible in the picture cannot be
/// debugged — which is how the silent discard survived two days of investigation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaneStats {
    pub datagrams_received: u64,
    /// Datagrams a source refused to deliver (loss injection, or a recording's own gaps).
    pub datagrams_dropped_by_source: u64,
    /// Datagrams the receiver rejected: malformed, unauthenticated, or late.
    pub datagrams_rejected: u64,
    pub frames_presented: u64,
    /// Frames the display rule refused. Every released frame is either presented or held, so
    /// `frames_presented + frames_held` is the number of frames that reached the display path.
    pub frames_held: u64,
    /// Frames the transport could not rebuild. **A subset of `frames_held`**, not a third bucket:
    /// a frame with no payload is held by definition. Counted separately because they mean
    /// different things — `held` is the gate working, `abandoned` is the link being worse than the
    /// code rate.
    pub frames_abandoned: u64,
    pub frames_repaired: u64,
    pub nacks_sent: u64,
    pub keyframe_requests: u64,
    pub resets: u64,
}

impl PlaneStats {
    pub fn summary(&self) -> String {
        format!(
            "video plane: {} datagrams in ({} dropped by source, {} rejected), {} frames presented, \
             {} held, {} abandoned, {} repaired by FEC, {} nack(s), {} keyframe request(s), {} reset(s)",
            self.datagrams_received,
            self.datagrams_dropped_by_source,
            self.datagrams_rejected,
            self.frames_presented,
            self.frames_held,
            self.frames_abandoned,
            self.frames_repaired,
            self.nacks_sent,
            self.keyframe_requests,
            self.resets,
        )
    }
}

/// The client's video receive path.
pub struct VideoPlane {
    receiver: Receiver,
    trust: x_transport::TrustGate,
    stall: StuckDetector,
    started: Instant,
    stats: PlaneStats,
}

impl VideoPlane {
    pub fn new(policy: ReleasePolicy, now: Instant) -> Self {
        Self {
            receiver: Receiver::new(policy, None),
            trust: x_transport::TrustGate::new(),
            stall: StuckDetector::new(now),
            started: now,
            stats: PlaneStats::default(),
        }
    }

    pub fn stats(&self) -> &PlaneStats {
        &self.stats
    }

    /// Milliseconds since this plane was created — the receiver's time base.
    fn clock(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }

    /// Pull datagrams until the source has nothing more, and hand each to the receiver.
    ///
    /// Returns whether the source is still open. A `Closed` source is not an error: a replay ends.
    ///
    /// `budget` bounds how long to keep pulling; **zero means drain**, which is what a replay or a
    /// test wants. A live socket needs no budget either — `recv` reports `Timeout` when it is
    /// empty and the loop stops there — so the budget exists only for a source that never yields.
    pub fn pump(
        &mut self,
        source: &mut impl DatagramSource,
        now: Instant,
        budget: Duration,
    ) -> bool {
        let clock = self.clock(now);
        let mut buffer = Vec::with_capacity(1500);
        let deadline = if budget.is_zero() {
            None
        } else {
            Some(Instant::now() + budget)
        };

        loop {
            match source.recv(&mut buffer, Duration::ZERO) {
                SourceEvent::Datagram => {
                    self.stats.datagrams_received += 1;
                    match self.receiver.on_datagram(&buffer, clock) {
                        RecvEvent::Rejected(_) => self.stats.datagrams_rejected += 1,
                        // Duplicates and late arrivals are counted by the receiver's own stats;
                        // neither is a reason to stop.
                        RecvEvent::Accepted { .. } | RecvEvent::Duplicate | RecvEvent::Late => {}
                    }
                }
                SourceEvent::Timeout => break,
                SourceEvent::Closed => return false,
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
        }
        true
    }

    /// Release whatever is ready and decide, for each frame, whether it may be shown.
    pub fn release(&mut self, now: Instant) -> Vec<PlaneEvent> {
        let clock = self.clock(now);
        let mut events = Vec::new();
        let mut presented_this_round = false;

        for frame in self.receiver.release(clock) {
            let event = self.classify(&frame, now);
            presented_this_round |= matches!(event, PlaneEvent::Present { .. });
            events.push(event);
        }

        if presented_this_round {
            self.stall.progress(now);
        } else {
            match self.stall.poll(now) {
                StuckAction::AskForKeyframe => {
                    self.stats.keyframe_requests += 1;
                    events.push(PlaneEvent::AskForKeyframe {
                        stalled_for: self.stall.stalled_for(now),
                    });
                }
                StuckAction::Reset => {
                    self.stats.resets += 1;
                    events.push(PlaneEvent::Reset {
                        stalled_for: self.stall.stalled_for(now),
                    });
                }
                StuckAction::Progress => {}
            }
        }

        // Ask for the fragments of anything still incomplete. The receiver knows which ones are
        // missing; this is the round trip that saves a frame instead of a keyframe.
        for frame_index in self.receiver.incomplete_frames() {
            let fragments = self.receiver.nack(frame_index);
            if !fragments.is_empty() {
                self.stats.nacks_sent += fragments.len() as u64;
                events.push(PlaneEvent::Nack {
                    frame_index,
                    fragments,
                });
            }
        }

        events
    }

    /// Apply the display rule to one released frame.
    fn classify(&mut self, frame: &DeliveredFrame, now: Instant) -> PlaneEvent {
        let usable = frame.is_displayable();
        if !usable {
            self.stats.frames_abandoned += 1;
        } else if matches!(frame.outcome, x_transport::FrameOutcome::Recovered { .. }) {
            self.stats.frames_repaired += 1;
        }

        // A frame with a payload has no hole in it, repaired or not — the transport only hands out
        // bytes it could rebuild. A frame without one is the only case that may not be shown.
        match self
            .trust
            .may_present(frame.frame_index, frame.is_keyframe, !usable)
        {
            FrameTrust::Trusted => {
                let len = frame.payload().map_or(0, <[u8]>::len);
                self.stats.frames_presented += 1;
                self.stall.progress(now);
                PlaneEvent::Present {
                    frame_index: frame.frame_index,
                    len,
                }
            }
            FrameTrust::Untrusted { reason, .. } => {
                self.stats.frames_held += 1;
                PlaneEvent::Held {
                    frame_index: frame.frame_index,
                    reason,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x_transport::{ParityPolicy, Packetizer};

    const MTU: usize = 1400;
    const SHARD: usize = MTU - x_transport::HEADER_LEN;

    fn payload(shards: usize) -> Vec<u8> {
        (0..shards * SHARD).map(|i| (i % 251) as u8).collect()
    }

    /// Packetise `count` frames at `parity` overhead, marking every `keyframe_every`.
    fn stream(
        count: u64,
        shards: usize,
        parity: ParityPolicy,
        keyframe_every: u64,
    ) -> Vec<Vec<u8>> {
        let packetizer = Packetizer::new(MTU, parity);
        let bytes = payload(shards);
        let mut seq = 0;
        let mut out = Vec::new();
        for frame_index in 1..=count {
            let (_, datagrams) = packetizer
                .fragment(
                    x_transport::FrameMeta {
                        frame_index,
                        target_timestamp_us: frame_index * 11_111,
                        is_keyframe: frame_index == 1 || frame_index.is_multiple_of(keyframe_every),
                    },
                    &bytes,
                    &mut seq,
                    None,
                )
                .unwrap();
            out.extend(datagrams);
        }
        out
    }

    /// Release policy: no jitter window, a tight deadline — a test should not wait.
    fn policy() -> ReleasePolicy {
        ReleasePolicy::new(0, Duration::from_millis(5))
    }

    #[test]
    fn a_clean_stream_presents_every_frame() {
        let mut plane = VideoPlane::new(policy(), Instant::now());
        let mut source = ReplaySource::new(stream(10, 4, ParityPolicy::Off, 30));

        let now = Instant::now();
        // `false` here means the replay ran out — which is how a recording ends, not a failure.
        let still_open = plane.pump(&mut source, now, Duration::ZERO);
        assert!(!still_open, "a finished replay should report itself closed");
        let events = plane.release(now + Duration::from_millis(50));

        let presented = events
            .iter()
            .filter(|e| matches!(e, PlaneEvent::Present { .. }))
            .count();
        assert_eq!(presented, 10, "a clean stream lost frames: {events:?}");
        assert_eq!(plane.stats().frames_held, 0);
        assert_eq!(plane.stats().frames_abandoned, 0);
        assert_eq!(plane.stats().frames_presented, 10);
    }

    #[test]
    fn loss_inside_the_fec_budget_is_repaired_and_never_held() {
        // The whole point of the media plane: a datagram is lost, the frame is not.
        let mut plane = VideoPlane::new(policy(), Instant::now());
        let mut source = LossySource::new(
            ReplaySource::new(stream(20, 8, ParityPolicy::Ratio { fraction: 0.25 }, 30)),
            0x5EED,
            20, // 2 % of datagrams
        );

        let mut presented = 0;
        let now = Instant::now();
        plane.pump(&mut source, now, Duration::ZERO);
        for event in plane.release(now + Duration::from_millis(50)) {
            match event {
                PlaneEvent::Present { .. } => presented += 1,
                PlaneEvent::Held { reason, .. } => {
                    panic!("held a frame at 2 % loss with 25 % parity: {reason:?}")
                }
                _ => {}
            }
        }

        assert!(source.dropped() > 0, "the test injected no loss");
        assert_eq!(
            presented, 20,
            "{} frames lost to {} dropped datagrams despite 25 % parity",
            plane.stats().frames_abandoned,
            source.dropped()
        );
    }

    #[test]
    fn loss_beyond_the_budget_is_held_and_never_presented() {
        // ADR-0011. With no parity, a dropped datagram loses a frame; a frame that could not be
        // rebuilt must not reach the display path at all.
        let mut plane = VideoPlane::new(policy(), Instant::now());
        let mut source = LossySource::new(
            ReplaySource::new(stream(40, 8, ParityPolicy::Off, 30)),
            0xC0FFEE,
            60, // 6 % of datagrams: whole frames will be lost
        );

        let now = Instant::now();
        plane.pump(&mut source, now, Duration::ZERO);
        for event in plane.release(now + Duration::from_millis(50)) {
            assert!(
                !matches!(event, PlaneEvent::Present { len: 0, .. }),
                "presented a frame with no payload"
            );
        }
        assert!(
            plane.stats().frames_abandoned > 0,
            "the test did not actually lose a frame"
        );
        assert_eq!(
            plane.stats().frames_presented + plane.stats().frames_held,
            40,
            "a released frame was neither presented nor held"
        );
        assert!(
            plane.stats().frames_abandoned <= plane.stats().frames_held,
            "a frame with no payload was somehow not held"
        );
        // A held frame is the invariant working; a presented unreconstructable one would be the
        // bug. `frames_abandoned` counts the latter, and it must equal the frames that had no
        // payload — which is exactly what `Held` records. So: nothing abandoned-then-presented.
        assert!(
            plane.stats().frames_held > 0,
            "no hold was recorded for the frames that could not be rebuilt"
        );
    }

    #[test]
    fn a_keyframe_after_a_loss_releases_the_hold() {
        // The recovery path end to end, including the wire bit that makes it possible.
        let mut plane = VideoPlane::new(policy(), Instant::now());
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let bytes = payload(4);
        let mut seq = 0;
        let mut datagrams = Vec::new();
        for frame_index in 1..=40u64 {
            let (_, d) = packetizer
                .fragment(
                    x_transport::FrameMeta {
                        frame_index,
                        target_timestamp_us: frame_index * 11_111,
                        // Keyframes at 1 and 30, so a hold can begin after 1 and end at 30.
                        is_keyframe: frame_index == 1 || frame_index == 30,
                    },
                    &bytes,
                    &mut seq,
                    None,
                )
                .unwrap();
            // Drop the whole of frame 5.
            if frame_index != 5 {
                datagrams.extend(d);
            }
        }

        let mut source = ReplaySource::new(datagrams);
        let now = Instant::now();
        plane.pump(&mut source, now, Duration::ZERO);
        let events = plane.release(now + Duration::from_millis(50));

        // Frame 5 never arrives at all, so it is released as unreconstructable and held. Every
        // frame after it inherits the broken chain and is held too — until the keyframe.
        let held = events
            .iter()
            .filter(|e| matches!(e, PlaneEvent::Held { .. }))
            .count();
        assert!(
            held >= 20,
            "the broken chain did not hold the frames behind it ({held} held): {events:?}"
        );

        // Frame 30 is a keyframe and must come out as presentable, ending the hold.
        let keyframe_presented = events
            .iter()
            .any(|e| matches!(e, PlaneEvent::Present { frame_index: 30, .. }));
        assert!(
            keyframe_presented,
            "the keyframe did not release the hold — the wire bit or the gate is wrong: {events:?}"
        );

        // Nothing between 6 and 29 may be shown: the chain is broken until the keyframe at 30.
        // Frames from 30 on are legitimately presentable, which is the other half of the claim.
        for event in &events {
            if let PlaneEvent::Present { frame_index, .. } = event {
                assert!(
                    !(6..30).contains(frame_index),
                    "frame {frame_index} was presented while the chain was broken"
                );
            }
        }
    }

    #[test]
    fn loss_injection_is_deterministic() {
        // Reproducibility is the whole reason for a seeded source: a regression has to be a diff.
        let datagrams = stream(20, 4, ParityPolicy::Off, 30);
        let count = |seed: u64| {
            let mut source = LossySource::new(ReplaySource::new(datagrams.clone()), seed, 33);
            let mut out = Vec::new();
            while let SourceEvent::Datagram = source.recv(&mut out, Duration::ZERO) {}
            source.dropped()
        };
        assert_eq!(count(0xA11CE), count(0xA11CE), "the same seed differed");
        assert!(count(0xA11CE) > 0, "loss injection injected nothing");
        // Different seeds are allowed to agree by chance, but not to be identical always.
        assert!(
            count(0xA11CE) != count(0xB0B) || count(0xB0B) != count(0xC0DE),
            "every seed dropped the same datagrams — the generator is not being used"
        );
    }

    #[test]
    fn a_silent_link_asks_for_a_keyframe_then_resets() {
        // The ladder, on the receive path this time rather than the decoder's.
        let mut plane = VideoPlane::new(policy(), Instant::now());
        let now = Instant::now();

        assert!(plane.release(now).is_empty());
        assert_eq!(plane.stats().keyframe_requests, 0);

        let asked = plane.release(now + crate::stall::ASK_FOR_KEYFRAME_AFTER);
        assert!(
            matches!(asked.as_slice(), [PlaneEvent::AskForKeyframe { .. }]),
            "no keyframe request after the ask threshold: {asked:?}"
        );
        assert_eq!(plane.stats().keyframe_requests, 1);

        assert!(plane.release(now + Duration::from_millis(500)).is_empty());
        assert_eq!(plane.stats().keyframe_requests, 1, "it asked twice");

        let reset = plane.release(now + crate::stall::HARD_RESET_AFTER);
        assert!(
            matches!(reset.as_slice(), [PlaneEvent::Reset { .. }]),
            "no reset after the reset threshold: {reset:?}"
        );
        assert_eq!(plane.stats().resets, 1);
    }

    #[test]
    fn incomplete_frames_are_nacked_rather_than_left_to_die() {
        // The reference client re-requests; so do we. A frame that is one datagram short and gets
        // it back is a frame that never needed a keyframe.
        let mut plane = VideoPlane::new(policy(), Instant::now());
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let bytes = payload(8);
        let mut seq = 0;
        let (_, datagrams) = packetizer
            .fragment(
                x_transport::FrameMeta {
                    frame_index: 1,
                    target_timestamp_us: 1,
                    is_keyframe: true,
                },
                &bytes,
                &mut seq,
                None,
            )
            .unwrap();

        // Deliver all but one data shard.
        let mut source = ReplaySource::new(
            datagrams
                .iter()
                .skip(1) // the first data shard is the one that never arrives
                .cloned()
                .collect(),
        );
        let now = Instant::now();
        plane.pump(&mut source, now, Duration::ZERO);

        // Nothing is released yet — the frame is incomplete — but it must be named.
        let events = plane.release(now + Duration::from_millis(1));
        let nacked = events.iter().find_map(|e| match e {
            PlaneEvent::Nack { fragments, .. } => Some(fragments.clone()),
            _ => None,
        });
        assert_eq!(
            nacked,
            Some(vec![0]),
            "the missing fragment was not named for re-request: {events:?}"
        );
    }

    #[test]
    fn the_summary_names_the_numbers_that_matter() {
        let stats = PlaneStats {
            datagrams_received: 1000,
            datagrams_dropped_by_source: 15,
            frames_presented: 300,
            frames_abandoned: 4,
            ..Default::default()
        };
        let line = stats.summary();
        for needle in ["1000 datagrams in", "15 dropped by source", "4 abandoned"] {
            assert!(line.contains(needle), "{line:?} does not mention {needle:?}");
        }
    }
}
