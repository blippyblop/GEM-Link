//! Minimal PNG writer (RGB8) for the emulated-prototype harness.
//!
//! Deliberately tiny: we need to look at decoded frames, not ship an image
//! library. Uses zlib via `flate2` (already in the workspace dependency graph)
//! and `flate2::Crc` for the chunk CRCs.

#![allow(clippy::cast_possible_truncation)]

use flate2::{Compression, Crc, write::ZlibEncoder};
use std::{fs::File, io::Write, path::Path};

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);

    let mut crc = Crc::new();
    crc.update(kind);
    crc.update(payload);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

/// Write an 8-bit RGB image. `rgb` must be `width * height * 3` bytes.
pub fn write_rgb(path: &Path, width: u32, height: u32, rgb: &[u8]) -> std::io::Result<()> {
    assert_eq!(rgb.len(), width as usize * height as usize * 3);

    // IHDR: width, height, bit depth 8, colour type 2 (truecolour), no interlace.
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);

    // Raw scanlines, each prefixed with filter type 0 (None).
    let stride = width as usize * 3;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in 0..height as usize {
        raw.push(0);
        raw.extend_from_slice(&rgb[row * stride..(row + 1) * stride]);
    }

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&raw)?;
    let idat = encoder.finish()?;

    let mut out = Vec::with_capacity(idat.len() + 128);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);

    File::create(path)?.write_all(&out)
}

/// BT.709 (or BT.601 for the SD case) YUV 4:2:0 → packed RGB8.
///
/// `matrix`: true = BT.709, false = BT.601. `full_range` follows the stream's
/// `video_full_range_flag`; guessing wrong here shows up as crushed blacks or a
/// washed-out image, which is exactly the kind of thing a prototype is for.
pub fn yuv420_to_rgb8(
    y: &[u8],
    y_stride: usize,
    u: &[u8],
    u_stride: usize,
    v: &[u8],
    v_stride: usize,
    width: usize,
    height: usize,
    matrix_709: bool,
    full_range: bool,
) -> Vec<u8> {
    // Scaling of the chroma difference, and of luma for the limited range.
    let (kr, kb) = if matrix_709 { (0.2126, 0.0722) } else { (0.299, 0.114) };
    let (kr, kb) = (kr as f32, kb as f32);
    let kg = 1.0 - kr - kb;

    let (y_scale, y_off, c_scale) = if full_range {
        (1.0f32, 0.0f32, 1.0f32)
    } else {
        (255.0 / 219.0, 16.0, 255.0 / 224.0)
    };

    let r_cr = 2.0 * (1.0 - kr);
    let b_cb = 2.0 * (1.0 - kb);
    let g_cb = 2.0 * kb * (1.0 - kb) / kg;
    let g_cr = 2.0 * kr * (1.0 - kr) / kg;

    let mut out = vec![0u8; width * height * 3];

    for row in 0..height {
        let y_row = &y[row * y_stride..];
        let c_row = row / 2;
        let u_row = &u[c_row * u_stride..];
        let v_row = &v[c_row * v_stride..];

        for col in 0..width {
            let yv = (f32::from(y_row[col]) - y_off) * y_scale;
            let c = col / 2;
            let cb = (f32::from(u_row[c]) - 128.0) * c_scale;
            let cr = (f32::from(v_row[c]) - 128.0) * c_scale;

            let r = yv + r_cr * cr;
            let g = yv - g_cb * cb - g_cr * cr;
            let b = yv + b_cb * cb;

            let o = (row * width + col) * 3;
            out[o] = r.clamp(0.0, 255.0) as u8;
            out[o + 1] = g.clamp(0.0, 255.0) as u8;
            out[o + 2] = b.clamp(0.0, 255.0) as u8;
        }
    }

    out
}
