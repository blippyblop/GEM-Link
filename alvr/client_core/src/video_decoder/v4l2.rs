//! The V4L2 M2M decoder backend — the Steam Frame's real decode path.
//!
//! On the device, `client_core::video_decoder` has been a stub: every method compiles to `false`
//! and the only backend is MediaCodec, which is Android's. The Frame is aarch64 Linux and decodes
//! through a **stateful memory-to-memory V4L2 device**, driven by a hardware block fed by a signed
//! firmware blob (`/usr/lib/firmware/qcom/vpu/vpu33_4v.mbn` on the image we hold).
//!
//! This module is written against a reference implementation rather than a guess. Valve ships
//! their own client for this exact device — `/opt/steamvr/tools/vrlink/bin/linuxarm64/vrlink` —
//! and its decoder class is `SVLCodecV4L2`. The structure below follows it, and every string
//! quoted in the comments is verbatim from that binary. See `VD_RE/52-frame-vrlink-client.md`.
//!
//! ## What is here and what is not
//!
//! This file deliberately contains **no ioctls yet**. It holds the parts that are policy rather
//! than kernel ABI — the stuck/reset ladder, the buffer pool accounting, and the setup table —
//! because those are the parts that can be argued about correctly and unit-tested off-device, and
//! they are where the two interesting defects live (a pool that silently starves, and a stall that
//! is never noticed). The ioctl layer implements [`V4l2Device`] next; the calls it must make are
//! enumerated in [`ioctls`].
//!
//! Nothing in this file has run on hardware. It compiles for `aarch64-unknown-linux-gnu` and its
//! logic is tested; it has never opened a `/dev/videoN`.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// The two stall thresholds, taken from the reference client:
///
/// ```text
/// SVLCodecV4L2::CheckStuck: > 300ms between our last forward progress. Asking remote side for a new IFrame
/// SVLCodecV4L2::CheckStuck: > 800ms between our last forward progress. Reset.
/// ```
///
/// Two rungs, not one. GemLink's client has the first (hold the last good frame and ask for a
/// keyframe — ADR-0011) but no upper bound: a client whose keyframe never arrives holds forever,
/// which is exactly the black screen we shipped and had to fix in `x_transport::TrustGate`. The
/// reference client does not have that failure mode because it stops waiting and rebuilds.
pub const ASK_FOR_KEYFRAME_AFTER: Duration = Duration::from_millis(300);
pub const HARD_RESET_AFTER: Duration = Duration::from_millis(800);

/// What the decoder decided to do about the passage of time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckAction {
    /// Forward progress is recent enough. Nothing to do.
    Progress,
    /// We have made no progress for [`ASK_FOR_KEYFRAME_AFTER`]. Ask the sender for a keyframe.
    /// This is ADR-0011's recovery, and the request must be made once per stall event, not once
    /// per poll.
    AskForKeyframe,
    /// We have made no progress for [`HARD_RESET_AFTER`]. The stream is not recoverable by asking;
    /// tear the decode session down and rebuild it. The reference client calls this
    /// `SVLCodecV4L2::HardReset`.
    Reset,
}

/// Watches for a stall in the decode pipeline.
///
/// "Forward progress" is any owned buffer coming back: a decoded frame released to the renderer,
/// or an OUTPUT buffer returned by the kernel. If neither has happened for
/// [`ASK_FOR_KEYFRAME_AFTER`], something is wrong — and it is not the decoder's job to guess
/// whether the cause was a lost frame, a lost parameter set or a wedged VPU.
///
/// Each rung fires **once per stall**, which is the whole point: a per-poll decision would emit a
/// keyframe request every frame the stall lasted, which on a 90 Hz link is the control-plane flood
/// that `x_transport::TrustGate` was written to avoid.
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
            // The ask may not have fired if the stall jumped past both thresholds between polls.
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

/// A fixed pool of buffers handed out to the kernel and back.
///
/// The reference client has this on both sides of the M2M pair, and it names the failure:
///
/// ```text
/// SVLCodecV4L2::AcquireEncodedDataBuffer: Could not find a free OUTPUT buffer
/// SVLCodecV4L2::AcquireEncodedDataBuffer: Not initted!
/// ```
///
/// The reason this is a first-class type rather than a `VecDeque` at the call site is the defect
/// we spent a session on (doc 50 §A10): the ALVR client's receive path holds a fixed pool, and
/// when it runs dry it **discards the datagram it just read**, silently. A pool that cannot hand
/// out a buffer has to say so at the type level, or the failure becomes somebody else's bug report.
#[derive(Debug)]
pub struct BufferPool {
    free: VecDeque<u32>,
    in_use: usize,
    capacity: usize,
}

impl BufferPool {
    pub fn new(capacity: usize, indices: impl IntoIterator<Item = u32>) -> Self {
        let free: VecDeque<u32> = indices.into_iter().collect();
        let capacity = capacity.max(free.len());
        Self {
            free,
            in_use: 0,
            capacity,
        }
    }

    /// Take a buffer, or `None` if every one is out. `None` is **not** a normal condition: the
    /// caller (or the kernel) is behind. It must be counted and surfaced, never swallowed.
    pub fn acquire(&mut self) -> Option<u32> {
        let index = self.free.pop_front()?;
        self.in_use += 1;
        Some(index)
    }

    pub fn release(&mut self, index: u32) {
        // Guard against a double-release, which would hand the same buffer to two holders.
        if self.in_use == 0 || self.free.contains(&index) {
            return;
        }
        self.in_use -= 1;
        self.free.push_back(index);
    }

    pub fn free(&self) -> usize {
        self.free.len()
    }

    pub fn in_use(&self) -> usize {
        self.in_use
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// The setup calls the ioctl layer must make, in order, with the reason each one matters.
///
/// Kept as data so the ordering is reviewable without reading `unsafe` code, and so a future
/// implementation has a checklist rather than a fog. Names are the reference client's.
pub mod ioctls {
    /// Sequence for `SVLCodecV4L2::InitializeDevice` / `OutputSetup` / `CaptureSetup`.
    pub const SETUP_SEQUENCE: &[(&str, &str)] = &[
        (
            "VIDIOC_QUERYCAP",
            "confirm V4L2_BUF_TYPE_VIDEO_M2M_MPLANE and the codec the session negotiated",
        ),
        (
            "VIDIOC_S_FMT (OUTPUT)",
            "compressed side: the codec's fourcc, and the coded size if known",
        ),
        (
            "S_CTRL DISPLAY_DELAY_ENABLE / DISPLAY_DELAY",
            "the reference client sets both explicitly. These make the *hardware* hold frames for \
             reordering and presentation rather than a userspace queue doing it — i.e. the latency \
             budget is spent inside the decoder, where the vendor's timing model knows about it. \
             A userspace reorder buffer on top is how you end up adding latency twice",
        ),
        (
            "VIDIOC_REQBUFS (OUTPUT)",
            "the encoded-input ring. Sized from the same pool accounting as BufferPool, because \
             'Could not find a free OUTPUT buffer' is the reference client's own named failure",
        ),
        ("VIDIOC_QUERYBUF + mmap (OUTPUT)", "or DMABUF export, to hand the kernel bitstream"),
        (
            "VIDIOC_STREAMON (OUTPUT)",
            "only after the ring exists; the reference client logs 'Failed to enable output stream'",
        ),
        (
            "VIDIOC_SUBSCRIBE_EVENT / V4L2_EVENT_SOURCE_CHANGE",
            "this is how a resolution change is learned. It is the mechanism that lets a runtime \
             quality step-down exist without a session restart — which today GemLink forbids, \
             because resolution and refresh are inside compute_restart_settings_hash",
        ),
        (
            "epoll + a nudge pipe",
            "the reference client runs a media thread on epoll_wait (SVLCodecV4L2::MediaThread, \
             InitializeEpoll) rather than polling the device. A nudge pipe is how it wakes that \
             thread for work that did not come from the kernel",
        ),
        (
            "VIDIOC_G_FMT (CAPTURE)",
            "learn the decoded size and format; the reference client logs \
             'New decode width, height, format -> %ux%u %s' and re-runs CaptureSetup when it moves",
        ),
        (
            "VIDIOC_REQBUFS + QUERYBUF + mmap/export (CAPTURE)",
            "the decoded-frame ring the renderer imports",
        ),
        (
            "VIDIOC_STREAMON (CAPTURE)",
            "start decode. Only now is the pipeline live, and only now should the stuck detector's \
             clock mean anything",
        ),
    ];

    /// The fourccs a Frame-class decoder is expected to accept on the OUTPUT side.
    pub const CODEC_FOURCCS: &[(&str, u32)] = &[
        ("V4L2_PIX_FMT_H264", 0x34363248),
        ("V4L2_PIX_FMT_HEVC", 0x43564548),
    ];
}

/// The kernel-facing half. Implemented next; named now so the seam is explicit and so the policy
/// above can be tested without a device.
pub trait V4l2Device {
    /// Open and configure the device, run the setup sequence, and start both queues.
    fn open(&mut self) -> Result<(), String>;
    /// Hand one access unit to the decoder. Returns false if no OUTPUT buffer was free — which the
    /// caller must count, not ignore.
    fn submit(&mut self, timestamp: Duration, nal: &[u8]) -> bool;
    /// Rebuild the pipeline. Called on [`StuckAction::Reset`].
    fn hard_reset(&mut self);
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
        // And it does not reset in a loop either.
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
        // A decoded frame arrives: the stall is over.
        stuck.progress(keyframe);
        assert_eq!(stuck.poll(keyframe), StuckAction::Progress);
        // A later, unrelated stall must ask again — not stay silent because it asked once.
        let second = keyframe + Duration::from_millis(1);
        assert_eq!(
            stuck.poll(second + ASK_FOR_KEYFRAME_AFTER),
            StuckAction::AskForKeyframe,
            "a second stall did not ask for a keyframe"
        );
    }

    #[test]
    fn a_stall_that_jumps_both_thresholds_resets_rather_than_asking() {
        // A poll interval longer than 800 ms must not emit an ask that is already too late.
        let start = t0();
        let mut stuck = StuckDetector::new(start);
        assert_eq!(
            stuck.poll(start + Duration::from_secs(5)),
            StuckAction::Reset
        );
        assert_eq!(stuck.poll(start + Duration::from_secs(6)), StuckAction::Progress);
    }

    #[test]
    fn an_exhausted_pool_says_so_instead_of_pretending() {
        let mut pool = BufferPool::new(3, [0, 1, 2]);
        assert_eq!(pool.free(), 3);
        assert_eq!(pool.acquire(), Some(0));
        assert_eq!(pool.acquire(), Some(1));
        assert_eq!(pool.acquire(), Some(2));
        // The reference client's named failure. `None` must be reportable, never swallowed.
        assert_eq!(pool.acquire(), None);
        assert_eq!(pool.in_use(), 3);

        pool.release(1);
        assert_eq!(pool.acquire(), Some(1));
        assert_eq!(pool.in_use(), 3);
    }

    #[test]
    fn a_double_release_does_not_hand_one_buffer_to_two_holders() {
        let mut pool = BufferPool::new(2, [7, 8]);
        assert_eq!(pool.acquire(), Some(7));
        pool.release(7);
        pool.release(7); // a decoder bug that would otherwise alias a live buffer
        assert_eq!(pool.free(), 2, "a double release duplicated a buffer");

        let a = pool.acquire().expect("a buffer");
        let b = pool.acquire().expect("a buffer");
        assert_ne!(a, b, "the pool handed the same buffer to two holders");
        assert_eq!(pool.free(), 0);
        assert_eq!(pool.acquire(), None);
    }
}
