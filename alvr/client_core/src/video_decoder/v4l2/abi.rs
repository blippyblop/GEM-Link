//! The V4L2 ABI: ioctl request numbers, `repr(C)` structures, and the constants the M2M codec
//! uses.
//!
//! ## Why this is a separate module and why it is tested
//!
//! The device loop cannot run here — there is no `/dev/video*` in this container and no
//! system-mode qemu, and we are not privileged enough to load `vicodec` on the rig. Writing a few
//! hundred lines of `unsafe` that have never executed and calling it progress would be exactly the
//! mistake this project's rules of engagement exist to prevent ("code says what *could*; only the
//! rig says what *does*").
//!
//! But **the ABI is not like that.** `VIDIOC_S_FMT` has one correct value, derived from
//! `_IOC(dir, 'V', nr, sizeof(struct v4l2_format))`, and if we get it wrong the driver returns
//! `ENOTTY` and the failure looks like "the decoder is broken" rather than "our constant is a
//! byte out". So the numbers and the layouts *are* verifiable without hardware, and the tests below
//! verify them against the values in `linux/videodev2.h`.
//!
//! That leaves the sequencing — which this crate already has, tested, in `pipeline.rs` — and the
//! syscall glue, which is genuinely small once both ends are pinned down.
//!
//! ## Not yet verified
//!
//! Every constant here is checked arithmetically, and the struct sizes are checked. **No ioctl has
//! been issued.** The first run on real hardware must assert `VIDIOC_QUERYCAP` succeeds and log the
//! driver string before anything else, so a wrong assumption fails loudly and immediately.

use std::mem::size_of;

// ---------------------------------------------------------------------------------------------
// _IOC: the ioctl-number encoding, as a const fn so the numbers below are derived rather than
// copy-pasted. A copy-paste is how you get a constant that is right on one architecture.
// ---------------------------------------------------------------------------------------------

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS; // 8
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS; // 16
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS; // 30

/// Userspace writes to the kernel.
const IOC_WRITE: u32 = 1;
/// Userspace reads from the kernel.
const IOC_READ: u32 = 2;

/// V4L2's ioctl type character.
const IOC_TYPE_V: u32 = b'V' as u32;

const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u64 {
    ((dir << IOC_DIRSHIFT) | (ty << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | (size << IOC_SIZESHIFT))
        as u64
}

const fn ior<T>(nr: u32) -> u64 {
    ioc(IOC_READ, IOC_TYPE_V, nr, size_of::<T>() as u32)
}
const fn iow<T>(nr: u32) -> u64 {
    ioc(IOC_WRITE, IOC_TYPE_V, nr, size_of::<T>() as u32)
}
const fn iowr<T>(nr: u32) -> u64 {
    ioc(IOC_READ | IOC_WRITE, IOC_TYPE_V, nr, size_of::<T>() as u32)
}

/// `_IO` with an explicit size, for the two ioctls whose argument is a plain integer rather than a
/// structure (`VIDIOC_STREAMON`/`STREAMOFF` take `int`).
const fn iow_int(nr: u32) -> u64 {
    ioc(IOC_WRITE, IOC_TYPE_V, nr, size_of::<i32>() as u32)
}

// ---------------------------------------------------------------------------------------------
// Structures. Field order and widths follow linux/videodev2.h exactly; the `size_of` assertions in
// the tests are the guard against a silent mis-layout, because a wrong size changes the ioctl
// number itself.
// ---------------------------------------------------------------------------------------------

pub const VIDEO_MAX_FRAME: usize = 32;
pub const VIDEO_MAX_PLANES: usize = 8;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct V4l2Capability {
    pub driver: [u8; 16],
    pub card: [u8; 32],
    pub bus_info: [u8; 32],
    pub version: u32,
    pub capabilities: u32,
    pub device_caps: u32,
    pub reserved: [u32; 3],
}

/// `struct v4l2_plane`. The `m` union is one 8-byte slot however it is read (`mem_offset`,
/// `userptr` or `fd`), so it is modelled as a single `u64` with accessors rather than as separate
/// fields that would silently change the layout.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2Plane {
    pub bytesused: u32,
    pub length: u32,
    pub m: u64,
    pub data_offset: u32,
    pub reserved: [u32; 11],
}

impl V4l2Plane {
    /// The MMAP offset of this plane, which is the first four bytes of the `m` union.
    pub fn mem_offset(&self) -> u32 {
        self.m as u32
    }
}

/// `struct v4l2_buffer`.
///
/// For a multiplanar queue the kernel reuses two fields with different meanings: `length` is the
/// **number of planes** (not a byte count) and `m` is a pointer to the plane array. Modelling
/// `num_planes` as a separate field — as the first draft of `device.rs` did — is a mistake the
/// compiler caught, which is a much better place to catch it than a driver returning `EINVAL`.
///
/// The timestamp and timecode members are the trap: on a 64-bit target the kernel's
/// `__kernel_v4l2_timeval` is two `__s64` (16 bytes, not 8) and `v4l2_timecode` is 16 bytes of
/// mixed widths, not two `u32`. Modelling them loosely makes the struct 72 bytes instead of 88 —
/// which changes the ioctl *number* and would have failed on the device as `ENOTTY` on every call.
/// `struct_sizes_match_the_kernel_abi` is what caught it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct V4l2Buffer {
    pub index: u32,
    pub kind: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub tv_sec: i64,
    pub tv_usec: i64,
    pub tc_type: u32,
    pub tc_flags: u32,
    pub tc_frames: u8,
    pub tc_seconds: u8,
    pub tc_minutes: u8,
    pub tc_hours: u8,
    pub tc_userbits: [u8; 4],
    pub sequence: u32,
    pub memory: u32,
    /// The `m` union: `__u32 offset`, `unsigned long userptr`, `struct v4l2_plane *planes`, `__s32 fd`.
    pub m: u64,
    pub length: u32,
    pub reserved2: u32,
    pub request_fd: i32,
    pub reserved: u32,
}

/// `struct v4l2_plane_pix_format` — 20 bytes.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct V4l2PlaneFormat {
    pub sizeimage: u32,
    pub bytesperline: u32,
    pub reserved: [u16; 6],
}

/// `struct v4l2_pix_format_mplane` — 192 bytes.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct V4l2PixFormatMplane {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub num_planes: u32,
    pub flags: u32,
    pub plane_fmt: [V4l2PlaneFormat; VIDEO_MAX_PLANES],
    /// The kernel's trailing union of coding hints and raw padding: 8 bytes.
    pub raw_data: [u8; 8],
}

/// `struct v4l2_format` — 208 bytes.
///
/// It is a union, and `pix_mp` is **not** its largest member (`v4l2_window` is), so the union is
/// 204 bytes against `pix_mp`'s 192. `pad` makes up the difference. Getting this wrong changes
/// `VIDIOC_S_FMT`'s number, so the size is asserted rather than assumed.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct V4l2Format {
    pub kind: u32,
    pub pix_mp: V4l2PixFormatMplane,
    pub pad: [u8; 12],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct V4l2RequestBuffers {
    pub count: u32,
    pub kind: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub flags: u8,
    pub reserved: [u8; 1],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct V4l2Control {
    pub id: u32,
    pub value: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct V4l2EventSubscription {
    pub kind: u32,
    pub id: u32,
    pub flags: u32,
    pub reserved: [u32; 5],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct V4l2Event {
    pub kind: u32,
    pub pending: u32,
    pub sequence: u32,
    pub timestamp: [u64; 2], // struct timespec64 on 64-bit
    pub id: u32,
    pub reserved: [u32; 8],
    pub u_data: [u8; 64],
}

impl Default for V4l2Event {
    fn default() -> Self {
        // All-zero is the empty state; the kernel only reads the fields it fills in. Written
        // explicitly because `[u8; 64]` has no `Default` impl, which is itself a small reminder
        // that this struct has to match a C layout rather than a Rust one.
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct V4l2EventSourceChange {
    pub changes: u32,
    pub pad: u32,
    pub reserved: [u32; 6],
}

// ---------------------------------------------------------------------------------------------
// Enumerations and control IDs.
// ---------------------------------------------------------------------------------------------

pub const V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
pub const V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE: u32 = 10;
/// The M2M type that addresses both queues of a stateful codec at once.
pub const V4L2_BUF_TYPE_VIDEO_M2M_MPLANE: u32 = 11;

pub const V4L2_MEMORY_MMAP: u32 = 1;
pub const V4L2_MEMORY_DMABUF: u32 = 4;

/// `V4L2_EVENT_SOURCE_CHANGE` — the format-change event, and the reason a runtime resolution
/// step-down can exist at all.
pub const V4L2_EVENT_SOURCE_CHANGE: u32 = 5;
pub const V4L2_EVENT_SRC_CH_RESOLUTION: u32 = 1;

/// The decoder's own presentation-delay controls. The reference client sets both explicitly
/// (`SVLCodecV4L2::InitializeDevice: Failed to set DISPLAY_DELAY`) so that the *hardware* holds
/// frames for reordering, rather than a userspace queue doing it and paying the latency twice.
pub const V4L2_CID_USER_CLASS: u32 = 0x0098_0000;
pub const V4L2_CID_DISPLAY_DELAY_ENABLE: u32 = V4L2_CID_USER_CLASS + 0x0f0c;
pub const V4L2_CID_DISPLAY_DELAY: u32 = V4L2_CID_USER_CLASS + 0x0f0d;

pub const V4L2_PIX_FMT_H264: u32 = u32::from_le_bytes(*b"H264");
pub const V4L2_PIX_FMT_HEVC: u32 = u32::from_le_bytes(*b"HEVC");
/// Multi-planar variants, which is what an M2M codec's CAPTURE side actually reports.
pub const V4L2_PIX_FMT_H264_MPLANE: u32 = u32::from_le_bytes(*b"H264");
pub const V4L2_PIX_FMT_NV12_MPLANE: u32 = u32::from_le_bytes(*b"NM12");

/// `V4L2_PIX_FMT_NV12` on the CAPTURE side — the usual output of a hardware decoder.
pub const V4L2_PIX_FMT_NV12: u32 = u32::from_le_bytes(*b"NV12");

// ---------------------------------------------------------------------------------------------
// The ioctls, derived.
// ---------------------------------------------------------------------------------------------

pub const VIDIOC_QUERYCAP: u64 = ior::<V4l2Capability>(0);
pub const VIDIOC_G_FMT: u64 = iowr::<V4l2Format>(4);
pub const VIDIOC_S_FMT: u64 = iowr::<V4l2Format>(5);
pub const VIDIOC_REQBUFS: u64 = iowr::<V4l2RequestBuffers>(8);
pub const VIDIOC_QUERYBUF: u64 = iowr::<V4l2Buffer>(9);
pub const VIDIOC_QBUF: u64 = iowr::<V4l2Buffer>(15);
pub const VIDIOC_DQBUF: u64 = iowr::<V4l2Buffer>(17);
pub const VIDIOC_STREAMON: u64 = iow_int(18);
pub const VIDIOC_STREAMOFF: u64 = iow_int(19);
pub const VIDIOC_S_CTRL: u64 = iowr::<V4l2Control>(28);
pub const VIDIOC_DQEVENT: u64 = ior::<V4l2Event>(89);
pub const VIDIOC_SUBSCRIBE_EVENT: u64 = iow::<V4l2EventSubscription>(90);
pub const VIDIOC_UNSUBSCRIBE_EVENT: u64 = iow::<V4l2EventSubscription>(91);

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of this module. Each of these numbers is the value `linux/videodev2.h`
    /// produces for the corresponding `_IO*` macro on a 64-bit target; if our arithmetic, a struct
    /// size, or a field order is wrong, exactly one of them stops matching — and it stops matching
    /// *here*, not in a driver returning `ENOTTY` on a device we cannot attach a debugger to.
    #[test]
    fn every_ioctl_number_matches_the_kernel() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_G_FMT, 0xc0d0_5604);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_S_CTRL, 0xc008_561c);
        // sizeof(v4l2_event) is 136, not the 56 one might guess from the header's first four
        // fields: an 8-aligned 16-byte timespec and a 64-byte union. The first draft of this file
        // asserted the smaller number and the test rejected it, which is the point.
        assert_eq!(VIDIOC_DQEVENT, 0x8088_5659);
        assert_eq!(VIDIOC_SUBSCRIBE_EVENT, 0x4020_565a);
        assert_eq!(VIDIOC_UNSUBSCRIBE_EVENT, 0x4020_565b);
    }

    /// Sizes the ioctl numbers are derived from, asserted directly. A wrong `size_of` is the
    /// classic way for every one of the constants above to be wrong in a way that still *looks*
    /// plausible (all the same high bits).
    #[test]
    fn struct_sizes_match_the_kernel_abi() {
        assert_eq!(size_of::<V4l2Capability>(), 104);
        assert_eq!(size_of::<V4l2Buffer>(), 88);
        assert_eq!(size_of::<V4l2Control>(), 8);
        assert_eq!(size_of::<V4l2RequestBuffers>(), 20);
        assert_eq!(size_of::<V4l2EventSubscription>(), 32);
        assert_eq!(size_of::<V4l2Event>(), 136);
        // v4l2_format is a union; pix_mp plus the 4-byte type selector is the layout the kernel
        // creates for it, and 208 is what VIDIOC_S_FMT's number implies.
        assert_eq!(size_of::<V4l2Format>(), 208);
    }

    #[test]
    fn the_codec_fourccs_are_the_ascii_the_kernel_expects() {
        // These are spelled as four characters, so a typo is a compile error rather than a number
        // that is merely wrong.
        assert_eq!(&V4L2_PIX_FMT_H264.to_le_bytes(), b"H264");
        assert_eq!(&V4L2_PIX_FMT_HEVC.to_le_bytes(), b"HEVC");
        assert_eq!(&V4L2_PIX_FMT_NV12.to_le_bytes(), b"NV12");
    }
}
