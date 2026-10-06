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
//! * **Slot pressure** ([`ReleasePolicy::late_hold_frames`]): a frame that could not be rebuilt is
//!   **kept**, not released, and the stream holds behind it — because a frame whose shards are still
//!   arriving is not a frame that was lost, and the display has not moved past it yet. It is given up
//!   on when `late_hold_frames` newer frames are waiting behind it, i.e. when the stream has genuinely
//!   moved on. **Not on a timer**: a timer fired on the frames whose repair was still in flight, which
//!   is the defect this rule replaced (22 000 datagrams over one run arrived after the frame they
//!   belonged to had already been released, and each of those frames became a hole).
//!
//! An important asymmetry makes the hold cheap when it matters most: when the holed frame is the
//! *newest* one, nothing is waiting behind it, so holding it costs no ordering latency at all — the
//! display is waiting for that frame anyway. The cost only appears when the stream has moved on, and
//! that is exactly when the hold ends.
//!
//! What bounds memory is not a clock but [`Receiver::with_max_in_flight`], which evicts the oldest
//! frame, plus the pressure rule above.

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
    /// The frame index the sender encoded this frame against, or `0` for "not stated". The display
    /// rule turns it into the one question that matters — *is the picture this frame depends on one
    /// I have already decoded?* — and with it a hole stops poisoning the frames behind it.
    pub reference_frame: u64,
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
    /// How many newer frames may pile up behind an unrebuilt frame before it is given up on.
    ///
    /// This is the whole of the abandonment rule. Zero restores the old behaviour (give up at the
    /// repair window, whatever is behind it); two or three is a grace period measured in the only
    /// unit that means anything — **the stream moving on**. It is deliberately not a time: a timer
    /// fires on frames whose repair is in flight, and the measurement is that this is most of them.
    pub late_hold_frames: u16,
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
            late_hold_frames: 0,
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
            // Two frames of grace. At 90 Hz that is 22 ms of extra patience on a frame whose repair
            // is in flight, paid only when the stream has already moved on without it.
            late_hold_frames: 2,
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

    /// The longest this policy will hold a frame it could not rebuild, given the link's pacing.
    ///
    /// This is the *sender's* side of the same fact: a repair that arrives after it is a datagram the
    /// client has stopped assembling. It is the repair window plus the grace
    /// [`Self::late_hold_frames`] buys, which is where the old `deadline` field's meaning went.
    pub fn hold_ceiling(&self, frame_interval: Duration) -> Duration {
        self.repair_delay + frame_interval * self.late_hold_frames as u32
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
    /// For the frames that could **not** be rebuilt: how many data shards were missing, and how
    /// many parity shards the frame carried, summed.
    ///
    /// The pair is the whole diagnosis. A frame that fails with *fewer* erasures than parity is a
    /// bug in the code — the repair was available and was not taken. A frame that fails with more
    /// is the link, and no amount of code would have saved it.
    pub unreconstructable_erasures: u64,
    pub unreconstructable_parity: u64,
    /// The worst single frame.
    pub max_erasures: u16,
    /// For the frames that could not be rebuilt: how long the frame waited before it was declared
    /// failed, how many of its shards (data **and** parity) had actually been read by then, and how
    /// many it had altogether.
    ///
    /// `unreconstructable_parity` is the parity the frame **declared**, which is what a repair
    /// needs to be *possible*; these are what it actually **had**. When the declared number is
    /// sufficient and the repair still fails, the difference between the two is the answer, and it
    /// is not a fact about the code.
    pub failed_waited_us: u64,
    pub failed_shards_present: u64,
    pub failed_shards_total: u64,
    /// Frames that could not be rebuilt when their repair window closed and were **kept** rather
    /// than dropped, and how many of those were later assembled from shards that arrived while they
    /// waited.
    ///
    /// These two numbers are the whole case for holding: `frames_rescued_late` frames are frames the
    /// old rule created holes from. A rescue rate near zero would mean the hold costs latency for
    /// nothing, and that would be the honest reason to put the timer back.
    pub frames_held_late: u64,
    pub frames_rescued_late: u64,
    /// Frames given up on because newer frames were waiting behind them — the stream moved on. The
    /// rest of `frames_unreconstructable` were given up on for the same reason inside the ordinary
    /// repair window, or evicted at the in-flight cap.
    pub frames_abandoned_pressure: u64,
    /// The **drain spread** of a usable frame — how long the client took to read the frame after
    /// its first shard appeared — as an EWMA and a worst case, in microseconds.
    ///
    /// This is the client's own queueing delay, and it is the signal that says the sender is
    /// outrunning the receiver. Everything else the client can see is ambiguous: a shard that has
    /// not arrived and a shard that is still queued are the same fact locally, which is why three
    /// attempts to fix the repair path made it worse instead.
    pub queue_delay_us: u64,
    pub queue_delay_max_us: u64,
    /// **Datagrams read per second**, as a maximum over the reporting window, and the raw counters it
    /// comes from.
    ///
    /// The sensor that works when nothing else does. Every other number here is a property of a frame
    /// — the drain spread needs one that completed, the missing share needs one that was released —
    /// and a client that cannot read a single keyframe produces neither. This counts datagrams off the
    /// socket, which is a fact even when every frame in the window was thrown away.
    ///
    /// A **lower bound** on capacity, because a reader that is never offered more than it can take
    /// reads exactly what it is offered: the sender is expected to overshoot occasionally so that this
    /// finds a ceiling rather than a description of its own pacing.
    pub read_per_sec: u32,
    /// Datagrams and microseconds since the last sensor window closed, so the window's rate can be
    /// taken and the maximum kept.
    pub window_datagrams: u64,
    pub window_started_us: u64,
    /// How many windows have closed since the caller last took the rate. Zero means the number it is
    /// holding is the previous window's, not a fresh one.
    pub windows_closed_since_take: u32,
    /// How much of each frame's **declared** shards actually arrived, in tenths of a percent missing,
    /// as an EWMA over every frame that finished — usable or not.
    ///
    /// The measurement the queue delay cannot make, and the live run is why it exists. The drain
    /// spread is only measurable on frames that completed, and a client that is losing half of every
    /// frame completes *only small frames* — so it reports a comfortable 21 ms of queueing delay
    /// while receiving a third of the stream. This counts what the frame said it was against what
    /// arrived, which no amount of reading faster can flatter, and it is the same number on a frame
    /// that was rebuilt and on one that was thrown away.
    pub missing_permille: u32,
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
    /// When the most recent shard of this frame arrived.
    ///
    /// The gap to `first_arrival` is the **drain spread**: how long the client took to *read* the
    /// frame after the first piece of it appeared. It is the client's own queueing delay, measured
    /// where it actually happens, and it is the signal the sender needs — a receiver that reads at
    /// wire speed has a spread of about one burst, and one that is behind has a spread that grows
    /// with its backlog. Nothing else the client can see distinguishes "this shard is lost" from
    /// "this shard is still queued behind me", which is why every repair-timing change made things
    /// worse.
    last_arrival: Duration,
    target_timestamp_us: u64,
    /// The frame index this frame's picture was encoded against, from the wire header; `0` means
    /// the sender did not say. Carried out with the frame so the display rule can ask whether the
    /// reference is one this client has decoded.
    reference_frame: u64,
    /// Learned from the first shard that arrives, whichever kind it is.
    is_keyframe: bool,
    frame_len: u32,
    data_count: u16,
    parity_count: u16,
    shards: Vec<Option<Vec<u8>>>,
    received: usize,
    /// When this frame's repair window closed with the frame still unrebuilt and it was **kept**
    /// anyway — see [`ReleasePolicy::late_hold_frames`]. `None` until then, and set once, so the
    /// hold is counted once per frame rather than once per release pass.
    held_late_at: Option<Duration>,
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
        if parity_per_block == 0 {
            return false;
        }

        for block in 0..blocks {
            let mut missing = 0usize;
            for (index, slot) in self.shards[..data_count].iter().enumerate() {
                if slot.is_none() && fec::block_of(index, blocks) == block {
                    missing += 1;
                }
            }
            if missing == 0 {
                continue;
            }

            // **The parity that has ARRIVED, not the parity that was declared.**
            //
            // This compared missing data shards against `parity_count` — the number the frame said it
            // carried — and never asked whether any of it was here. A frame with five data shards
            // missing and five declared parity shards is then "repairable" at its straggler window, is
            // released, fails to decode, and becomes a hole: exactly the shape the live runs kept
            // reporting, `4.4 erasures vs 5.0 parity on average`, where the parity was declared and
            // still in flight. It is also why the release window and the repair window could never be
            // tuned into agreement — the decision to stop waiting was made on evidence that did not
            // exist.
            let mut present = 0usize;
            for position in 0..parity_per_block {
                let wire_index = data_count + position * blocks + block;
                if self.shards.get(wire_index).is_some_and(Option::is_some) {
                    present += 1;
                }
            }
            if missing > present {
                return false;
            }
        }
        true
    }
}

/// The read rate assumed before the client has measured one. Low on purpose: it makes the first hold
/// generous, and a hold that is too long costs latency while one that is too short costs the frame.
const MIN_READ_RATE_PER_SEC: u64 = 50;

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

    /// The client's own queueing delay — the drain spread of a usable frame, as an EWMA in
    /// microseconds. See [`ReceiverStats::queue_delay_us`].
    pub fn queue_delay_us(&self) -> u64 {
        self.stats.queue_delay_us
    }

    /// The reading-rate sensor: close the window if it is due, and return the maximum rate seen.
    ///
    /// Called on **every** datagram, before anything is decided about it, because the whole point is
    /// to count what was read rather than what was usable. `window` is how long a window lasts; the
    /// caller reports the maximum to the sender and the sender plans against it.
    pub fn observe_read_rate(&mut self, now: Duration, window: Duration) -> u32 {
        let started = self.stats.window_started_us;
        let now_us = now.as_micros() as u64;
        if now_us.saturating_sub(started) < window.as_micros() as u64 {
            return self.stats.read_per_sec;
        }
        let elapsed_us = now_us.saturating_sub(started).max(1);
        let rate = (self.stats.window_datagrams as f64 * 1_000_000.0 / elapsed_us as f64) as u32;
        // The maximum, not the mean: a window in which the sender offered more than usual is the only
        // one that says anything about capacity, and a mean over a mostly-idle link reads as a slow
        // client.
        self.stats.read_per_sec = self.stats.read_per_sec.max(rate);
        self.stats.window_datagrams = 0;
        self.stats.window_started_us = now_us;
        self.stats.windows_closed_since_take += 1;
        self.stats.read_per_sec
    }

    /// Close the sensor window and start a fresh maximum, for a caller that has reported the current
    /// one. The maximum is per reporting interval, so a burst is not averaged away by the minutes
    /// around it.
    pub fn take_read_rate(&mut self) -> u32 {
        // **A window that produced no reading is not a reading of zero.** The sensor closes on
        // datagrams, so a client being offered one frame every 300 ms does not close a window inside a
        // 250 ms report interval at all — and reporting the zero it never measured is how a run that
        // was presenting 325 frames told the sender it was reading nothing. The last measurement is
        // still the best evidence, decayed rather than discarded.
        if self.stats.windows_closed_since_take == 0 {
            self.stats.read_per_sec = self.stats.read_per_sec * 3 / 4;
            return self.stats.read_per_sec;
        }
        self.stats.windows_closed_since_take = 0;
        std::mem::take(&mut self.stats.read_per_sec)
    }

    /// How long this frame needs to be read at the rate the client has been able to sustain.
    ///
    /// **The window a frame's own size implies.** A fixed release window is a statement about a frame
    /// of average size, and the frame that matters — the one that must complete for anything to work
    /// — is the biggest one there is: measured, a 77 KB keyframe is about sixty-four datagrams, and at
    /// 285 datagrams/s that is 220 ms of pure reading against windows of 6, 12 and 66 ms. So the
    /// hold is derived per frame from what it declares: shards divided by the read rate, plus half
    /// again as margin for the stragglers and the round trip the *repair* will take.
    pub fn hold_for_frame(&self, declared_shards: usize) -> Duration {
        if self.stats.read_per_sec == 0 {
            // No reading has been measured yet, so there is nothing to derive a window from. The
            // policy's own window is then the only honest answer, and it is what the caller falls
            // back to. (A default rate here would be a guess dressed as a measurement, and the guess
            // would be wrong in whichever direction the link is unlike the one it was written for.)
            return Duration::ZERO;
        }
        let read = (self.stats.read_per_sec as u64).max(MIN_READ_RATE_PER_SEC);
        let reading_us = declared_shards as u64 * 1_000_000 / read;
        Duration::from_micros(reading_us + reading_us / 2 + 5_000)
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
        // Counted before it is judged: the sensor is of what was *read*, and a datagram that is late,
        // duplicated, malformed or foreign still cost the time it took to read.
        self.stats.window_datagrams += 1;

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
                last_arrival: now,
                target_timestamp_us: header.target_timestamp_us,
                reference_frame: header.reference_frame,
                is_keyframe: header.flags.is_keyframe(),
                frame_len: header.frame_len,
                data_count: header.data_count,
                parity_count: header.parity_count,
                shards: vec![None; header.data_count as usize + header.parity_count as usize],
                received: 0,
                held_late_at: None,
            });

        // Every shard carries the keyframe bit, so a frame whose first shard was lost still
        // learns the truth from any later one.
        entry.is_keyframe |= header.flags.is_keyframe();

        // A datagram that disagrees with the frame's established shape is a bug or an
        // attack; the frame it belongs to is not trustworthy, so the whole frame is.
        if entry.data_count != header.data_count
            || entry.parity_count != header.parity_count
            || entry.frame_len != header.frame_len
            || entry.reference_frame != header.reference_frame
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
        entry.last_arrival = now;

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

    /// Release everything that is ready, oldest frame first, **stopping at the first frame that is
    /// not** — so what comes out is always in frame order.
    ///
    /// Three ways a frame becomes ready:
    ///
    /// 1. **It is complete.** Nothing is missing, so there is nothing to wait for. This is the
    ///    change that removed a whole frame interval of latency from every frame: the cover exists
    ///    to give a *hole* time to be filled by a straggler or a repair, and a frame without one has
    ///    no use for it. The old rule held every frame until a later one completed, which on a clean
    ///    stream released each frame at the very end of its own period — late by definition.
    /// 2. Its straggler window has elapsed **and** the FEC can rebuild it from what has arrived —
    ///    the data is here, so stop waiting. See [`ReleasePolicy::straggler_delay`].
    /// 3. It is holed, its repair window has closed, and **the stream has moved on without it**:
    ///    [`ReleasePolicy::late_hold_frames`] newer frames are waiting behind it. This is the only
    ///    way a frame is given up on, and it is why a frame whose repair is in flight is no longer
    ///    released a moment before that repair lands. See the module docs.
    ///
    /// The stopping is what keeps ordering: frame `n + 1` may not overtake a held frame `n`, or the
    /// display path would show the newer picture and then the older one.
    pub fn release(&mut self, now: Duration) -> Vec<DeliveredFrame> {
        // Which frames are ready, and which unrebuilt frames are entering (or continuing) the late
        // hold. Computed in one pass over an immutable borrow, then applied.
        let mut ready: Vec<u64> = Vec::new();
        let mut newly_held: Vec<u64> = Vec::new();

        for (index, frame) in self.partial.iter() {
            let waited = now.saturating_sub(frame.first_arrival);
            let stragglers_over = waited >= self.policy.straggler_delay;

            // How long this frame may have, from its own size and the rate the client has shown it can
            // read at. Never less than the policy's window: a frame small enough to read instantly
            // still gets the round trip a repair needs.
            let hold = self
                .policy
                .repair_delay
                .max(self.hold_for_frame(frame.data_count as usize + frame.parity_count as usize));
            let hold_over = waited >= hold;

            // Complete: nothing to wait for.
            // FEC can rebuild it: the data is here, so stop waiting for the stragglers.
            // Otherwise: hold it, unless the stream has moved on without it.
            let due = frame.has_all_data()
                || (stragglers_over && frame.fec_repairable())
                || (hold_over
                    && self.newer_frames_behind(*index) >= self.policy.late_hold_frames as usize);

            if due {
                ready.push(*index);
            } else {
                if !frame.has_all_data() && waited >= self.policy.repair_delay {
                    if frame.held_late_at.is_none() {
                        newly_held.push(*index);
                    }
                }
                // Holding a hole holds everything behind it — for as long as the rule above says.
                break;
            }
        }

        for index in newly_held {
            if let Some(frame) = self.partial.get_mut(&index) {
                frame.held_late_at = Some(now);
                self.stats.frames_held_late += 1;
            }
        }

        let mut out = Vec::with_capacity(ready.len());
        for index in ready {
            let Some(mut frame) = self.partial.remove(&index) else {
                continue;
            };
            let abandoned_late = frame.held_late_at.is_some() && !frame.has_all_data();
            if abandoned_late {
                // Held past its repair window and then given up on because the stream moved on.
                self.stats.frames_abandoned_pressure += 1;
            }
            self.max_released = Some(self.max_released.map_or(index, |r| r.max(index)));
            out.push(self.finish(index, &mut frame, now));
        }
        out
    }

    /// How many frames newer than `index` are waiting in the reassembly buffer.
    ///
    /// The stream has moved on when this is non-zero: a newer frame's first shard has arrived, so the
    /// sender is past `index` and the display is being held for it.
    fn newer_frames_behind(&self, index: u64) -> usize {
        self.partial.range((index + 1)..).count()
    }

    fn finish(&mut self, index: u64, frame: &mut PartialFrame, now: Duration) -> DeliveredFrame {
        let missing = frame.missing_data();

        // What the frame declared against what arrived — **before** the FEC is consulted, because
        // the question is what the client read, not what it could rebuild from. Counted for every
        // finished frame, including the ones that could not be rebuilt: those are the frames the
        // drain is actually failing on.
        {
            let total = frame.data_count as u64 + frame.parity_count as u64;
            if total > 0 {
                let present = frame.shards.iter().filter(|s| s.is_some()).count() as u64;
                let permille = ((total - present) * 1000 / total) as u32;
                // A slow EWMA: this is a property of the link over seconds, not of a frame.
                let ewma = &mut self.stats.missing_permille;
                *ewma = if *ewma == 0 {
                    permille
                } else {
                    (*ewma * 7 + permille) / 8
                };
            }
        }

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

        // The client's own queueing delay, measured on the frame the display path is about to see.
        //
        // **Only frames that completed on their own.** A frame that was held late and rescued is a
        // frame whose *repair* took a round trip: its spread measures the repair path, not the drain,
        // and folding the two together is how a run where the drain was 3 ms reported 139 ms of
        // queueing delay and drove the sender's ladder to its floor for no reason. A frame that was
        // abandoned says nothing at all. Between the two, an EWMA over frames that arrived when they
        // were supposed to is the honest measure of how far behind the reader is.
        if outcome.is_usable() && frame.held_late_at.is_none() {
            let spread = frame
                .last_arrival
                .saturating_sub(frame.first_arrival)
                .as_micros() as u64;
            let ewma = &mut self.stats.queue_delay_us;
            *ewma = if *ewma == 0 {
                spread
            } else {
                *ewma - *ewma / 8 + spread / 8
            };
            if spread > self.stats.queue_delay_max_us {
                self.stats.queue_delay_max_us = spread;
            }
        }

        match outcome {
            FrameOutcome::Complete => {
                self.stats.frames_complete += 1;
                if frame.held_late_at.is_some() {
                    self.stats.frames_rescued_late += 1;
                }
            }
            FrameOutcome::Recovered { .. } => {
                self.stats.frames_recovered += 1;
                if frame.held_late_at.is_some() {
                    self.stats.frames_rescued_late += 1;
                }
            }
            FrameOutcome::Unreconstructable { .. } => {
                self.stats.frames_unreconstructable += 1;
                // The erasure count against the parity that was *there*. A frame that could not be
                // rebuilt with fewer erasures than parity is a bug in this code; one that could not
                // be rebuilt with more is the link. Without both numbers the two are the same fact.
                let erasures = missing.len();
                self.stats.unreconstructable_erasures += erasures as u64;
                self.stats.unreconstructable_parity += frame.parity_count as u64;
                if erasures as u16 > self.stats.max_erasures {
                    self.stats.max_erasures = erasures as u16;
                }
                // What the frame actually had, and how long it was given to get it.
                self.stats.failed_waited_us +=
                    now.saturating_sub(frame.first_arrival).as_micros() as u64;
                self.stats.failed_shards_present +=
                    frame.shards.iter().filter(|s| s.is_some()).count() as u64;
                self.stats.failed_shards_total += frame.shards.len() as u64;
            }
        }
        self.stats.payload_bytes_delivered += payload.as_ref().map_or(0, Vec::len) as u64;
        let _ = now;

        DeliveredFrame {
            frame_index: index,
            target_timestamp_us: frame.target_timestamp_us,
            released_at: now,
            outcome,
            is_keyframe: frame.is_keyframe,
            reference_frame: frame.reference_frame,
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
            late_hold_frames: 0,
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
                    reference_frame: 0,
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
    fn parity_that_has_not_arrived_is_not_parity_the_frame_can_be_repaired_with() {
        // **The bug the live rig kept reporting and nothing caught.** Two data shards are missing and
        // two parity shards were declared — but neither parity shard has arrived. The old rule called
        // that repairable, released the frame at its straggler window, failed to decode, and turned it
        // into a hole: `4.4 erasures vs 5.0 parity`, where the parity was still in flight.
        let packetizer = Packetizer::new(MTU, ParityPolicy::Fixed(2));
        let bytes = payload(SHARD * 5);
        let mut first = receiver();
        let mut seq = 0;
        // Two data shards and both parity shards are missing. Repairable: two erasures, two parity.
        send(
            &mut first,
            &packetizer,
            1,
            &bytes,
            &[1, 3, 5, 6],
            Duration::ZERO,
            &mut seq,
        );
        // *Not* repairable in fact — the parity is not here — so the straggler window must not release
        // it as though it were.
        let released = first.release(Duration::from_millis(50));
        assert!(
            !released.iter().any(|f| f.is_displayable()),
            "a frame with two erasures and no parity in hand was released as repairable: {released:?}"
        );

        // And when the parity does arrive, it is repairable and repaired.
        let mut fresh = receiver();
        let mut seq = 0;
        send(
            &mut fresh,
            &packetizer,
            1,
            &bytes,
            &[1, 3],
            Duration::ZERO,
            &mut seq,
        );
        let released = fresh.release(Duration::from_millis(50));
        assert_eq!(
            released[0].outcome,
            FrameOutcome::Recovered { repaired: 2 },
            "two erasures with two parity shards present must be repaired"
        );
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
                    reference_frame: 0,
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
                    reference_frame: 0,
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
                    reference_frame: 0,
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
                    reference_frame: 0,
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
                    reference_frame: 0,
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
    fn a_hole_with_nothing_behind_it_is_kept_rather_than_thrown_away() {
        // The rule that replaced the timer. A frame whose shards are still arriving is not a lost
        // frame, and when it is the newest frame there is nothing behind it — so holding it costs no
        // ordering latency at all. It is given up on when the stream moves on without it, never
        // because a clock said so.
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let mut receiver = Receiver::new(
            ReleasePolicy {
                straggler_delay: Duration::from_millis(2),
                repair_delay: Duration::from_millis(4),
                late_hold_frames: 2,
                jitter_frames: 0,
            },
            None,
        );
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

        // Long past every window the old policy had, and still nothing has been given up on.
        let released = receiver.release(Duration::from_millis(500));
        assert!(
            released.is_empty(),
            "a hole in the newest frame was released without a repair even being in flight: \
             {released:?}"
        );
        assert_eq!(receiver.stats().frames_held_late, 1);
        assert_eq!(
            receiver.in_flight(),
            1,
            "the reassembly state is still alive"
        );

        // And then the shard that was missing arrives, 500 ms late. Under the old rule this frame was
        // already gone: the datagram would have been counted `late` and dropped, and the frame
        // released with no payload.
        let mut seq = 0;
        let (_, datagrams) = packetizer
            .fragment(
                FrameMeta {
                    frame_index: 1,
                    target_timestamp_us: 11_111,
                    is_keyframe: false,
                    key_epoch: 0,
                    reference_frame: 0,
                },
                &payload(SHARD * 3),
                &mut seq,
                None,
            )
            .unwrap();
        receiver.on_datagram(&datagrams[0], Duration::from_millis(500));

        let released = receiver.release(Duration::from_millis(501));
        assert_eq!(released.len(), 1);
        assert!(
            released[0].is_displayable(),
            "the frame was complete as soon as its last shard arrived"
        );
        assert_eq!(released[0].payload().unwrap().len(), SHARD * 3);
        assert_eq!(receiver.stats().frames_rescued_late, 1);
        assert_eq!(receiver.stats().frames_unreconstructable, 0);
    }

    #[test]
    fn a_held_hole_is_abandoned_when_the_stream_moves_on_without_it() {
        // The other half: the grace is finite, and the unit it is measured in is the stream's own
        // progress. Two newer frames waiting behind the hole is the point at which waiting for frame
        // 1 costs frame 2 and frame 3, and that is worse than the hole.
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let mut receiver = Receiver::new(
            ReleasePolicy {
                straggler_delay: Duration::from_millis(2),
                repair_delay: Duration::from_millis(4),
                late_hold_frames: 2,
                jitter_frames: 0,
            },
            None,
        );
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

        // One newer frame: not enough. The hold stands.
        send(
            &mut receiver,
            &packetizer,
            2,
            &payload(SHARD),
            &[],
            Duration::ZERO,
            &mut seq,
        );
        let released = receiver.release(Duration::from_millis(10));
        assert!(
            released.is_empty(),
            "one frame behind a hole is not pressure"
        );

        // Two newer frames: the stream has moved on, so the hole goes.
        send(
            &mut receiver,
            &packetizer,
            3,
            &payload(SHARD),
            &[],
            Duration::ZERO,
            &mut seq,
        );
        let released = receiver.release(Duration::from_millis(11));
        assert_eq!(released.len(), 3, "the hole and the two frames behind it");
        assert_eq!(released[0].frame_index, 1);
        assert!(
            !released[0].is_displayable(),
            "the hole itself is still a hole — it is released, not invented"
        );
        assert!(released[1].is_displayable());
        assert!(released[2].is_displayable());
        assert_eq!(receiver.stats().frames_abandoned_pressure, 1);
        assert_eq!(receiver.stats().frames_held_late, 1);
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
                    reference_frame: 0,
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
                        reference_frame: 0,
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
                    reference_frame: 0,
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
                    reference_frame: 0,
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
                    reference_frame: 0,
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
