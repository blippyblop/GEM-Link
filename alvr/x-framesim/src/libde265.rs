//! Real HEVC decode for the emulated-prototype harness, via **libde265**.
//!
//! ## Why dlopen
//!
//! The emulated prototype has to decode what the streamer actually sends (HEVC,
//! ADR-0008). There is no Rust HEVC decoder worth using, and this environment
//! has no C toolchain (no gcc, no make, no nasm) and no ffmpeg — but
//! `libde265.so.0` is already present on the sim host and in the aarch64
//! sysroot. Loading it at runtime gives a real decoder with **zero build
//! dependencies**.
//!
//! ## Scope / licence
//!
//! **Harness only.** libde265 is LGPL-3 and must never be linked into shipped
//! code or merged into this repo (CHARTER: MIT stays pure). It is loaded by
//! name at runtime by an `x-*` harness crate and is absent from every product
//! path. The shipping decoder is the Frame's V4L2 `iris` path, which is
//! hardware and third-party-code free.
//!
//! Decode happens through the *public* `client_core` API
//! (`set_decoder_input_callback` / `report_frame_decoded`), so `client_core`
//! itself is untouched.

use crate::png;
use alvr_common::anyhow::{Result, anyhow, bail};
use libloading::Library;
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

/// `de265_error`: 0 == DE265_OK, 13 == DE265_ERROR_WAITING_FOR_INPUT_DATA.
/// Anything else is a real error (the enum also has positive warning values,
/// which `de265_isOK` accepts; we treat 0/13 as fine and log the rest).
const DE265_OK: c_int = 0;
const DE265_WAITING_FOR_INPUT_DATA: c_int = 13;

type DecoderCtx = c_void;
type Error = c_int;
type Pts = i64;

#[repr(C)]
struct Image {
    _private: [u8; 0],
}

macro_rules! api {
    ($lib:expr, $($name:literal => $ty:ty),* $(,)?) => {
        {
            #[allow(unused_unsafe)]
            unsafe {
                ($(
                    {
                        let sym: libloading::Symbol<$ty> = $lib.get(concat!($name, "\0").as_bytes())?;
                        *sym
                    },
                )*)
            }
        }
    };
}

struct Api {
    _lib: Library,
    new_decoder: unsafe extern "C" fn() -> *mut DecoderCtx,
    free_decoder: unsafe extern "C" fn(*mut DecoderCtx) -> Error,
    start_worker_threads: unsafe extern "C" fn(*mut DecoderCtx, c_int) -> Error,
    push_nal:
        unsafe extern "C" fn(*mut DecoderCtx, *const c_void, c_int, Pts, *mut c_void) -> Error,
    push_end_of_frame: unsafe extern "C" fn(*mut DecoderCtx),
    decode: unsafe extern "C" fn(*mut DecoderCtx, *mut c_int) -> Error,
    get_next_picture: unsafe extern "C" fn(*mut DecoderCtx) -> *const Image,
    release_next_picture: unsafe extern "C" fn(*mut DecoderCtx),
    image_width: unsafe extern "C" fn(*const Image, c_int) -> c_int,
    image_height: unsafe extern "C" fn(*const Image, c_int) -> c_int,
    image_plane: unsafe extern "C" fn(*const Image, c_int, *mut c_int) -> *const u8,
    chroma_format: unsafe extern "C" fn(*const Image) -> c_int,
    bits_per_pixel: unsafe extern "C" fn(*const Image, c_int) -> c_int,
    image_pts: unsafe extern "C" fn(*const Image) -> Pts,
    full_range_flag: unsafe extern "C" fn(*const Image) -> c_int,
    matrix_coefficients: unsafe extern "C" fn(*const Image) -> c_int,
    version: unsafe extern "C" fn() -> *const c_char,
    error_text: unsafe extern "C" fn(Error) -> *const c_char,
    is_ok: unsafe extern "C" fn(Error) -> c_int,
}

impl Api {
    fn load(path: &Path) -> Result<Self> {
        // SAFETY: libde265's C ABI is stable across the 1.0.x series (and the
        // two builds we use: 1.0.8 on aarch64, 1.0.15 on x86_64). Symbols are
        // resolved by name and every signature below is taken from the shipped
        // `de265.h`, not from memory.
        let lib = unsafe { Library::new(path) }
            .map_err(|e| anyhow!("cannot load libde265 from {}: {e}", path.display()))?;

        let (
            new_decoder,
            free_decoder,
            start_worker_threads,
            push_nal,
            push_end_of_frame,
            decode,
            get_next_picture,
            release_next_picture,
            image_width,
            image_height,
            image_plane,
            chroma_format,
            bits_per_pixel,
            image_pts,
            full_range_flag,
            matrix_coefficients,
            version,
            error_text,
            is_ok,
        ) = api!(
            lib,
            "de265_new_decoder" => unsafe extern "C" fn() -> *mut DecoderCtx,
            "de265_free_decoder" => unsafe extern "C" fn(*mut DecoderCtx) -> Error,
            "de265_start_worker_threads" => unsafe extern "C" fn(*mut DecoderCtx, c_int) -> Error,
            "de265_push_NAL" => unsafe extern "C" fn(*mut DecoderCtx, *const c_void, c_int, Pts, *mut c_void) -> Error,
            "de265_push_end_of_frame" => unsafe extern "C" fn(*mut DecoderCtx),
            "de265_decode" => unsafe extern "C" fn(*mut DecoderCtx, *mut c_int) -> Error,
            "de265_get_next_picture" => unsafe extern "C" fn(*mut DecoderCtx) -> *const Image,
            "de265_release_next_picture" => unsafe extern "C" fn(*mut DecoderCtx),
            "de265_get_image_width" => unsafe extern "C" fn(*const Image, c_int) -> c_int,
            "de265_get_image_height" => unsafe extern "C" fn(*const Image, c_int) -> c_int,
            "de265_get_image_plane" => unsafe extern "C" fn(*const Image, c_int, *mut c_int) -> *const u8,
            "de265_get_chroma_format" => unsafe extern "C" fn(*const Image) -> c_int,
            "de265_get_bits_per_pixel" => unsafe extern "C" fn(*const Image, c_int) -> c_int,
            "de265_get_image_PTS" => unsafe extern "C" fn(*const Image) -> Pts,
            "de265_get_image_full_range_flag" => unsafe extern "C" fn(*const Image) -> c_int,
            "de265_get_image_matrix_coefficients" => unsafe extern "C" fn(*const Image) -> c_int,
            "de265_get_version" => unsafe extern "C" fn() -> *const c_char,
            "de265_get_error_text" => unsafe extern "C" fn(Error) -> *const c_char,
            "de265_isOK" => unsafe extern "C" fn(Error) -> c_int,
        );

        Ok(Self {
            _lib: lib,
            new_decoder,
            free_decoder,
            start_worker_threads,
            push_nal,
            push_end_of_frame,
            decode,
            get_next_picture,
            release_next_picture,
            image_width,
            image_height,
            image_plane,
            chroma_format,
            bits_per_pixel,
            image_pts,
            full_range_flag,
            matrix_coefficients,
            version,
            error_text,
            is_ok,
        })
    }

    fn version_string(&self) -> String {
        unsafe {
            let p = (self.version)();
            if p.is_null() {
                "unknown".into()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }

    fn describe(&self, err: Error) -> String {
        unsafe {
            let p = (self.error_text)(err);
            if p.is_null() {
                format!("error {err}")
            } else {
                format!("{} ({err})", CStr::from_ptr(p).to_string_lossy())
            }
        }
    }
}

/// One decoded picture, planes copied out of libde265's buffers.
pub struct Frame {
    pub pts_ns: i64,
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub y_stride: usize,
    pub u: Vec<u8>,
    pub u_stride: usize,
    pub v: Vec<u8>,
    pub v_stride: usize,
    /// `video_full_range_flag` and colour matrix, read from the bitstream by
    /// libde265 — not assumed.
    pub full_range: bool,
    pub matrix_709: bool,
}

impl Frame {
    pub fn to_rgb8(&self) -> Vec<u8> {
        png::yuv420_to_rgb8(
            &self.y,
            self.y_stride,
            &self.u,
            self.u_stride,
            &self.v,
            self.v_stride,
            self.width,
            self.height,
            self.matrix_709,
            self.full_range,
        )
    }
}

pub struct HevcDecoder {
    api: Api,
    ctx: *mut DecoderCtx,
    /// Monotonic counters, surfaced as harness metrics.
    pub pushed_units: u64,
    pub decoded: u64,
    pub errors: u64,
    pub skipped_10bit: u64,
    pub last_error: Option<String>,
    saw_config: bool,
    /// One-shot: print the first picture's real geometry and plane statistics.
    /// If the decoder is misreading the bitstream this is where it shows up.
    logged_geometry: AtomicBool,
    /// Coherence tracking: mean absolute luma difference between consecutive
    /// decoded pictures. A real stream of a mostly-static scene gives a small
    /// number; a broken reference chain (missing frames, or parameter sets fed
    /// mid-stream) gives one as large as two unrelated pictures. This is the
    /// metric that says "the pixels are real *and* in the right order".
    prev_luma: Option<Vec<u8>>,
    cmp_count: u64,
    cmp_sum: f64,
    cmp_similar: u64,
    cmp_max: f64,
}

// SAFETY: the decoder context is owned by this struct and is only ever touched
// while the enclosing `Mutex` is held, from a single thread at a time. libde265
// itself is internally threaded (worker threads) but its API is used here from
// one caller at a time.
unsafe impl Send for HevcDecoder {}

impl HevcDecoder {
    pub fn new(lib_path: &Path, threads: usize) -> Result<Self> {
        let api = Api::load(lib_path)?;
        let version = api.version_string();
        println!(
            "[framesim] libde265 {version} loaded from {}",
            lib_path.display()
        );

        let ctx = unsafe { (api.new_decoder)() };
        if ctx.is_null() {
            bail!("de265_new_decoder returned null");
        }
        if threads > 0 {
            let err = unsafe { (api.start_worker_threads)(ctx, threads as c_int) };
            if err != DE265_OK && unsafe { (api.is_ok)(err) } == 0 {
                println!(
                    "[framesim] decode threads not started: {}",
                    api.describe(err)
                );
            } else {
                println!("[framesim] decode: {threads} worker thread(s)");
            }
        }

        Ok(Self {
            api,
            ctx,
            pushed_units: 0,
            decoded: 0,
            errors: 0,
            skipped_10bit: 0,
            last_error: None,
            saw_config: false,
            logged_geometry: AtomicBool::new(false),
            prev_luma: None,
            cmp_count: 0,
            cmp_sum: 0.0,
            cmp_similar: 0,
            cmp_max: 0.0,
        })
    }

    /// Feed the stream's parameter sets (`DecoderConfig.config_nal`). Idempotent
    /// enough: re-feeding parameter sets is legal HEVC.
    pub fn push_config(&mut self, data: &[u8]) {
        self.saw_config = true;
        let n = self.push_stream(data, 0, "config");
        println!(
            "[framesim] decode: fed {n} config NAL(s) ({} bytes)",
            data.len()
        );
    }

    /// Whether the parameter sets have been fed yet. Video can arrive before the
    /// `DecoderConfig` event is polled, so the callback checks this and feeds the
    /// CSD itself if the event has not landed.
    pub fn saw_config(&self) -> bool {
        self.saw_config
    }

    /// Track how much consecutive decoded pictures differ, on the luma plane.
    fn note_coherence(&mut self, frame: &Frame) {
        let (Some(prev), len) = (self.prev_luma.as_ref(), frame.y.len()) else {
            self.prev_luma = Some(frame.y.clone());
            return;
        };
        if prev.len() != len {
            self.prev_luma = Some(frame.y.clone());
            return;
        }
        let mut sum = 0f64;
        let mut n = 0f64;
        for (a, b) in prev.iter().zip(frame.y.iter()).step_by(4) {
            sum += f64::from(a.abs_diff(*b));
            n += 1.0;
        }
        let mean = sum / n.max(1.0);
        self.cmp_count += 1;
        self.cmp_sum += mean;
        self.cmp_max = self.cmp_max.max(mean);
        if mean < 2.0 {
            self.cmp_similar += 1;
        }
        self.prev_luma = Some(frame.y.clone());
    }

    /// Mean luma difference between consecutive pictures (and how many pairs
    /// were near-identical), for the run summary.
    pub fn coherence(&self) -> Option<(f64, f64, u64, u64)> {
        if self.cmp_count == 0 {
            return None;
        }
        Some((
            self.cmp_sum / self.cmp_count as f64,
            self.cmp_max,
            self.cmp_similar,
            self.cmp_count,
        ))
    }

    /// Feed one access unit as delivered by the stream socket, then drain every
    /// picture it unlocks. `pts_ns` is the sender's frame timestamp, threaded
    /// through libde265 so a decoded picture can be reported back with the
    /// timestamp the compositor knows it by.
    pub fn push_access_unit(&mut self, data: &[u8], pts_ns: i64) -> Vec<Frame> {
        self.push_stream(data, pts_ns, "frame");
        self.drain()
    }

    /// Split whatever framing we were handed (Annex-B start codes, or 4-byte
    /// length-prefixed AVCC) into NAL units. ALVR forwards the encoder's output
    /// opaquely, so we detect rather than assume — and log which one it was,
    /// because that is a real fact about the wire worth knowing.
    fn push_stream(&mut self, data: &[u8], pts_ns: i64, what: &str) -> usize {
        let units: Vec<&[u8]> = if starts_with_start_code(data) {
            split_annexb(data)
        } else {
            split_avcc(data).unwrap_or_else(|| vec![data])
        };

        let mut pushed = 0;
        for unit in units {
            let unit = trim_zeros(unit);
            if unit.len() < 2 {
                continue;
            }
            let err = unsafe {
                (self.api.push_nal)(
                    self.ctx,
                    unit.as_ptr().cast(),
                    unit.len() as c_int,
                    pts_ns,
                    std::ptr::null_mut(),
                )
            };
            if err != DE265_OK && unsafe { (self.api.is_ok)(err) } == 0 {
                self.errors += 1;
                self.last_error = Some(format!("push_NAL({what}): {}", self.api.describe(err)));
            }
            pushed += 1;
            self.pushed_units += 1;
        }

        unsafe { (self.api.push_end_of_frame)(self.ctx) };
        pushed
    }

    fn drain(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();

        // `de265_decode` reports (err, more): keep calling while `more` is set,
        // draining pictures after each call. A stalled state returns a non-OK
        // error with `more` set and simply needs more input — not fatal.
        for _ in 0..256 {
            let mut more: c_int = 0;
            let err = unsafe { (self.api.decode)(self.ctx, &mut more) };
            if err != DE265_OK
                && err != DE265_WAITING_FOR_INPUT_DATA
                && unsafe { (self.api.is_ok)(err) } == 0
            {
                self.errors += 1;
                self.last_error = Some(format!("decode: {}", self.api.describe(err)));
            }

            loop {
                let img = unsafe { (self.api.get_next_picture)(self.ctx) };
                if img.is_null() {
                    break;
                }
                if let Some(frame) = self.copy_image(img) {
                    self.note_coherence(&frame);
                    frames.push(frame);
                    self.decoded += 1;
                } else {
                    self.skipped_10bit += 1;
                }
                unsafe { (self.api.release_next_picture)(self.ctx) };
            }

            if more == 0 {
                break;
            }
        }

        frames
    }

    fn copy_image(&self, img: *const Image) -> Option<Frame> {
        unsafe {
            // 8-bit only. The Frame decodes 8-bit HEVC/H.264; 10-bit is a
            // nice-to-have we do not claim (ADR-0008).
            if (self.api.bits_per_pixel)(img, 0) != 8 {
                return None;
            }
            // Mono/4:2:2/4:4:4 would need different conversion; the stream is 4:2:0.
            if (self.api.chroma_format)(img) != 1 {
                return None;
            }

            let width = (self.api.image_width)(img, 0) as usize;
            let height = (self.api.image_height)(img, 0) as usize;
            if width == 0 || height == 0 {
                return None;
            }

            let plane = |channel: c_int, rows: usize| -> (Vec<u8>, usize) {
                let mut stride: c_int = 0;
                let ptr = (self.api.image_plane)(img, channel, &mut stride);
                let stride = stride as usize;
                if ptr.is_null() || stride == 0 {
                    return (Vec::new(), 0);
                }
                let len = stride * rows;
                (std::slice::from_raw_parts(ptr, len).to_vec(), stride)
            };

            let (y, y_stride) = plane(0, height);
            let (u, u_stride) = plane(1, height.div_ceil(2));
            let (v, v_stride) = plane(2, height.div_ceil(2));

            if !self.logged_geometry.swap(true, Ordering::SeqCst) {
                let mean = |p: &[u8]| {
                    if p.is_empty() {
                        f64::NAN
                    } else {
                        p.iter().map(|&b| f64::from(b)).sum::<f64>() / p.len() as f64
                    }
                };
                let chroma = (self.api.chroma_format)(img);
                println!(
                    "[framesim] decode: first picture {width}x{height} (luma), chroma={chroma}, \
                     Y stride {y_stride}, Y range [{}..{}] mean {:.1}, U mean {:.1}, V mean {:.1}",
                    y.iter().copied().min().unwrap_or(0),
                    y.iter().copied().max().unwrap_or(0),
                    mean(&y),
                    mean(&u),
                    mean(&v),
                );
            }

            Some(Frame {
                pts_ns: (self.api.image_pts)(img),
                width,
                height,
                y,
                y_stride,
                u,
                u_stride,
                v,
                v_stride,
                full_range: (self.api.full_range_flag)(img) != 0,
                // matrix_coefficients: 1 == BT.709. Everything else (2, 5, 6
                // unspecified) falls back to 709, which is what the stream uses.
                matrix_709: (self.api.matrix_coefficients)(img) != 6,
            })
        }
    }
}

impl Drop for HevcDecoder {
    fn drop(&mut self) {
        unsafe { (self.api.free_decoder)(self.ctx) };
    }
}

fn starts_with_start_code(data: &[u8]) -> bool {
    data.len() >= 4 && (data[..4] == [0, 0, 0, 1] || data[..3] == [0, 0, 1])
}

fn trim_zeros(nal: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < nal.len() && nal[start] == 0 {
        start += 1;
    }
    let mut end = nal.len();
    while end > start && nal[end - 1] == 0 {
        end -= 1;
    }
    &nal[start..end]
}

fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut pos = None;
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(start) = pos {
                units.push(&data[start..i]);
            }
            pos = Some(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some(start) = pos {
        units.push(&data[start..]);
    }
    units
}

fn split_avcc(data: &[u8]) -> Option<Vec<&[u8]>> {
    let mut units = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if len == 0 || i + len > data.len() {
            return None;
        }
        units.push(&data[i..i + len]);
        i += len;
    }
    if i == data.len() && !units.is_empty() {
        Some(units)
    } else {
        None
    }
}
