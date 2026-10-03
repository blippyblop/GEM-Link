//! Frame phase, frame deadline, and the present/hold/skip decision.
//!
//! This is the gap the reference client exposed. Valve's `vrlink` has `cFramePhase`, `sFramePhase`,
//! `targetPhaseInFrame` and `frmDeadline` — the server targets *where in the display period* to
//! deliver a frame, and the client decides per frame whether to show it, show the previous one
//! again, or throw it away (`CR Roll Norm`, `CR Roll Double`, `CR Roll Skip`). GemLink had none of
//! it: "send it now, and hold if it breaks" is a strictly worse position on the one metric this
//! project is scored on, and no amount of throughput compensates for not deciding *when* a frame is
//! shown.
//!
//! ## The model, in one paragraph
//!
//! The server stamps every frame with a target time in **its** clock. The client displays on
//! **its** clock. Nothing joins the two except the observation of when frames actually arrive, and
//! that observation is asymmetric in a way that makes it usable: network delay can only make a
//! frame *later* than its target, never earlier. So the running minimum of `arrival - target` is an
//! estimate of the offset, and [`TimebaseOffset`] maintains it. With a deadline in the client's own
//! clock, each display period becomes a single three-way question, and that is [`FrameScheduler`].
//!
//! ## Why "Skip" is not the same as "Double"
//!
//! `Double` shows the previous frame again and **keeps** what is waiting: it is not in time for this
//! period, but it will be in time for the next one. `Skip` shows the previous frame again and
//! **discards** what is waiting, because it is more than a whole period stale and showing it would
//! mean showing the past. Without `Skip` a client that falls behind stays behind — every frame it
//! finally presents is older than the last one it should have presented — and that is a queue, which
//! is latency, which is the thing this project refuses to spend.

use std::time::Duration;

/// What the display path should do this period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Roll {
    /// Show the frame that is ready. Its deadline has not passed: the stream is in time.
    Norm,
    /// Show the previous frame again. Either nothing was ready, or what is ready is better shown
    /// next period than shown late. Nothing is discarded.
    Double,
    /// Show the previous frame again and **discard** what is ready: it is more than a whole period
    /// stale, and showing it would spend a display period on the past.
    Skip,
    /// Nothing to show and nothing ever shown: the first frames have not arrived yet.
    Idle,
}

/// What happened to a frame the moment it became decodable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Held for the next display period.
    Queued,
    /// Discarded on arrival: it was *already* past its deadline when it got here. Only shows up when
    /// the client is decoding slower than the stream arrives, which is exactly the condition
    /// `doc 50 §A10` describes.
    DroppedLate,
}

/// Maps the server's frame timestamps onto the client's clock.
///
/// The estimate is the **minimum** of `arrival - target`, decayed slowly so it can recover from a
/// one-off spike. Minimum is the right statistic and not a heuristic: a frame cannot arrive before
/// its target unless the clocks disagree, and any delay can only push it later, so the smallest
/// difference seen is the closest thing to a clock-offset measurement that a one-way stream can give.
#[derive(Debug, Clone)]
pub struct TimebaseOffset {
    offset: Duration,
    /// How long a minimum is trusted. Long, because the quantity is stable and a jittery estimator
    /// here becomes jitter in every deadline downstream.
    hold: Duration,
    observed_at: Option<Duration>,
}

impl TimebaseOffset {
    /// `hold` is how long the current minimum is kept before a newer observation may replace it.
    pub fn new(hold: Duration) -> Self {
        Self {
            offset: Duration::ZERO,
            hold,
            observed_at: None,
        }
    }

    /// Record that a frame with server-time `target` arrived at client-time `arrival`.
    pub fn observe(&mut self, arrival: Duration, target: Duration) {
        let difference = arrival.saturating_sub(target);

        let should_replace = match self.observed_at {
            None => true,
            Some(seen) => {
                // A smaller difference is always taken: it is a better clock observation.
                difference < self.offset
                    // Or the current minimum has been held long enough that it may be stale — a
                    // held minimum from a lucky burst would otherwise pin every deadline early.
                    || arrival.saturating_sub(seen) > self.hold
            }
        };

        if should_replace {
            self.offset = difference;
            self.observed_at = Some(arrival);
        }
    }

    /// The current estimate, in client time, of where the server's zero is.
    pub fn offset(&self) -> Duration {
        self.offset
    }

    pub fn is_initialised(&self) -> bool {
        self.observed_at.is_some()
    }

    /// Translate a server timestamp into the client's clock.
    pub fn to_client(&self, target: Duration) -> Duration {
        target + self.offset
    }

    /// Translate this end's clock into the stream's — the inverse of [`Self::to_client`].
    ///
    /// The *sending* end needs this and not the other direction: its frames carry a target time in
    /// the stream's clock, and its clock is its own, so "how stale is this frame" is
    /// `from_local(now) - target`. Without the correction the two clocks are unrelated numbers and
    /// every deadline check is nonsense.
    pub fn from_local(&self, local: Duration) -> Duration {
        local.saturating_sub(self.offset)
    }
}

/// The knobs. Only two, and both are facts about the device rather than tuning parameters.
#[derive(Debug, Clone, Copy)]
pub struct SchedulerConfig {
    /// One display period. At 90 Hz this is 11.11 ms; the project's budget is "0 % of frames over
    /// this", so it is the number every decision below is compared against.
    pub frame_interval: Duration,
    /// Where in the period the server aims to deliver (`targetPhaseInFrame`). Not used to *decide*
    /// anything — it is the reference the arrival phase is measured against, so the client can say
    /// whether the server is delivering where it said it would.
    pub target_phase: Duration,
}

impl SchedulerConfig {
    pub fn at_frame_interval(frame_interval: Duration) -> Self {
        Self {
            frame_interval,
            target_phase: Duration::ZERO,
        }
    }

    /// The last moment at which a frame aimed at `target` is still **the frame for period
    /// `target`**.
    ///
    /// There is no separate "deadline" knob, and there should not be: a frame is late exactly when
    /// the display has moved on to the period after the one it was aimed at, and that threshold is
    /// the period itself. Making it tunable would make it possible to run a client whose idea of
    /// "late" has nothing to do with its display.
    pub fn usable_until(&self, target: Duration) -> Duration {
        target + self.frame_interval
    }
}

/// What the scheduler has decided, over a session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SchedulerStats {
    pub norm: u64,
    pub double: u64,
    pub skip: u64,
    pub idle: u64,
    /// Frames discarded the instant they became decodable.
    pub dropped_on_arrival: u64,
    /// Arrival phase relative to the target, in microseconds: **positive is late**. This is the
    /// client's evidence about the server's frame phase, and the number that says whether a
    /// deadline change actually moved anything.
    pub phase_samples: u64,
    pub phase_min_us: i64,
    pub phase_max_us: i64,
    pub phase_sum_us: i64,
}

impl SchedulerStats {
    fn record_phase(&mut self, phase_us: i64) {
        if self.phase_samples == 0 {
            self.phase_min_us = phase_us;
            self.phase_max_us = phase_us;
        } else {
            self.phase_min_us = self.phase_min_us.min(phase_us);
            self.phase_max_us = self.phase_max_us.max(phase_us);
        }
        self.phase_sum_us += phase_us;
        self.phase_samples += 1;
    }

    /// Mean arrival phase in microseconds; `None` before any frame has been measured.
    pub fn phase_mean_us(&self) -> Option<f64> {
        (self.phase_samples > 0).then(|| self.phase_sum_us as f64 / self.phase_samples as f64)
    }

    /// The spread of arrival phase, which is the quantity a *phase window* is a claim about. A mean
    /// on target with a 20 ms spread is not a controlled stream.
    pub fn phase_spread_us(&self) -> i64 {
        if self.phase_samples == 0 {
            0
        } else {
            self.phase_max_us - self.phase_min_us
        }
    }

    /// Fraction of periods answered with a fresh frame. `1.0` means never a repeat.
    pub fn fresh_fraction(&self) -> f64 {
        let decided = self.norm + self.double + self.skip;
        if decided == 0 {
            1.0
        } else {
            self.norm as f64 / decided as f64
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "frame scheduling: {} norm, {} double, {} skip, {} idle ({} % fresh); arrival phase \
             mean {} us over {} frame(s), spread {} us, {} dropped on arrival",
            self.norm,
            self.double,
            self.skip,
            self.idle,
            (self.fresh_fraction() * 100.0 * 10.0).round() / 10.0,
            self.phase_mean_us()
                .map_or("n/a".to_owned(), |mean| format!("{mean:.0}")),
            self.phase_samples,
            self.phase_spread_us(),
            self.dropped_on_arrival,
        )
    }
}

/// A frame that has been decoded and is waiting for a display period.
#[derive(Debug)]
struct Ready {
    /// The client-clock time the server aimed this frame at.
    target: Duration,
}

/// Decides, once per display period, what to put on screen.
///
/// Deliberately clock-driven and free of any notion of "now": every method takes the time it is
/// being asked about. That is what lets the whole thing be tested against a synthetic timeline —
/// which is the only way to test a latency controller, because a real one is too slow to iterate on
/// and too noisy to read.
#[derive(Debug)]
pub struct FrameScheduler {
    config: SchedulerConfig,
    ready: Option<Ready>,
    /// Whether anything has ever been shown. Not an index: what matters to the decision below is
    /// only whether repeating the previous image means anything yet.
    on_screen: bool,
    stats: SchedulerStats,
}

impl FrameScheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            ready: None,
            on_screen: false,
            stats: SchedulerStats::default(),
        }
    }

    pub fn stats(&self) -> &SchedulerStats {
        &self.stats
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// The server's frame phase was changed under us; the reference does this at runtime
    /// (`Change frame phase of when waitgetposes is requested.`).
    pub fn set_target_phase(&mut self, target_phase: Duration) {
        // Clamped into the period: a phase outside it names a frame that is not this frame.
        self.config.target_phase = target_phase.min(self.config.frame_interval);
    }

    /// A frame became decodable at `arrived`, targeted at client-clock `target`.
    ///
    /// The target belongs to the *previous* frame once a newer one exists, so a late frame arriving
    /// does not displace a good one that is already queued: the older one is the one that is late.
    pub fn frame_ready(&mut self, arrived: Duration, target: Duration) -> Disposition {
        // Phase is measured against the target the server named, so it is a statement about the
        // server rather than about us. Positive means late, which is the only direction a network
        // can push it — which is what makes the running minimum usable as a clock estimate.
        let phase_us = if target >= arrived {
            -((target - arrived).as_micros() as i64)
        } else {
            (arrived - target).as_micros() as i64
        };
        self.stats.record_phase(phase_us);

        if arrived > self.config.usable_until(target) {
            // Its own period is already over. Queueing it would put a stale frame in front of the
            // fresh ones behind it, and every later decision would inherit the error.
            self.stats.dropped_on_arrival += 1;
            return Disposition::DroppedLate;
        }

        match &self.ready {
            // Keep the older frame: it has the earlier target, and it is the one that would be
            // wasted by waiting. A frame arriving out of order must not displace its predecessor.
            Some(existing) if existing.target <= target => Disposition::DroppedLate,
            _ => {
                self.ready = Some(Ready { target });
                Disposition::Queued
            }
        }
    }

    /// The compositor is about to present at `vsync`. Decide what to show.
    ///
    /// The three-way question, in the order it is asked: is what is waiting still in time (**Norm**);
    /// if not, is it so stale that showing it would spend a period on the past (**Skip**); if not,
    /// can it take the next period instead (**Double**). A period with nothing waiting at all is a
    /// **Double** — the previous image stays — unless nothing has ever been shown, which is
    /// **Idle**.
    pub fn roll(&mut self, vsync: Duration) -> Roll {
        let Some(ready) = self.ready.take() else {
            if self.on_screen {
                self.stats.double += 1;
                return Roll::Double;
            }
            self.stats.idle += 1;
            return Roll::Idle;
        };

        if vsync <= self.config.usable_until(ready.target) {
            self.on_screen = true;
            self.stats.norm += 1;
            return Roll::Norm;
        }

        // Its period has gone by and it is still in hand: showing it now would spend this period on
        // the period before last, and keeping it would leave a stale frame in front of the fresh
        // ones behind it. Discard it — that is the whole of `Skip`, and it is what stops a client
        // that has fallen behind from staying behind.
        self.stats.skip += 1;
        Roll::Skip
    }

    /// Nothing has ever been shown and nothing is waiting.
    pub fn is_idle(&self) -> bool {
        self.ready.is_none() && !self.on_screen
    }

    /// Whether a frame is queued for the next period.
    pub fn has_ready_frame(&self) -> bool {
        self.ready.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD_MS: u64 = 11; // 90 Hz, rounded to a millisecond.

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn scheduler() -> FrameScheduler {
        FrameScheduler::new(SchedulerConfig::at_frame_interval(ms(PERIOD_MS)))
    }

    #[test]
    fn a_frame_that_is_in_time_is_shown_immediately() {
        let mut scheduler = scheduler();
        // Target 10 ms from now, arrived 2 ms before its deadline.
        assert_eq!(scheduler.frame_ready(ms(10), ms(18)), Disposition::Queued);
        assert_eq!(scheduler.roll(ms(11)), Roll::Norm);
        assert_eq!(scheduler.stats().norm, 1);
        assert_eq!(scheduler.stats().fresh_fraction(), 1.0);
    }

    #[test]
    fn with_nothing_ready_the_previous_frame_is_shown_again() {
        let mut scheduler = scheduler();
        scheduler.frame_ready(ms(10), ms(18));
        assert_eq!(scheduler.roll(ms(11)), Roll::Norm);

        // The next period arrives with no frame behind it. The picture must not go away.
        assert_eq!(scheduler.roll(ms(22)), Roll::Double);
        assert_eq!(scheduler.stats().double, 1);
        assert_eq!(scheduler.stats().norm, 1);
    }

    #[test]
    fn a_frame_more_than_a_period_stale_is_skipped_rather_than_shown_late() {
        let mut scheduler = scheduler();
        scheduler.frame_ready(ms(10), ms(18));
        assert_eq!(scheduler.roll(ms(11)), Roll::Norm);

        // A frame targeted at 18 ms is the frame for 18..29 ms. Presenting at 41 ms is a whole
        // period later than that: showing it would be showing the past.
        scheduler.frame_ready(ms(18), ms(18));
        assert_eq!(scheduler.roll(ms(41)), Roll::Skip);
        assert_eq!(scheduler.stats().skip, 1);
        assert!(
            !scheduler.has_ready_frame(),
            "a skipped frame must be discarded, not kept for the next period — keeping it is how a \
             client that is behind stays behind"
        );
    }

    #[test]
    fn a_frame_late_by_less_than_a_period_is_still_shown() {
        let mut scheduler = scheduler();
        // Targeted at 18 ms, the frame owns 18..29 ms. Shown at 25 ms it is late by 7 ms — inside
        // its own period, so it is still the right frame for this period and worth showing.
        scheduler.frame_ready(ms(18), ms(18));
        assert_eq!(scheduler.roll(ms(25)), Roll::Norm);
        assert_eq!(scheduler.stats().skip, 0);
    }

    #[test]
    fn a_frame_already_past_its_deadline_is_refused_at_arrival() {
        let mut scheduler = scheduler();
        // Targeted at 10 ms, so it owns 10..21 ms, and it arrives at 30 ms: the decoder was behind
        // by two whole periods. It must never enter the queue.
        assert_eq!(
            scheduler.frame_ready(ms(30), ms(10)),
            Disposition::DroppedLate
        );
        assert_eq!(scheduler.stats().dropped_on_arrival, 1);
        assert!(
            !scheduler.has_ready_frame(),
            "a frame that arrives after its own deadline must never enter the queue"
        );
    }

    #[test]
    fn nothing_to_show_and_nothing_shown_is_idle_rather_than_a_repeat() {
        let mut scheduler = scheduler();
        assert_eq!(scheduler.roll(ms(11)), Roll::Idle);
        assert_eq!(scheduler.stats().idle, 1);
        assert_eq!(scheduler.stats().double, 0);
    }

    /// The measurement the frame-phase surface exists for: where in the display period frames
    /// actually arrive, and how tightly.
    #[test]
    fn arrival_phase_is_measured_against_the_server_s_targets() {
        let mut scheduler = scheduler();

        // Arriving 2 ms early, right on target, and 3 ms late.
        scheduler.frame_ready(ms(8), ms(10));
        scheduler.frame_ready(ms(20), ms(20));
        scheduler.frame_ready(ms(24), ms(21));

        let stats = scheduler.stats();
        assert_eq!(stats.phase_samples, 3);
        assert_eq!(
            stats.phase_min_us, -2_000,
            "2 ms early must read as negative"
        );
        assert_eq!(stats.phase_max_us, 3_000, "3 ms late must read as positive");
        assert_eq!(
            stats.phase_spread_us(),
            stats.phase_max_us - stats.phase_min_us
        );
        // The mean is what a phase change moves; the spread is what a phase *window* is a claim
        // about, and the two must be reported separately.
        assert!(stats.phase_mean_us().unwrap().abs() < 1_000.0);
    }

    #[test]
    fn a_frame_phase_change_moves_what_the_client_measures() {
        let mut early = scheduler();
        let mut late = scheduler();

        // The same 2 ms of arrival jitter, against two different target phases.
        for step in 0..10u64 {
            early.frame_ready(ms(step * 11 + 9), ms(step * 11 + 11));
            late.frame_ready(ms(step * 11 + 9), ms(step * 11 + 13));
        }

        let early_mean = early.stats().phase_mean_us().unwrap();
        let late_mean = late.stats().phase_mean_us().unwrap();
        assert!(
            late_mean < early_mean,
            "moving the server's target later must move the measured arrival phase earlier \
             relative to it ({late_mean} vs {early_mean})"
        );
    }

    #[test]
    fn the_target_phase_is_clamped_into_the_period() {
        let mut scheduler = scheduler();
        scheduler.set_target_phase(ms(1_000));
        assert_eq!(scheduler.config().target_phase, ms(PERIOD_MS));
    }

    #[test]
    fn the_timebase_offset_is_the_smallest_difference_ever_seen() {
        let mut offset = TimebaseOffset::new(Duration::from_secs(60));

        // The client's clock is 100 s ahead of the server's.
        offset.observe(Duration::from_millis(100_050), Duration::from_millis(50));
        assert_eq!(offset.offset(), Duration::from_millis(100_000));

        // A delayed frame is later, never earlier: it must not move the estimate.
        offset.observe(Duration::from_millis(100_200), Duration::from_millis(50));
        assert_eq!(offset.offset(), Duration::from_millis(100_000));

        // A better observation is taken.
        offset.observe(Duration::from_millis(100_040), Duration::from_millis(50));
        assert_eq!(offset.offset(), Duration::from_millis(99_990));

        assert_eq!(
            offset.to_client(Duration::from_millis(1_000)),
            Duration::from_millis(100_990)
        );
    }

    #[test]
    fn the_summary_names_every_decision() {
        let mut scheduler = scheduler();
        scheduler.frame_ready(ms(10), ms(18));
        scheduler.roll(ms(11)); // Norm
        scheduler.roll(ms(22)); // Double: nothing arrived
        scheduler.frame_ready(ms(22), ms(22));
        scheduler.roll(ms(40)); // Skip: that frame's period is long gone

        let line = scheduler.stats().summary();
        for needle in ["1 norm", "1 double", "1 skip", "arrival phase"] {
            assert!(
                line.contains(needle),
                "{line:?} does not mention {needle:?}"
            );
        }
    }
}
