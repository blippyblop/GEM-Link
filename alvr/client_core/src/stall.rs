//! The stall ladder: how long a client waits, and what it does at each threshold.
//!
//! One ladder, used by both ends of the video path. The decoder watches for the device going quiet
//! ([`crate::video_decoder`]); the receive loop watches for the display going stale. They are the
//! same shape, the same thresholds, and the same failure if they disagree — so they are one type,
//! and the thresholds are named once.
//!
//! ## Where the numbers come from
//!
//! Valve's client for this device, whose decoder class is `SVLCodecV4L2` and whose log strings are
//! quoted verbatim below (see `VD_RE/52-frame-vrlink-client.md`):
//!
//! ```text
//! SVLCodecV4L2::CheckStuck: > 300ms between our last forward progress. Asking remote side for a new IFrame
//! SVLCodecV4L2::CheckStuck: > 800ms between our last forward progress. Reset.
//! SVLCodecV4L2::HardReset: Called!
//! ```
//!
//! **Two rungs, and GemLink only ever had the first.** `x_transport::TrustGate` holds an untrusted
//! frame and asks for a keyframe — that is rung one, and it is correct. But it has no upper bound:
//! a hold whose keyframe never arrives is a hold that never ends. That is not a hypothetical, it is
//! the black screen this project shipped on its first hardware run and spent a session finding. The
//! reference client does not have that failure mode because at 800 ms it stops waiting and rebuilds.
//!
//! Each rung fires **once per stall**, which is what keeps a stall from becoming a control-plane
//! flood: a per-poll decision would emit a keyframe request every frame the stall lasted.

use std::time::{Duration, Instant};

/// No forward progress for this long: ask the sender for a keyframe.
pub const ASK_FOR_KEYFRAME_AFTER: Duration = Duration::from_millis(300);
/// No forward progress for this long: stop waiting, tear down and rebuild.
pub const HARD_RESET_AFTER: Duration = Duration::from_millis(800);

/// What to do about the passage of time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckAction {
    /// Forward progress is recent enough. Nothing to do.
    Progress,
    /// No progress for [`ASK_FOR_KEYFRAME_AFTER`]. Ask the sender for a keyframe.
    AskForKeyframe,
    /// No progress for [`HARD_RESET_AFTER`]. Asking is not working. Rebuild.
    Reset,
}

/// Watches a pipeline for a stall.
///
/// "Forward progress" is whatever the pipeline produces: a decoded frame, a presented frame, a
/// format change. It is deliberately the caller's definition, because the caller is the only one
/// that knows what progress means in its half of the path.
#[derive(Debug, Clone)]
pub struct StuckDetector {
    last_progress: Instant,
    asked: bool,
    reset: bool,
}

impl StuckDetector {
    pub fn new(now: Instant) -> Self {
        Self {
            last_progress: now,
            asked: false,
            reset: false,
        }
    }

    /// Call on any forward progress. Re-arms both rungs.
    pub fn progress(&mut self, now: Instant) {
        self.last_progress = now;
        self.asked = false;
        self.reset = false;
    }

    /// Call every poll. Returns at most one action per rung per stall.
    pub fn poll(&mut self, now: Instant) -> StuckAction {
        let stalled = now.saturating_duration_since(self.last_progress);

        if stalled >= HARD_RESET_AFTER && !self.reset {
            self.reset = true;
            // A poll interval longer than the gap must not emit an ask that is already too late.
            self.asked = true;
            return StuckAction::Reset;
        }
        if stalled >= ASK_FOR_KEYFRAME_AFTER && !self.asked {
            self.asked = true;
            return StuckAction::AskForKeyframe;
        }

        StuckAction::Progress
    }

    /// Time since the last forward progress, for logging.
    pub fn stalled_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_progress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_stall_asks_for_a_keyframe_once_and_then_resets() {
        // The two rungs, from the reference client's own log lines.
        let start = t0();
        let mut stuck = StuckDetector::new(start);

        assert_eq!(stuck.poll(start), StuckAction::Progress);
        assert_eq!(
            stuck.poll(start + Duration::from_millis(299)),
            StuckAction::Progress,
            "asked before the 300 ms rung"
        );
        assert_eq!(
            stuck.poll(start + Duration::from_millis(300)),
            StuckAction::AskForKeyframe
        );
        // Everything between the rungs is silent. This is the property that keeps a stall from
        // becoming a control-plane flood.
        for ms in 301..800 {
            assert_eq!(
                stuck.poll(start + Duration::from_millis(ms)),
                StuckAction::Progress,
                "re-asked at {ms} ms"
            );
        }
        assert_eq!(
            stuck.poll(start + Duration::from_millis(800)),
            StuckAction::Reset
        );
        for ms in 801..2000 {
            assert_eq!(
                stuck.poll(start + Duration::from_millis(ms)),
                StuckAction::Progress,
                "re-reset at {ms} ms"
            );
        }
    }

    #[test]
    fn progress_rearms_both_rungs() {
        let start = t0();
        let mut stuck = StuckDetector::new(start);

        let keyframe = start + ASK_FOR_KEYFRAME_AFTER;
        assert_eq!(stuck.poll(keyframe), StuckAction::AskForKeyframe);
        stuck.progress(keyframe);
        assert_eq!(stuck.poll(keyframe), StuckAction::Progress);

        let second = keyframe + Duration::from_millis(1);
        assert_eq!(
            stuck.poll(second + ASK_FOR_KEYFRAME_AFTER),
            StuckAction::AskForKeyframe,
            "a second stall did not ask for a keyframe"
        );
    }

    #[test]
    fn a_stall_that_jumps_both_thresholds_resets_rather_than_asking() {
        let start = t0();
        let mut stuck = StuckDetector::new(start);
        assert_eq!(
            stuck.poll(start + Duration::from_secs(5)),
            StuckAction::Reset
        );
        assert_eq!(
            stuck.poll(start + Duration::from_secs(6)),
            StuckAction::Progress
        );
    }

    #[test]
    fn the_rungs_are_the_reference_clients_numbers() {
        // If these drift, the comment above stops being true. Cheap to assert, and the whole
        // reason the ladder is one type rather than two copies of a constant.
        assert_eq!(ASK_FOR_KEYFRAME_AFTER, Duration::from_millis(300));
        assert_eq!(HARD_RESET_AFTER, Duration::from_millis(800));
        assert!(HARD_RESET_AFTER > ASK_FOR_KEYFRAME_AFTER);
    }
}
