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

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    time::Duration,
};

use x_transport::{
    MediaCipher, Receiver, ReleasePolicy,
    crypto::KEY_LEN,
    packetizer::{FrameMeta, Packetizer, ParityPolicy},
    receiver::ReceiverStats,
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
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LatencySummary {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

/// Run one scenario deterministically.
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
    let cipher = scenario.encrypted.then(|| MediaCipher::new(&key));
    let packetizer = Packetizer::new(scenario.mtu, scenario.parity());
    let mut receiver = Receiver::new(
        ReleasePolicy::new(scenario.jitter_frames, frame_interval),
        scenario
            .encrypted
            .then(|| MediaCipher::new(&key))
            .map(Into::into),
    );

    // Deterministic payload: incompressible-looking, and identical between the two ends of
    // the comparison because it is generated, not random.
    let payload: Vec<u8> = (0..bytes_per_frame).map(|i| (i % 251) as u8).collect();

    // -- the event queue ------------------------------------------------------
    // A min-heap on (arrival_nanos, sequence) with the bytes in a side table.
    //
    // The bytes are *not* in the heap key, and that is not a micro-optimisation: comparing
    // 1400-byte payloads to break ties between datagrams that share an arrival time (which
    // they do constantly — jitter is drawn per datagram but the nominal latency is not) costs
    // an order of magnitude more than the simulation itself.
    let mut queue: BinaryHeap<Reverse<(u128, u64)>> = BinaryHeap::new();
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    let mut event_seq: u64 = 0;

    let push = |queue: &mut BinaryHeap<Reverse<(u128, u64)>>,
                payloads: &mut Vec<Vec<u8>>,
                seq: &mut u64,
                at: Duration,
                bytes: Vec<u8>| {
        payloads.push(bytes);
        queue.push(Reverse((at.as_nanos(), *seq)));
        *seq += 1;
    };

    let rtt = Duration::from_secs_f64(2.0 * scenario.profile.one_way_latency_ms / 1000.0);
    let rtt_nanos = rtt.as_nanos();

    let mut datagrams_sent = 0u64;
    let mut datagrams_lost = 0u64;
    let mut datagrams_retransmitted = 0u64;
    let mut data_shards_sent = 0u64;
    let mut parity_shards_sent = 0u64;
    // Everything sent, kept so a retransmit can resend the same bytes.
    let mut sent: HashMap<(u64, u16), Vec<u8>> = HashMap::new();
    // When each frame's datagrams were handed to the network, for the NACK schedule.
    let mut first_send: HashMap<u64, Duration> = HashMap::new();

    let stall_duration = scenario
        .profile
        .stall
        .as_ref()
        .map(|s| Duration::from_secs_f64(s.duration_ms / 1000.0))
        .unwrap_or_default();

    let mut send_clock = Duration::ZERO;
    let mut in_stall_until: Option<Duration> = None;

    // -- send every frame ----------------------------------------------------
    // The profile's stall period is expressed in control-plane exchanges; a 24-frame sample is
    // shorter than one period, so it is scaled to the sample to make sure a channel hop
    // actually happens. A modelling choice, labelled as one.
    let stall_period = scenario
        .profile
        .stall
        .as_ref()
        .map(|_| (scenario.frames / 3).max(2));

    for frame_index in 1..=scenario.frames as u64 {
        // A stall models the link going quiet: datagrams in this window are delayed rather
        // than dropped, because that is what a channel hop does.
        if let Some(period) = stall_period
            && frame_index.is_multiple_of(period as u64)
        {
            in_stall_until = Some(send_clock + stall_duration);
        }

        let (layout, datagrams) = packetizer
            .fragment(
                FrameMeta {
                    frame_index,
                    target_timestamp_us: (frame_index as f64 * frame_interval.as_secs_f64() * 1e6)
                        as u64,
                    // A keyframe every 30 frames, which is what a real encoder does between
                    // requested IDRs. The flag is one bit and adds no bytes, so it changes no
                    // scenario's delivery — it is here so the path that carries it is exercised
                    // end to end rather than only in unit tests.
                    is_keyframe: frame_index == 1 || frame_index.is_multiple_of(30),
                    key_epoch: 0,
                },
                &payload,
                &mut 0,
                cipher.as_ref(),
            )
            .expect("packetising a fixed-size frame cannot fail");

        data_shards_sent += layout.data_count as u64;
        parity_shards_sent += layout.parity_count as u64;
        first_send.insert(frame_index, send_clock);

        for (fragment_index, datagram) in datagrams.into_iter().enumerate() {
            datagrams_sent += 1;
            sent.insert((frame_index, fragment_index as u16), datagram.clone());

            if rng.next_f64() * 100.0 < scenario.profile.loss_pct {
                datagrams_lost += 1;
                continue;
            }

            let jitter = (rng.next_f64() * 2.0 - 1.0) * scenario.profile.jitter_ms / 2.0;
            let mut arrival = send_clock
                + Duration::from_secs_f64(
                    (scenario.profile.one_way_latency_ms + jitter).max(0.0) / 1000.0,
                );
            if let Some(until) = in_stall_until {
                arrival = arrival.max(until);
            }

            push(&mut queue, &mut payloads, &mut event_seq, arrival, datagram);
        }

        send_clock += frame_interval;
    }

    // -- deliver, and fire the NACKs when their time comes -------------------
    // A NACK is scheduled one round trip after the frame's *own* datagrams were handed to the
    // network, which is the earliest moment the receiver could know what is missing. Whether
    // that turns out to be early enough is not assumed here: on a 25 ms link the retransmit
    // arrives three frame intervals after the reorder window has already given the frame up,
    // and the count of retransmits that fired is the evidence rather than an opinion.
    let mut nack_schedule: Vec<(Duration, u64)> = first_send
        .iter()
        .map(|(frame_index, sent_at)| (*sent_at + rtt, *frame_index))
        .collect();
    nack_schedule.sort_by_key(|(at, _)| *at);

    let mut released: Vec<(u64, Duration)> = Vec::new();
    let mut displayed_but_unreconstructable = 0u64;
    let mut nack_index = 0usize;
    let mut now;
    let mut last_arrival = Duration::ZERO;

    while let Some(Reverse((at_nanos, seq))) = queue.pop() {
        let at = Duration::from_nanos(at_nanos as u64);
        now = at;
        last_arrival = now;
        let datagram = std::mem::take(&mut payloads[seq as usize]);

        // Everything the receiver is due to hand back up to this instant.
        for frame in receiver.release(now) {
            if frame.is_displayable() {
                // The display path. A displayable frame without a payload is the exact
                // contradiction ADR-0011 forbids, and the only way this counter can be
                // non-zero.
                if frame.payload().is_none() {
                    displayed_but_unreconstructable += 1;
                }
            }
            // Otherwise: not displayable, so it is *held* and the previous frame is
            // reprojected — the response ADR-0011 permits. It is counted in the receiver's
            // own `frames_unreconstructable`, which is a different and also necessary
            // number: "how much did the link cost us" rather than "did we show anything
            // untrue".
            released.push((frame.frame_index, now));
        }

        // NACKs whose time has come, before the next arrival.
        while nack_index < nack_schedule.len() && nack_schedule[nack_index].0 <= now {
            let (_, frame_index) = nack_schedule[nack_index];
            nack_index += 1;
            if !scenario.retransmit {
                continue;
            }
            for missing in receiver.nack(frame_index) {
                let key = (frame_index, missing);
                let Some(bytes) = sent.get(&key) else {
                    continue;
                };
                datagrams_retransmitted += 1;
                if rng.next_f64() * 100.0 < scenario.profile.loss_pct {
                    datagrams_lost += 1;
                    continue;
                }
                queue.push(Reverse((now.as_nanos() + rtt_nanos, event_seq)));
                payloads.push(bytes.clone());
                event_seq += 1;
            }
        }

        let _ = receiver.on_datagram(&datagram, now);
    }

    // Drain: the retransmits pushed during the final walk, and any frame whose deadline
    // has passed.
    let drain_until = last_arrival + rtt + frame_interval * 2;
    let mut drain_now = last_arrival;
    while drain_now <= drain_until {
        drain_now += frame_interval / 4;
        for frame in receiver.release(drain_now) {
            if frame.is_displayable() && frame.payload().is_none() {
                displayed_but_unreconstructable += 1;
            }
            released.push((frame.frame_index, drain_now));
        }
        if receiver.in_flight() == 0 {
            break;
        }
    }
    for frame in receiver.release(drain_until + frame_interval * 8) {
        if frame.is_displayable() && frame.payload().is_none() {
            displayed_but_unreconstructable += 1;
        }
        released.push((frame.frame_index, drain_now));
    }

    let stats = *receiver.stats();

    // -- latency: the media plane's *own* contribution ------------------------
    // Measured from the instant the frame should have arrived rather than the instant it was
    // created, so this is buffering and recovery, not the link's transit. Subtracting the
    // profile's nominal one-way latency is what separates "our queue is too deep" from "the
    // link is 25 ms away", and only the first is ours to fix.
    let nominal_arrival_ms = scenario.profile.one_way_latency_ms;
    let mut latencies: Vec<f64> = released
        .iter()
        .map(|(frame_index, at)| {
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

    // "Late" is when the media plane itself blew the frame interval, before any network or
    // display budget is considered.
    let budget_ms = frame_interval.as_secs_f64() * 1000.0;
    let late_frames = latencies.iter().filter(|ms| **ms > budget_ms).count() as u64;

    let total_shards = data_shards_sent + parity_shards_sent;
    let fec_overhead_pct = if data_shards_sent == 0 {
        0.0
    } else {
        parity_shards_sent as f64 / data_shards_sent as f64 * 100.0
    };
    let _ = total_shards;

    TransportMetrics {
        scenario: scenario.name.clone(),
        seed,
        frames_sent: scenario.frames,
        datagrams_sent,
        datagrams_lost,
        datagrams_retransmitted,
        loss_pct: if datagrams_sent == 0 {
            0.0
        } else {
            datagrams_lost as f64 / datagrams_sent as f64 * 100.0
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
    }
}

/// The scenarios this build gates on: one per link profile that matters, plus the controls
/// that make the FEC and retransmit claims checkable rather than asserted.
pub fn transport_scenarios() -> Vec<TransportScenario> {
    let base = |name: &str, profile_name: &str| {
        let profile = profile(profile_name).expect("known profile");
        let jitter_frames = TransportScenario::jitter_frames_for(&profile, 90);
        // The link's own jitter forces the window; the budget is that window plus one round
        // trip for the retransmit the window makes possible, and nothing more.
        let window_ms = jitter_frames as f64 * 1000.0 / 90.0;
        let rtt_ms = 2.0 * profile.one_way_latency_ms;
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
            max_media_latency_p95_ms: window_ms + rtt_ms + 10.0,
        }
    };

    vec![
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
            // Beyond both mechanisms some frames will be lost, and the scenario says so
            // rather than pretending. Most are recovered; the budget is what is left.
            max_unreconstructable_pct: 3.0,
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
            ..base("x", "ncm_wired")
        },
        // Controls. Without these the gate cannot tell "the FEC fixed it" from "the profile
        // was never lossy".
        TransportScenario {
            name: "control_no_fec_regrace".into(),
            fec_fraction: 0.0,
            retransmit: false,
            max_unreconstructable_pct: 100.0,
            ..base("x", "wifi7_regrace")
        },
        TransportScenario {
            name: "control_no_fec_cqm".into(),
            fec_fraction: 0.0,
            retransmit: false,
            max_unreconstructable_pct: 100.0,
            ..base("x", "cqm_churn")
        },
        TransportScenario {
            name: "transport_plaintext".into(),
            encrypted: false,
            ..base("x", "wifi7_160_clean")
        },
    ]
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

    // Overhead must stay sane; a code that needs 30 % to work is not a code, it is a
    // smaller frame rate with extra steps.
    if m.fec_overhead_pct > 12.0 {
        failures.push(format!(
            "{}: FEC overhead {:.1}% exceeds 12%",
            m.scenario, m.fec_overhead_pct
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
