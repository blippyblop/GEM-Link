//! Adapting the FEC ratio to the loss actually observed.
//!
//! `ParityPolicy` chooses a fixed overhead: `Ratio { fraction: 0.05 }` means "5 % parity, always".
//! That is the right *default* and the wrong *setting*. A link losing 8 % of datagrams needs more
//! than 5 % parity or it fails, and a clean link paying 5 % is paying for nothing — at 300 Mbps /
//! 90 Hz that is real budget spent on shards that are never used.
//!
//! The reference client solves this the same way: it renegotiates the code rate from the loss it is
//! measuring. Its symbol is `SVLDataLink::HandleFECChange( %d )`, and it re-requests a frame it
//! could not rebuild rather than letting the loss accumulate silently.
//!
//! ## The shape of the controller, and why exactly this one
//!
//! **Fast up, slow down, and a deadband between.** Each of those is load-bearing:
//!
//! - **Fast up.** Parity you needed and did not have cost you a frame. The next window has to be
//!   protected, so a loss observation raises the ratio immediately — one step, per window.
//! - **Slow down.** Parity you have and do not need costs bandwidth, which is recoverable. A single
//!   clean window proves very little (loss is bursty — the bench found that i.i.d. loss is kinder
//!   than reality), so lowering takes several consecutive clean windows.
//! - **A deadband.** Without it, a link hovering at the threshold oscillates: raise, clean, lower,
//!   raise. Every transition changes the code rate, which means the receiver must be told, which
//!   means a control round trip — the cure would cost more than the disease. Inside the band,
//!   nothing happens and nothing is spent.
//!
//! This is a deliberately small state machine: it has no clock, takes one number per window, and
//! every transition is assertable in a unit test. Anything cleverer could not be validated in the
//! bench, and an FEC controller that cannot be validated is a latency regression waiting to happen.

use crate::packetizer::ParityPolicy;

/// Tuning. Defaults are chosen to be safe rather than optimal, and every one of them is a knob a
/// bench scenario can move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdaptiveConfig {
    /// Where the ratio starts. Matches the static default so behaviour without adaptation is
    /// unchanged for the first window.
    pub initial: f32,
    /// Never go below this.
    pub min: f32,
    /// Never go above this. A ratio is overhead on every byte; past this it is cheaper to lower
    /// the source rate than to keep buying repair shards.
    pub max: f32,
    /// Multiplicative step upward. Multiplicative, not additive, because the useful ratio scales
    /// with the loss: 1 % -> 2 % is one doubling, and so is 8 % -> 16 %.
    pub up_factor: f32,
    /// Multiplicative step downward, applied only after a run of clean windows.
    pub down_factor: f32,
    /// Measured loss above this raises the ratio.
    pub high_water: f64,
    /// Measured loss below this (sustained) lowers it.
    pub low_water: f64,
    /// Consecutive clean windows required before lowering. This is the "slow down" half.
    pub clean_windows: u32,
}

impl Default for AdaptiveConfig {
    fn default() -> Self {
        Self {
            initial: 0.05,
            min: 0.0,
            // 50 % overhead is the ceiling: beyond it the code rate is below 1.5:1 and the honest
            // move is to send fewer pixels, not more shards.
            max: 0.5,
            up_factor: 1.6,
            down_factor: 0.8,
            // A window at or below 0.5 % loss is "clean"; above 2 % it needs more parity. Between
            // them, do nothing — that band is what stops the oscillation.
            high_water: 0.02,
            low_water: 0.005,
            clean_windows: 8,
        }
    }
}

/// Below this a ratio is treated as zero. A ratio that is not actually zero costs a shard per
/// frame under the ceil policy, so "almost off" is not a meaningful state.
const NEGLIGIBLE_RATIO: f32 = 1e-4;

/// What the controller decided, so a caller can log it or put it on the wire.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RatioChange {
    Unchanged,
    Raised { from: f32, to: f32 },
    Lowered { from: f32, to: f32 },
}

impl RatioChange {
    pub fn changed(&self) -> bool {
        !matches!(self, RatioChange::Unchanged)
    }
}

/// The controller.
#[derive(Debug, Clone)]
pub struct AdaptiveParity {
    config: AdaptiveConfig,
    fraction: f32,
    /// Consecutive windows below `low_water`.
    clean_run: u32,
    /// Counters, because a controller whose behaviour is only visible in the ratio is a controller
    /// nobody will notice is stuck.
    observations: u64,
    raises: u64,
    lowers: u64,
}

impl AdaptiveParity {
    pub fn new(config: AdaptiveConfig) -> Self {
        Self {
            fraction: config.initial.clamp(config.min, config.max),
            config,
            clean_run: 0,
            observations: 0,
            raises: 0,
            lowers: 0,
        }
    }

    pub fn config(&self) -> &AdaptiveConfig {
        &self.config
    }

    pub fn fraction(&self) -> f32 {
        self.fraction
    }

    /// The policy to hand the packetiser for the *next* frame.
    pub fn policy(&self) -> ParityPolicy {
        if self.fraction <= 0.0 {
            ParityPolicy::Off
        } else {
            ParityPolicy::Ratio {
                fraction: self.fraction,
            }
        }
    }

    pub fn observations(&self) -> u64 {
        self.observations
    }

    pub fn raises(&self) -> u64 {
        self.raises
    }

    pub fn lowers(&self) -> u64 {
        self.lowers
    }

    /// One window's measured loss, as a fraction of datagrams sent.
    ///
    /// The caller owns the window: how long it is, and whether it is a frame or a second's worth
    /// of frames. This type deliberately does not decide, because the right window depends on the
    /// rate — and because a controller with no clock is one that can be tested.
    pub fn observe(&mut self, loss_fraction: f64) -> RatioChange {
        self.observations += 1;

        if loss_fraction > self.config.high_water {
            // Any loss means the current ratio was insufficient. Raise now, and reset the clean
            // run: a lossy window is not a clean one.
            self.clean_run = 0;
            let to = (self.fraction * self.config.up_factor).min(self.config.max);
            if to > self.fraction {
                let from = self.fraction;
                self.fraction = to;
                self.raises += 1;
                return RatioChange::Raised { from, to };
            }
            return RatioChange::Unchanged;
        }

        if loss_fraction <= self.config.low_water {
            self.clean_run += 1;
            if self.clean_run >= self.config.clean_windows {
                self.clean_run = 0;
                let to = (self.fraction * self.config.down_factor).max(self.config.min);
                // Multiplicative decay approaches zero without reaching it. Snap the last step,
                // because a negligible-but-nonzero ratio still buys a parity shard per frame under
                // the ceil policy — the opposite of what "ratchet the overhead down" is for.
                let to = if to < NEGLIGIBLE_RATIO {
                    self.config.min
                } else {
                    to
                };
                if to < self.fraction {
                    let from = self.fraction;
                    self.fraction = to;
                    self.lowers += 1;
                    return RatioChange::Lowered { from, to };
                }
            }
            return RatioChange::Unchanged;
        }

        // Between the waters: not bad enough to buy parity for, not good enough to sell it. The
        // deadband. Reset the clean run — this window was not clean — and do nothing.
        self.clean_run = 0;
        RatioChange::Unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller() -> AdaptiveParity {
        AdaptiveParity::new(AdaptiveConfig::default())
    }

    #[test]
    fn loss_raises_the_ratio_immediately() {
        let mut c = controller();
        assert_eq!(c.fraction(), 0.05);
        let change = c.observe(0.10);
        assert!(
            matches!(change, RatioChange::Raised { .. }),
            "a lossy window must buy parity at once, not after a run of them"
        );
        assert!(c.fraction() > 0.05);
    }

    #[test]
    fn a_single_clean_window_does_not_lower_it() {
        // Loss is bursty; one quiet window proves very little. If this were allowed, the ratio
        // would sawtooth on any link with occasional bursts.
        let mut c = controller();
        c.observe(0.10);
        let raised = c.fraction();
        assert_eq!(c.observe(0.0), RatioChange::Unchanged);
        assert_eq!(c.fraction(), raised, "lowered after a single clean window");
    }

    #[test]
    fn a_sustained_clean_link_lowers_it_slowly() {
        let mut c = controller();
        c.observe(0.10);
        let raised = c.fraction();

        let mut lowered = 0;
        for _ in 0..(c.config().clean_windows * 3) {
            if matches!(c.observe(0.0), RatioChange::Lowered { .. }) {
                lowered += 1;
            }
        }
        // Exactly one per run of `clean_windows`, and never back to normal in one step.
        assert!(lowered >= 2, "the ratio never came down ({lowered})");
        assert!(c.fraction() < raised);
        assert!(lowered <= 3, "it fell faster than one step per run");
    }

    #[test]
    fn the_deadband_stops_it_oscillating() {
        // A link sitting between the two waters must produce no transitions at all. Every one
        // would cost a control round trip and a code-rate change.
        let mut c = controller();
        let middle = (c.config().high_water + c.config().low_water) / 2.0;
        for _ in 0..100 {
            assert_eq!(
                c.observe(middle),
                RatioChange::Unchanged,
                "the deadband leaked a transition"
            );
        }
        assert_eq!(c.raises(), 0);
        assert_eq!(c.lowers(), 0);
    }

    #[test]
    fn it_saturates_at_both_ends_instead_of_running_away() {
        let mut c = controller();
        for _ in 0..100 {
            c.observe(0.9);
        }
        assert_eq!(c.fraction(), c.config().max, "it exceeded the ceiling");

        for _ in 0..1000 {
            c.observe(0.0);
        }
        assert_eq!(c.fraction(), c.config().min, "it went below the floor");
    }

    #[test]
    fn it_reacts_faster_than_it_relaxes() {
        // The asymmetry, asserted rather than asserted-in-a-comment.
        let mut c = controller();
        let start = c.fraction();

        // One lossy window: a raise.
        c.observe(0.5);
        assert!(c.fraction() > start);

        // One clean window: nothing.
        let after_raise = c.fraction();
        c.observe(0.0);
        assert_eq!(c.fraction(), after_raise);
    }

    #[test]
    fn the_policy_it_produces_carries_the_ratio_it_claims() {
        // The controller and the packetiser must not be able to disagree: the fraction the
        // controller reports is the fraction the wire pays.
        let mut c = controller();
        c.observe(0.5);
        let fraction = c.fraction();
        let policy = c.policy();

        let data = 1000;
        let parity = policy.parity_for(data);
        let actual = parity as f32 / data as f32;
        assert!(
            (actual - fraction).abs() < 0.02,
            "controller says {fraction}, packetiser charges {actual}"
        );
    }

    #[test]
    fn a_zero_ratio_is_off_and_off_costs_nothing() {
        let mut c = AdaptiveParity::new(AdaptiveConfig {
            initial: 0.0,
            ..Default::default()
        });
        for _ in 0..100 {
            c.observe(0.0);
        }
        assert_eq!(c.policy(), ParityPolicy::Off);
        assert_eq!(c.policy().parity_for(1000), 0);
    }
}
