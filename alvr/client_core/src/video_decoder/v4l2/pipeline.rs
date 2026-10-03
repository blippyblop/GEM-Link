//! The decoder pipeline: policy on top of the device.
//!
//! [`super::StuckDetector`] knows when the pipeline has stalled and [`super::BufferPool`] knows
//! whether a buffer can be handed out. This module is what ties them to a real device and decides
//! what each condition *means*. It is deliberately generic over [`V4l2Device`] so the policy can be
//! driven by a scripted mock — which is the only way any of it can be tested here, because there is
//! no `/dev/video*` in this container and no system-mode qemu; see the module header in `mod.rs`.
//!
//! ## The rule this file exists to enforce
//!
//! `doc 50 §A10` is a session spent finding that the ALVR client's receive path, when its buffer
//! pool ran dry, **discarded the datagram it had just read and told nobody**. Every counter read
//! zero. The loss looked like the network's fault for two days.
//!
//! So: [`V4l2Device::submit`] returns a [`SubmitStatus`], and [`DecoderStats`] counts every
//! starvation. There is no path through this file where a buffer shortage is silent. That is not
//! defensive coding; it is the specific defect, made structurally impossible in this subsystem.

use std::time::{Duration, Instant};

use super::{BufferPool, StuckAction, StuckDetector};

/// Something the kernel told us, drained from the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    /// A decoded frame is ready. `index` is the CAPTURE buffer holding it, which the renderer must
    /// hand back with [`V4l2Device::release_capture`] when it is done.
    Decoded { index: u32, timestamp: Duration },
    /// The stream's format changed mid-session. On the Frame this arrives as
    /// `V4L2_EVENT_SOURCE_CHANGE`, and it is the mechanism that makes a runtime quality step-down
    /// possible without a session restart — which GemLink cannot do today, because resolution and
    /// refresh sit inside `compute_restart_settings_hash`.
    SourceChange {
        width: u32,
        height: u32,
        fourcc: u32,
    },
}

/// Whether the device took the access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitStatus {
    Queued,
    /// Every OUTPUT buffer is held. This is the reference client's
    /// `SVLCodecV4L2::AcquireEncodedDataBuffer: Could not find a free OUTPUT buffer`, and it is
    /// counted, not swallowed.
    NoFreeOutputBuffer,
}

/// What the caller should do after a poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineEvent {
    /// A frame is ready to render, on this CAPTURE buffer.
    FrameReady { index: u32, timestamp: Duration },
    /// The pipeline is stalled past [`ASK_FOR_KEYFRAME_AFTER`]. Ask the sender for a keyframe,
    /// once. This is ADR-0011's recovery; the reference client logs it as
    /// `CheckStuck: > 300ms between our last forward progress. Asking remote side for a new IFrame`.
    AskForKeyframe { stalled_for: Duration },
    /// The pipeline is stalled past [`HARD_RESET_AFTER`] and the session was rebuilt. GemLink has
    /// had the keyframe rung since ADR-0011 and never had this one — which is exactly the latch
    /// that black-screened the first hardware run of that change.
    Reset { stalled_for: Duration },
    /// The decoder's format changed and the renderer must reconfigure.
    FormatChanged {
        width: u32,
        height: u32,
        fourcc: u32,
    },
}

/// Counters for the session summary. The shape follows the rest of the tree: a session ends by
/// printing a summary, so the answer never again depends on grepping a log — which is how both of
/// the voided measurements in `doc 50` were lost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecoderStats {
    pub access_units_submitted: u64,
    /// Every time the device could not take an access unit. **If this is non-zero the client is
    /// dropping frames, and it is the client's fault, not the wire's.**
    pub output_buffer_starved: u64,
    pub frames_decoded: u64,
    pub capture_buffers_released: u64,
    /// A decoded frame named a CAPTURE buffer the renderer was already holding, or one the device
    /// never handed out. Either is a real bug — an aliased frame, or a device that has lost track.
    pub duplicate_captures: u64,
    /// The renderer returned a buffer it did not hold. Harmless in isolation, a strong smell of a
    /// double-release bug that would otherwise hand one buffer to two readers.
    pub spurious_releases: u64,
    pub source_changes: u64,
    pub keyframe_requests: u64,
    pub hard_resets: u64,
}

impl DecoderStats {
    /// One line, printed at session end.
    pub fn summary(&self) -> String {
        format!(
            "video decoder: {} access units in, {} decoded, {} starved (no free OUTPUT buffer), \
             {} capture buffers returned, {} format change(s), {} keyframe request(s), {} reset(s)",
            self.access_units_submitted,
            self.frames_decoded,
            self.output_buffer_starved,
            self.capture_buffers_released,
            self.source_changes,
            self.keyframe_requests,
            self.hard_resets,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V4l2Error {
    /// The device could not be opened. Carries the path, because "there is no decoder" and "the
    /// decoder is busy" are different problems and both happen on a real device.
    Open {
        path: String,
        reason: String,
    },
    Io(String),
}

impl std::fmt::Display for V4l2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            V4l2Error::Open { path, reason } => write!(f, "cannot open {path}: {reason}"),
            V4l2Error::Io(reason) => write!(f, "{reason}"),
        }
    }
}

/// The kernel-facing half of the decoder.
///
/// The real implementation (`device.rs`) issues the ioctls; the mock in this file's tests drives
/// the policy. Nothing here is Android-shaped: the decoded frame is identified by a CAPTURE buffer
/// **index the renderer imports**, not by an `AHardwareBuffer` pointer, because on Linux the frame
/// reaches Vulkan as a dma-buf — and because a raw pointer across this boundary is what makes the
/// existing Android code unsafe to port in the first place.
pub trait V4l2Device {
    fn open(&mut self) -> Result<(), V4l2Error>;
    fn submit(&mut self, timestamp: Duration, nal: &[u8]) -> Result<SubmitStatus, V4l2Error>;
    fn poll_events(&mut self, out: &mut Vec<DeviceEvent>) -> Result<(), V4l2Error>;
    fn release_capture(&mut self, index: u32) -> Result<(), V4l2Error>;
    fn hard_reset(&mut self) -> Result<(), V4l2Error>;
    /// The device path, for the log line and for the error above.
    fn path(&self) -> &str;
}

/// Ties the device, the stall detector and the accounting together.
pub struct DecoderPipeline<D: V4l2Device> {
    device: D,
    stuck: StuckDetector,
    /// Mirrors the CAPTURE buffers the renderer is holding, so a leak is visible rather than
    /// inferred from a device that eventually stops producing.
    captured: BufferPool,
    stats: DecoderStats,
}

impl<D: V4l2Device> DecoderPipeline<D> {
    /// `capture_buffers` is the number the device granted for CAPTURE; the pool is the client's
    /// accounting of who holds them.
    pub fn new(device: D, capture_buffers: usize, now: Instant) -> Self {
        Self {
            device,
            stuck: StuckDetector::new(now),
            captured: BufferPool::new(capture_buffers, 0..capture_buffers as u32),
            stats: DecoderStats::default(),
        }
    }

    pub fn stats(&self) -> &DecoderStats {
        &self.stats
    }

    pub fn device(&self) -> &D {
        &self.device
    }

    /// Hand one access unit to the decoder.
    ///
    /// `Ok(false)` means the device had no free OUTPUT buffer — counted, and the caller must treat
    /// it as a dropped frame rather than a transport event. It is not an error and it is not
    /// silence.
    pub fn submit(&mut self, timestamp: Duration, nal: &[u8]) -> Result<bool, V4l2Error> {
        self.stats.access_units_submitted += 1;
        match self.device.submit(timestamp, nal)? {
            SubmitStatus::Queued => Ok(true),
            SubmitStatus::NoFreeOutputBuffer => {
                self.stats.output_buffer_starved += 1;
                Ok(false)
            }
        }
    }

    /// Drain the device and decide what the stall detector thinks. Call once per iteration of the
    /// client's receive/render loop.
    pub fn poll(&mut self, now: Instant) -> Result<Vec<PipelineEvent>, V4l2Error> {
        let mut device_events = Vec::new();
        self.device.poll_events(&mut device_events)?;

        let mut out = Vec::with_capacity(device_events.len() + 1);

        for event in device_events {
            match event {
                DeviceEvent::Decoded { index, timestamp } => {
                    self.stats.frames_decoded += 1;
                    // The renderer now holds this buffer. If it was not free, the device has
                    // handed out a buffer twice or the renderer never returned the last one —
                    // both are worth a counter, neither is worth ignoring.
                    if !self.captured.acquire_specific(index) {
                        self.stats.duplicate_captures += 1;
                    }
                    // A decoded frame is the definition of forward progress.
                    self.stuck.progress(now);
                    out.push(PipelineEvent::FrameReady { index, timestamp });
                }
                DeviceEvent::SourceChange {
                    width,
                    height,
                    fourcc,
                } => {
                    self.stats.source_changes += 1;
                    // A format change is progress too: it means the device is alive.
                    self.stuck.progress(now);
                    out.push(PipelineEvent::FormatChanged {
                        width,
                        height,
                        fourcc,
                    });
                }
            }
        }

        // Only consult the stall detector if the device produced nothing this round; a round that
        // decoded a frame has already reset it.
        if out.is_empty() {
            match self.stuck.poll(now) {
                StuckAction::AskForKeyframe => {
                    self.stats.keyframe_requests += 1;
                    out.push(PipelineEvent::AskForKeyframe {
                        stalled_for: self.stuck.stalled_for(now),
                    });
                }
                StuckAction::Reset => {
                    self.stats.hard_resets += 1;
                    // The reset may fail; the caller decides whether that is fatal, and the
                    // counter has already recorded the attempt.
                    self.device.hard_reset()?;
                    self.stuck.progress(now);
                    out.push(PipelineEvent::Reset {
                        stalled_for: self.stuck.stalled_for(now),
                    });
                }
                StuckAction::Progress => {}
            }
        }

        Ok(out)
    }

    /// The renderer is finished with a decoded frame; give the buffer back.
    pub fn release(&mut self, index: u32) -> Result<(), V4l2Error> {
        self.device.release_capture(index)?;
        if self.captured.release(index) {
            self.stats.capture_buffers_released += 1;
        } else {
            self.stats.spurious_releases += 1;
        }
        Ok(())
    }

    /// How many CAPTURE buffers the renderer is still holding. A number that only grows is a leak;
    /// on a device with a fixed buffer count it is also a decoder that will stop, and it is much
    /// cheaper to notice here than to infer it from a frozen picture.
    pub fn held_by_renderer(&self) -> usize {
        self.captured.in_use()
    }

    /// The stall detector's view, for logging.
    pub fn stalled_for(&self, now: Instant) -> Duration {
        self.stuck.stalled_for(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video_decoder::v4l2::{ASK_FOR_KEYFRAME_AFTER, HARD_RESET_AFTER};

    /// A scripted device. It is told what to do, so the *policy* is what is under test.
    #[derive(Default)]
    struct MockDevice {
        opened: bool,
        /// Access units accepted so far, used to synthesise decode events.
        pending: std::collections::VecDeque<DeviceEvent>,
        /// While set, every submit reports a starved OUTPUT ring.
        starved: bool,
        resets: u32,
        released: Vec<u32>,
    }

    impl MockDevice {
        fn with_events(events: impl IntoIterator<Item = DeviceEvent>) -> Self {
            Self {
                opened: true,
                pending: events.into_iter().collect(),
                ..Default::default()
            }
        }
    }

    impl V4l2Device for MockDevice {
        fn open(&mut self) -> Result<(), V4l2Error> {
            self.opened = true;
            Ok(())
        }
        fn submit(&mut self, _t: Duration, _nal: &[u8]) -> Result<SubmitStatus, V4l2Error> {
            if self.starved {
                Ok(SubmitStatus::NoFreeOutputBuffer)
            } else {
                Ok(SubmitStatus::Queued)
            }
        }
        fn poll_events(&mut self, out: &mut Vec<DeviceEvent>) -> Result<(), V4l2Error> {
            out.extend(self.pending.drain(..));
            Ok(())
        }
        fn release_capture(&mut self, index: u32) -> Result<(), V4l2Error> {
            self.released.push(index);
            Ok(())
        }
        fn hard_reset(&mut self) -> Result<(), V4l2Error> {
            self.resets += 1;
            Ok(())
        }
        fn path(&self) -> &str {
            "/dev/video-mock"
        }
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_decoded_frame_is_progress_and_resets_the_stall_clock() {
        let start = t0();
        let dev = MockDevice::with_events([DeviceEvent::Decoded {
            index: 3,
            timestamp: Duration::from_millis(16),
        }]);
        let mut pipe = DecoderPipeline::new(dev, 4, start);

        let events = pipe.poll(start + Duration::from_millis(900)).unwrap();
        assert_eq!(
            events,
            vec![PipelineEvent::FrameReady {
                index: 3,
                timestamp: Duration::from_millis(16)
            }],
            "a frame arrived, so 900 ms of silence before it must not also trigger a reset"
        );
        assert_eq!(pipe.stats().frames_decoded, 1);
        assert_eq!(pipe.stats().hard_resets, 0);
        assert_eq!(pipe.stats().keyframe_requests, 0);
    }

    #[test]
    fn a_stall_asks_once_then_resets_the_device() {
        let start = t0();
        let mut pipe = DecoderPipeline::new(MockDevice::default(), 4, start);

        // 300 ms: ask, once.
        let asked = pipe.poll(start + ASK_FOR_KEYFRAME_AFTER).unwrap();
        assert!(matches!(
            asked.as_slice(),
            [PipelineEvent::AskForKeyframe { .. }]
        ));
        assert_eq!(pipe.stats().keyframe_requests, 1);

        // Still stalled, between the rungs: silent.
        assert!(
            pipe.poll(start + Duration::from_millis(500))
                .unwrap()
                .is_empty()
        );
        assert_eq!(pipe.stats().keyframe_requests, 1, "it asked twice");

        // 800 ms: the device is rebuilt, and the counter says so.
        let reset = pipe.poll(start + HARD_RESET_AFTER).unwrap();
        assert!(matches!(reset.as_slice(), [PipelineEvent::Reset { .. }]));
        assert_eq!(pipe.stats().hard_resets, 1);
    }

    #[test]
    fn a_starved_output_ring_is_counted_and_never_silent() {
        // The doc 50 §A10 defect, as a test. The device cannot take the access unit; the pipeline
        // must say so rather than dropping it on the floor.
        let start = t0();
        let mut pipe = DecoderPipeline::new(
            MockDevice {
                starved: true,
                ..Default::default()
            },
            4,
            start,
        );

        for _ in 0..5 {
            assert!(
                !pipe.submit(Duration::ZERO, &[0u8; 16]).unwrap(),
                "a starved submit reported success"
            );
        }
        assert_eq!(pipe.stats().output_buffer_starved, 5);
        assert_eq!(pipe.stats().access_units_submitted, 5);
        assert!(
            pipe.stats().summary().contains("5 starved"),
            "the summary hides the starvation: {}",
            pipe.stats().summary()
        );
    }

    #[test]
    fn a_format_change_is_progress_and_is_forwarded() {
        let start = t0();
        let dev = MockDevice::with_events([DeviceEvent::SourceChange {
            width: 1216,
            height: 544,
            fourcc: 0x43564548,
        }]);
        let mut pipe = DecoderPipeline::new(dev, 4, start);

        let events = pipe.poll(start + Duration::from_millis(900)).unwrap();
        assert_eq!(
            events,
            vec![PipelineEvent::FormatChanged {
                width: 1216,
                height: 544,
                fourcc: 0x43564548
            }]
        );
        assert_eq!(pipe.stats().source_changes, 1);
        assert_eq!(
            pipe.stats().hard_resets,
            0,
            "a format change is the device working, not a stall"
        );
    }

    #[test]
    fn captured_buffers_are_accounted_so_a_leak_and_a_double_release_cannot_hide() {
        let start = t0();
        let events = (0..3).map(|index| DeviceEvent::Decoded {
            index,
            timestamp: Duration::ZERO,
        });
        let mut pipe = DecoderPipeline::new(MockDevice::with_events(events), 3, start);

        pipe.poll(start).unwrap();
        assert_eq!(
            pipe.held_by_renderer(),
            3,
            "the renderer holds three frames"
        );

        // Returning one is a clean release; returning it again is not.
        pipe.release(1).unwrap();
        assert_eq!(pipe.held_by_renderer(), 2);
        pipe.release(1).unwrap();
        assert_eq!(pipe.stats().capture_buffers_released, 1);
        assert_eq!(
            pipe.stats().spurious_releases,
            1,
            "a double release went unnoticed"
        );

        // And a device that names a buffer the renderer already holds is caught, not aliased.
        let mut pipe2 = DecoderPipeline::new(
            MockDevice::with_events([
                DeviceEvent::Decoded {
                    index: 0,
                    timestamp: Duration::ZERO,
                },
                DeviceEvent::Decoded {
                    index: 0,
                    timestamp: Duration::ZERO,
                },
            ]),
            2,
            start,
        );
        pipe2.poll(start).unwrap();
        assert_eq!(
            pipe2.stats().duplicate_captures,
            1,
            "an aliased frame went unnoticed"
        );
    }

    #[test]
    fn the_summary_line_names_every_counter() {
        let stats = DecoderStats {
            access_units_submitted: 1000,
            output_buffer_starved: 7,
            frames_decoded: 993,
            ..Default::default()
        };
        let line = stats.summary();
        for needle in ["1000 access units in", "993 decoded", "7 starved"] {
            assert!(line.contains(needle), "{line:?} lacks {needle:?}");
        }
    }
}
