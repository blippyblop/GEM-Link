//! The syscall glue: a real M2M V4L2 codec, behind [`V4l2Device`].
//!
//! The policy above it (`pipeline.rs`) is tested; the ABI under it (`abi.rs`) is verified against
//! the numbers in `linux/videodev2.h`. This file is the part in between, and it is the only part of
//! the decoder that **cannot** be checked here: there is no `/dev/video*`, no system-mode qemu, and
//! no privilege to load `vicodec`. It has never executed.
//!
//! That is written down rather than glossed, because "it compiles" and "the ioctls are right" are
//! very different claims and this project has been bitten by the difference. What the untested risk
//! is *now* bounded to: the exact ordering of the setup calls — which is enumerated in
//! [`super::ioctls::SETUP_SEQUENCE`] — and the two buffer loops, which are a dozen lines each.
//!
//! ## The shape, from the reference client
//!
//! Valve's `SVLCodecV4L2`, on this device, runs the device on an `epoll`-driven media thread with a
//! nudge pipe (`SVLCodecV4L2::MediaThread`, `InitializeEpoll`) rather than polling. This type does
//! **not** own a thread or an epoll set: it exposes non-blocking `submit`/`poll_events`, and the
//! client's own loop decides when to call them. That keeps the thread model in one place — the
//! client already has a receive loop — and keeps the device testable by a mock, which is the whole
//! reason `V4l2Device` is a trait.

use std::{
    ffi::{CString, c_void},
    io,
    os::fd::RawFd,
    ptr,
    time::Duration,
};

use super::{
    abi::*,
    pipeline::{DeviceEvent, SubmitStatus, V4l2Device, V4l2Error},
};
use alvr_graphics::{DmaBufFrame, drm_fourcc_from_v4l2};

/// One mmap'd plane of one buffer.
#[derive(Debug, Clone, Copy)]
struct MappedPlane {
    ptr: *mut u8,
    len: usize,
    /// The plane's offset inside the buffer, as `VIDIOC_QUERYBUF` reported it. A dma-buf import
    /// needs it: the fd describes the whole buffer, and this says which byte the plane starts at.
    offset: u32,
}

/// The buffers of one queue (`OUTPUT` or `CAPTURE`), each with its planes.
#[derive(Debug, Default)]
struct Queue {
    kind: u32,
    buffers: Vec<Vec<MappedPlane>>,
}

impl Queue {
    fn from_kind(kind: u32) -> Self {
        Self {
            kind,
            buffers: Vec::new(),
        }
    }
}

/// A stateful memory-to-memory V4L2 decoder — the Frame's decode path.
#[derive(Debug)]
pub struct V4l2M2mDecoder {
    path: String,
    fd: RawFd,
    /// The coded size the session asked for, kept so `hard_reset` can rebuild it.
    coded: (u32, u32),
    fourcc: u32,
    output: Queue,
    capture: Queue,
    /// Learned from the device at `open`, not from the session: a decoder pads its output to its
    /// own alignment and reports the result here.
    capture_geometry: Option<DmaBufFrame>,
}

// The fd is a plain kernel handle; the mmaps are owned solely by this type and released on drop.
unsafe impl Send for V4l2M2mDecoder {}

impl V4l2M2mDecoder {
    /// Open a decoder for a codec at a declared coded size. `path` is explicit because on the Frame
    /// the device node is discovered, not assumed — the reference client logs
    /// `Failed to get video device path`.
    pub fn new(path: impl Into<String>, fourcc: u32, coded: (u32, u32)) -> Self {
        Self {
            path: path.into(),
            fd: -1,
            coded,
            fourcc,
            output: Queue::from_kind(V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE),
            capture: Queue::from_kind(V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE),
            capture_geometry: None,
        }
    }

    pub fn coded(&self) -> (u32, u32) {
        self.coded
    }

    pub fn fourcc(&self) -> u32 {
        self.fourcc
    }

    /// Where this CAPTURE plane is mapped, for a caller that has to fall back to the pixels when
    /// the device cannot export a dma-buf. Returns `(address, length)`.
    pub fn capture_plane_ptr(&self, index: u32, plane: usize) -> Option<(*mut u8, usize)> {
        self.capture
            .buffers
            .get(index as usize)?
            .get(plane)
            .map(|mapped| (mapped.ptr, mapped.len))
    }

    fn err<T>(&self, what: &str, e: io::Error) -> Result<T, V4l2Error> {
        Err(V4l2Error::Io(format!("{}: {}", what, e)))
    }

    /// `ioctl` with an argument, which is the only way this layer talks to the kernel.
    ///
    /// # Safety
    /// The caller guarantees `arg` points at a structure of the layout the request number was
    /// derived from — which `abi.rs`'s tests are what make trustworthy.
    unsafe fn ioctl<T>(&self, request: u64, arg: *mut T) -> io::Result<i32> {
        let rc = unsafe { libc::ioctl(self.fd, request as libc::c_ulong, arg) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rc)
        }
    }

    fn querycap(&self) -> Result<String, V4l2Error> {
        let mut cap = V4l2Capability::default();
        if let Err(e) = unsafe { self.ioctl(VIDIOC_QUERYCAP, &mut cap) } {
            return Err(V4l2Error::Io(format!("VIDIOC_QUERYCAP: {e}")));
        }
        // Logged, not asserted: on a device we cannot debug, the driver string is the single most
        // useful thing to have in the first failure. `abi.rs` says to do this first.
        let driver: String = cap
            .driver
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as char)
            .collect();
        let card: String = cap
            .card
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as char)
            .collect();
        Ok(format!("{driver} / {card} (v{:x})", cap.version))
    }

    fn set_format(
        &mut self,
        kind: u32,
        width: u32,
        height: u32,
        fourcc: u32,
    ) -> Result<(), V4l2Error> {
        let mut fmt = V4l2Format {
            kind,
            ..Default::default()
        };
        fmt.pix_mp.width = width;
        fmt.pix_mp.height = height;
        fmt.pix_mp.pixelformat = fourcc;
        fmt.pix_mp.field = 1; // V4L2_FIELD_NONE on the compressed side
        fmt.pix_mp.num_planes = 1;

        match unsafe { self.ioctl(VIDIOC_S_FMT, &mut fmt) } {
            Ok(_) => Ok(()),
            Err(e) => self.err("VIDIOC_S_FMT", e),
        }
    }

    fn get_format(&self, kind: u32) -> Result<V4l2PixFormatMplane, V4l2Error> {
        let mut fmt = V4l2Format {
            kind,
            ..Default::default()
        };
        match unsafe { self.ioctl(VIDIOC_G_FMT, &mut fmt) } {
            Ok(_) => Ok(fmt.pix_mp),
            Err(e) => self.err("VIDIOC_G_FMT", e),
        }
    }

    /// Set one of the decoder's presentation-delay controls.
    ///
    /// These are why a userspace reorder buffer should not exist on top: the hardware can hold
    /// frames itself, and doing it twice spends the latency twice. Best-effort — a decoder that does
    /// not implement them is still usable, just less efficient — but the reference client logs a
    /// failure, and so do we.
    fn set_ctrl(&self, id: u32, value: i32) -> bool {
        let mut ctrl = V4l2Control { id, value };
        unsafe { self.ioctl(VIDIOC_S_CTRL, &mut ctrl) }.is_ok()
    }

    fn request_buffers(&self, kind: u32, count: u32) -> Result<u32, V4l2Error> {
        let mut req = V4l2RequestBuffers {
            count,
            kind,
            memory: V4L2_MEMORY_MMAP,
            ..Default::default()
        };
        match unsafe { self.ioctl(VIDIOC_REQBUFS, &mut req) } {
            // The kernel may grant fewer than asked; that is information, not a failure.
            Ok(_) => Ok(req.count),
            Err(e) => self.err("VIDIOC_REQBUFS", e),
        }
    }

    fn map_queue(
        &mut self,
        kind: u32,
        count: u32,
        planes_per_buffer: u32,
    ) -> Result<Queue, V4l2Error> {
        let mut queue = Queue::from_kind(kind);

        for index in 0..count {
            let mut planes = [V4l2Plane::default(); VIDEO_MAX_PLANES];
            let mut buf = V4l2Buffer {
                index,
                kind,
                memory: V4L2_MEMORY_MMAP,
                length: planes_per_buffer,
                m: planes.as_mut_ptr() as u64,
                ..Default::default()
            };

            if let Err(e) = unsafe { self.ioctl(VIDIOC_QUERYBUF, &mut buf) } {
                return self.err("VIDIOC_QUERYBUF", e);
            }

            let mut mapped = Vec::with_capacity(planes_per_buffer as usize);
            for plane in planes.iter().take(planes_per_buffer as usize) {
                let len = plane.length as usize;
                if len == 0 {
                    continue;
                }
                let addr = unsafe {
                    libc::mmap(
                        ptr::null_mut(),
                        len,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        self.fd,
                        plane.mem_offset() as libc::off_t,
                    )
                };
                if addr == libc::MAP_FAILED {
                    return self.err("mmap", io::Error::last_os_error());
                }
                mapped.push(MappedPlane {
                    ptr: addr as *mut u8,
                    len,
                    offset: plane.data_offset,
                });
            }
            queue.buffers.push(mapped);
        }

        Ok(queue)
    }

    /// Hand every buffer of a queue to the kernel, which is what makes it usable.
    fn queue_all(&self, queue: &Queue, planes_per_buffer: u32) -> Result<(), V4l2Error> {
        for (index, mapped) in queue.buffers.iter().enumerate() {
            let mut planes = [V4l2Plane::default(); VIDEO_MAX_PLANES];
            for (slot, plane) in planes.iter_mut().zip(mapped) {
                slot.length = plane.len as u32;
            }
            let mut buf = V4l2Buffer {
                index: index as u32,
                kind: queue.kind,
                memory: V4L2_MEMORY_MMAP,
                length: planes_per_buffer,
                m: planes.as_mut_ptr() as u64,
                ..Default::default()
            };
            if let Err(e) = unsafe { self.ioctl(VIDIOC_QBUF, &mut buf) } {
                return self.err("VIDIOC_QBUF", e);
            }
        }
        Ok(())
    }

    fn stream_on(&self, kind: u32) -> Result<(), V4l2Error> {
        let mut kind = kind as i32;
        match unsafe { self.ioctl(VIDIOC_STREAMON, &mut kind) } {
            Ok(_) => Ok(()),
            Err(e) => self.err("VIDIOC_STREAMON", e),
        }
    }

    fn stream_off(&self, kind: u32) -> Result<(), V4l2Error> {
        let mut kind = kind as i32;
        match unsafe { self.ioctl(VIDIOC_STREAMOFF, &mut kind) } {
            Ok(_) => Ok(()),
            Err(e) => self.err("VIDIOC_STREAMOFF", e),
        }
    }

    fn subscribe_source_change(&self) -> Result<(), V4l2Error> {
        let sub = V4l2EventSubscription {
            kind: V4L2_EVENT_SOURCE_CHANGE,
            ..Default::default()
        };
        match unsafe { self.ioctl(VIDIOC_SUBSCRIBE_EVENT, &mut { sub }) } {
            Ok(_) => Ok(()),
            Err(e) => self.err("VIDIOC_SUBSCRIBE_EVENT", e),
        }
    }

    /// Non-blocking: `Ok(None)` means the kernel had nothing dequeued yet, which is normal.
    fn dqbuf(
        &self,
        kind: u32,
        planes_per_buffer: u32,
    ) -> Result<Option<(u32, u32, i64)>, V4l2Error> {
        let mut planes = [V4l2Plane::default(); VIDEO_MAX_PLANES];
        let mut buf = V4l2Buffer {
            kind,
            memory: V4L2_MEMORY_MMAP,
            length: planes_per_buffer,
            m: planes.as_mut_ptr() as u64,
            ..Default::default()
        };

        match unsafe { self.ioctl(VIDIOC_DQBUF, &mut buf) } {
            Ok(_) => Ok(Some((
                buf.index,
                buf.bytesused,
                buf.tv_sec * 1_000_000_000 + buf.tv_usec * 1000,
            ))),
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => Ok(None),
            Err(e) => self.err("VIDIOC_DQBUF", e),
        }
    }

    fn qbuf(
        &self,
        kind: u32,
        index: u32,
        bytesused: u32,
        planes_per_buffer: u32,
    ) -> Result<(), V4l2Error> {
        let mut planes = [V4l2Plane::default(); VIDEO_MAX_PLANES];
        planes[0].bytesused = bytesused;
        let mut buf = V4l2Buffer {
            index,
            kind,
            memory: V4L2_MEMORY_MMAP,
            bytesused,
            length: planes_per_buffer,
            m: planes.as_mut_ptr() as u64,
            ..Default::default()
        };
        if let Err(e) = unsafe { self.ioctl(VIDIOC_QBUF, &mut buf) } {
            return self.err("VIDIOC_QBUF", e);
        }
        Ok(())
    }

    fn planes_of(&self, kind: u32) -> u32 {
        let queue = if kind == V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE {
            &self.output
        } else {
            &self.capture
        };
        queue
            .buffers
            .first()
            .map_or(1, |planes| planes.len().max(1) as u32)
    }

    /// Write `data` into the buffer the kernel handed back for INPUT, and requeue it.
    fn write_into(&self, index: u32, data: &[u8]) -> Result<(), V4l2Error> {
        let Some(mapped) = self
            .output
            .buffers
            .get(index as usize)
            .and_then(|p| p.first())
        else {
            return Err(V4l2Error::Io(format!(
                "OUTPUT buffer {index} is not mapped"
            )));
        };
        if data.len() > mapped.len {
            return Err(V4l2Error::Io(format!(
                "access unit of {} bytes does not fit a {}-byte OUTPUT buffer",
                data.len(),
                mapped.len
            )));
        }
        // Safety: `mapped` is a live mapping owned by `self`, and the bounds check above is the
        // only precondition.
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), mapped.ptr, data.len()) };
        Ok(())
    }
}

impl V4l2Device for V4l2M2mDecoder {
    fn open(&mut self) -> Result<(), V4l2Error> {
        let path = CString::new(self.path.clone()).map_err(|_| V4l2Error::Open {
            path: self.path.clone(),
            reason: "path contains a NUL".into(),
        })?;

        // O_NONBLOCK on the fd is what makes every DQBUF in this file non-blocking, which is what
        // lets the client's loop stay in charge of its own timing.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(V4l2Error::Open {
                path: self.path.clone(),
                reason: io::Error::last_os_error().to_string(),
            });
        }
        self.fd = fd;

        let _capability = self.querycap()?;

        self.set_format(self.output.kind, self.coded.0, self.coded.1, self.fourcc)?;

        // Best effort, and logged by the caller if it fails: the hardware holds frames for
        // reordering so userspace does not have to.
        let _ = self.set_ctrl(V4L2_CID_DISPLAY_DELAY_ENABLE, 1);
        let _ = self.set_ctrl(V4L2_CID_DISPLAY_DELAY, 0);

        let out_count = self.request_buffers(self.output.kind, 8)?;
        self.output = self.map_queue(self.output.kind, out_count, 1)?;
        self.queue_all(&self.output, 1)?;
        self.stream_on(self.output.kind)?;

        // Subscribe *before* the CAPTURE side exists: the first thing a decoder does is discover
        // the format, and a change event that arrives before we are listening is a change we miss.
        self.subscribe_source_change()?;

        let capture_format = self.get_format(self.capture.kind)?;
        let cap_planes = capture_format.num_planes.max(1);
        let cap_count = self.request_buffers(self.capture.kind, 8)?;
        self.capture = self.map_queue(self.capture.kind, cap_count, cap_planes)?;
        self.queue_all(&self.capture, cap_planes)?;
        self.stream_on(self.capture.kind)?;

        // Recorded from the device rather than from the session: the decoder decides its own
        // output geometry, and the renderer has to import *that*, stride and all.
        let offsets = self
            .capture
            .buffers
            .first()
            .map(|planes| {
                [
                    planes.first().map_or(0, |p| p.offset),
                    planes.get(1).map_or(0, |p| p.offset),
                ]
            })
            .unwrap_or([0, 0]);
        self.capture_geometry = Some(DmaBufFrame {
            width: capture_format.width,
            height: capture_format.height,
            drm_fourcc: drm_fourcc_from_v4l2(capture_format.pixelformat)
                .unwrap_or(capture_format.pixelformat),
            planes: cap_planes,
            strides: [
                capture_format.plane_fmt[0].bytesperline,
                capture_format.plane_fmt[1].bytesperline,
            ],
            offsets,
            // The fds are per-frame exports, not part of the format: the same CAPTURE buffer keeps
            // the same dma-buf for its whole life, and the frame producer fills these in.
            fds: [-1, -1],
        });

        Ok(())
    }

    fn submit(&mut self, timestamp: Duration, nal: &[u8]) -> Result<SubmitStatus, V4l2Error> {
        let Some((index, _bytesused, _ts)) =
            self.dqbuf(self.output.kind, self.planes_of(self.output.kind))?
        else {
            // Every OUTPUT buffer is with the kernel. This is the reference client's
            // `Could not find a free OUTPUT buffer`, and the caller counts it.
            return Ok(SubmitStatus::NoFreeOutputBuffer);
        };

        if nal.is_empty() {
            // The reference client logs this rather than queueing an empty buffer:
            // `SubmitFrameForDecode: Called submit with an empty buffer`.
            self.qbuf(self.output.kind, index, 0, self.planes_of(self.output.kind))?;
            return Ok(SubmitStatus::Queued);
        }

        self.write_into(index, nal)?;
        let _ = timestamp;
        self.qbuf(
            self.output.kind,
            index,
            nal.len() as u32,
            self.planes_of(self.output.kind),
        )?;
        Ok(SubmitStatus::Queued)
    }

    fn poll_events(&mut self, out: &mut Vec<DeviceEvent>) -> Result<(), V4l2Error> {
        // Events first: a source change changes what `Decoded` even means.
        loop {
            let mut event = V4l2Event::default();
            match unsafe { self.ioctl(VIDIOC_DQEVENT, &mut event) } {
                Ok(_) => {
                    if event.kind == V4L2_EVENT_SOURCE_CHANGE {
                        let change = unsafe {
                            ptr::read_unaligned(
                                event.u_data.as_ptr() as *const V4l2EventSourceChange
                            )
                        };
                        if change.changes & V4L2_EVENT_SRC_CH_RESOLUTION != 0 {
                            // The decoder renegotiates its own CAPTURE format; we only report it.
                            let fmt = self.get_format(self.capture.kind)?;
                            out.push(DeviceEvent::SourceChange {
                                width: fmt.width,
                                height: fmt.height,
                                fourcc: fmt.pixelformat,
                            });
                        }
                    }
                }
                Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => break,
                Err(e) => return self.err("VIDIOC_DQEVENT", e),
            }
        }

        // Then decoded frames.
        loop {
            let planes = self.planes_of(self.capture.kind);
            match self.dqbuf(self.capture.kind, planes)? {
                Some((index, _bytesused, ts_ns)) => out.push(DeviceEvent::Decoded {
                    index,
                    timestamp: Duration::from_nanos(ts_ns.max(0) as u64),
                }),
                None => break,
            }
        }

        Ok(())
    }

    fn release_capture(&mut self, index: u32) -> Result<(), V4l2Error> {
        let planes = self.planes_of(self.capture.kind);
        self.qbuf(self.capture.kind, index, 0, planes)
    }

    fn export_capture(&self, index: u32, plane: u32) -> Result<RawFd, V4l2Error> {
        let mut export = V4l2ExportBuffer {
            kind: V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            index,
            plane,
            ..Default::default()
        };

        match unsafe { self.ioctl(VIDIOC_EXPBUF, &mut export) } {
            Ok(_) => Ok(export.fd),
            Err(e) => self.err("VIDIOC_EXPBUF", e),
        }
    }

    fn capture_buffer_count(&self) -> usize {
        self.capture.buffers.len()
    }

    fn capture_geometry(&self) -> Option<DmaBufFrame> {
        self.capture_geometry
    }

    fn hard_reset(&mut self) -> Result<(), V4l2Error> {
        // Stop both queues, close, and rebuild. Best effort by design: a reset that itself fails is
        // reported by the caller's counters, and the alternative — leaving a wedged stream running
        // — is worse. The reference client does the same thing and calls it `HardReset`.
        let _ = self.stream_off(self.capture.kind);
        let _ = self.stream_off(self.output.kind);
        self.unmap_all();
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
        let _ = self.open();
        Ok(())
    }

    fn path(&self) -> &str {
        &self.path
    }
}

impl V4l2M2mDecoder {
    fn unmap_all(&mut self) {
        for queue in [&mut self.output, &mut self.capture] {
            for planes in queue.buffers.drain(..) {
                for plane in planes {
                    unsafe { libc::munmap(plane.ptr as *mut c_void, plane.len) };
                }
            }
        }
        // The geometry described buffers that no longer exist; a reset re-learns it.
        self.capture_geometry = None;
    }
}

impl Drop for V4l2M2mDecoder {
    fn drop(&mut self) {
        let _ = self.stream_off(self.capture.kind);
        let _ = self.stream_off(self.output.kind);
        self.unmap_all();
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Finding the device
// ---------------------------------------------------------------------------------------------

/// One `/dev/videoN`, opened, asked its capabilities, and asked whether it will take this codec.
///
/// The last step is what makes this a *search* rather than a guess. Capability flags alone cannot
/// distinguish the HEVC decoder from the H.264 one, and on a device with an ISP, a camera and a
/// codec all presenting as M2M nodes, guessing is how a client ends up configuring the camera. So
/// the probe sets the OUTPUT format to the codec the session actually negotiated and keeps the
/// node only if the kernel accepts it.
fn probe_node(path: &str, fourcc: u32) -> Result<V4l2Capability, V4l2Error> {
    let c_path = CString::new(path).map_err(|_| V4l2Error::Open {
        path: path.to_owned(),
        reason: "path contains a NUL".into(),
    })?;

    // SAFETY: `c_path` is a valid C string.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(V4l2Error::Open {
            path: path.to_owned(),
            reason: io::Error::last_os_error().to_string(),
        });
    }

    // Every return below must close `fd`; a leaked fd per probe across 64 nodes is a real leak.
    #[inline]
    fn close(fd: RawFd) {
        // SAFETY: `fd` came from `open` above and is closed exactly once on each path.
        unsafe { libc::close(fd) };
    }

    let ioctl = |fd: RawFd, request: u64, arg: *mut c_void| -> io::Result<i32> {
        // SAFETY: the caller passes an argument of the layout the request number implies.
        let rc = unsafe { libc::ioctl(fd, request as libc::c_ulong, arg) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rc)
        }
    };

    let mut capability = V4l2Capability::default();
    if let Err(e) = ioctl(
        fd,
        VIDIOC_QUERYCAP,
        (&mut capability as *mut V4l2Capability).cast(),
    ) {
        close(fd);
        return Err(V4l2Error::Io(format!("{path}: VIDIOC_QUERYCAP: {e}")));
    }

    if !capability.is_m2m_codec() {
        close(fd);
        return Err(V4l2Error::Io(format!(
            "{path}: {} ({}) is not a memory-to-memory codec",
            capability.card_name(),
            capability.driver_name()
        )));
    }

    // Does it take *our* codec? A 2x2 coded size is the smallest thing the kernel will not reject
    // for being degenerate, and the probe closes the node immediately afterwards.
    let mut format = V4l2Format {
        kind: V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
        ..Default::default()
    };
    format.pix_mp.width = 2;
    format.pix_mp.height = 2;
    format.pix_mp.pixelformat = fourcc;
    format.pix_mp.num_planes = 1;

    if let Err(e) = ioctl(fd, VIDIOC_S_FMT, (&mut format as *mut V4l2Format).cast()) {
        close(fd);
        return Err(V4l2Error::Io(format!(
            "{path}: {} ({}) refused the codec: {e}",
            capability.card_name(),
            capability.driver_name()
        )));
    }

    close(fd);
    Ok(capability)
}

/// Search `/dev/video*` for the stateful M2M decoder that accepts `fourcc`.
///
/// The device node on a Frame is discovered, not fixed: the reference client logs
/// `Failed to get video device path` for exactly this search coming up empty, and a client that
/// hard-codes `/dev/video0` configures a camera on any device where that is not true.
///
/// Returns the first node that both advertises an M2M codec and accepts the negotiated codec, plus
/// the identification of every node tried, so a failed search can be reported rather than guessed
/// at.
pub fn discover_device(fourcc: u32) -> (Option<String>, Vec<String>) {
    const MAX_NODES: u32 = 64;

    let override_path = std::env::var("ALVR_V4L2_DEVICE").ok();
    if let Some(path) = override_path {
        // An explicit device wins, but it is still probed: believing a setting over the kernel is
        // how a misconfiguration becomes a mystery.
        return match probe_node(&path, fourcc) {
            Ok(capability) => (
                Some(path.clone()),
                vec![format!(
                    "{path}: {} ({}) — forced by ALVR_V4L2_DEVICE",
                    capability.card_name(),
                    capability.driver_name()
                )],
            ),
            Err(e) => (
                None,
                vec![format!("{path}: {e} — forced by ALVR_V4L2_DEVICE")],
            ),
        };
    }

    let mut tried = Vec::new();
    for index in 0..MAX_NODES {
        let path = format!("/dev/video{index}");
        if !std::path::Path::new(&path).exists() {
            continue;
        }
        match probe_node(&path, fourcc) {
            Ok(capability) => {
                tried.push(format!(
                    "{path}: {} ({}) — accepted the codec",
                    capability.card_name(),
                    capability.driver_name()
                ));
                return (Some(path), tried);
            }
            Err(e) => tried.push(e.to_string()),
        }
    }

    (None, tried)
}
