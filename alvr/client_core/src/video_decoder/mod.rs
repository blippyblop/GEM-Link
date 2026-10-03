#[cfg(target_os = "android")]
mod android;

// The Steam Frame's decode path. aarch64 Linux decodes through a stateful M2M V4L2 device, which
// this tree has never had: `video_decoder` was a stub everywhere except Android. Written against
// Valve's own client for the same device (`SVLCodecV4L2`) — see VD_RE/52-frame-vrlink-client.md.
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub mod v4l2;

use alvr_common::anyhow::Result;
use alvr_session::{CodecType, MediacodecProperty};
use std::time::Duration;

#[derive(Clone, Default, PartialEq)]
pub struct VideoDecoderConfig {
    pub codec: CodecType,
    pub force_software_decoder: bool,
    pub max_buffering_frames: f32,
    pub buffering_history_weight: f32,
    pub options: Vec<(String, MediacodecProperty)>,
    pub config_buffer: Vec<u8>,
    /// The coded size the session negotiated, when the caller knows it.
    ///
    /// The decoder needs it to configure the device *before* the first frame arrives. A stateful
    /// decoder derives its own geometry from the bitstream, so this is a hint rather than a
    /// contract — but a hint that is absent is a device configured at size zero, which is not a
    /// state every driver is willing to start from.
    pub coded_size: Option<(u32, u32)>,
}

// The renderer-facing description of a decoded frame lives in `alvr_graphics` — it is the
// renderer's input, and duplicating it here would be a second definition of a layout that both
// sides must agree on exactly.
pub use alvr_graphics::{DmaBufFrame, NativeFrame};

/// A decoded frame on its way to the renderer.
#[derive(Clone, Copy, Debug)]
pub struct VideoFrame {
    pub timestamp: Duration,
    /// The decoder's own name for the buffer this frame lives in. Hand it back with
    /// [`VideoDecoderSource::release_frame`] when the renderer is done: on Linux the decoder cannot
    /// reuse a CAPTURE buffer until then, and a client that never returns them stops decoding —
    /// quietly, and looking exactly like a dead stream.
    pub buffer: u32,
    /// What the graphics API has to import. [`NativeFrame::None`] means a frame arrived with
    /// nothing importable behind it, which every backend that can do that reports loudly.
    pub frame: NativeFrame,
}

pub struct VideoDecoderSink {
    #[cfg(target_os = "android")]
    inner: android::VideoDecoderSink,
    #[cfg(target_os = "linux")]
    inner: linux::VideoDecoderSink,
}

impl VideoDecoderSink {
    // returns true if frame has been successfully enqueued
    #[allow(unused_variables)]
    pub fn push_nal(&mut self, timestamp: Duration, nal: &[u8]) -> bool {
        #[cfg(target_os = "android")]
        {
            alvr_common::show_err(self.inner.push_frame_nal(timestamp, nal)).unwrap_or(false)
        }
        #[cfg(target_os = "linux")]
        {
            self.inner.push_frame_nal(timestamp, nal).unwrap_or(false)
        }
        #[cfg(not(any(target_os = "android", target_os = "linux")))]
        {
            false
        }
    }
}

pub struct VideoDecoderSource {
    #[cfg(target_os = "android")]
    inner: android::VideoDecoderSource,
    #[cfg(target_os = "linux")]
    inner: linux::VideoDecoderSource,
}

impl VideoDecoderSource {
    /// If a frame is available, return it. The caller **must** pass `frame.buffer` to
    /// [`Self::release_frame`] once the renderer is finished with it.
    pub fn get_frame(&mut self) -> Option<VideoFrame> {
        #[cfg(target_os = "android")]
        {
            self.inner
                .dequeue_frame()
                .map(|(timestamp, native)| VideoFrame {
                    timestamp,
                    buffer: 0,
                    frame: NativeFrame::HardwareBuffer(native as usize),
                })
        }
        #[cfg(target_os = "linux")]
        {
            self.inner.dequeue_frame()
        }
        #[cfg(not(any(target_os = "android", target_os = "linux")))]
        {
            None
        }
    }

    /// Give the decoder's buffer back. A no-op on platforms whose decoder releases on its own
    /// (Android's `ImageReader`), because the call site should not have to know which it is.
    #[allow(unused_variables)]
    pub fn release_frame(&mut self, buffer: u32) {
        #[cfg(target_os = "linux")]
        self.inner.release_frame(buffer);
    }
}

// report_frame_decoded: (target_timestamp: Duration) -> ()
#[allow(unused_variables)]
pub fn create_decoder(
    config: VideoDecoderConfig,
    report_frame_decoded: impl Fn(Result<Duration>) + Send + Sync + 'static,
) -> (VideoDecoderSink, VideoDecoderSource) {
    #[cfg(target_os = "android")]
    {
        let (sink, source) = android::video_decoder_split(
            config.clone(),
            config.config_buffer,
            report_frame_decoded,
        )
        .unwrap();

        (
            VideoDecoderSink { inner: sink },
            VideoDecoderSource { inner: source },
        )
    }
    #[cfg(target_os = "linux")]
    {
        match linux::video_decoder_split(config, report_frame_decoded) {
            Ok((sink, source)) => (
                VideoDecoderSink { inner: sink },
                VideoDecoderSource { inner: source },
            ),
            Err(e) => {
                // A decoder that could not be created at all is not the same as a decoder that
                // decodes nothing, and it must not present as one: the sinks below report every
                // frame as not accepted, which the receive loop reads as saturation and answers by
                // asking for a keyframe — a behaviour that is *visible* — rather than passing
                // frames into a hole.
                alvr_common::error!("cannot create the video decoder: {e}");
                let (sink, source) = linux::dead_decoder(&format!("{e}"));
                (
                    VideoDecoderSink { inner: sink },
                    VideoDecoderSource { inner: source },
                )
            }
        }
    }
    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    (VideoDecoderSink {}, VideoDecoderSource {})
}
