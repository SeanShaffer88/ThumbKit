//! PNG decoder, written from scratch on top of
//! `crate::inflate` (the crate is zero-dependency).
//!
//! Scope (a real decoder for these variants, per the
//! kit's no-placeholder rule):
//! - Color types 0 (grayscale), 2 (RGB), 3 (palette),
//!   4 (grayscale + alpha) and 6 (RGBA).
//! - Bit depths 1/2/4/8/16 wherever the PNG spec allows
//!   them for the color type. Sub-8 samples are unpacked
//!   (and grayscale is scaled to the full 0..=255 range);
//!   16-bit samples keep their high byte.
//! - All five scanline filters, multiple IDAT chunks,
//!   and Adam7 interlacing.
//! - Alpha is dropped (colour kept un-premultiplied) —
//!   the same treatment the 32-bit BMP decoder gives its
//!   fourth byte. tRNS is parsed over and ignored for the
//!   same reason.
//!
//! Chunk CRCs are not verified (the widespread decoder
//! practice); corruption is still caught — the zlib
//! Adler-32 inside IDAT is verified, and every length
//! and structure is bounds-checked. Anything malformed
//! is a clean error, never a panic.
//!
//! The decoder memory cap (100 MP / 256 MiB) is checked
//! from the IHDR before any IDAT byte is inflated.

use crate::{check_pixel_cap, DecodedImage, ThResult, ERR_INVALID_ARG, ERR_NOT_SUPPORTED};

fn err<T>(msg: &str) -> ThResult<T> {
    Err((ERR_INVALID_ARG, format!("invalid PNG: {msg}")))
}

fn unsupported<T>(msg: String) -> ThResult<T> {
    Err((ERR_NOT_SUPPORTED, msg))
}

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

fn u32be(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Header-only PNG dimensions, when the bytes carry a
/// valid signature and IHDR.
pub fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 29 || &bytes[0..8] != SIGNATURE {
        return None;
    }
    if &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32be(bytes, 16);
    let h = u32be(bytes, 20);
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

#[derive(Clone, Copy)]
struct Ihdr {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    interlace: u8,
}

impl Ihdr {
    fn channels(&self) -> usize {
        match self.color_type {
            0 | 3 => 1,
            2 => 3,
            4 => 2,
            6 => 4,
            _ => 0,
        }
    }
    /// Bytes per pixel for filtering: the spec defines
    /// bpp as the size of one pixel rounded UP to a whole
    /// byte, with a minimum of 1 (sub-8-bit pixels filter
    /// with bpp 1).
    fn filter_bpp(&self) -> usize {
        (self.channels() * self.bit_depth as usize)
            .div_ceil(8)
            .max(1)
    }
    /// Bytes in one packed scanline of `pixels` pixels.
    fn row_bytes(&self, pixels: u32) -> usize {
        (pixels as usize * self.channels() * self.bit_depth as usize).div_ceil(8)
    }
}

/// Adam7 pass table: (x_start, y_start, x_step, y_step).
const ADAM7: [(u32, u32, u32, u32); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

fn pass_dim(full: u32, start: u32, step: u32) -> u32 {
    if full <= start {
        0
    } else {
        (full - start).div_ceil(step)
    }
}

/// Decode a PNG file to the kit's RGB image form.
pub fn decode_png(bytes: &[u8]) -> ThResult<DecodedImage> {
    if bytes.len() < 8 || &bytes[0..8] != SIGNATURE {
        return err("missing the 8-byte PNG signature");
    }
    let mut pos = 8usize;
    let mut ihdr: Option<Ihdr> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();
    let mut seen_iend = false;
    while pos < bytes.len() {
        if pos + 8 > bytes.len() {
            return err("chunk header is truncated");
        }
        let len = u32be(bytes, pos) as usize;
        let ctype = &bytes[pos + 4..pos + 8];
        let data_start = pos + 8;
        let data_end = match data_start.checked_add(len) {
            Some(e) if e + 4 <= bytes.len() => e,
            _ => return err("chunk length runs past the end of the file"),
        };
        let data = &bytes[data_start..data_end];
        match ctype {
            b"IHDR" => {
                if ihdr.is_some() {
                    return err("duplicate IHDR chunk");
                }
                if len != 13 {
                    return err("IHDR chunk is not 13 bytes");
                }
                let h = Ihdr {
                    width: u32be(data, 0),
                    height: u32be(data, 4),
                    bit_depth: data[8],
                    color_type: data[9],
                    interlace: data[12],
                };
                if h.width == 0 || h.height == 0 {
                    return err("zero image dimension");
                }
                if data[10] != 0 {
                    return unsupported(format!(
                        "unsupported PNG compression method {} (only 0 is defined)",
                        data[10]
                    ));
                }
                if data[11] != 0 {
                    return unsupported(format!(
                        "unsupported PNG filter method {} (only 0 is defined)",
                        data[11]
                    ));
                }
                if h.interlace > 1 {
                    return err("interlace method must be 0 or 1");
                }
                match h.color_type {
                    0 | 2 | 3 | 4 | 6 => {}
                    other => {
                        return unsupported(format!(
                            "unsupported PNG color type {other} (only 0, 2, 3, 4 and 6 are defined)"
                        ))
                    }
                }
                let depth_ok = match h.color_type {
                    0 => matches!(h.bit_depth, 1 | 2 | 4 | 8 | 16),
                    2 | 4 | 6 => matches!(h.bit_depth, 8 | 16),
                    3 => matches!(h.bit_depth, 1 | 2 | 4 | 8),
                    _ => false,
                };
                if !depth_ok {
                    return unsupported(format!(
                        "unsupported PNG bit depth {} for color type {}",
                        h.bit_depth, h.color_type
                    ));
                }
                // The memory cap is a safety path: check
                // it before any IDAT byte is inflated.
                check_pixel_cap(h.width, h.height, "PNG")?;
                ihdr = Some(h);
            }
            b"PLTE" => {
                if len % 3 != 0 {
                    return err("PLTE chunk length is not a multiple of 3");
                }
                if len / 3 > 256 {
                    return err("PLTE chunk holds more than 256 entries");
                }
                palette = data.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => {
                seen_iend = true;
                break;
            }
            // tRNS and all ancillary chunks: skipped over.
            // Alpha is dropped by this decoder, so tRNS
            // has nothing to change.
            _ => {}
        }
        pos = data_end + 4; // skip the CRC (not verified)
    }
    let ihdr = match ihdr {
        Some(h) => h,
        None => return err("no IHDR chunk"),
    };
    if !seen_iend {
        return err("no IEND chunk (file truncated?)");
    }
    if ihdr.color_type == 3 && palette.is_empty() {
        return err("palette image has no PLTE chunk");
    }
    if idat.is_empty() {
        return err("no IDAT chunks");
    }

    // Expected inflated size: for every pass (one for
    // non-interlaced), each row is 1 filter byte plus
    // the packed row bytes.
    let passes: &[(u32, u32, u32, u32)] = if ihdr.interlace == 1 {
        &ADAM7
    } else {
        &[(0, 0, 1, 1)]
    };
    let mut expected: usize = 0;
    for &(xs, ys, xst, yst) in passes {
        let pw = pass_dim(ihdr.width, xs, xst);
        let ph = pass_dim(ihdr.height, ys, yst);
        if pw == 0 || ph == 0 {
            continue;
        }
        expected += ph as usize * (1 + ihdr.row_bytes(pw));
    }
    let raw = crate::inflate::zlib_decompress(&idat, expected).map_err(|e| {
        (
            ERR_INVALID_ARG,
            format!("invalid PNG: IDAT zlib stream: {e}"),
        )
    })?;
    if raw.len() != expected {
        return err("inflated IDAT size does not match the IHDR geometry");
    }

    let mut img = DecodedImage {
        width: ihdr.width,
        height: ihdr.height,
        rgb: vec![0u8; ihdr.width as usize * ihdr.height as usize * 3],
    };
    let bpp = ihdr.filter_bpp();
    let mut cursor = 0usize;
    for &(xs, ys, xst, yst) in passes {
        let pw = pass_dim(ihdr.width, xs, xst);
        let ph = pass_dim(ihdr.height, ys, yst);
        if pw == 0 || ph == 0 {
            continue;
        }
        let rb = ihdr.row_bytes(pw);
        let mut prev: Vec<u8> = vec![0u8; rb];
        for row in 0..ph {
            let filter = raw[cursor];
            let line = &raw[cursor + 1..cursor + 1 + rb];
            cursor += 1 + rb;
            let unfiltered = unfilter(filter, line, &prev, bpp)?;
            let y = ys + row * yst;
            for col in 0..pw {
                let x = xs + col * xst;
                let rgb = pixel_rgb(&ihdr, &palette, &unfiltered, col as usize)?;
                let d = (y as usize * ihdr.width as usize + x as usize) * 3;
                img.rgb[d..d + 3].copy_from_slice(&rgb);
            }
            prev = unfiltered;
        }
    }
    Ok(img)
}

/// Reverse one scanline filter. `prev` is the already
/// unfiltered row above (zeros for the first row).
fn unfilter(filter: u8, line: &[u8], prev: &[u8], bpp: usize) -> ThResult<Vec<u8>> {
    let mut out = vec![0u8; line.len()];
    match filter {
        0 => out.copy_from_slice(line),
        1 => {
            for i in 0..line.len() {
                let left = if i >= bpp { out[i - bpp] } else { 0 };
                out[i] = line[i].wrapping_add(left);
            }
        }
        2 => {
            for i in 0..line.len() {
                out[i] = line[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..line.len() {
                let left = if i >= bpp { out[i - bpp] as u16 } else { 0 };
                let up = prev[i] as u16;
                out[i] = line[i].wrapping_add(((left + up) / 2) as u8);
            }
        }
        4 => {
            for i in 0..line.len() {
                let a = if i >= bpp { out[i - bpp] } else { 0 };
                let b = prev[i];
                let c = if i >= bpp { prev[i - bpp] } else { 0 };
                out[i] = line[i].wrapping_add(paeth(a, b, c));
            }
        }
        other => {
            return Err((
                ERR_INVALID_ARG,
                format!("invalid PNG: scanline filter type {other} (only 0..=4 are defined)"),
            ))
        }
    }
    Ok(out)
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (a, b, c) = (a as i32, b as i32, c as i32);
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

/// Extract sample `idx` from a packed sub-8-bit row:
/// samples are stored most-significant-bit first.
fn sub_byte_sample(row: &[u8], idx: usize, depth: u8) -> u8 {
    let bits = idx * depth as usize;
    let byte = row[bits / 8];
    let shift = 8 - depth - (bits % 8) as u8;
    (byte >> shift) & ((1u8 << depth) - 1)
}

/// One pixel of an unfiltered row as RGB. Alpha (types
/// 4 and 6, and tRNS) is dropped, matching the built-in
/// BMP decoder's treatment of its fourth byte.
fn pixel_rgb(ihdr: &Ihdr, palette: &[[u8; 3]], row: &[u8], col: usize) -> ThResult<[u8; 3]> {
    let bd = ihdr.bit_depth;
    match (ihdr.color_type, bd) {
        (3, _) => {
            let idx = if bd == 8 {
                row[col]
            } else {
                sub_byte_sample(row, col, bd)
            };
            palette.get(idx as usize).copied().ok_or_else(|| {
                (
                    ERR_INVALID_ARG,
                    format!("invalid PNG: palette index {idx} out of range"),
                )
            })
        }
        (0, 16) => {
            let v = row[col * 2];
            Ok([v, v, v])
        }
        (0, 8) => {
            let v = row[col];
            Ok([v, v, v])
        }
        (0, _) => {
            let v = sub_byte_sample(row, col, bd);
            let max = (1u16 << bd) - 1;
            let scaled = (v as u16 * 255 / max) as u8;
            Ok([scaled, scaled, scaled])
        }
        (2, 8) => Ok([row[col * 3], row[col * 3 + 1], row[col * 3 + 2]]),
        (2, 16) => Ok([row[col * 6], row[col * 6 + 2], row[col * 6 + 4]]),
        (4, 8) => {
            let v = row[col * 2];
            Ok([v, v, v])
        }
        (4, 16) => {
            let v = row[col * 4];
            Ok([v, v, v])
        }
        (6, 8) => Ok([row[col * 4], row[col * 4 + 1], row[col * 4 + 2]]),
        (6, 16) => Ok([row[col * 8], row[col * 8 + 2], row[col * 8 + 4]]),
        _ => Err((
            ERR_NOT_SUPPORTED,
            format!(
                "unsupported PNG bit depth {} for color type {}",
                bd, ihdr.color_type
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn at(img: &DecodedImage, x: u32, y: u32) -> [u8; 3] {
        let d = (y as usize * img.width as usize + x as usize) * 3;
        [img.rgb[d], img.rgb[d + 1], img.rgb[d + 2]]
    }

    #[test]
    fn fixture_rgb8_decodes_exactly() {
        let img = decode_png(&fixture("png_rgb8.png")).unwrap();
        assert_eq!((img.width, img.height), (4, 4));
        for y in 0..4u32 {
            for x in 0..4u32 {
                assert_eq!(
                    at(&img, x, y),
                    [(x * 60) as u8, (y * 60) as u8, ((x + y) * 30) as u8],
                    "pixel {x},{y}"
                );
            }
        }
    }

    #[test]
    fn fixture_palette_decodes_exactly() {
        let img = decode_png(&fixture("png_palette.png")).unwrap();
        assert_eq!((img.width, img.height), (4, 2));
        let want = [
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [255, 255, 0],
            [255, 255, 0],
            [0, 0, 255],
            [0, 255, 0],
            [255, 0, 0],
        ];
        for (i, w) in want.iter().enumerate() {
            assert_eq!(at(&img, (i % 4) as u32, (i / 4) as u32), *w, "pixel {i}");
        }
    }

    #[test]
    fn fixture_rgba_decodes_rgb_and_drops_alpha() {
        let img = decode_png(&fixture("png_rgba.png")).unwrap();
        assert_eq!((img.width, img.height), (3, 2));
        let want = [
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [10, 20, 30],
            [200, 150, 100],
            [1, 2, 3],
        ];
        for (i, w) in want.iter().enumerate() {
            assert_eq!(at(&img, (i % 3) as u32, (i / 3) as u32), *w, "pixel {i}");
        }
    }

    #[test]
    fn fixture_gray8_decodes_exactly() {
        let img = decode_png(&fixture("png_gray8.png")).unwrap();
        assert_eq!((img.width, img.height), (4, 1));
        for (x, v) in [0u8, 85, 170, 255].iter().enumerate() {
            assert_eq!(at(&img, x as u32, 0), [*v, *v, *v]);
        }
    }

    #[test]
    fn fixture_interlaced_adam7_decodes_exactly() {
        let img = decode_png(&fixture("png_interlaced.png")).unwrap();
        assert_eq!((img.width, img.height), (8, 8));
        for y in 0..8u32 {
            for x in 0..8u32 {
                assert_eq!(
                    at(&img, x, y),
                    [(x * 36) as u8, (y * 36) as u8, ((x + y) * 18) as u8],
                    "pixel {x},{y}"
                );
            }
        }
    }

    // A 2x2 RGB PNG embedded as bytes (assembled with
    // Python's zlib from the spec scanlines and read
    // back through PIL at generation time), so core
    // decode coverage needs no fixture file: pixels are
    // red, green / blue, white.
    const EMBEDDED_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR len/type
        0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, // 2 x 2
        0x08, 0x02, 0x00, 0x00, 0x00, // 8-bit RGB, no interlace
        0xFD, 0xD4, 0x9A, 0x73, // IHDR crc
        0x00, 0x00, 0x00, 0x19, 0x49, 0x44, 0x41, 0x54, // IDAT len/type
        0x78, 0x01, 0x01, 0x0E, 0x00, 0xF1, 0xFF, // zlib hdr + stored block
        0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, // row 0: filter 0, red, green
        0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, // row 1: filter 0, blue, white
        0x1F, 0xEE, 0x05, 0xFB, // adler32
        0xDE, 0xDD, 0xEC, 0x2B, // IDAT crc
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, // IEND
        0xAE, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn embedded_png_decodes_exactly() {
        // First prove the fixture itself is spec-valid:
        // Python's zlib (development-time check, mirrored
        // offline) agrees; here we check decode results.
        let img = decode_png(EMBEDDED_PNG).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(at(&img, 0, 0), [255, 0, 0]);
        assert_eq!(at(&img, 1, 0), [0, 255, 0]);
        assert_eq!(at(&img, 0, 1), [0, 0, 255]);
        assert_eq!(at(&img, 1, 1), [255, 255, 255]);
    }

    #[test]
    fn dimensions_from_header() {
        assert_eq!(png_dimensions(&fixture("png_rgb8.png")), Some((4, 4)));
        assert_eq!(png_dimensions(b"not a png"), None);
        assert_eq!(png_dimensions(&[]), None);
    }

    #[test]
    fn oversize_header_is_rejected_before_decode() {
        // A header claiming 20000x20000 (400 MP) with no
        // IDAT at all must fail with the CAP error, not a
        // truncation error — the cap is the safety path
        // and is checked first (family rule).
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SIGNATURE);
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&20000u32.to_be_bytes());
        bytes.extend_from_slice(&20000u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 2, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(b"IEND");
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let (code, msg) = decode_png(&bytes).unwrap_err();
        assert_eq!(code, ERR_INVALID_ARG);
        assert!(msg.contains("exceeds the decoder cap"), "{msg}");
    }

    #[test]
    fn truncations_and_garbage_never_panic() {
        let good = fixture("png_rgb8.png");
        for cut in 0..good.len() {
            assert!(decode_png(&good[..cut]).is_err(), "cut {cut}");
        }
        assert!(decode_png(&[]).is_err());
        assert!(decode_png(b"\x89PNG\r\n\x1a\n").is_err());
        let mut bad = good.clone();
        bad[19] = 0; // low byte of the big-endian width: 4 -> 0, invalid
        assert!(decode_png(&bad).is_err());
    }

    #[test]
    fn all_five_filters_roundtrip() {
        // Build a 6x5 RGB PNG by hand using each filter
        // type on successive rows, then decode it back to
        // the known pixel pattern. The filtered bytes are
        // computed here from the filter definitions (an
        // independent forward implementation).
        let (w, h) = (6u32, 5u32);
        let px = |x: u32, y: u32| [(x * 40) as u8, (y * 50) as u8, ((x + y) * 20) as u8];
        let mut rows: Vec<Vec<u8>> = Vec::new();
        for y in 0..h {
            let mut row = Vec::new();
            for x in 0..w {
                row.extend_from_slice(&px(x, y));
            }
            rows.push(row);
        }
        let bpp = 3usize;
        let mut raw = Vec::new();
        let mut prev = vec![0u8; w as usize * 3];
        for (y, row) in rows.iter().enumerate() {
            let f = (y % 5) as u8;
            raw.push(f);
            for i in 0..row.len() {
                let a = if i >= bpp { row[i - bpp] } else { 0 };
                let b = prev[i];
                let c = if i >= bpp { prev[i - bpp] } else { 0 };
                let pred = match f {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => ((a as u16 + b as u16) / 2) as u8,
                    _ => paeth(a, b, c),
                };
                raw.push(row[i].wrapping_sub(pred));
            }
            prev = row.clone();
        }
        let idat = zlib_compress_for_test(&raw);
        let mut png = Vec::new();
        png.extend_from_slice(SIGNATURE);
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&w.to_be_bytes());
        png.extend_from_slice(&h.to_be_bytes());
        png.extend_from_slice(&[8, 2, 0, 0, 0]);
        png.extend_from_slice(&[0, 0, 0, 0]);
        png.extend_from_slice(&(idat.len() as u32).to_be_bytes());
        png.extend_from_slice(b"IDAT");
        png.extend_from_slice(&idat);
        png.extend_from_slice(&[0, 0, 0, 0]);
        png.extend_from_slice(&0u32.to_be_bytes());
        png.extend_from_slice(b"IEND");
        png.extend_from_slice(&[0, 0, 0, 0]);
        let img = decode_png(&png).unwrap();
        for y in 0..h {
            for x in 0..w {
                assert_eq!(at(&img, x, y), px(x, y), "pixel {x},{y}");
            }
        }
    }

    /// Test-only zlib compressor: header, stored blocks
    /// and an Adler-32 trailer, using the crate's own
    /// adler32 so the wrap is at least self-consistent.
    /// (The inflater's correctness against real zlib
    /// output is covered by its own Python vectors.)
    fn zlib_compress_for_test(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let mut chunks: Vec<&[u8]> = data.chunks(65535).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        let n = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            out.push(if i == n - 1 { 1 } else { 0 });
            out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
            out.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        out.extend_from_slice(&crate::inflate::adler32(data).to_be_bytes());
        out
    }
}
