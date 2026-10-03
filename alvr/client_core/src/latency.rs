//! Latency markers: where a frame's time actually goes, on the client.
//!
//! `ROADMAP.md` lists the glass-to-glass number as **blocked**, and names the reason: there is no
//! shared clock between the PC and the client, and nothing in the tree records when a given frame
//! crossed a given boundary. The reference client does not have that problem — it carries twenty
//! named markers (`CR Flip Frame Start`, `CR WaitGetPoses Up`, `CR Render Eye0 Submit`,
//! `CR Render VSYNC Done`, `CR Rephase`, …) and uses them to decide the frame phase and the deadline
//! described in [`x_transport::FrameScheduler`].
//!
//! This is our half of that. It is deliberately small: a frame-indexed record of *when*, and a
//! report of *how long*. The join across machines is the per-frame index, which the wire already
//! carries and which is the same key the trust gate and the scheduler use — so a server trace and a
//! client trace can be aligned without either owning the other's clock.
//!
//! ## What it is not
//!
//! It is not a profiler and it does not sample: a frame is marked at the boundaries that decide the
//! score, and nothing else. And it is not gated by a setting — a marker that a preference can switch
//! off is a marker that is missing exactly when a session goes wrong, which is ADR-0014's rule
//! applied to measurement rather than to logging.
//!
//! The cost of that decision, stated rather than hidden: the trace holds a bounded window of recent
//! frames and does the arithmetic only when a summary is asked for, so the steady-state cost is one
//! `Instant::now()` and one store per boundary.

use std::{collections::VecDeque, time::Instant};

/// A boundary worth timing. Named after what the client *does*, so a number in the report can be
/// argued about against the code rather than against a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// The media plane released the frame: it is reconstructed and trustworthy.
    Received,
    /// Handed to the decoder.
    DecodeStart,
    /// The decoder produced a frame for it.
    DecodeDone,
    /// The renderer finished and the runtime was told about it.
    Submitted,
    /// The runtime said when it will show it.
    Displayed,
}

impl Stage {
    pub const ALL: [Stage; 5] = [
        Stage::Received,
        Stage::DecodeStart,
        Stage::DecodeDone,
        Stage::Submitted,
        Stage::Displayed,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Stage::Received => "received",
            Stage::DecodeStart => "decode-start",
            Stage::DecodeDone => "decode-done",
            Stage::Submitted => "submitted",
            Stage::Displayed => "displayed",
        }
    }

    fn index(self) -> usize {
        match self {
            Stage::Received => 0,
            Stage::DecodeStart => 1,
            Stage::DecodeDone => 2,
            Stage::Submitted => 3,
            Stage::Displayed => 4,
        }
    }
}

/// A span the report is about: two stages, and the time between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// Receive to decoded: how long the client takes to turn a frame into pixels.
    Decode,
    /// Receive to submitted: the client's own contribution to the latency, end to end.
    Client,
    /// Received to the display time the runtime predicted.
    ReceivedToDisplay,
    /// The interval the frames actually arrived at, which is the stream's cadence as the client
    /// experienced it — and the number that says whether it is keeping up.
    FrameInterval,
}

impl Span {
    pub const ALL: [Span; 4] = [
        Span::Decode,
        Span::Client,
        Span::ReceivedToDisplay,
        Span::FrameInterval,
    ];

    pub const fn from_to(self) -> (Stage, Stage) {
        match self {
            Span::Decode => (Stage::DecodeStart, Stage::DecodeDone),
            Span::Client => (Stage::Received, Stage::Submitted),
            Span::ReceivedToDisplay => (Stage::Received, Stage::Displayed),
            Span::FrameInterval => (Stage::Received, Stage::Received),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Span::Decode => "decode",
            Span::Client => "receive->submit",
            Span::ReceivedToDisplay => "receive->display",
            Span::FrameInterval => "frame interval",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FrameTrace {
    frame_index: u64,
    stamps: [Option<u64>; 5],
    /// Marks that arrived for a stage that had already been marked, or with no frame behind them.
    contradictory: bool,
}

/// A bounded, frame-indexed record of boundary times, and the report over it.
#[derive(Debug)]
pub struct LatencyTrace {
    start: Instant,
    capacity: usize,
    frames: VecDeque<FrameTrace>,
    /// Frames dropped from the front of the window because it was full.
    evicted: u64,
    /// Marks that could not be attributed: a second stamp for the same boundary, or a boundary for
    /// a frame that had already been evicted. Counted, because a trace that quietly loses marks is
    /// a trace whose numbers are wrong in a way nobody can see.
    unattributable: u64,
}

impl LatencyTrace {
    /// `capacity` is how many recent frames to keep. 4096 is about 45 s at 90 Hz — long enough for
    /// a session summary, short enough to be a fixed few hundred kilobytes.
    pub fn new(capacity: usize) -> Self {
        Self {
            start: Instant::now(),
            capacity: capacity.max(1),
            frames: VecDeque::with_capacity(capacity.max(1)),
            evicted: 0,
            unattributable: 0,
        }
    }

    /// Microseconds since the trace started — the trace's own clock, which is the only one a
    /// client can be sure of.
    fn stamp(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.start).as_micros() as u64
    }

    /// Record that a frame was released by the media plane, and when the server aimed it.
    ///
    /// This is the one mark that *creates* a record; every other mark finds one. A frame that
    /// arrives here a second time is a duplicate frame index, which is a defect worth counting
    /// rather than overwriting.
    pub fn frame_received(&mut self, frame_index: u64, _target_timestamp_us: u64, at: Instant) {
        if self.frames.iter().any(|f| f.frame_index == frame_index) {
            self.unattributable += 1;
            return;
        }

        if self.frames.len() == self.capacity {
            self.frames.pop_front();
            self.evicted += 1;
        }

        let mut stamps = [None; 5];
        stamps[Stage::Received.index()] = Some(self.stamp(at));
        self.frames.push_back(FrameTrace {
            frame_index,
            stamps,
            contradictory: false,
        });
    }

    /// Mark a boundary for a frame that is already in the window.
    pub fn mark(&mut self, frame_index: u64, stage: Stage, at: Instant) {
        if stage == Stage::Received {
            // `Received` is what creates a record; a bare mark for it is a caller that skipped
            // `frame_received`, which would silently produce a frame with no arrival time.
            self.unattributable += 1;
            return;
        }

        let stamp = self.stamp(at);
        let Some(frame) = self
            .frames
            .iter_mut()
            .rev()
            .find(|f| f.frame_index == frame_index)
        else {
            self.unattributable += 1;
            return;
        };

        let slot = &mut frame.stamps[stage.index()];
        if slot.is_some() {
            frame.contradictory = true;
            self.unattributable += 1;
            return;
        }
        *slot = Some(stamp);
    }

    /// How many frames the window holds.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn frame_index_at(&self, position: usize) -> Option<u64> {
        self.frames.get(position).map(|f| f.frame_index)
    }

    /// The durations of one span, in microseconds, for every frame that has both ends.
    pub fn samples(&self, span: Span) -> Vec<u64> {
        let (from, to) = span.from_to();

        match span {
            // The interval between consecutive arrivals, which needs two frames rather than two
            // marks.
            Span::FrameInterval => self
                .frames
                .iter()
                .filter_map(|f| f.stamps[Stage::Received.index()])
                .collect::<Vec<_>>()
                .windows(2)
                .map(|pair| pair[1].saturating_sub(pair[0]))
                .collect(),
            _ => self
                .frames
                .iter()
                .filter(|f| !f.contradictory)
                .filter_map(|f| {
                    let start = f.stamps[from.index()]?;
                    let end = f.stamps[to.index()]?;
                    Some(end.saturating_sub(start))
                })
                .collect(),
        }
    }

    /// `(p50, p95)` in microseconds, or `None` when the span has no complete samples.
    pub fn percentiles(&self, span: Span) -> Option<(u64, u64)> {
        let mut samples = self.samples(span);
        if samples.is_empty() {
            return None;
        }
        samples.sort_unstable();
        Some((percentile(&samples, 50), percentile(&samples, 95)))
    }

    /// One line per span, plus what could not be attributed.
    pub fn summary(&self) -> String {
        let mut out = format!(
            "latency trace: {} frame(s) in a {}-frame window ({} evicted, {} mark(s) \
             unattributable)",
            self.frames.len(),
            self.capacity,
            self.evicted,
            self.unattributable,
        );

        for span in Span::ALL {
            match self.percentiles(span) {
                Some((p50, p95)) => {
                    out.push_str(&format!(
                        "\n  {:<16} p50 {:>8.3} ms   p95 {:>8.3} ms",
                        span.name(),
                        p50 as f64 / 1000.0,
                        p95 as f64 / 1000.0,
                    ));
                }
                None => out.push_str(&format!("\n  {:<16} no samples", span.name())),
            }
        }

        out
    }
}

/// The `percentile`th value of a sorted slice. Nearest-rank, which for a tail budget ("0 % of frames
/// over 11.11 ms") is the right definition: it returns a real sample rather than an interpolation
/// that may be a number no frame ever had.
fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    let rank = (sorted.len() * percentile).div_ceil(100).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn a_frame_with_both_ends_produces_a_span() {
        let mut trace = LatencyTrace::new(16);
        let start = Instant::now();

        trace.frame_received(1, 11_111, start);
        trace.mark(1, Stage::DecodeStart, start + ms(2));
        trace.mark(1, Stage::DecodeDone, start + ms(12));
        trace.mark(1, Stage::Submitted, start + ms(15));
        trace.mark(1, Stage::Displayed, start + ms(16));

        assert_eq!(trace.samples(Span::Decode), vec![10_000]);
        assert_eq!(trace.samples(Span::Client), vec![15_000]);
        assert_eq!(trace.samples(Span::ReceivedToDisplay), vec![16_000]);
    }

    #[test]
    fn a_missing_end_drops_the_sample_instead_of_guessing_one() {
        let mut trace = LatencyTrace::new(16);
        let start = Instant::now();

        trace.frame_received(1, 0, start);
        trace.mark(1, Stage::DecodeStart, start + ms(1));
        // No DecodeDone: the frame is still in the decoder. The span has no sample, and neither
        // does the one that depends on it.
        trace.mark(1, Stage::Submitted, start + ms(3));

        assert!(trace.samples(Span::Decode).is_empty());
        assert_eq!(trace.samples(Span::Client), vec![3_000]);
    }

    #[test]
    fn a_second_mark_for_one_boundary_is_counted_rather_than_overwriting_the_first() {
        let mut trace = LatencyTrace::new(16);
        let start = Instant::now();

        trace.frame_received(1, 0, start);
        trace.mark(1, Stage::DecodeStart, start + ms(1));
        trace.mark(1, Stage::DecodeStart, start + ms(2));

        assert!(trace.summary().contains("1 mark(s) unattributable"));
        assert_eq!(trace.samples(Span::Decode), Vec::<u64>::new());
    }

    #[test]
    fn a_mark_for_a_frame_that_was_never_received_is_counted_rather_than_ignored() {
        let mut trace = LatencyTrace::new(16);
        trace.mark(99, Stage::DecodeDone, Instant::now());
        assert!(trace.summary().contains("1 mark(s) unattributable"));
        assert!(trace.is_empty());
    }

    #[test]
    fn a_duplicate_frame_index_does_not_recycle_a_record() {
        let mut trace = LatencyTrace::new(16);
        let start = Instant::now();

        trace.frame_received(7, 0, start);
        trace.frame_received(7, 0, start + ms(1));

        assert_eq!(trace.len(), 1, "the record was overwritten");
        assert!(trace.summary().contains("1 mark(s) unattributable"));
    }

    #[test]
    fn the_window_is_bounded_and_evictions_are_reported() {
        let mut trace = LatencyTrace::new(4);
        let start = Instant::now();
        for index in 0..10 {
            trace.frame_received(index, 0, start + ms(index));
        }

        assert_eq!(trace.len(), 4);
        assert_eq!(trace.frame_index_at(0), Some(6));
        assert!(trace.summary().contains("6 evicted"));
    }

    #[test]
    fn the_frame_interval_is_measured_between_arrivals() {
        let mut trace = LatencyTrace::new(16);
        let start = Instant::now();
        for (index, offset) in [(1u64, 0u64), (2, 11), (3, 22), (4, 34)] {
            trace.frame_received(index, 0, start + ms(offset));
        }

        assert_eq!(
            trace.samples(Span::FrameInterval),
            vec![11_000, 11_000, 12_000]
        );
    }

    /// The tail is the score, so the percentile must return a value a frame actually had.
    #[test]
    fn the_percentile_is_a_sample_and_not_an_interpolation() {
        // The window must hold every frame the assertion counts: a bounded window is the reason
        // the first version of this test read 92 ms where it expected 95.
        let mut trace = LatencyTrace::new(128);
        let start = Instant::now();

        // 100 frames with spans of 1..=100 ms. The 95th value of a hundred samples is the 95th
        // smallest, i.e. 95 ms.
        for index in 0..100u64 {
            trace.frame_received(index, 0, start + ms(index));
            trace.mark(index, Stage::Submitted, start + ms(index) + ms(index + 1));
        }

        let (p50, p95) = trace.percentiles(Span::Client).unwrap();
        assert_eq!(p50, 50_000);
        assert_eq!(p95, 95_000);
    }

    #[test]
    fn the_summary_names_every_span() {
        let mut trace = LatencyTrace::new(16);
        let start = Instant::now();
        trace.frame_received(1, 0, start);
        trace.mark(1, Stage::DecodeStart, start + ms(1));
        trace.mark(1, Stage::DecodeDone, start + ms(2));
        trace.mark(1, Stage::Submitted, start + ms(3));
        trace.mark(1, Stage::Displayed, start + ms(4));

        let line = trace.summary();
        for needle in [
            "decode",
            "receive->submit",
            "receive->display",
            "frame interval",
        ] {
            assert!(
                line.contains(needle),
                "{line:?} does not mention {needle:?}"
            );
        }
    }
}
