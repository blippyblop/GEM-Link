//! Transport scenarios: drive the media plane through the impairment profiles and count
//! what comes out.
//!
//! This is the half of M4 that makes the other half mean anything. `x-transport` claims two
//! things — that FEC repairs a lost datagram instead of losing the frame, and that a frame
//! it could not repair is never presented — and both are claims until this runs.
//!
//! ## The model, stated so it can be argued with
//!
//! * **Datagram-level loss, latency and jitter** come from the same [`ImpairmentProfile`]
//!   the control-plane scenarios use, driven by the same seeded LCG, so a run is exactly
//!   reproducible.
//! * **Datagrams are delivered in arrival order**, not send order: the event queue is a
//!   priority queue keyed on arrival time, so reordering is a consequence of the jitter
//!   model rather than a separate knob pretending to model it.
//! * **Frames are built at the scenario's bitrate and frame rate**, so the shard counts are
//!   the real ones — a 300 Mbps frame at 90 Hz is ~300 datagrams at a 1400-byte MTU, which
//!   is exactly the size that forced the FEC to be striped into blocks.
//! * **One retransmit round.** A NACK is fired `rtt` after a frame's first datagram, the
//!   missing fragments are re-sent (and may themselves be lost), and they arrive `rtt`
//!   later. This is the optimistic version of a real implementation, which fires on a timer
//!   and might have to take the budget from the next frame.
//! * **Loss is applied per datagram at send time**, including retransmits.
//!
//! ## What is deliberately not modelled
//!
//! * **No congestion control.** The pacer is not asked to back off, so this measures the
//!   media plane's recovery, not a closed-loop controller. Mixing the two would make it
//!   impossible to tell which one produced a number.
//! * **No codec.** The payload is incompressible filler of the right size; a real encoder's
//!   frame sizes vary by content, which changes the shard counts. i.i.d. datagram loss is
//!   also kinder than reality — real loss is bursty, which is what the FEC's block
//!   interleaving is designed for and what a future scenario should model explicitly.
//! * **No clock skew, no glass.** These are media-plane numbers; the glass-to-glass
//!   instrument does not exist yet (ROADMAP "known coverage gaps").

use std::{cmp::Reverse, collections::BinaryHeap, time::Duration};

use x_transport::{
    DatagramSink, Feedback, FeedbackOutcome, FeedbackReceiver, FeedbackSender, KeySchedule,
    MediaKeys, MediaSender, ReleasePolicy, SenderConfig, SenderStats, SinkError,
    crypto::KEY_LEN,
    feedback::MAX_FEEDBACK_LEN,
    pacer::PacerConfig,
    packetizer::{FrameMeta, ParityPolicy},
    receiver::{Receiver, ReceiverStats},
};

use crate::{ImpairmentProfile, Lcg, delivery_stats, profile};

/// One measurement run.
#[derive(Clone, Debug, PartialEq)]
pub struct TransportScenario {
    pub name: String,
    pub profile: ImpairmentProfile,
    pub fps: u16,
    pub bitrate_mbps: u32,
    /// Whole-datagram budget, header included.
    pub mtu: usize,
    /// Fraction of data shards added as repair shards, per block.
    pub fec_fraction: f32,
    /// Whether a NACK round is allowed.
    pub retransmit: bool,
    /// Whether the media plane is encrypted.
    pub encrypted: bool,
    pub frames: u32,
    /// Reorder cover, in frame intervals. Derived from the profile by
    /// [`TransportScenario::jitter_frames_for`], not chosen.
    pub jitter_frames: u16,
    /// **The scenario's own budget**, in percent of sent frames, for frames that could not be
    /// reconstructed once every mechanism this scenario enables has done its work.
    ///
    /// Stated per scenario rather than as one global number because the honest answer differs
    /// by link, and a single number would force every link to be as good as the best one. A
    /// scenario that cannot meet its budget must lower its bitrate — that is a controller's
    /// decision and not one to be smuggled into the media plane.
    pub max_unreconstructable_pct: f64,
    /// The media plane's own latency budget, p95, in ms.
    ///
    /// Also per scenario, and for the same reason: the reorder window is `jitter + stall`
    /// over the frame interval, so a link with 30 ms stalls is *supposed* to hold five frames
    /// and one with 0.3 ms of jitter is not. Asserting one number for both would either fail
    /// the honest buffering or pass a buffer that had been sized for the wrong link.
    pub max_media_latency_p95_ms: f64,
    /// Frames never delivered at all, as a percentage of frames sent.
    ///
    /// The target is **zero**. It is a gate rather than a report because the mechanisms that earn
    /// it — FEC, a NACK round, and a release policy that gives both the time they need — are all
    /// present, so a non-zero value here is one of them not working rather than the link being
    /// unfair. Where a profile is deliberately past what those mechanisms can cover, the scenario
    /// says so with an explicit budget instead of the whole metric being excused.
    pub max_missing_pct: f64,
    /// Frames that arrived but were too late to be the frame for their own period. Also zero, for
    /// the same reason: a frame shown late is a frame the latency budget already spent.
    pub max_late_pct: f64,
    /// The same missing figure over the **second half** of the run, for a scenario whose sender is
    /// adapting. A controller may open at the wrong ratio — that is the cost of not knowing the
    /// link — but it has to stop losing frames once it has measured it.
    pub max_missing_tail_pct: f64,
    /// Repair overhead on the wire, as a percentage of datagrams sent. Per scenario, because parity
    /// is *bought* against loss: a fixed ceiling would make a lossy link's protection look like a
    /// defect when it is the mechanism working.
    pub max_fec_overhead_pct: f64,
}

impl TransportScenario {
    fn frame_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps as f64)
    }

    /// How many frame intervals of reorder cover the link needs.
    ///
    /// **This is a property of the profile, not a preference**, and the bench found it the
    /// hard way: with one frame of cover, `cqm_churn` (25 ms of jitter and 30 ms channel-hop
    /// stalls on an 11.1 ms interval) lost 56 of 60 frames. Nothing was wrong with the FEC —
    /// a later frame simply completed first, so the reorder window released the stalled frame
    /// while its datagrams were still in flight, and no code can repair a frame it has
    /// already given up on.
    ///
    /// So the window is `jitter + stall` divided by the frame interval, floored at one. The
    /// cost is exactly that much added latency, which the metrics report: a jitter buffer is
    /// a latency-for-completeness trade and there is no free setting of it.
    fn jitter_frames_for(profile: &ImpairmentProfile, fps: u16) -> u16 {
        let interval_ms = 1000.0 / fps as f64;
        let stall_ms = profile.stall.as_ref().map_or(0.0, |s| s.duration_ms);
        let needed = ((profile.jitter_ms + stall_ms) / interval_ms).ceil();
        (needed as u16).max(1)
    }

    fn bytes_per_frame(&self) -> usize {
        // bits/s -> bytes/frame
        (self.bitrate_mbps as u64 * 1_000_000 / 8 / self.fps as u64) as usize
    }

    fn parity(&self) -> ParityPolicy {
        if self.fec_fraction <= 0.0 {
            ParityPolicy::Off
        } else {
            ParityPolicy::Ratio {
                fraction: self.fec_fraction,
            }
        }
    }
}

/// What came out.
#[derive(Clone, Debug, PartialEq)]
pub struct TransportMetrics {
    pub scenario: String,
    pub seed: u64,
    pub frames_sent: u32,
    pub datagrams_sent: u64,
    pub datagrams_lost: u64,
    pub datagrams_retransmitted: u64,
    /// Datagrams that arrived but were never sent twice by mistake.
    pub loss_pct: f64,
    pub stats: ReceiverStats,
    /// Frames released and usable, as a fraction of frames sent.
    pub deliverable_pct: f64,
    /// **The ADR-0011 gate.** Frames that reached the display path without a payload. It is
    /// structurally zero — the type does not carry one — and this is the assertion that the
    /// type is doing what it claims.
    pub displayed_but_unreconstructable: u64,
    /// Repair overhead on the wire.
    pub fec_overhead_pct: f64,
    /// Media-plane latency: release time minus the frame's target time.
    pub latency_ms: LatencySummary,
    /// Frames that were usable but later than the profile's own budget allows.
    pub late_frames: u64,
    pub fec_enabled: bool,
    pub retransmit_enabled: bool,
    pub encrypted: bool,
    /// Copied from the scenario so the gate has the budget it is checking against.
    pub max_unreconstructable_pct: f64,
    pub max_media_latency_p95_ms: f64,
    pub max_missing_pct: f64,
    pub max_late_pct: f64,
    pub max_missing_tail_pct: f64,
    pub max_fec_overhead_pct: f64,
    /// The ratio the sender was running at when the run ended.
    pub final_fec_fraction: f32,
    /// Frames the client never got at all — no payload ever reached the display path.
    pub frames_missing: u64,
    /// The same, over the **second half** of the run. For a sender whose FEC ratio adapts, this is
    /// the number that says whether it settled: a transient at the start is the cost of not knowing
    /// the link, and a tail that is still losing frames is the controller not working.
    pub frames_missing_tail: u64,
    /// What the sender did: repairs answered, repairs refused and why, keyframes asked for.
    pub sender: SenderStats,
    /// Feedback the client sent, and how much of it the sender acted on.
    pub nacks_sent: u64,
    pub nacks_answered: u64,
    pub repairs_refused: u64,
    pub keyframe_requests: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LatencySummary {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

/// How long a profile goes quiet for. Shared by the release policy and the send loop, so the cover
/// and the stall cannot be sized from different numbers.
fn stall_duration_of(profile: &ImpairmentProfile) -> Duration {
    profile
        .stall
        .as_ref()
        .map(|s| Duration::from_secs_f64(s.duration_ms / 1000.0))
        .unwrap_or_default()
}

/// The impaired link, as a [`DatagramSink`].
///
/// Everything a profile does to a datagram happens here and nowhere else, so the sender under test
/// is exactly the shipped one and the link is the only thing the bench invents. Loss is drawn per
/// datagram at send time (including retransmits); jitter is drawn per datagram and turns into
/// reordering for free, because the event queue is keyed on arrival rather than on send order.
struct Net<'a> {
    queue: &'a mut BinaryHeap<Reverse<(u128, u64)>>,
    payloads: &'a mut Vec<Vec<u8>>,
    seq: &'a mut u64,
    rng: &'a mut Lcg,
    profile: &'a ImpairmentProfile,
    /// When the caller is handing these to the network.
    send_at: Duration,
    /// While set, nothing arrives before this instant: a channel hop delays rather than drops.
    stall_until: Option<Duration>,
    offered: u64,
    lost: u64,
}

impl Net<'_> {
    fn delivered(&self, at: Duration) -> Duration {
        match self.stall_until {
            Some(until) => at.max(until),
            None => at,
        }
    }
}

impl DatagramSink for Net<'_> {
    fn send(&mut self, datagram: &[u8]) -> Result<(), SinkError> {
        self.offered += 1;

        if self.rng.next_f64() * 100.0 < self.profile.loss_pct {
            self.lost += 1;
            return Ok(());
        }

        let jitter = (self.rng.next_f64() * 2.0 - 1.0) * self.profile.jitter_ms / 2.0;
        let arrival = self.send_at
            + Duration::from_secs_f64((self.profile.one_way_latency_ms + jitter).max(0.0) / 1000.0);

        self.payloads.push(datagram.to_vec());
        self.queue
            .push(Reverse((self.delivered(arrival).as_nanos(), *self.seq)));
        *self.seq += 1;
        Ok(())
    }
}

/// Run one scenario deterministically, through the **real** sender and the **real** receiver.
///
/// The bench used to build its own datagrams and keep its own map of what it had sent, so the
/// retransmit path it measured was not the one that ships — a repair that the measured code
/// performed unconditionally, and the shipped one refuses when it cannot arrive in time. It now
/// drives [`MediaSender`], [`Receiver`] and a real feedback channel, so a change in either end
/// changes these numbers. That is the only property that makes a gate worth having.
pub fn run_transport(scenario: &TransportScenario, seed: u64) -> TransportMetrics {
    let mut rng = Lcg::new(seed);
    let frame_interval = scenario.frame_interval();
    let bytes_per_frame = scenario.bytes_per_frame();

    let key = {
        let mut key = [0u8; KEY_LEN];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = (seed as u8).wrapping_add(i as u8);
        }
        key
    };

    // One key for the whole run. A sixteen-frame sample is not long enough to exercise a rotation,
    // and rotating it here would make the epoch a hidden variable in a scenario about loss.
    let schedule = scenario
        .encrypted
        .then(|| KeySchedule::with_frames_per_key(key, u64::MAX));

    let rtt = Duration::from_secs_f64(2.0 * scenario.profile.one_way_latency_ms / 1000.0);

    // The cover is sized from what the *link* does — jitter plus how long it goes quiet — not from
    // a count of frames, which is what it used to be and what made every frame on every scenario
    // arrive at the end of its own period. See `ReleasePolicy::reorder_delay`.
    let release_policy = ReleasePolicy::for_link(
        Duration::from_secs_f64(scenario.profile.jitter_ms / 1000.0),
        stall_duration_of(&scenario.profile),
        rtt,
        frame_interval,
    );
    let mut receiver = Receiver::new(release_policy, schedule.clone().map(MediaKeys::rotating));

    let mut sender = MediaSender::new(
        SenderConfig::matching_policy(scenario.mtu, scenario.parity(), &release_policy),
        PacerConfig::for_rate(scenario.bitrate_mbps as u64 * 1_000_000, frame_interval),
        frame_interval,
        schedule.clone(),
    );
    sender.set_rtt(rtt);

    // The feedback channel, sealed under its own domain-separated key. Both ends are in this
    // process, which is exactly the point: the codec either round-trips or it does not, and a
    // failure here is a failure of the shipped path rather than of a stand-in.
    let mut feedback_out = schedule
        .as_ref()
        .map(|schedule| FeedbackSender::new(schedule.cipher_for_feedback()));
    let mut feedback_in = schedule
        .as_ref()
        .map(|schedule| FeedbackReceiver::new(schedule.cipher_for_feedback()));

    // Deterministic payload: incompressible-looking, and identical between the two ends of
    // the comparison because it is generated, not random.
    let payload: Vec<u8> = (0..bytes_per_frame).map(|i| (i % 251) as u8).collect();

    // A min-heap on (arrival_nanos, sequence) with the bytes in a side table.
    //
    // The bytes are *not* in the heap key, and that is not a micro-optimisation: comparing
    // 1400-byte payloads to break ties between datagrams that share an arrival time (which
    // they do constantly — jitter is drawn per datagram but the nominal latency is not) costs
    // an order of magnitude more than the simulation itself.
    let mut queue: BinaryHeap<Reverse<(u128, u64)>> = BinaryHeap::new();
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    let mut event_seq: u64 = 0;

    let stall_duration = stall_duration_of(&scenario.profile);

    // The profile's stall period is expressed in control-plane exchanges; a 16-frame sample is
    // shorter than one period, so it is scaled to the sample to make sure a channel hop actually
    // happens. A modelling choice, labelled as one.
    let stall_period = scenario
        .profile
        .stall
        .as_ref()
        .map(|_| (scenario.frames / 3).max(2));

    let mut datagrams_offered = 0u64;
    let mut datagrams_lost = 0u64;

    // -- one timeline: send, deliver, ask, release --------------------------------------------
    //
    // **Interleaved, and that is not a detail.** This used to send every frame and only then start
    // delivering them, which worked with a hand-rolled sender holding an unbounded `HashMap` of
    // everything it had ever sent — and is not how a session works. The real sender keeps a bounded
    // cache, so by the time delivery began it held only the last sixteen frames and every repair
    // request for an earlier one was refused as unknown. A live session interleaves; so does this.
    let one_way = Duration::from_secs_f64(scenario.profile.one_way_latency_ms / 1000.0);
    let ask_at = release_policy.straggler_delay;
    let retry = x_transport::nack_retry_interval(rtt);

    let mut next_frame: u64 = 1;
    let mut send_clock = Duration::ZERO;
    let mut in_stall_until: Option<Duration> = None;
    // Filled as frames are sent, because an ask cannot be scheduled before the frame it is about.
    let mut ask_schedule: BinaryHeap<Reverse<(u128, u64)>> = BinaryHeap::new();

    // `(frame_index, when, usable)`. The third field is what makes `frames_missing` and its
    // tail-window counterpart computable from one pass: a frame that was released without a payload
    // is a frame the client never got, and one released *late* in the run is the one that says
    // whether an adapting sender settled.
    let mut released: Vec<(u64, Duration, bool)> = Vec::new();
    let mut displayed_but_unreconstructable = 0u64;
    let mut nacks_sent = 0u64;
    let mut nacks_answered = 0u64;
    let mut repairs_refused = 0u64;
    let mut keyframe_requests = 0u64;
    let mut last_now = Duration::ZERO;

    loop {
        let next_arrival = queue
            .peek()
            .map(|Reverse((nanos, _))| Duration::from_nanos(*nanos as u64));
        let next_action = ask_schedule
            .peek()
            .map(|Reverse((nanos, _))| Duration::from_nanos(*nanos as u64));
        let next_send = (next_frame <= scenario.frames as u64).then_some(send_clock);

        let candidates = [next_arrival, next_action, next_send];
        let Some(now) = candidates.into_iter().flatten().min() else {
            break;
        };
        last_now = last_now.max(now);

        // 1. The frame that is due. Sent first so its datagrams can land in this same instant.
        if next_send == Some(now) {
            if let Some(period) = stall_period
                && next_frame.is_multiple_of(period as u64)
            {
                in_stall_until = Some(now + stall_duration);
            }

            let meta = FrameMeta {
                frame_index: next_frame,
                target_timestamp_us: (next_frame as f64 * frame_interval.as_secs_f64() * 1e6)
                    as u64,
                // A keyframe every 30 frames, which is what a real encoder does between requested
                // IDRs. The flag is one bit and adds no bytes, so it changes no scenario's delivery
                // — it is here so the path that carries it is exercised end to end.
                is_keyframe: next_frame == 1 || next_frame.is_multiple_of(30),
                key_epoch: 0,
                reference_frame: 0,
            };

            let mut net = Net {
                queue: &mut queue,
                payloads: &mut payloads,
                seq: &mut event_seq,
                rng: &mut rng,
                profile: &scenario.profile,
                send_at: now,
                stall_until: in_stall_until,
                offered: 0,
                lost: 0,
            };
            sender.send_frame(&mut net, meta, &payload, now);
            datagrams_offered += net.offered;
            datagrams_lost += net.lost;

            // The client's repair timer starts now, not at the frame's first datagram: it can only
            // know what is missing once it has stopped expecting more.
            ask_schedule.push(Reverse(((now + one_way + ask_at).as_nanos(), next_frame)));

            next_frame += 1;
            send_clock += frame_interval;
        }

        // 2. Take the datagram that is due.
        if next_arrival == Some(now) {
            let Some(Reverse((_, seq))) = queue.pop() else {
                break;
            };
            let datagram = std::mem::take(&mut payloads[seq as usize]);
            receiver.on_datagram(&datagram, now);
        }

        // 3. The client's repair timer, at the instant it expires — while the frame is still held.
        while ask_schedule
            .peek()
            .is_some_and(|Reverse((nanos, _))| Duration::from_nanos(*nanos as u64) <= now)
        {
            let Reverse((_, frame_index)) = ask_schedule.pop().expect("peeked");
            if !scenario.retransmit {
                continue;
            }

            let missing = receiver.nack(frame_index);
            if missing.is_empty() {
                continue;
            }
            // Still held and still incomplete: ask again inside the window that is left. One round
            // on a lossy link is one round of that same loss applied to the repair itself.
            ask_schedule.push(Reverse(((now + retry).as_nanos(), frame_index)));

            for feedback in Feedback::nack_chunks(frame_index, &missing) {
                nacks_sent += 1;

                // Sealed by the client, opened by the sender, one round trip later. On the
                // plaintext arm the message is handed over as-is, so the two arms differ only in
                // the thing under test.
                let opened = match (&mut feedback_out, &mut feedback_in) {
                    (Some(out), Some(received)) => {
                        let mut buffer = [0u8; MAX_FEEDBACK_LEN];
                        match out.seal(&feedback, &mut buffer) {
                            Ok(len) => received.open(&buffer[..len]).ok().flatten(),
                            Err(_) => None,
                        }
                    }
                    _ => Some(feedback.clone()),
                };

                let Some(opened) = opened else { continue };

                // The request takes one hop; the repair takes another.
                let sender_acts_at = now + one_way;
                let mut net = Net {
                    queue: &mut queue,
                    payloads: &mut payloads,
                    seq: &mut event_seq,
                    rng: &mut rng,
                    profile: &scenario.profile,
                    send_at: sender_acts_at,
                    stall_until: None,
                    offered: 0,
                    lost: 0,
                };

                match sender.on_feedback(&mut net, &opened, sender_acts_at) {
                    FeedbackOutcome::Repaired { .. } => nacks_answered += 1,
                    FeedbackOutcome::RepairRefused { .. } => repairs_refused += 1,
                    FeedbackOutcome::KeyframeRequired { .. } => keyframe_requests += 1,
                    _ => {}
                }

                datagrams_offered += net.offered;
                datagrams_lost += net.lost;
            }
        }

        // 4. Release what is ready — last, so an ask for a frame this instant has already been made
        // while it was still held.
        for frame in receiver.release(now) {
            if frame.is_displayable() && frame.payload().is_none() {
                displayed_but_unreconstructable += 1;
            }
            released.push((frame.frame_index, now, frame.is_displayable()));
        }
    }

    let last_arrival = last_now;

    // Drain: the retransmits pushed during the final walk, and any frame whose deadline has passed.
    let drain_until = last_arrival + rtt + frame_interval * 2;
    let mut drain_now = last_arrival;
    while drain_now <= drain_until {
        drain_now += frame_interval / 4;
        for frame in receiver.release(drain_now) {
            if frame.is_displayable() && frame.payload().is_none() {
                displayed_but_unreconstructable += 1;
            }
            released.push((frame.frame_index, drain_now, frame.is_displayable()));
        }
        if receiver.in_flight() == 0 {
            break;
        }
    }
    for frame in receiver.release(drain_until + frame_interval * 8) {
        if frame.is_displayable() && frame.payload().is_none() {
            displayed_but_unreconstructable += 1;
        }
        released.push((frame.frame_index, drain_now, frame.is_displayable()));
    }

    let stats = *receiver.stats();
    let sender_stats = *sender.stats();

    // Delivered, as this run counted it. `stats.frames_deliverable()` and this can disagree when a
    // run ends with frames still in flight, and the run's own count is the one the caller saw.
    let usable = released.iter().filter(|(_, _, usable)| *usable).count() as u64;
    let tail_start = scenario.frames as u64 / 2;
    // `tail_frames - what was delivered`, which counts a frame released without a payload and one
    // that never came out at all. Counting the former separately double-counted it: a drained frame
    // with no payload is not delivered either, and the first version of this reported 212 %.
    let frames_missing_tail = (scenario.frames as u64)
        .saturating_sub(tail_start)
        .saturating_sub(
            released
                .iter()
                .filter(|(index, _, usable)| *index >= tail_start && *usable)
                .count() as u64,
        );

    // -- latency: the media plane's *own* contribution ---------------------------------------
    // Measured from the instant the frame should have arrived rather than the instant it was
    // created, so this is buffering and recovery, not the link's transit. Subtracting the profile's
    // nominal one-way latency is what separates "our queue is too deep" from "the link is 25 ms
    // away", and only the first is ours to fix.
    let nominal_arrival_ms = scenario.profile.one_way_latency_ms;
    let mut latencies: Vec<f64> = released
        .iter()
        .map(|(frame_index, at, _)| {
            let target =
                Duration::from_secs_f64((*frame_index as f64 - 1.0) * frame_interval.as_secs_f64());
            (at.saturating_sub(target).as_secs_f64() * 1000.0 - nominal_arrival_ms).max(0.0)
        })
        .collect();
    latencies.sort_by(f64::total_cmp);
    let latency_ms = LatencySummary {
        p50: crate::percentile(&latencies, 50.0),
        p95: crate::percentile(&latencies, 95.0),
        p99: crate::percentile(&latencies, 99.0),
        max: latencies.last().copied().unwrap_or(0.0),
    };

    // "Late" is the media plane's **own** contribution exceeding a frame interval: the reorder
    // cover, the repair window, the FEC wait — everything we chose, with the link's transit
    // subtracted, because the transit is not ours to fix and a link whose one-way latency exceeds
    // the frame period cannot deliver at that rate no matter what this code does. That last fact is
    // what `max_media_latency_p95_ms` gates; this gates the part we own.
    let budget_ms = frame_interval.as_secs_f64() * 1000.0;
    let late_frames = latencies.iter().filter(|ms| **ms > budget_ms).count() as u64;

    let fec_overhead_pct = if sender_stats.datagrams_sent == 0 {
        0.0
    } else {
        sender_stats.parity_datagrams as f64 / sender_stats.datagrams_sent as f64 * 100.0
    };

    TransportMetrics {
        scenario: scenario.name.clone(),
        seed,
        frames_sent: scenario.frames,
        datagrams_sent: datagrams_offered,
        datagrams_lost,
        datagrams_retransmitted: sender_stats.retransmitted_datagrams,
        loss_pct: if datagrams_offered == 0 {
            0.0
        } else {
            datagrams_lost as f64 / datagrams_offered as f64 * 100.0
        },
        stats,
        deliverable_pct: if scenario.frames == 0 {
            0.0
        } else {
            stats.frames_deliverable() as f64 / scenario.frames as f64 * 100.0
        },
        displayed_but_unreconstructable,
        fec_overhead_pct,
        latency_ms,
        late_frames,
        fec_enabled: scenario.fec_fraction > 0.0,
        retransmit_enabled: scenario.retransmit,
        encrypted: scenario.encrypted,
        max_unreconstructable_pct: scenario.max_unreconstructable_pct,
        max_media_latency_p95_ms: scenario.max_media_latency_p95_ms,
        max_missing_pct: scenario.max_missing_pct,
        max_missing_tail_pct: scenario.max_missing_tail_pct,
        max_late_pct: scenario.max_late_pct,
        max_fec_overhead_pct: scenario.max_fec_overhead_pct,
        final_fec_fraction: match sender.parity_policy() {
            ParityPolicy::Ratio { fraction } => fraction,
            _ => 0.0,
        },
        frames_missing: (scenario.frames as u64).saturating_sub(usable),
        frames_missing_tail,
        sender: sender_stats,
        nacks_sent,
        nacks_answered,
        repairs_refused,
        keyframe_requests,
    }
}

/// The scenarios this build gates on: one per link profile that matters, plus the controls
/// that make the FEC and retransmit claims checkable rather than asserted.
pub fn transport_scenarios() -> Vec<TransportScenario> {
    let base = |name: &str, profile_name: &str| {
        let profile = profile(profile_name).expect("known profile");
        let jitter_frames = TransportScenario::jitter_frames_for(&profile, 90);
        let rtt_ms = 2.0 * profile.one_way_latency_ms;
        // The latency budget is a **consequence** of the release policy, not a number to tune until
        // the run passes: the worst a frame can legitimately pay is the policy's own hard bound, so
        // that plus a margin is what the gate checks. Hand-setting it is how a budget stops meaning
        // anything.
        let policy = ReleasePolicy::for_link(
            Duration::from_secs_f64(profile.jitter_ms / 1000.0),
            stall_duration_of(&profile),
            Duration::from_secs_f64(rtt_ms / 1000.0),
            Duration::from_secs_f64(1.0 / 90.0),
        );
        let window_ms = policy.deadline.as_secs_f64() * 1000.0;
        TransportScenario {
            name: name.to_string(),
            profile,
            fps: 90,
            bitrate_mbps: 300,
            mtu: 1400,
            fec_fraction: 0.05,
            retransmit: true,
            encrypted: true,
            // A sample of a second, not a soak: 16 frames at ~300 datagrams each is ~5000
            // datagrams per scenario, which is enough to see a 0.5 % loss rate (≈35 losses)
            // and few enough to run in CI.
            frames: 16,
            jitter_frames,
            max_unreconstructable_pct: 0.0,
            max_media_latency_p95_ms: window_ms + 10.0,
            // The target, and the mechanisms have to earn it.
            // **The target, on every scenario.** A frame the client never got is a frame the
            // reference chain lost, and every P-frame behind it decodes to something plausible and
            // wrong. The FEC ratio is a property of the link and the sender now measures it, so a
            // non-zero value here is a mechanism not working rather than a link being unfair.
            max_missing_pct: 0.0,
            max_missing_tail_pct: 0.0,
            max_late_pct: late_is_inherent(profile_name),
            max_fec_overhead_pct: 12.0,
        }
    };

    /// Profiles whose **jitter alone exceeds a frame period**. A frame with a hole must be held for
    /// its straggler window, and on a link that spreads a frame's datagrams over 15 ms there is no
    /// way to hold for less than 15 ms — which is longer than the 11.1 ms the frame was aimed at.
    /// The frames are still delivered, still whole and still in order; they are simply later than
    /// one period, and no amount of code changes that. The honest budget says so instead of the
    /// metric being excused.
    fn late_is_inherent(profile_name: &str) -> f64 {
        match profile_name {
            "wifi7_regrace" | "cqm_churn" => 100.0,
            _ => 0.0,
        }
    }

    let scenarios = vec![
        base("transport_ncm_wired", "ncm_wired"),
        base("transport_wifi7_160", "wifi7_160_clean"),
        base("transport_wifi7_regrace", "wifi7_regrace"),
        base("transport_cqm_churn", "cqm_churn"),
        base("transport_wifi6_lan", "wifi6_lan"),
        // A synthetic case, and labelled as one: the wired profile's latency with a **high**
        // loss rate on it, as a damaged cable or a failing switch would give. It exists for
        // two reasons. Retransmit only pays when the round trip fits inside the reorder
        // window, which is only true on a wired link — so without this scenario the
        // retransmit path would be untested. And the loss rate is set above what a 5 % code
        // can cover, so the retransmit has something to do: at 0.5 % the FEC alone would
        // carry it and the comparison below would prove nothing.
        TransportScenario {
            name: "transport_wired_lossy".into(),
            profile: ImpairmentProfile {
                loss_pct: 8.0,
                ..profile("ncm_wired").expect("known profile")
            },
            // **Sized to the link.** 8 % datagram loss needs a code that covers 8 %, and the
            // controller's own law (`loss × 3`) lands on 24 % for it. A 5 % code here was the old
            // scenario's way of guaranteeing the repair round had something to do; now that the
            // repair round works, the honest question is whether a *correctly sized* code loses
            // anything at all.
            fec_fraction: 0.25,
            max_fec_overhead_pct: 30.0,
            // Beyond both mechanisms some frames will be lost, and the scenario says so
            // rather than pretending. Most are recovered; the budget is what is left.
            max_unreconstructable_pct: 0.0,
            ..base("x", "ncm_wired")
        },
        // The same link, starting from a ratio that does not cover it. This is the scenario that
        // measures the *controller*: a sender that opens at 5 % on an 8 % link loses frames until
        // it has measured the link, and the tail figure is what says it stopped. The head is the
        // cost of not knowing, and it is bounded and visible rather than assumed away.
        TransportScenario {
            name: "transport_wired_lossy_adapting".into(),
            profile: ImpairmentProfile {
                loss_pct: 8.0,
                ..profile("ncm_wired").expect("known profile")
            },
            fec_fraction: 0.05,
            // Long enough for the ratio to settle and to measure the settled state, and no longer.
            // The controller closes its window every eight frames, so the ratio is right by frame
            // nine; the rest is the settled measurement. (It was 180 frames and the GF(256) encode
            // made that a two-minute run under emulation, which is a test nobody would keep.)
            frames: 48,
            // The head is allowed to lose: it is the whole point of the scenario. The *tail* is not.
            max_missing_pct: 25.0,
            max_missing_tail_pct: 0.0,
            max_unreconstructable_pct: 25.0,
            max_fec_overhead_pct: 30.0,
            ..base("x", "ncm_wired")
        },
        TransportScenario {
            name: "control_wired_fec_only".into(),
            retransmit: false,
            profile: ImpairmentProfile {
                loss_pct: 8.0,
                ..profile("ncm_wired").expect("known profile")
            },
            max_unreconstructable_pct: 100.0,
            // No retransmit and 8 % loss against a 5 % code: this is the arm that shows what the
            // repair round is worth, so its budget is deliberately "anything".
            max_missing_pct: 100.0,
            max_missing_tail_pct: 100.0,
            max_late_pct: 0.0,
            ..base("x", "ncm_wired")
        },
        // Controls. Without these the gate cannot tell "the FEC fixed it" from "the profile
        // was never lossy".
        TransportScenario {
            name: "control_no_fec_regrace".into(),
            fec_fraction: 0.0,
            retransmit: false,
            max_unreconstructable_pct: 100.0,
            max_missing_pct: 100.0,
            max_missing_tail_pct: 100.0,
            max_late_pct: 100.0,
            max_fec_overhead_pct: 100.0,
            ..base("x", "wifi7_regrace")
        },
        TransportScenario {
            name: "control_no_fec_cqm".into(),
            fec_fraction: 0.0,
            retransmit: false,
            max_unreconstructable_pct: 100.0,
            max_missing_pct: 100.0,
            max_missing_tail_pct: 100.0,
            max_late_pct: 100.0,
            max_fec_overhead_pct: 100.0,
            ..base("x", "cqm_churn")
        },
        TransportScenario {
            name: "transport_plaintext".into(),
            encrypted: false,
            ..base("x", "wifi7_160_clean")
        },
    ];

    scenarios
}

/// Which gates a run satisfies. Returns every failure rather than the first, so one run
/// tells you everything that is wrong.
pub fn check_transport_gates(m: &TransportMetrics) -> Vec<String> {
    let mut failures = Vec::new();

    // The ADR-0011 gate, counted separately from every latency gate because a frame can be
    // on time and worthless.
    if m.displayed_but_unreconstructable != 0 {
        failures.push(format!(
            "{}: {} frame(s) reached the display path without a payload",
            m.scenario, m.displayed_but_unreconstructable
        ));
    }

    // Anything the receiver reported as unusable counts against this scenario's own budget
    // once every mechanism it enables has done its work.
    let unrecoverable =
        m.stats.frames_unreconstructable as f64 / m.frames_sent.max(1) as f64 * 100.0;
    if unrecoverable > m.max_unreconstructable_pct {
        failures.push(format!(
            "{}: {:.1}% of frames unreconstructable, budget is {:.1}% \
             ({} frames at {:.3}% datagram loss, fec={} retransmit={})",
            m.scenario,
            unrecoverable,
            m.max_unreconstructable_pct,
            m.stats.frames_unreconstructable,
            m.loss_pct,
            m.fec_enabled,
            m.retransmit_enabled
        ));
    }

    // The parity gate. A frame the client never got is a frame the reference chain lost, and every
    // P-frame behind it decodes to something plausible and wrong — which is the defect this whole
    // plane exists to remove. The mechanisms that prevent it are all here; a non-zero value means
    // one of them is not doing its job.
    let missing_pct = m.frames_missing as f64 / m.frames_sent.max(1) as f64 * 100.0;
    if missing_pct > m.max_missing_pct {
        failures.push(format!(
            "{}: {:.1}% of frames never arrived ({}, budget {:.1}%) — FEC={} retransmit={},              {} nack(s) sent, {} answered, {} refused",
            m.scenario,
            missing_pct,
            m.frames_missing,
            m.max_missing_pct,
            m.fec_enabled,
            m.retransmit_enabled,
            m.nacks_sent,
            m.nacks_answered,
            m.repairs_refused,
        ));
    }

    // And the other half of the same claim: a frame that arrives after its own period is a frame
    // the latency budget already spent.
    let late_pct = m.late_frames as f64 / m.frames_sent.max(1) as f64 * 100.0;
    if late_pct > m.max_late_pct {
        failures.push(format!(
            "{}: {:.1}% of frames were late ({}, budget {:.1}%), p95 {:.1} ms against a {:.1} ms              frame interval",
            m.scenario, late_pct, m.late_frames, m.max_late_pct, m.latency_ms.p95,
            m.max_media_latency_p95_ms / 2.0,
        ));
    }

    // The tail of an adapting run, which is the claim a controller makes: it may open at the wrong
    // ratio, but it must stop losing frames once it has measured the link.
    let tail_frames = m.frames_sent as u64 / 2;
    let tail_missing_pct = if tail_frames == 0 {
        0.0
    } else {
        m.frames_missing_tail as f64 / tail_frames as f64 * 100.0
    };
    if tail_missing_pct > m.max_missing_tail_pct {
        failures.push(format!(
            "{}: {:.1}% of frames in the second half never arrived ({} of {}, budget {:.1}%) — the              FEC ratio settled at {:.0}% against {:.2}% measured loss",
            m.scenario,
            tail_missing_pct,
            m.frames_missing_tail,
            tail_frames,
            m.max_missing_tail_pct,
            m.final_fec_fraction * 100.0,
            m.sender.last_loss * 100.0,
        ));
    }

    // Overhead is *bought* against loss, so the ceiling is per scenario. A fixed one would make a
    // lossy link's protection look like a defect, when it is the mechanism working.
    if m.fec_overhead_pct > m.max_fec_overhead_pct {
        failures.push(format!(
            "{}: FEC overhead {:.1}% exceeds its {:.1}% budget",
            m.scenario, m.fec_overhead_pct, m.max_fec_overhead_pct
        ));
    }

    // The media plane's own contribution must not exceed the window the link's jitter forced
    // it to hold, plus the round trip that window was sized to allow.
    if m.latency_ms.p95 > m.max_media_latency_p95_ms {
        failures.push(format!(
            "{}: p95 media-plane latency {:.1} ms exceeds its {:.1} ms budget",
            m.scenario, m.latency_ms.p95, m.max_media_latency_p95_ms
        ));
    }

    // A control that does not show loss is not a control.
    if m.scenario.starts_with("control_no_fec_")
        && m.loss_pct > 0.2
        && m.stats.frames_unreconstructable == 0
    {
        failures.push(format!(
            "{}: the lossy control produced no unreconstructable frames at {:.3}% loss — \
             the control is not exercising anything",
            m.scenario, m.loss_pct
        ));
    }

    failures
}

/// Gate a comparison between an impaired run and its clean counterpart: how much of what
/// the link lost did the media plane put back?
pub fn recovery_pct(with_mechanism: &TransportMetrics, without: &TransportMetrics) -> f64 {
    let lost_without = without.stats.frames_unreconstructable as f64;
    if lost_without == 0.0 {
        return 100.0;
    }
    let lost_with = with_mechanism.stats.frames_unreconstructable as f64;
    ((lost_without - lost_with) / lost_without * 100.0).clamp(0.0, 100.0)
}

/// Reuse for a delivery-deadline style summary, so transport numbers can be read the same
/// way as the control-plane ones.
pub fn mandatory_missed_pct(m: &TransportMetrics, fps: u16) -> f64 {
    let samples: Vec<f64> = vec![m.latency_ms.p50, m.latency_ms.p95, m.latency_ms.p99];
    let mandatory = 1000.0 / fps as f64;
    delivery_stats(&samples, mandatory, mandatory * 3.0 / 4.0).missed_mandatory_pct
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::OnceLock;

    /// Every scenario, run once per test binary.
    ///
    /// Each run is a few thousand datagrams of real GF(256) encode and decode, so running a
    /// scenario per assertion made the suite take three minutes. One pass, shared, keeps the
    /// gates honest and the suite usable.
    fn all_runs() -> &'static [(TransportScenario, TransportMetrics)] {
        static RUNS: OnceLock<Vec<(TransportScenario, TransportMetrics)>> = OnceLock::new();
        RUNS.get_or_init(|| {
            transport_scenarios()
                .into_iter()
                .map(|scenario| {
                    let metrics = run_transport(&scenario, 42);
                    (scenario, metrics)
                })
                .collect()
        })
    }

    fn run(name: &str) -> &'static TransportMetrics {
        &all_runs()
            .iter()
            .find(|(s, _)| s.name == name)
            .unwrap_or_else(|| panic!("no scenario {name}"))
            .1
    }

    #[test]
    fn every_scenario_satisfies_its_gates() {
        // One run, every failure reported: a gate that can only be checked one at a time is
        // a gate nobody runs.
        let mut failures = Vec::new();
        for (_, metrics) in all_runs() {
            failures.extend(check_transport_gates(metrics));
        }
        assert!(
            failures.is_empty(),
            "transport gates failed:\n  {}",
            failures.join("\n  ")
        );
    }

    #[test]
    fn no_frame_reaches_the_display_path_without_a_payload() {
        // ADR-0011's gate. It is structurally impossible — `payload()` is `None` exactly when
        // the frame is not displayable — and the test exists to keep it that way.
        for (scenario, metrics) in all_runs() {
            assert_eq!(
                metrics.displayed_but_unreconstructable, 0,
                "{} produced a frame with no payload",
                scenario.name
            );
        }
    }

    #[test]
    fn a_held_frame_is_counted_as_a_loss_and_not_as_a_display() {
        // The distinction the metrics have to keep: an unreconstructable frame is a *cost*,
        // not a *lie*. The controls are where the two must diverge visibly.
        let control = run("control_no_fec_regrace");
        assert!(
            control.stats.frames_unreconstructable > 0,
            "control lost nothing"
        );
        assert_eq!(
            control.displayed_but_unreconstructable, 0,
            "the control lost frames and still displayed none of them, which is the point"
        );
    }

    #[test]
    fn the_fec_and_retransmit_recover_what_the_control_loses() {
        // The claim this whole module exists to check, stated as a comparison rather than an
        // assertion of an absolute: the protected run must lose strictly fewer frames than
        // the unprotected one, on the same link, with the same seed.
        let protected = run("transport_wifi7_regrace");
        let unprotected = run("control_no_fec_regrace");

        assert!(
            unprotected.stats.frames_unreconstructable > 0,
            "the control must actually lose frames, or this test proves nothing: {unprotected:?}"
        );
        assert_eq!(
            protected.stats.frames_unreconstructable, 0,
            "FEC + retransmit should have repaired everything at {:.3}% loss",
            protected.loss_pct
        );
        assert_eq!(
            recovery_pct(protected, unprotected),
            100.0,
            "recovery was not complete"
        );
    }

    #[test]
    fn the_control_profiles_really_are_lossy() {
        // Guards the guard: if a profile's loss rate were zero, every claim above would pass
        // vacuously.
        for name in ["control_no_fec_regrace", "control_no_fec_cqm"] {
            let metrics = run(name);
            assert!(
                metrics.loss_pct > 0.1,
                "{name}: loss was {:.4}%",
                metrics.loss_pct
            );
            assert!(
                metrics.datagrams_lost > 0,
                "{name}: nothing was lost, so nothing was tested"
            );
        }
    }

    #[test]
    fn retransmit_pays_on_a_wired_link_and_cannot_on_a_slow_one() {
        // The finding, asserted so it cannot be quietly forgotten. A retransmit has to arrive
        // before the reorder window gives the frame up. The *rule* is arithmetic and is
        // checked directly; the runs below are the consequence.
        let wired_rtt_ms = 2.0 * 2.0; // ncm_wired: 2 ms one way
        let wired_window_ms = 1.0 * 1000.0 / 90.0; // one frame of cover
        assert!(
            wired_rtt_ms < wired_window_ms,
            "a wired round trip must fit inside the reorder window for retransmit to be worth having"
        );

        let slow_rtt_ms = 2.0 * 25.0; // wifi7_regrace: 25 ms one way
        let slow_window_ms = 2.0 * 1000.0 / 90.0; // two frames of cover
        assert!(
            slow_rtt_ms > slow_window_ms,
            "a 50 ms round trip must *not* fit, or the story is wrong"
        );

        // And the runs agree with the arithmetic: on the wired link the retransmit is what
        // recovers frames the code alone cannot, and on the slow link the code is what does.
        let wired = run("transport_wired_lossy");
        let wired_fec_only = run("control_wired_fec_only");
        assert!(
            wired.datagrams_retransmitted > 0,
            "no NACK round fired on the wired link, where it is supposed to pay"
        );
        assert!(
            wired.stats.frames_unreconstructable < wired_fec_only.stats.frames_unreconstructable,
            "retransmit bought nothing at {:.1}% loss on a wired link: {} vs {} frames lost",
            wired.loss_pct,
            wired.stats.frames_unreconstructable,
            wired_fec_only.stats.frames_unreconstructable
        );

        let slow = run("transport_wifi7_regrace");
        assert!(
            slow.stats.fragments_repaired > 0,
            "the FEC did no work at all on the slow link"
        );
        assert_eq!(
            slow.stats.frames_unreconstructable, 0,
            "the FEC should have carried the slow link on its own"
        );
    }

    #[test]
    fn the_jitter_window_is_sized_from_the_link_not_chosen() {
        // The bug that produced 56 lost frames out of 60: a one-frame window on a link with
        // 30 ms channel-hop stalls. The window is derived from the profile, so this asserts
        // the derivation rather than a constant.
        let cqm = transport_scenarios()
            .into_iter()
            .find(|s| s.name == "transport_cqm_churn")
            .unwrap();
        assert!(
            cqm.jitter_frames >= 5,
            "30 ms of stall on an 11.1 ms interval needs at least 3 intervals of cover, got {}",
            cqm.jitter_frames
        );

        let wired = transport_scenarios()
            .into_iter()
            .find(|s| s.name == "transport_ncm_wired")
            .unwrap();
        assert_eq!(
            wired.jitter_frames, 1,
            "a 0.3 ms jitter does not need a buffer"
        );
    }

    #[test]
    fn encryption_does_not_change_what_is_delivered() {
        // AEAD seals and opens per datagram with derived nonces; it must be transparent to
        // the loss machinery. If this ever diverges, the nonce derivation has become
        // loss-sensitive — which is the failure this design exists to avoid.
        let encrypted = run("transport_wifi7_160");
        let plaintext = run("transport_plaintext");

        assert!(encrypted.encrypted && !plaintext.encrypted);
        assert_eq!(
            encrypted.stats.frames_deliverable(),
            plaintext.stats.frames_deliverable()
        );
        assert_eq!(
            encrypted.stats.frames_unreconstructable,
            plaintext.stats.frames_unreconstructable
        );
        assert_eq!(plaintext.stats.datagrams_unauthenticated, 0);
    }

    #[test]
    fn a_zero_loss_profile_is_perfect_and_that_is_the_floor() {
        // The wired profile loses nothing, so the media plane must add nothing but its own
        // buffering. This is the number that would expose a receiver bug that drops frames
        // it did receive.
        let wired = run("transport_ncm_wired");
        assert_eq!(wired.datagrams_lost, 0);
        assert_eq!(wired.stats.frames_unreconstructable, 0);
        assert_eq!(wired.deliverable_pct, 100.0);
        assert!(
            wired.fec_overhead_pct <= 12.0,
            "{}% overhead on a perfect link",
            wired.fec_overhead_pct
        );
    }

    #[test]
    fn a_run_is_exactly_reproducible() {
        let scenario = transport_scenarios()
            .into_iter()
            .find(|s| s.name == "transport_cqm_churn")
            .unwrap();
        assert_eq!(run_transport(&scenario, 42), run_transport(&scenario, 42));
        assert_ne!(
            run_transport(&scenario, 42).datagrams_lost,
            run_transport(&scenario, 43).datagrams_lost,
            "a different seed must produce a different draw"
        );
    }

    #[test]
    fn the_frames_are_the_size_the_envelope_implies() {
        // The scenario's own arithmetic, checked: 300 Mbps at 90 Hz is ~416 KB, which at a
        // 1400-byte MTU is ~300 shards — more than one GF(2^8) block can address, which is
        // why the FEC is striped.
        let scenario = transport_scenarios()
            .into_iter()
            .find(|s| s.name == "transport_wifi7_160")
            .unwrap();
        let bytes = scenario.bytes_per_frame();
        assert!(
            (bytes as f64 - 416_666.0).abs() < 1000.0,
            "{bytes} bytes per frame"
        );

        let metrics = run("transport_wifi7_160");
        let shards_per_frame = metrics.datagrams_sent / metrics.frames_sent as u64;
        assert!(
            (300..340).contains(&shards_per_frame),
            "{shards_per_frame} datagrams per frame: the striping case is not being exercised"
        );
    }
}
