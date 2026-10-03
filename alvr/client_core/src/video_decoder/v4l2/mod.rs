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

pub mod abi;
pub mod device;
pub mod pipeline;

use std::collections::VecDeque;

// The stall ladder used to live here. It is not a V4L2 idea — the receive loop needs the same
// two rungs for the same reason — so it moved to `crate::stall`, which is now the only place the
// 300 ms / 800 ms thresholds are written down. Re-exported so this module keeps its vocabulary.
pub use crate::stall::{ASK_FOR_KEYFRAME_AFTER, HARD_RESET_AFTER, StuckAction, StuckDetector};

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

    /// Take one *specific* buffer, for callers that are handed an index by someone else — a CAPTURE
    /// buffer named by a device event, say. Returns false if it was not free, which is either a
    /// double-decode or a device handing out a buffer it never got back. Both are worth counting.
    pub fn acquire_specific(&mut self, index: u32) -> bool {
        let Some(pos) = self.free.iter().position(|&i| i == index) else {
            return false;
        };
        self.free.remove(pos);
        self.in_use += 1;
        true
    }

    /// Give a buffer back. Returns whether it was actually held, so a caller can tell a clean
    /// return from a double-release instead of both looking like success.
    pub fn release(&mut self, index: u32) -> bool {
        if self.in_use == 0 || self.free.contains(&index) {
            return false;
        }
        self.in_use -= 1;
        self.free.push_back(index);
        true
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
        (
            "VIDIOC_QUERYBUF + mmap (OUTPUT)",
            "or DMABUF export, to hand the kernel bitstream",
        ),
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

// The kernel-facing half lives in `pipeline::V4l2Device`, next to the policy that drives it, so
// the two cannot drift apart.

#[cfg(test)]
mod tests {
    use super::*;

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

        assert!(pool.release(1), "releasing a held buffer must succeed");
        assert_eq!(pool.acquire(), Some(1));
        assert_eq!(pool.in_use(), 3);
    }

    #[test]
    fn a_double_release_does_not_hand_one_buffer_to_two_holders() {
        let mut pool = BufferPool::new(2, [7, 8]);
        assert_eq!(pool.acquire(), Some(7));
        assert!(pool.release(7));
        assert!(
            !pool.release(7),
            "a double release must be refused, not counted twice"
        );
        assert_eq!(pool.free(), 2, "a double release duplicated a buffer");

        let a = pool.acquire().expect("a buffer");
        let b = pool.acquire().expect("a buffer");
        assert_ne!(a, b, "the pool handed the same buffer to two holders");
        assert_eq!(pool.free(), 0);
        assert_eq!(pool.acquire(), None);
    }
}
