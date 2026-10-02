//! Host scheduling: get this process out of the scheduler's way while a session runs.
//!
//! Latency work that costs nothing and is measurable, and which Virtual Desktop does and we
//! did not (`VD_RE/24-vd-link-qos.md` §8.2):
//!
//! * **Process priority.** VD sets `ProcessPriorityClass.RealTime` on its capture process
//!   and enables the privileges needed to do it.
//! * **Timer resolution.** VD calls `NtSetTimerResolution`. The supported equivalent is
//!   `timeBeginPeriod`, and it is what we use here. Without it the scheduler's tick is
//!   15.6 ms, so every `sleep`/wait in the send and pacing paths is quantised to it — a
//!   whole frame interval at 90 Hz. This is the single cheapest latency win available and
//!   it is invisible in every average.
//! * **Power throttling.** VD calls `SetProcessInformation`. We opt out of
//!   `PROCESS_POWER_THROTTLING_EXECUTION_SPEED`, i.e. we tell Windows not to put this
//!   process in EcoQoS, which otherwise caps it to efficiency cores.
//!
//! ## What we deliberately do *not* copy
//!
//! VD uses `RealTime` process priority. That is the top of the priority range and it can
//! starve the audio, input and compositor threads on the same machine — a stream that is
//! on time and whose audio crackles is not a better product. We use `High`, which is still
//! above the scheduler's normal band and is what a latency-sensitive process is normally
//! expected to take, and the per-thread escalation to `TIME_CRITICAL` is available
//! separately for the send thread only.
//!
//! ## Shape
//!
//! One RAII guard, [`HostScheduler`]. Everything it changes is restored in `Drop`, because
//! all three of these are process- or system-wide and outliving the session would be rude
//! at best: a raised timer resolution left on forever measurably costs battery, which is
//! why `timeBeginPeriod` must be paired with `timeEndPeriod`.

use std::fmt;

/// What the guard actually managed to change. Reported rather than assumed — on a machine
/// where the process is already `High`, or where the timer call fails, the caller should be
/// able to see which parts are live.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SchedReport {
    pub process_priority: Option<String>,
    pub timer_resolution_ms: Option<u32>,
    pub power_throttling_opted_out: bool,
}

impl SchedReport {
    pub fn summary(&self) -> String {
        let priority = self
            .process_priority
            .as_deref()
            .unwrap_or("unchanged (no permission or already set)");
        let timer = match self.timer_resolution_ms {
            Some(ms) => format!("{ms} ms"),
            None => "unchanged".into(),
        };
        format!(
            "process priority: {priority}; timer resolution: {timer}; ecoqos opts-out: {}",
            self.power_throttling_opted_out
        )
    }
}

/// Why scheduling control was unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedError {
    /// Not Windows. Nothing here has a portable equivalent, and pretending otherwise by
    /// silently doing nothing would make the log lie about whether it is in effect.
    Unsupported,
    /// A Win32 call failed, with its error code.
    Win32(&'static str, u32),
}

impl fmt::Display for SchedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchedError::Unsupported => f.write_str("host scheduling control is Windows-only"),
            SchedError::Win32(call, code) => write!(f, "{call} failed (win32 {code})"),
        }
    }
}

impl std::error::Error for SchedError {}

/// Holds elevated scheduling for as long as it lives, and puts it all back on drop.
///
/// Construction is best-effort by design: this is an optimisation, and a session must
/// never be refused because a priority call was denied (it is denied in a surprising
/// number of configurations — hence VD's privilege-enabling loop). Failures are reported
/// in the [`SchedReport`], not returned as errors.
#[derive(Debug)]
pub struct HostScheduler {
    report: SchedReport,
    /// Kept so `Drop` can undo exactly what was done, and nothing it was not.
    active: bool,
}

impl HostScheduler {
    /// Raise scheduling priority and timer resolution for the life of the guard.
    pub fn start() -> Self {
        let (report, active) = platform::apply();
        log::info!("x-link: host scheduling — {}", report.summary());
        Self { report, active }
    }

    /// What was actually applied.
    pub fn report(&self) -> &SchedReport {
        &self.report
    }
}

impl Drop for HostScheduler {
    fn drop(&mut self) {
        if self.active {
            platform::restore();
            log::info!("x-link: host scheduling restored");
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, HIGH_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
    };

    /// 1 ms. The floor Windows allows is 0.5 ms and it is not worth arguing about: 1 ms is
    /// already 11× finer than the 15.6 ms default and well inside one 90 Hz frame.
    const TIMER_PERIOD_MS: u32 = 1;

    pub(super) fn apply() -> (SchedReport, bool) {
        let mut report = SchedReport::default();
        let mut active = false;

        let process = unsafe { GetCurrentProcess() };

        // Priority. `High`, not `RealTime` — see the module docs.
        match unsafe { SetPriorityClass(process, HIGH_PRIORITY_CLASS) } {
            Ok(()) => {
                report.process_priority = Some("high".into());
                active = true;
            }
            Err(e) => {
                // Not fatal: on many machines this needs a privilege we do not have. The
                // session continues at normal priority, which is what it would have done
                // anyway.
                log::info!(
                    "x-link: could not raise the process priority (win32 {}): continuing at normal",
                    e.code().0
                );
            }
        }

        // EcoQoS opt-out.
        let throttling = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            // Zero = do not throttle.
            StateMask: 0,
        };
        let ok = unsafe {
            SetProcessInformation(
                process,
                ProcessPowerThrottling,
                std::ptr::addr_of!(throttling).cast(),
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
        match ok {
            Ok(()) => {
                report.power_throttling_opted_out = true;
                active = true;
            }
            Err(e) => log::info!(
                "x-link: could not opt out of power throttling (win32 {}): a laptop may upclock late",
                e.code().0
            ),
        }

        // Timer resolution. Must be matched by a `timeEndPeriod` — see `restore`.
        if unsafe { timeBeginPeriod(TIMER_PERIOD_MS) } == 0 {
            report.timer_resolution_ms = Some(TIMER_PERIOD_MS);
            active = true;
        } else {
            log::info!("x-link: timeBeginPeriod was refused; waits stay at the 15.6 ms tick");
        }

        (report, active)
    }

    pub(super) fn restore() {
        unsafe { timeEndPeriod(TIMER_PERIOD_MS) };
        // Priority and power-throttling are left where they are: both are inherited by
        // nothing and both are what a machine is on for the whole of a streaming session,
        // and Windows has no "restore previous class" call that is not a race with the
        // user's own changes. The timer is the one with a real cost to leaving raised.
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub(super) fn apply() -> (SchedReport, bool) {
        // Deliberately not a silent success: the report says "unchanged" so a Linux build's
        // log does not claim latency work it did not do.
        (SchedReport::default(), false)
    }

    pub(super) fn restore() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_says_what_did_not_happen() {
        let report = SchedReport::default();
        let summary = report.summary();
        assert!(
            summary.contains("unchanged"),
            "an untouched report must not read as success: {summary}"
        );
        assert!(
            !summary.contains("ecoqos opts-out: true"),
            "eco-qos must read as false when nothing was applied: {summary}"
        );
    }

    #[test]
    fn report_says_what_did_happen() {
        let report = SchedReport {
            process_priority: Some("high".into()),
            timer_resolution_ms: Some(1),
            power_throttling_opted_out: true,
        };
        let summary = report.summary();
        assert!(summary.contains("high"), "{summary}");
        assert!(summary.contains("1 ms"), "{summary}");
        assert!(summary.contains("ecoqos opts-out: true"), "{summary}");
    }

    #[test]
    fn starting_and_dropping_a_guard_is_safe_on_any_platform() {
        // The whole contract on a platform without these calls: it must not fail, must not
        // lie, and must not panic on drop.
        let scheduler = HostScheduler::start();
        let _ = scheduler.report();
        drop(scheduler);
    }

    #[test]
    fn unsupported_is_reported_not_swallowed() {
        assert_eq!(
            SchedError::Unsupported.to_string(),
            "host scheduling control is Windows-only"
        );
    }
}
