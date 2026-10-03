//! The Linux decode backend: a stateful M2M V4L2 codec, driven on its own thread.
//!
//! Android decodes in MediaCodec and hands the renderer an `AHardwareBuffer *`. The Frame has
//! neither: it decodes in a kernel-managed device and hands the renderer a **dma-buf file
//! descriptor**. This module is that backend, and it is the piece that made
//! `video_decoder::create_decoder` stop being a stub off Android — until now `push_nal` returned
//! `false` for every frame on this platform, which the receive loop reads as "the decoder refuses
//! everything" and answers by asking for a keyframe, forever.
//!
//! ## Why a thread
//!
//! The device is non-blocking by design (`device.rs` says so), so it could be polled from the
//! client's own loop. It is not, for two reasons. First, the reference client runs it on a media
//! thread with an `epoll` set and a nudge pipe (`SVLCodecV4L2::MediaThread`, `InitializeEpoll`),
//! and the ordering that matters — submit, then drain decode events, then release — is easier to
//! get right in one place. Second, and more important: the receive loop calls the decoder
//! **synchronously**, and `doc 50 §A10` is a session spent discovering what a synchronous,
//! over-budget decode does to the socket. A thread does not make decode faster, but it moves the
//! queue between the two into a place where *this* code can see it and count it, instead of into
//! the kernel's socket buffer where nothing can.
//!
//! ## The three failure modes this file refuses to hide
//!
//! 1. A starved OUTPUT ring. `pipeline::DecoderStats::output_buffer_starved`.
//! 2. A starved CAPTURE ring — a renderer that does not hand buffers back. The decoder thread
//!    cannot see that directly, but `held_by_renderer` is logged with the session summary.
//! 3. **No importable handle.** If the device cannot export a dma-buf we say so, loudly, at the
//!    moment it happens. A frame the renderer cannot import is a black screen, and a black screen
//!    with no log line is the exact defect class ADR-0014 exists to stop.

use super::v4l2::{
    abi::{V4L2_PIX_FMT_H264, V4L2_PIX_FMT_HEVC},
    device::{self, V4l2M2mDecoder},
    pipeline::{DecoderPipeline, DeviceEvent, PipelineEvent, SubmitStatus, V4l2Device, V4l2Error},
};
use super::{NativeFrame, VideoDecoderConfig, VideoFrame};
use alvr_common::{
    anyhow::{Result, anyhow},
    error, info, warn,
};
use alvr_graphics::DmaBufFrame;
use alvr_session::CodecType;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// The V4L2 fourcc for a negotiated codec, or `None` for one this device cannot decode.
///
/// AV1 is in this list only to be *named*: the Frame's decoder is fed by Qualcomm's signed VPU
/// firmware and the reference client's setup table lists H.264 and HEVC, so a session that
/// negotiates AV1 is a session whose settings are wrong, and it should say that rather than fail
/// silently later.
pub fn fourcc_for(codec: CodecType) -> Option<u32> {
    match codec {
        CodecType::H264 => Some(V4L2_PIX_FMT_H264),
        CodecType::Hevc => Some(V4L2_PIX_FMT_HEVC),
        CodecType::AV1 => None,
    }
}

/// Control messages. Separate from access units because they must **never** be refused: a
/// `Release` that is dropped because the queue was full is a CAPTURE buffer leaked, which is a
/// decoder that stops.
enum Control {
    /// The renderer is finished with a CAPTURE buffer.
    Release(u32),
    Shutdown,
}

/// How many access units may be waiting for the decoder.
///
/// This is a **latency budget, not a buffer size**, which is the whole point. Before this queue
/// existed the decoder was called synchronously from the receive loop: a decode that took 36 ms
/// against a 29.6 ms frame interval fell behind monotonically, the client's own 10-datagram socket
/// pool ran dry, and the reader discarded 1.5 % of everything it read *without recording it*
/// (`doc 50 §A10`). A thread and a queue remove that specific failure — but an unbounded queue
/// would replace "frames silently dropped" with "frames queued, latency unbounded", which is worse
/// for the metric this project is scored on.
///
/// So the queue is bounded by the same knob the setting offers for exactly this trade
/// (`video.max_buffering_frames`, "Increasing this value will help reduce stutter but it will
/// increase latency"), and a full queue is **refused and counted**, not queued and not silently
/// dropped.
const MAX_QUEUED_ACCESS_UNITS_RANGE: (usize, usize) = (1, 8);

fn queue_capacity(config: &VideoDecoderConfig) -> usize {
    let frames = config.max_buffering_frames.ceil();
    if !frames.is_finite() || frames < 1.0 {
        return 1;
    }
    (frames as usize).clamp(
        MAX_QUEUED_ACCESS_UNITS_RANGE.0,
        MAX_QUEUED_ACCESS_UNITS_RANGE.1,
    )
}

pub struct VideoDecoderSink {
    units: mpsc::SyncSender<(Duration, Vec<u8>)>,
    control: mpsc::Sender<Control>,
    /// Access units refused because the queue was full. Shared with the thread so the session
    /// summary carries it: a decoder that is behind is a fact about the session, and it belongs
    /// next to the other counters rather than in a log line nobody greps.
    refused: Arc<AtomicU64>,
    join_handle: Option<JoinHandle<()>>,
    /// Repeated in the warning above so the operator knows the budget without reading the settings.
    capacity: usize,
}

// A channel sender is Send; the join handle only ever joins.
unsafe impl Send for VideoDecoderSink {}

impl VideoDecoderSink {
    /// Hand one access unit to the decoder thread. `false` means it could not be taken — the queue
    /// is full, or the thread is gone — which the receive loop already reads as decoder saturation.
    ///
    /// It must not return `false` for a frame that *was* queued: the caller answers `false` by
    /// asking the sender for a keyframe, and doing that for a frame that is about to decode is a
    /// bitrate spike attached to nothing.
    pub fn push_frame_nal(&mut self, timestamp: Duration, data: &[u8]) -> Result<bool> {
        match self.units.try_send((timestamp, data.to_vec())) {
            Ok(()) => Ok(true),
            Err(mpsc::TrySendError::Full(_)) => {
                let refused = self.refused.fetch_add(1, Ordering::Relaxed) + 1;
                if refused == 1 || refused.is_multiple_of(120) {
                    warn!(
                        "video decoder: {refused} access unit(s) refused — the decoder is more than \
                         {} frame(s) behind and the queue is full. Dropping the frame keeps the \
                         latency budget; queueing it would spend latency to hide it.",
                        self.capacity
                    );
                }
                Ok(false)
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                error!("video decoder: the decoder thread is gone; no frame can be decoded");
                Ok(false)
            }
        }
    }

    /// Stop the decoder thread and wait for it. Called from `Drop`.
    fn shutdown(&mut self) {
        let _ = self.control.send(Control::Shutdown);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for VideoDecoderSink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub struct VideoDecoderSource {
    frames: mpsc::Receiver<VideoFrame>,
    control: mpsc::Sender<Control>,
    /// The buffers handed out and not yet returned. A number that only grows means the renderer is
    /// dropping them, which on a fixed-size CAPTURE ring is a decoder that stops.
    outstanding: VecDeque<u32>,
}

unsafe impl Send for VideoDecoderSource {}

impl VideoDecoderSource {
    pub fn dequeue_frame(&mut self) -> Option<VideoFrame> {
        match self.frames.try_recv() {
            Ok(frame) => {
                self.outstanding.push_back(frame.buffer);
                Some(frame)
            }
            Err(_) => None,
        }
    }

    /// Give a CAPTURE buffer back to the device. Must be called for every frame `dequeue_frame`
    /// returned, or the decoder's output ring empties and it stops producing.
    pub fn release_frame(&mut self, buffer: u32) {
        if let Some(position) = self.outstanding.iter().position(|&b| b == buffer) {
            self.outstanding.remove(position);
        }
        let _ = self.control.send(Control::Release(buffer));
    }
}

/// Create the Linux decoder: a real V4L2 device if there is one, a loopback if there is not.
pub fn video_decoder_split(
    config: VideoDecoderConfig,
    report_frame_decoded: impl Fn(Result<Duration>) + Send + Sync + 'static,
) -> Result<(VideoDecoderSink, VideoDecoderSource)> {
    let report_frame_decoded: Box<dyn Fn(Result<Duration>) + Send + Sync> =
        Box::new(report_frame_decoded);

    let Some(fourcc) = fourcc_for(config.codec) else {
        return Err(anyhow!(
            "this device decodes H.264 and HEVC; the session negotiated {:?}",
            config.codec
        ));
    };

    let (device, description) = select_device(&config, fourcc);

    let capacity = queue_capacity(&config);
    let (unit_sender, unit_receiver) = mpsc::sync_channel(capacity);
    let (control_sender, control_receiver) = mpsc::channel();
    let (frame_sender, frame_receiver) = mpsc::channel();
    let refused = Arc::new(AtomicU64::new(0));
    let refused_in_thread = Arc::clone(&refused);

    info!("video decoder: {description} (input queue: {capacity} frame(s))");

    let join_handle = thread::Builder::new()
        .name("alvr-decoder".to_owned())
        .spawn(move || {
            decoder_thread(
                device,
                config,
                unit_receiver,
                control_receiver,
                frame_sender,
                refused_in_thread,
                report_frame_decoded,
            )
        })
        .map_err(|e| anyhow!("cannot start the decoder thread: {e}"))?;

    Ok((
        VideoDecoderSink {
            units: unit_sender,
            control: control_sender.clone(),
            refused,
            join_handle: Some(join_handle),
            capacity,
        },
        VideoDecoderSource {
            frames: frame_receiver,
            control: control_sender,
            outstanding: VecDeque::new(),
        },
    ))
}

/// A decoder that could not be created at all.
///
/// The sinks it returns refuse every frame — `push_frame_nal` reports `false`, which the receive
/// loop already understands as decoder saturation — so a decoder that does not exist degrades into
/// a *visible* behaviour (keyframe requests, counters, a warn line) rather than into a client that
/// counts frames it silently threw away.
pub fn dead_decoder(reason: &str) -> (VideoDecoderSink, VideoDecoderSource) {
    error!("video decoder: unusable — {reason}");

    let (units, unit_receiver) = mpsc::sync_channel(1);
    let (control, control_receiver) = mpsc::channel();
    let source_control = control.clone();
    // Dropping the receivers is what makes every `send` fail, which is the behaviour above.
    drop(unit_receiver);
    drop(control_receiver);

    let (_frames, frame_receiver) = mpsc::channel();

    (
        VideoDecoderSink {
            units,
            control,
            refused: Arc::new(AtomicU64::new(0)),
            join_handle: None,
            capacity: 1,
        },
        VideoDecoderSource {
            frames: frame_receiver,
            control: source_control,
            outstanding: VecDeque::new(),
        },
    )
}

/// Pick a backend, and say **in words** which one and why.
///
/// A loopback is chosen only when asked for or when there is genuinely no device, and never
/// quietly: it produces no pixels, so the client will present nothing, and the difference between
/// "the decoder is broken" and "there is no decoder" has to be in the log before the picture is
/// missing rather than after.
fn select_device(config: &VideoDecoderConfig, fourcc: u32) -> (Box<dyn V4l2Device + Send>, String) {
    if config.force_software_decoder {
        return (
            Box::new(LoopbackDevice::new(
                "loopback (forced by force_software_decoder)",
            )),
            "loopback decoder, forced by `force_software_decoder` — **frames are counted, not \
             decoded; nothing will be displayed**"
                .to_owned(),
        );
    }

    let (path, tried) = device::discover_device(fourcc);
    match path {
        Some(path) => {
            let coded = config.coded_size.unwrap_or((0, 0));
            (
                Box::new(V4l2M2mDecoder::new(path.clone(), fourcc, coded)),
                format!("V4L2 M2M decoder at {path}, coded size {coded:?}"),
            )
        }
        None => {
            let detail = if tried.is_empty() {
                "there is no /dev/video* on this machine".to_owned()
            } else {
                format!("no node accepted the codec: {}", tried.join("; "))
            };
            warn!(
                "video decoder: no hardware decoder found ({detail}) — falling back to the \
                 loopback decoder. **Frames will be counted but not decoded and nothing will be \
                 displayed.** This is a diagnosis, not a working stream."
            );
            (
                Box::new(LoopbackDevice::new("loopback (no V4L2 device)")),
                format!("loopback decoder, no V4L2 device found: {detail}"),
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decoder_thread(
    mut device: Box<dyn V4l2Device + Send>,
    config: VideoDecoderConfig,
    units: mpsc::Receiver<(Duration, Vec<u8>)>,
    control: mpsc::Receiver<Control>,
    frames: mpsc::Sender<VideoFrame>,
    refused: Arc<AtomicU64>,
    report_frame_decoded: Box<dyn Fn(Result<Duration>) + Send + Sync>,
) {
    let path = device.path().to_owned();

    if let Err(e) = device.open() {
        let message = format!("video decoder: {e}");
        error!("{message}");
        report_frame_decoded(Err(anyhow!(message)));
        return;
    }

    let capture_buffers = match device.capture_buffer_count() {
        count if count > 0 => count,
        // Not fatal: a device that cannot say how many CAPTURE buffers it made still decodes. The
        // pool is the client's accounting and it is permissive rather than authoritative.
        _ => 8,
    };

    let mut pipeline = DecoderPipeline::new(device, capture_buffers, Instant::now());

    // One dma-buf per CAPTURE buffer, exported once and reused.
    //
    // Exporting on every frame would work and would be worse: a dma-buf import is the expensive
    // half of the handoff, and a decoder that hands out a new fd sixty times a second is asking the
    // renderer to rebuild its EGLImage sixty times a second. The buffer is the same memory every
    // time; the fd is an identity for it, not a copy of it.
    let mut dma_buf_fds: Vec<i32> = vec![-1; capture_buffers];

    // Import failures are reported once, not per frame — but they *are* reported, because a
    // renderer that cannot import a frame shows nothing at all.
    let mut import_failure_reported = false;

    info!(
        "video decoder: {} codec-configuration byte(s), {}",
        config.config_buffer.len(),
        if config.config_buffer.is_empty() {
            "no parameter sets yet (they arrive with the first keyframe)"
        } else {
            "parameter sets in hand"
        }
    );

    loop {
        // Control first, and drained to empty: a `Release` is a buffer coming back to the device,
        // and holding one behind a queue of access units is how a decoder starves itself.
        loop {
            match control.try_recv() {
                Ok(Control::Release(index)) => {
                    if let Err(e) = pipeline.release(index) {
                        error!("video decoder: releasing CAPTURE buffer {index}: {e}");
                    }
                }
                Ok(Control::Shutdown) => {
                    report_summary(&pipeline, &refused);
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    report_summary(&pipeline, &refused);
                    return;
                }
            }
        }

        // Then one access unit, with a short tick so a decode event is never sat on. The bounded
        // queue is what makes this a `recv_timeout` rather than a drain: at most `capacity` frames
        // are ever waiting, and the producer learns immediately when it cannot add one.
        match units.recv_timeout(Duration::from_millis(2)) {
            Ok((timestamp, nal)) => match pipeline.submit(timestamp, &nal) {
                Ok(true) => {}
                Ok(false) => {
                    // Counted by the pipeline. This is a frame the client itself dropped, and it
                    // must be visible as such rather than as a network loss.
                    warn!("video decoder: no free OUTPUT buffer; dropped an access unit");
                }
                Err(e) => {
                    error!("video decoder: {e}");
                    report_frame_decoded(Err(anyhow!("{e}")));
                    return;
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                report_summary(&pipeline, &refused);
                return;
            }
        }

        let events = match pipeline.poll(Instant::now()) {
            Ok(events) => events,
            Err(e) => {
                error!("video decoder: {e}");
                report_frame_decoded(Err(anyhow!("{e}")));
                return;
            }
        };

        for event in events {
            match event {
                PipelineEvent::FrameReady { index, timestamp } => {
                    let native = export_or_reuse(&pipeline, &mut dma_buf_fds, index);
                    if native == 0 && !import_failure_reported {
                        import_failure_reported = true;
                        error!(
                            "video decoder: {path} decoded a frame but could not export a dma-buf \
                             for CAPTURE buffer {index}; the renderer has nothing to import and \
                             the picture will be black. This is the client's fault, not the wire's."
                        );
                    }

                    // The layout travels with the frame because the fd means nothing without it:
                    // an importer that guesses the stride gets a sheared picture.
                    let native_frame = pipeline
                        .device()
                        .capture_geometry()
                        .map(|mut dma_buf| {
                            dma_buf.fds[0] = native as i32;
                            NativeFrame::DmaBuf(dma_buf)
                        })
                        .unwrap_or(NativeFrame::None);

                    let frame = VideoFrame {
                        timestamp,
                        buffer: index,
                        frame: native_frame,
                    };

                    // Losing the receiver means the client is shutting down.
                    if frames.send(frame).is_err() {
                        report_summary(&pipeline, &refused);
                        return;
                    }

                    report_frame_decoded(Ok(timestamp));
                }
                PipelineEvent::FormatChanged {
                    width,
                    height,
                    fourcc,
                } => {
                    info!("video decoder: format changed to {width}x{height} {fourcc:#x}");
                }
                PipelineEvent::AskForKeyframe { stalled_for } => {
                    // The device has produced nothing for 300 ms. The *receive* loop's own ladder
                    // answers this today (it sees the same silence as a lack of decoded frames);
                    // this line is here so the decoder's view of the stall is in the log too, and
                    // so the two can be joined when they disagree.
                    warn!(
                        "video decoder: no decoded frame for {:.0} ms — the sender should be \
                         sending a keyframe",
                        stalled_for.as_secs_f64() * 1e3
                    );
                }
                PipelineEvent::Reset { stalled_for } => {
                    error!(
                        "video decoder: stalled for {:.0} ms; rebuilt the device",
                        stalled_for.as_secs_f64() * 1e3
                    );
                }
            }
        }
    }
}

/// Print the session's decode accounting.
///
/// Two counters, because they are two different failures with the same symptom.
/// `output_buffer_starved` is the *device* refusing an access unit; `refused` is the *queue* being
/// full, which means the decoder never even saw it. The first is the decoder being pushed past what
/// it can do, the second is the client choosing latency over smoothness — and a client that cannot
/// tell them apart cannot be tuned.
fn report_summary<D: V4l2Device>(pipeline: &DecoderPipeline<D>, refused: &AtomicU64) {
    let refused = refused.load(Ordering::Relaxed);
    info!(
        "{}; {} access unit(s) refused by the client's own input queue",
        pipeline.stats().summary(),
        refused
    );
    if refused > 0 {
        warn!(
            "the decoder could not keep up and the client dropped {refused} frame(s) rather than \
             queue them. That is the intended trade — a queued frame is pure added latency — but it \
             means the negotiated settings do not fit this device."
        );
    }
}

/// The dma-buf fd for a CAPTURE buffer, exported on first use.
fn export_or_reuse<D: V4l2Device>(
    pipeline: &DecoderPipeline<D>,
    fds: &mut [i32],
    index: u32,
) -> usize {
    let slot = index as usize;
    if slot >= fds.len() {
        return 0;
    }
    if fds[slot] < 0 {
        match pipeline.device().export_capture(index, 0) {
            Ok(fd) => fds[slot] = fd,
            Err(_) => return 0,
        }
    }
    fds[slot].max(0) as usize
}

// ---------------------------------------------------------------------------------------------
// The fallback
// ---------------------------------------------------------------------------------------------

/// A decoder that accepts access units and produces frames with **no pixels in them**.
///
/// It exists so that a machine without a V4L2 codec can still run the whole client-side video
/// path — the receive loop, the accounting, the render handoff — and so that a missing decoder is a
/// named, logged condition instead of a client that silently never shows anything. What it is *not*
/// is a software decoder, and it must never be mistaken for one: every path that selects it says
/// so at `warn` or louder, because a fallback that makes a broken stream look like a working one is
/// precisely the failure ADR-0014 names.
///
/// `vicodec` (`modprobe vicodec`) is the real answer for a machine without hardware: it is a
/// software stateful M2M codec, so it exercises this backend's ioctls for real.
pub struct LoopbackDevice {
    path: String,
    opened: bool,
    pending: VecDeque<DeviceEvent>,
    released: Vec<u32>,
    /// Whether the caller has been told this device decodes nothing. See the type docs.
    announced: bool,
}

impl LoopbackDevice {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            opened: false,
            pending: VecDeque::new(),
            released: Vec::new(),
            announced: false,
        }
    }
}

impl V4l2Device for LoopbackDevice {
    fn open(&mut self) -> Result<(), V4l2Error> {
        self.opened = true;
        Ok(())
    }

    fn submit(&mut self, timestamp: Duration, nal: &[u8]) -> Result<SubmitStatus, V4l2Error> {
        if !self.opened {
            return Err(V4l2Error::Open {
                path: self.path.clone(),
                reason: "not open".into(),
            });
        }

        if !self.announced {
            self.announced = true;
            warn!(
                "loopback decoder: {} access-unit byte(s) accepted, 0 decoded — this backend \
                 produces no pixels by construction",
                nal.len()
            );
        }

        // One frame in, one frame out, immediately. The *timing* is a lie (a real decoder has a
        // depth and a delay); the *plumbing* is not, which is all this is for.
        self.pending.push_back(DeviceEvent::Decoded {
            index: (self.pending.len() as u32) % 8,
            timestamp,
        });
        Ok(SubmitStatus::Queued)
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
        self.pending.clear();
        Ok(())
    }

    fn export_capture(&self, _index: u32, _plane: u32) -> Result<i32, V4l2Error> {
        // Explicitly not a silent zero: the caller turns this into the "could not export a
        // dma-buf" error, which is true and is the point.
        Err(V4l2Error::Io(
            "the loopback decoder has no buffers to export".into(),
        ))
    }

    fn capture_buffer_count(&self) -> usize {
        8
    }

    fn capture_geometry(&self) -> Option<DmaBufFrame> {
        None
    }

    fn path(&self) -> &str {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The queue is a latency budget, and the whole reason it is bounded is that the alternative —
    /// unbounded — trades a dropped frame for an unbounded delay, which is the worse half of the
    /// bargain on a device scored on latency.
    #[test]
    fn the_input_queue_is_a_latency_budget_and_is_bounded_at_both_ends() {
        fn with_budget(frames: f32) -> usize {
            queue_capacity(&VideoDecoderConfig {
                max_buffering_frames: frames,
                ..Default::default()
            })
        }

        // The setting's own range starts at 1.0, but a settings file is not a type: a zero, a NaN
        // or a 10 000 arrives here eventually, and none of them may become a zero-length or an
        // unbounded queue.
        assert_eq!(
            with_budget(0.0),
            1,
            "an unset budget still needs room for a frame"
        );
        assert_eq!(with_budget(f32::NAN), 1);
        assert_eq!(with_budget(-3.0), 1);

        assert_eq!(with_budget(2.0), 2);
        // `ceil`, not truncation: a budget of 2.1 frames means "more than two", and rounding it
        // down would deliver less buffering than the operator asked for.
        assert_eq!(with_budget(2.1), 3);

        assert_eq!(
            with_budget(10_000.0),
            MAX_QUEUED_ACCESS_UNITS_RANGE.1,
            "an unbounded decoder queue is unbounded latency, which is the thing this queue exists \
             to bound"
        );
    }

    /// A decoder that does not exist must refuse frames visibly, not accept them into a hole.
    #[test]
    fn a_dead_decoder_refuses_every_frame() {
        let (mut sink, mut source) = dead_decoder("test");

        assert!(
            !sink
                .push_frame_nal(Duration::from_millis(1), &[0, 0, 0, 1])
                .unwrap(),
            "a dead decoder must refuse the frame, not accept it into a hole"
        );
        assert!(
            source.dequeue_frame().is_none(),
            "a dead decoder must not hand out a frame it did not decode"
        );
        // Releasing a buffer it never had must be harmless rather than a panic.
        source.release_frame(3);
    }

    #[test]
    fn only_the_codecs_the_device_decodes_have_a_fourcc() {
        assert_eq!(fourcc_for(CodecType::H264), Some(V4L2_PIX_FMT_H264));
        assert_eq!(fourcc_for(CodecType::Hevc), Some(V4L2_PIX_FMT_HEVC));
        assert_eq!(
            fourcc_for(CodecType::AV1),
            None,
            "AV1 must be refused at creation, not silently attempted"
        );
    }

    #[test]
    fn the_loopback_counts_frames_and_produces_no_handle() {
        let mut device = LoopbackDevice::new("test");

        // Before `open`, the device refuses — a loopback must not be *more* permissive than a real
        // one, or the tests above it stop meaning anything.
        assert!(device.submit(Duration::ZERO, &[1, 2, 3]).is_err());

        device.open().unwrap();
        assert_eq!(
            device.submit(Duration::from_millis(5), &[1, 2, 3]).unwrap(),
            SubmitStatus::Queued
        );

        let mut events = Vec::new();
        device.poll_events(&mut events).unwrap();
        assert_eq!(events.len(), 1, "one access unit in, one frame out");
        assert!(
            device.export_capture(0, 0).is_err(),
            "the loopback must not pretend to have an importable handle; the error is what \
             produces the loud failure at the call site"
        );
        assert!(device.capture_geometry().is_none());
    }
}
