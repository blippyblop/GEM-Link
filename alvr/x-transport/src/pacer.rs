//! Pacing: decide *when* a datagram may go, so the queue never has to be emptied by
//! discarding.
//!
//! This exists because of the shape of the defect ADR-0011 was written about. The send path
//! today is:
//!
//! ```text
//!   pace → ENCODE → discover the channel is full → DISCARD
//! ```
//!
//! which is the worst possible order. Encoding has already happened, the reference chain has
//! already been extended, and the frame is thrown away *silently* — the client is then handed
//! a P-frame whose reference never arrived, which decodes to flat grey. Backpressure applied
//! *after* the expensive, order-dependent stage cannot be recovered from.
//!
//! The correct order is `check deliverability → pace → encode → send`, and the pacer is the
//! piece that makes the second step possible: given a rate, it answers "when may this
//! datagram go" *before* anything has been committed to it. Nothing here drops. A pacer that
//! drops is a queue with extra steps.
//!
//! ## Shape
//!
//! A leaky bucket in integer nanoseconds: strict spacing with a bounded burst credit, so a
//! frame's worth of datagrams leaves at the link rate rather than as a microburst that the
//! first queue on the path will tail-drop. No floating point, so a bench run is exactly
//! reproducible.
//!
//! ## Rate is an input, not a decision
//!
//! Deliberately *not* adaptive. Deciding what the rate should be is the bitrate controller's
//! job, and a pacer that also adjusts the rate is two controllers fighting over one actuator
//! — the classic way to get oscillation. The caller sets the rate; this obeys it.

use std::time::Duration;

/// Pacer configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacerConfig {
    /// Sustained rate.
    pub rate_bits_per_sec: u64,
    /// How much credit may accumulate while idle, in bytes.
    ///
    /// Zero is strict pacing (a late datagram earns no burst). A small non-zero value is what
    /// you want in practice: a frame is produced in one go and would otherwise be spread
    /// over a whole frame interval, adding a frame of latency. The right value is roughly
    /// `rate × frame_interval / 8` — one frame's worth — and
    /// [`PacerConfig::for_rate`] computes exactly that.
    pub burst_bytes: u64,
}

impl PacerConfig {
    /// A rate with a burst credit of one frame interval.
    pub const fn for_rate(rate_bits_per_sec: u64, frame_interval: Duration) -> Self {
        Self {
            rate_bits_per_sec,
            burst_bytes: rate_bits_per_sec / 8 * frame_interval.as_nanos() as u64 / 1_000_000_000,
        }
    }

    /// Bytes per second, saturating rather than overflowing.
    pub const fn bytes_per_sec(&self) -> u64 {
        self.rate_bits_per_sec / 8
    }

    /// Nanoseconds to transmit `bytes` at this rate.
    const fn ns_for(&self, bytes: u64) -> u64 {
        if self.rate_bits_per_sec == 0 || self.bytes_per_sec() == 0 {
            return u64::MAX;
        }
        // bytes * 1e9 / bytes_per_sec, with the multiply done in u128 so a large burst
        // cannot wrap.
        ((bytes as u128 * 1_000_000_000) / self.bytes_per_sec() as u128) as u64
    }

    /// Nanoseconds of burst credit.
    const fn burst_ns(&self) -> u64 {
        self.ns_for(self.burst_bytes)
    }
}

/// A leaky bucket. `now` is supplied by the caller so this is testable without sleeping and
/// so a bench run is reproducible.
#[derive(Debug, Clone)]
pub struct Pacer {
    config: PacerConfig,
    /// The earliest instant the next datagram may leave, ignoring credit.
    next_send: Duration,
    /// Set once anything has been scheduled, so a fresh pacer does not treat `now = 0` as
    /// "idle since the beginning of time" and hand out a full burst to everyone.
    started: bool,
}

impl Pacer {
    pub const fn new(config: PacerConfig) -> Self {
        Self {
            config,
            next_send: Duration::ZERO,
            started: false,
        }
    }

    pub const fn config(&self) -> PacerConfig {
        self.config
    }

    /// How long the caller must wait before `bytes` may be sent, and *commit* to sending
    /// them now.
    ///
    /// This is the whole API for a caller that is going to send the datagram regardless:
    /// schedule, sleep for the returned duration, send. Returns [`Duration::ZERO`] when the
    /// datagram may go immediately.
    ///
    /// The commit is unconditional and the caller cannot "un-commit". That is intentional:
    /// a pacing decision that can be revoked is a queue, and the point of this module is to
    /// have no queue to overflow.
    pub fn schedule(&mut self, bytes: usize, now: Duration) -> Duration {
        let bytes = bytes as u64;

        if !self.started {
            self.started = true;
            self.next_send = now;
        }

        // Credit: `next_send` may fall behind `now` by at most the burst allowance.
        let burst = Duration::from_nanos(self.config.burst_ns());
        if let Some(floor) = now.checked_sub(burst) {
            if self.next_send < floor {
                self.next_send = floor;
            }
        } else {
            // `now` is inside the burst window of zero, so there is no floor to apply.
            self.next_send = self.next_send.max(now);
        }

        let send_at = self.next_send.max(now);
        let wait = send_at.saturating_sub(now);

        // Advance from where the schedule already was, not from `send_at`: a datagram sent
        // during a banked burst has to consume the credit it used, or the credit would be
        // reusable indefinitely and the pacer would not pace.
        let cost = Duration::from_nanos(self.config.ns_for(bytes));
        self.next_send = self
            .next_send
            .checked_add(cost)
            .unwrap_or(Duration::from_secs(u64::MAX / 2));

        wait
    }

    /// When the pacer believes the link will next be free. Useful for a caller deciding
    /// whether a frame can still make its deadline.
    pub const fn next_send(&self) -> Duration {
        self.next_send
    }

    /// Reset for a new session: back to a full burst credit and no committed schedule.
    pub fn reset(&mut self) {
        self.next_send = Duration::ZERO;
        self.started = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEGABIT: u64 = 1_000_000;

    #[test]
    fn a_rate_of_zero_never_sends_but_does_not_hang() {
        let mut pacer = Pacer::new(PacerConfig {
            rate_bits_per_sec: 0,
            burst_bytes: 0,
        });
        // The first call is admitted (there is no schedule to wait for), and the second has
        // to wait essentially forever. Neither panics.
        assert_eq!(pacer.schedule(100, Duration::ZERO), Duration::ZERO);
        assert!(pacer.schedule(100, Duration::ZERO) > Duration::from_secs(1_000_000));
    }

    #[test]
    fn the_first_datagram_goes_immediately() {
        let mut pacer = Pacer::new(PacerConfig::for_rate(
            100 * MEGABIT,
            Duration::from_millis(11),
        ));
        assert_eq!(pacer.schedule(1400, Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn datagrams_are_spaced_at_the_rate() {
        // 8 Mbps = 1 MB/s, so a 1000-byte datagram takes exactly 1 ms.
        let mut pacer = Pacer::new(PacerConfig {
            rate_bits_per_sec: 8 * MEGABIT,
            burst_bytes: 0,
        });

        let mut now = Duration::ZERO;
        let mut sends = Vec::new();
        for _ in 0..5 {
            let wait = pacer.schedule(1000, now);
            now += wait;
            sends.push(now);
            // The next one must wait, because we just spent the whole interval.
            now += Duration::from_micros(0);
        }

        for window in sends.windows(2) {
            assert_eq!(
                window[1] - window[0],
                Duration::from_millis(1),
                "datagrams are not spaced at the configured rate: {sends:?}"
            );
        }
    }

    #[test]
    fn an_idle_pacer_banks_at_most_one_burst() {
        // Burst credit is bounded, or a long idle period would authorise an unbounded
        // microburst — which is the thing pacing exists to prevent. Counted rather than
        // asserted at an exact boundary, because "how many fit inside the credit" is an
        // off-by-one question and the property that matters is that it is *bounded*.
        let config = PacerConfig {
            rate_bits_per_sec: 8 * MEGABIT, // 1 MB/s, so 1000 bytes is exactly 1 ms
            burst_bytes: 3000,
        };
        let mut pacer = Pacer::new(config);

        // Some traffic first, so the pacer is not fresh.
        let _ = pacer.schedule(1000, Duration::ZERO);

        // A minute later: at most the burst's worth goes without waiting.
        let later = Duration::from_secs(60);
        let mut immediate = 0;
        let mut at = later;
        for _ in 0..50 {
            let wait = pacer.schedule(1000, at);
            if wait > Duration::ZERO {
                break;
            }
            immediate += 1;
            at += wait;
        }
        assert!(
            (2..=4).contains(&immediate),
            "expected the 3 KB burst to admit 3 datagrams (±1), admitted {immediate}"
        );

        // And it keeps metering afterwards rather than resetting.
        assert!(pacer.schedule(1000, later) > Duration::ZERO);
    }

    #[test]
    fn the_burst_credit_is_consumed_not_reused() {
        // The bug this guards: advancing the schedule from the send instant rather than from
        // the banked position makes the credit reusable forever, and the pacer stops pacing.
        let config = PacerConfig {
            rate_bits_per_sec: 8 * MEGABIT,
            burst_bytes: 10_000,
        };
        let mut pacer = Pacer::new(config);
        let now = Duration::from_secs(1);

        let mut total = Duration::ZERO;
        for _ in 0..1000 {
            total += pacer.schedule(1000, now + total);
        }
        // 1000 KB at 1 MB/s cannot take less than a second, whatever the burst was.
        assert!(
            total >= Duration::from_millis(900),
            "1000 KB took only {total:?} at 1 MB/s: the credit is being reused"
        );
    }

    #[test]
    fn a_frame_is_spread_over_no_more_than_its_own_interval() {
        // A frame that fits inside one interval must not take longer than one interval to
        // schedule, or the pacer itself becomes the latency.
        //
        // The arithmetic: 300 Mbps is 37.5 MB/s, so a frame interval of 11.111 ms carries
        // 416,666 bytes. A 290-fragment frame is 406,000 bytes, which fits.
        let interval = Duration::from_millis(11);
        let mut pacer = Pacer::new(PacerConfig::for_rate(300 * MEGABIT, interval));

        let mut now = Duration::ZERO;
        for _ in 0..290 {
            now += pacer.schedule(1400, now);
        }
        assert!(
            now <= interval,
            "290 fragments (406 KB) took {now:?}, more than the {interval:?} interval they fit in"
        );

        // A frame that does not fit must be metered, not waved through: the pacer is the
        // layer that refuses to exceed the rate. 420 KB is more than one interval holds.
        let mut pacer = Pacer::new(PacerConfig::for_rate(300 * MEGABIT, interval));
        let mut now = Duration::ZERO;
        for _ in 0..300 {
            now += pacer.schedule(1400, now);
        }
        assert!(
            now > interval,
            "420 KB cannot fit in one {interval:?} at 300 Mbps, but the pacer admitted it"
        );
    }

    #[test]
    fn scheduling_is_exactly_reproducible() {
        // Same inputs, same answer, every time — the bench depends on it.
        let run = || {
            let mut pacer = Pacer::new(PacerConfig::for_rate(
                100 * MEGABIT,
                Duration::from_millis(11),
            ));
            let mut now = Duration::ZERO;
            let mut total = Duration::ZERO;
            for i in 0..1000u64 {
                let wait = pacer.schedule(1400 + (i % 7) as usize, now);
                now += wait;
                total += wait;
            }
            total
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn reset_restores_the_initial_state() {
        let mut pacer = Pacer::new(PacerConfig {
            rate_bits_per_sec: 8 * MEGABIT,
            burst_bytes: 0,
        });
        let _ = pacer.schedule(8000, Duration::ZERO);
        assert!(pacer.schedule(1000, Duration::ZERO) > Duration::ZERO);
        pacer.reset();
        assert_eq!(
            pacer.schedule(1000, Duration::from_secs(10)),
            Duration::ZERO
        );
    }
}
