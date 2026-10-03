//! Frame phase, deadline and the Roll decision — measured, not asserted.
//!
//! `x_transport::FrameScheduler` implements what `vrlink` calls `frmDeadline` plus
//! `CR Roll Norm/Double/Skip`. It is the client-side half of the latency-control surface this
//! project had none of, and of everything in that gap it is the piece that can be **fully exercised
//! without hardware**: it is a function of a timeline, and a timeline can be built.
//!
//! What this module adds over the scheduler's own unit tests is the link. Arrival times are
//! generated from the bench's own [`ImpairmentProfile`]s — the same deterministic jitter and the
//! same seeded generator `transport.rs` runs the media plane against — so the phase window this
//! closes over is the window a *named link* produces, not a number chosen to make the test pass.

use std::time::Duration;

use x_transport::{Disposition, FrameScheduler, SchedulerConfig, SchedulerStats, TimebaseOffset};

use crate::{ImpairmentProfile, Lcg, profile};

/// One run of the scheduler over a synthetic but link-derived timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulingMetrics {
    pub frames: u64,
    pub stats: SchedulerStats,
    /// Frames whose arrival was inside the phase window this scenario declared, as a percentage.
    pub inside_window_pct_x10: u64,
}

/// A window the server's frame phase is claimed to hold: arrival phase within ±`window_us` of the
/// target. Declared per scenario so a run can *fail* it.
#[derive(Debug, Clone, Copy)]
pub struct SchedulingScenario {
    pub name: &'static str,
    pub profile_name: &'static str,
    pub fps: u16,
    pub frames: u64,
    /// Half-width of the phase window the run is claimed to hold, in microseconds.
    pub window_us: i64,
    /// The **sender** stops for this many frames starting here: a stall, a decode rebuild, an
    /// encoder hiccup. The display keeps ticking.
    pub stall_after: Option<u64>,
    pub stall_frames: u64,
    /// The **client** misses this many display periods starting here: its own loop was blocked, as
    /// a compositor or a decoder rebuild blocks it. This is the only way a stale frame ends up in
    /// hand at a roll, and therefore the only way `Skip` is reached — which is why it is a separate
    /// knob rather than a bigger `stall_frames`.
    pub client_gap_after: Option<u64>,
    pub client_gap_frames: u64,
}

impl SchedulingScenario {
    pub fn frame_interval(&self) -> Duration {
        Duration::from_nanos(1_000_000_000 / self.fps as u64)
    }
}

pub fn scheduling_scenarios() -> Vec<SchedulingScenario> {
    vec![
        // A wired tether: micro-latency, sub-millisecond jitter. This is the window the design is
        // for, and the run that says the scheduler can hold it.
        SchedulingScenario {
            name: "schedule_ncm_wired",
            profile_name: "ncm_wired",
            fps: 90,
            frames: 90,
            window_us: 2_000,
            stall_after: None,
            stall_frames: 0,
            client_gap_after: None,
            client_gap_frames: 0,
        },
        // 6 GHz with a re-grace event: real jitter on a real link. A wider window, because the
        // point of this scenario is that the claim adapts to the link rather than to the test.
        SchedulingScenario {
            name: "schedule_wifi7_regrace",
            profile_name: "wifi7_regrace",
            fps: 90,
            frames: 90,
            window_us: 20_000,
            stall_after: None,
            stall_frames: 0,
            client_gap_after: None,
            client_gap_frames: 0,
        },
        // The stream stops for five frames and then resumes. The display does not stop, so the
        // scheduler must repeat the previous image for each of those periods and then pick the
        // stream back up.
        SchedulingScenario {
            name: "schedule_sender_stall",
            profile_name: "ncm_wired",
            fps: 90,
            frames: 60,
            window_us: 2_000,
            stall_after: Some(30),
            stall_frames: 5,
            client_gap_after: None,
            client_gap_frames: 0,
        },
        // The client's own loop misses three display periods while a frame is in hand. That frame
        // is now more than a whole period stale, and showing it would spend a period on the past.
        SchedulingScenario {
            name: "schedule_client_gap",
            profile_name: "ncm_wired",
            fps: 90,
            frames: 60,
            window_us: 2_000,
            stall_after: None,
            stall_frames: 0,
            client_gap_after: Some(30),
            client_gap_frames: 3,
        },
    ]
}

/// Drive the scheduler over the scenario's timeline.
pub fn run_scheduling(scenario: &SchedulingScenario, seed: u64) -> SchedulingMetrics {
    let profile: ImpairmentProfile = profile(scenario.profile_name).expect("known profile");
    let interval = scenario.frame_interval();
    let mut scheduler = FrameScheduler::new(SchedulerConfig::at_frame_interval(interval));
    // The client's clock is a long way from the server's, and the scheduler must not care.
    let timebase = Duration::from_secs(1_000_003);
    let mut offset = TimebaseOffset::new(Duration::from_secs(10));

    let mut rng = Lcg::new(seed);
    let mut inside_window: u64 = 0;

    for index in 0..scenario.frames {
        // The server's own timeline, in the server's clock, plus a fixed phase inside the period.
        let target_ms = index as f64 / scenario.fps as f64 * 1_000.0;
        let target = Duration::from_secs_f64(target_ms / 1_000.0);

        let stalled = scenario
            .stall_after
            .is_some_and(|after| index >= after && index < after + scenario.stall_frames);

        // The display keeps ticking through a stall; only the frames stop.
        let vsync = timebase + Duration::from_secs_f64(target_ms / 1_000.0);

        if !stalled {
            let jitter = (rng.next_f64() * 2.0 - 1.0) * profile.jitter_ms / 2.0;
            let arrival_ms = target_ms + profile.one_way_latency_ms + jitter;
            let arrived = timebase + Duration::from_secs_f64(arrival_ms / 1_000.0);

            // The two clocks are unrelated — the client's is a thousand seconds away from the
            // server's — and this is the only thing that joins them. Feeding a server-clock target
            // to a client-clock scheduler would make every frame look a quarter of an hour late,
            // which is exactly what the first run of this file did.
            offset.observe(arrived, target);
            let client_target = offset.to_client(target);

            if let Disposition::Queued = scheduler.frame_ready(arrived, client_target) {
                // Phase is measured **after** the clock correction, so what it reports is lateness
                // relative to the best-served frame rather than the offset between two machines.
                // Always non-negative, for the same reason the offset estimate is a minimum: a
                // frame cannot beat the best one, it can only miss it.
                let phase_us = (arrived.as_micros() as i64) - (client_target.as_micros() as i64);
                if phase_us.abs() <= scenario.window_us {
                    inside_window += 1;
                }
            }
        }

        // One display period, whether or not a frame arrived for it — unless the client's own
        // loop is the thing that stopped.
        let client_gap = scenario
            .client_gap_after
            .is_some_and(|after| index >= after && index < after + scenario.client_gap_frames);
        if !client_gap {
            scheduler.roll(vsync);
        }
    }

    SchedulingMetrics {
        frames: scenario.frames,
        stats: *scheduler.stats(),
        inside_window_pct_x10: (inside_window * 1_000)
            .checked_div(scenario.frames)
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(name: &str) -> SchedulingMetrics {
        let scenario = scheduling_scenarios()
            .into_iter()
            .find(|s| s.name == name)
            .expect("known scenario");
        run_scheduling(&scenario, 7)
    }

    #[test]
    fn a_wired_link_holds_the_phase_window() {
        let m = metrics("schedule_ncm_wired");
        assert!(
            m.inside_window_pct_x10 >= 950,
            "only {} per mille of frames landed inside a 2 ms window on a wired link: {}\n{}",
            m.inside_window_pct_x10,
            m.stats.summary(),
            m.stats.summary(),
        );
    }

    #[test]
    fn every_period_on_a_wired_link_with_no_loss_is_a_fresh_frame() {
        let m = metrics("schedule_ncm_wired");
        assert_eq!(
            m.stats.skip,
            0,
            "a clean wired link must never discard a frame for staleness: {}",
            m.stats.summary()
        );
        assert_eq!(m.stats.dropped_on_arrival, 0);
        assert!(m.stats.fresh_fraction() > 0.99, "{}", m.stats.summary());
    }

    /// The assertion the plan asks for, in as many words: *a Skip/Double decision was taken under
    /// a stall*. Without this the three-way decision is a type nothing has ever driven into its
    /// second branch.
    #[test]
    fn a_sender_stall_makes_the_client_repeat_rather_than_freeze() {
        let clean = metrics("schedule_ncm_wired");
        let stalled = metrics("schedule_sender_stall");

        assert!(
            stalled.stats.double > clean.stats.double,
            "a five-frame sender stall produced no repeats: {} vs {}",
            stalled.stats.summary(),
            clean.stats.summary()
        );
        assert!(
            stalled.stats.norm > 40,
            "the stream never resumed after the stall: {}",
            stalled.stats.summary()
        );
        assert!(
            stalled.stats.fresh_fraction() > 0.85,
            "after five stalled frames the client never got back to fresh frames: {}",
            stalled.stats.summary()
        );
    }

    /// The other half of the same claim. A frame that is in hand when the client's own loop misses
    /// periods must be **discarded**, not shown a period late: showing it spends this period on the
    /// past, and keeping it puts a stale frame in front of the fresh ones behind it.
    #[test]
    fn a_client_gap_discards_the_stale_frame_rather_than_showing_the_past() {
        let m = metrics("schedule_client_gap");

        assert!(
            m.stats.skip >= 1,
            "three missed display periods did not produce a single skip: {}",
            m.stats.summary()
        );
        // And the periods that were missed are simply not decided — a gap is a gap, not a stale
        // frame shown three times.
        assert_eq!(
            m.stats.norm + m.stats.double + m.stats.skip + m.stats.idle + 3,
            m.frames,
            "the three missed periods were accounted as decisions: {}",
            m.stats.summary()
        );
    }

    /// The finding this scenario exists to make explicit, and it is worth stating even though it is
    /// a negative result: **a marginal 6 GHz link at 90 Hz cannot hold a frame period.** The
    /// re-grace profile carries 15 ms of jitter against an 11.1 ms period, so frames arrive a whole
    /// period late, and no scheduler can fix that — the link has to deliver less often, or the rate
    /// has to come down. What the client *can* do is say so, and this asserts that it does.
    #[test]
    fn a_link_wider_than_a_frame_period_is_reported_rather_than_hidden() {
        let m = metrics("schedule_wifi7_regrace");

        assert!(
            m.stats.phase_samples > 0,
            "no arrival phase was measured at all: {}",
            m.stats.summary()
        );
        assert!(
            m.stats.double > 0 || m.stats.dropped_on_arrival > 0,
            "a profile with 15 ms of jitter at 90 Hz produced no repeats and dropped nothing, \
             which cannot be right: {}",
            m.stats.summary()
        );
        assert!(
            m.stats.phase_spread_us() > 5_000,
            "the measured phase spread does not reflect the profile's jitter: {}",
            m.stats.summary()
        );
    }

    #[test]
    fn a_run_is_exactly_reproducible() {
        let scenario = scheduling_scenarios()
            .into_iter()
            .find(|s| s.name == "schedule_wifi7_regrace")
            .unwrap();
        assert_eq!(
            run_scheduling(&scenario, 11),
            run_scheduling(&scenario, 11),
            "the same seed and the same link must give the same decisions"
        );
    }

    #[test]
    fn the_roll_decisions_are_reported_separately_from_the_phase() {
        let m = metrics("schedule_sender_stall");
        let line = m.stats.summary();
        for needle in ["norm", "double", "skip", "arrival phase"] {
            assert!(
                line.contains(needle),
                "{line:?} does not mention {needle:?}"
            );
        }
    }

    #[test]
    fn the_roll_decisions_add_up_to_the_periods_that_were_decided() {
        let m = metrics("schedule_sender_stall");
        assert_eq!(
            m.stats.norm + m.stats.double + m.stats.skip + m.stats.idle,
            m.frames,
            "every display period must be answered with exactly one decision: {}",
            m.stats.summary()
        );
    }
}
