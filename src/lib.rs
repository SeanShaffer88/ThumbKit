//! ThumbKit — a thumbnail and preview pipeline behind a
//! small C ABI.
//!
//! The pipeline is the product:
//! - **Pluggable generators.** A provider is registered
//!   for a set of file extensions and is called with
//!   `(source_path, out_path, max_w, max_h, user_data)`; it
//!   writes a thumbnail file to `out_path` and returns 0
//!   on success. Built-in providers cover BMP (24/32-bit
//!   uncompressed) and PPM (P6) for real — decode,
//!   nearest-neighbour downscale (never upscale), BMP out.
//!   Formats without a decoder (PNG/JPEG/GIF/…) return
//!   `unsupported` until a provider is registered for them.
//!   There are no placeholder thumbnails.
//! - **Bounded cache.** Entries live in `cache_dir` as
//!   `<sha256>.thumb` plus a `<sha256>.json` sidecar. The
//!   key covers the canonical source path, source size,
//!   source mtime, preset dimensions and provider name, so
//!   a changed source produces a new key (the old entry
//!   becomes an orphan that `sweep` removes). Bounds
//!   (`max_cache_bytes`, `max_entries`) are enforced by
//!   LRU eviction (sidecar `last_access`) after every
//!   completed generation and on `sweep`.
//! - **Async retrieval.** Requests run on a bounded
//!   worker pool (default 2 workers, bounded queue).
//!   Callers poll, wait, or cancel queued requests by id.
//!
//! Memory bound: the built-in decoders reject any image
//! whose pixel count exceeds 100 megapixels or whose RGB
//! buffer would exceed 256 MiB, with a clear error, before
//! allocating it.

pub mod ffi;
pub mod json;
pub mod platform;
pub mod sha256;

pub const VERSION: &str = "0.1.0";
pub const VERSION_MAJOR: u32 = 0;
pub const VERSION_MINOR: u32 = 1;
pub const VERSION_PATCH: u32 = 0;

// Error codes (mirror include/thumbkit.h).
pub const TH_OK: i32 = 0;
pub const ERR_INVALID_ARG: i32 = 1;
pub const ERR_NOT_SUPPORTED: i32 = 2;
pub const ERR_NOT_FOUND: i32 = 3;
pub const ERR_ACCESS_DENIED: i32 = 4;
pub const ERR_PLATFORM: i32 = 5;
pub const ERR_INTERNAL: i32 = 6;
pub const ERR_PARSE: i32 = 7;
pub const ERR_CANCELLED: i32 = 8;
pub const ERR_INVALID_STATE: i32 = 9;
pub const ERR_TIMEOUT: i32 = 10;
pub const ERR_QUEUE_FULL: i32 = 11;

pub type ThResult<T> = Result<T, (i32, String)>;

/// Decoder memory bound: maximum decoded pixel count.
pub const MAX_PIXELS: u64 = 100_000_000;
/// Decoder memory bound: maximum decoded RGB buffer size.
pub const MAX_PIXEL_BYTES: u64 = 256 * 1024 * 1024;

/// Escape a string for inclusion inside JSON double quotes.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Config and presets
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub cache_dir: String,
    pub max_cache_bytes: u64,
    pub max_entries: u64,
    pub max_thumb_width: u32,
    pub max_thumb_height: u32,
    pub worker_count: usize,
    pub queue_capacity: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            cache_dir: String::new(),
            max_cache_bytes: 64 * 1024 * 1024,
            max_entries: 1024,
            max_thumb_width: 256,
            max_thumb_height: 256,
            worker_count: 2,
            queue_capacity: 64,
        }
    }
}

fn cfg_u64(v: &json::Value, key: &str, min: i64, max: i64) -> ThResult<u64> {
    match v.as_i64() {
        Some(n) if n >= min && n <= max => Ok(n as u64),
        _ => Err((
            ERR_INVALID_ARG,
            format!("\"{key}\" must be an integer in {min}..={max}"),
        )),
    }
}

/// Parse a `th_create` config JSON object. `cache_dir` is
/// required (string, non-empty); every other key is
/// optional with the defaults on `Config`. Unknown keys
/// and duplicate keys are `ERR_PARSE`.
pub fn parse_config(text: &str) -> ThResult<Config> {
    let value =
        json::parse(text).map_err(|e| (ERR_PARSE, format!("config is not valid JSON: {e}")))?;
    let obj = value
        .as_object()
        .ok_or((ERR_PARSE, "config must be a JSON object".to_string()))?;
    for (key, _) in obj {
        if !matches!(
            key.as_str(),
            "cache_dir"
                | "max_cache_bytes"
                | "max_entries"
                | "max_thumb_width"
                | "max_thumb_height"
                | "worker_count"
                | "queue_capacity"
        ) {
            return Err((ERR_PARSE, format!("unknown config key \"{key}\"")));
        }
    }
    let mut cfg = Config::default();
    match value.get("cache_dir") {
        None => {
            return Err((
                ERR_INVALID_ARG,
                "config is missing required key \"cache_dir\"".to_string(),
            ))
        }
        Some(json::Value::Str(s)) if !s.is_empty() => cfg.cache_dir = s.clone(),
        Some(json::Value::Str(_)) => {
            return Err((
                ERR_INVALID_ARG,
                "\"cache_dir\" must not be empty".to_string(),
            ))
        }
        Some(other) => {
            return Err((
                ERR_INVALID_ARG,
                format!("\"cache_dir\" must be a string, got {}", other.type_name()),
            ))
        }
    }
    if let Some(v) = value.get("max_cache_bytes") {
        cfg.max_cache_bytes = cfg_u64(v, "max_cache_bytes", 1, i64::MAX)?;
    }
    if let Some(v) = value.get("max_entries") {
        cfg.max_entries = cfg_u64(v, "max_entries", 1, 1_000_000)?;
    }
    if let Some(v) = value.get("max_thumb_width") {
        cfg.max_thumb_width = cfg_u64(v, "max_thumb_width", 1, 8192)? as u32;
    }
    if let Some(v) = value.get("max_thumb_height") {
        cfg.max_thumb_height = cfg_u64(v, "max_thumb_height", 1, 8192)? as u32;
    }
    if let Some(v) = value.get("worker_count") {
        cfg.worker_count = cfg_u64(v, "worker_count", 1, 16)? as usize;
    }
    if let Some(v) = value.get("queue_capacity") {
        cfg.queue_capacity = cfg_u64(v, "queue_capacity", 1, 4096)? as usize;
    }
    Ok(cfg)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preset {
    pub max_w: u32,
    pub max_h: u32,
}

/// Parse a request preset `{"max_w":N,"max_h":N}` (either
/// key optional; `None`/empty text means "no preset").
/// Effective dimensions never exceed the config maximums.
pub fn parse_preset(text: Option<&str>, cfg: &Config) -> ThResult<Preset> {
    let mut p = Preset {
        max_w: cfg.max_thumb_width,
        max_h: cfg.max_thumb_height,
    };
    let text = match text {
        None => return Ok(p),
        Some(t) if t.trim().is_empty() => return Ok(p),
        Some(t) => t,
    };
    let value =
        json::parse(text).map_err(|e| (ERR_PARSE, format!("preset is not valid JSON: {e}")))?;
    let obj = value
        .as_object()
        .ok_or((ERR_PARSE, "preset must be a JSON object".to_string()))?;
    for (key, _) in obj {
        if !matches!(key.as_str(), "max_w" | "max_h") {
            return Err((ERR_PARSE, format!("unknown preset key \"{key}\"")));
        }
    }
    if let Some(v) = value.get("max_w") {
        p.max_w = cfg_u64(v, "max_w", 1, 8192)? as u32;
    }
    if let Some(v) = value.get("max_h") {
        p.max_h = cfg_u64(v, "max_h", 1, 8192)? as u32;
    }
    p.max_w = p.max_w.min(cfg.max_thumb_width);
    p.max_h = p.max_h.min(cfg.max_thumb_height);
    Ok(p)
}

// ---------------------------------------------------------------------------
// Built-in decoders: BMP and PPM -> RGB -> downscaled BMP
// ---------------------------------------------------------------------------

/// A decoded image: top-down rows, 3 bytes per pixel (RGB).
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

fn check_pixel_cap(width: u32, height: u32, what: &str) -> ThResult<()> {
    let pixels = width as u64 * height as u64;
    if pixels > MAX_PIXELS || pixels.saturating_mul(3) > MAX_PIXEL_BYTES {
        return Err((
            ERR_INVALID_ARG,
            format!(
                "{what} image {width}x{height} exceeds the decoder cap \
                 ({MAX_PIXELS} pixels / {MAX_PIXEL_BYTES} bytes); refusing to decode"
            ),
        ));
    }
    Ok(())
}

fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn u32le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn i32le(b: &[u8], off: usize) -> i32 {
    u32le(b, off) as i32
}

/// Header-only BMP dimensions, when the bytes look like a
/// BMP this decoder accepts.
pub fn bmp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 54 || &bytes[0..2] != b"BM" {
        return None;
    }
    let w = i32le(bytes, 18);
    let h = i32le(bytes, 22);
    if w <= 0 || h == 0 {
        return None;
    }
    Some((w as u32, h.unsigned_abs()))
}

/// Decode an uncompressed 24- or 32-bit BMP.
pub fn decode_bmp(bytes: &[u8]) -> ThResult<DecodedImage> {
    let err = |m: &str| (ERR_INVALID_ARG, format!("invalid BMP: {m}"));
    if bytes.len() < 54 {
        return Err(err("file shorter than the 54-byte header"));
    }
    if &bytes[0..2] != b"BM" {
        return Err(err("missing \"BM\" magic"));
    }
    let data_off = u32le(bytes, 10) as usize;
    let dib = u32le(bytes, 14);
    if dib < 40 {
        return Err(err("DIB header smaller than BITMAPINFOHEADER"));
    }
    let raw_w = i32le(bytes, 18);
    let raw_h = i32le(bytes, 22);
    if raw_w <= 0 {
        return Err(err("width must be positive"));
    }
    if raw_h == 0 {
        return Err(err("height must not be zero"));
    }
    let top_down = raw_h < 0;
    let width = raw_w as u32;
    let height = raw_h.unsigned_abs();
    if u16le(bytes, 26) != 1 {
        return Err(err("planes must be 1"));
    }
    let bpp = u16le(bytes, 28);
    if bpp != 24 && bpp != 32 {
        return Err((
            ERR_NOT_SUPPORTED,
            format!("unsupported BMP bit depth {bpp} (only 24 and 32 are supported)"),
        ));
    }
    if u32le(bytes, 30) != 0 {
        return Err((
            ERR_NOT_SUPPORTED,
            "compressed BMP files are not supported (BI_RGB only)".to_string(),
        ));
    }
    check_pixel_cap(width, height, "BMP")?;
    let bytes_per_px = (bpp / 8) as usize;
    let stride = (bpp as usize * width as usize).div_ceil(32) * 4;
    let need = data_off
        .checked_add(stride * height as usize)
        .ok_or_else(|| err("size overflow"))?;
    if bytes.len() < need {
        return Err(err("file truncated: pixel data is incomplete"));
    }
    let mut rgb = vec![0u8; width as usize * height as usize * 3];
    for y in 0..height as usize {
        // BMP rows are bottom-up unless the height is negative.
        let src_row = if top_down { y } else { height as usize - 1 - y };
        let row_start = data_off + src_row * stride;
        for x in 0..width as usize {
            let s = row_start + x * bytes_per_px;
            let d = (y * width as usize + x) * 3;
            rgb[d] = bytes[s + 2]; // R
            rgb[d + 1] = bytes[s + 1]; // G
            rgb[d + 2] = bytes[s]; // B
        }
    }
    Ok(DecodedImage { width, height, rgb })
}

/// Header-only PPM (P6) dimensions.
pub fn ppm_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let (w, h, _, _) = parse_ppm_header(bytes).ok()?;
    Some((w, h))
}

/// Parse a P6 header: returns (width, height, maxval,
/// offset of the binary pixel data).
fn parse_ppm_header(bytes: &[u8]) -> ThResult<(u32, u32, u32, usize)> {
    let err = |m: &str| (ERR_INVALID_ARG, format!("invalid PPM: {m}"));
    if bytes.len() < 2 || &bytes[0..2] != b"P6" {
        return Err(err("missing \"P6\" magic"));
    }
    let mut pos = 2usize;
    let mut fields: Vec<u32> = Vec::new();
    while fields.len() < 3 {
        // Skip whitespace and #-to-end-of-line comments.
        loop {
            match bytes.get(pos) {
                Some(b) if b.is_ascii_whitespace() => pos += 1,
                Some(b'#') => {
                    while matches!(bytes.get(pos), Some(b) if *b != b'\n') {
                        pos += 1;
                    }
                }
                _ => break,
            }
        }
        let start = pos;
        while matches!(bytes.get(pos), Some(b) if b.is_ascii_digit()) {
            pos += 1;
        }
        if start == pos {
            return Err(err("expected a number in the header"));
        }
        let n: u32 = std::str::from_utf8(&bytes[start..pos])
            .unwrap()
            .parse()
            .map_err(|_| err("header number out of range"))?;
        fields.push(n);
    }
    // Exactly one whitespace byte separates header and data.
    match bytes.get(pos) {
        Some(b) if b.is_ascii_whitespace() => pos += 1,
        _ => return Err(err("missing whitespace before pixel data")),
    }
    Ok((fields[0], fields[1], fields[2], pos))
}

/// Decode a binary PPM (P6, maxval 255).
pub fn decode_ppm(bytes: &[u8]) -> ThResult<DecodedImage> {
    let (width, height, maxval, off) = parse_ppm_header(bytes)?;
    if width == 0 || height == 0 {
        return Err((ERR_INVALID_ARG, "invalid PPM: zero dimension".to_string()));
    }
    if maxval != 255 {
        return Err((
            ERR_NOT_SUPPORTED,
            format!("unsupported PPM maxval {maxval} (only 255 is supported)"),
        ));
    }
    check_pixel_cap(width, height, "PPM")?;
    let need = width as usize * height as usize * 3;
    if bytes.len() < off + need {
        return Err((
            ERR_INVALID_ARG,
            "invalid PPM: file truncated: pixel data is incomplete".to_string(),
        ));
    }
    Ok(DecodedImage {
        width,
        height,
        rgb: bytes[off..off + need].to_vec(),
    })
}

/// Downscale (never upscale) with nearest-neighbour
/// sampling so the result fits `max_w` x `max_h`.
pub fn downscale(img: &DecodedImage, max_w: u32, max_h: u32) -> DecodedImage {
    let scale = (max_w as f64 / img.width as f64)
        .min(max_h as f64 / img.height as f64)
        .min(1.0);
    let nw = ((img.width as f64 * scale).floor() as u32).max(1);
    let nh = ((img.height as f64 * scale).floor() as u32).max(1);
    if nw == img.width && nh == img.height {
        return img.clone();
    }
    let mut rgb = vec![0u8; nw as usize * nh as usize * 3];
    for y in 0..nh as usize {
        let sy = y * img.height as usize / nh as usize;
        for x in 0..nw as usize {
            let sx = x * img.width as usize / nw as usize;
            let s = (sy * img.width as usize + sx) * 3;
            let d = (y * nw as usize + x) * 3;
            rgb[d..d + 3].copy_from_slice(&img.rgb[s..s + 3]);
        }
    }
    DecodedImage {
        width: nw,
        height: nh,
        rgb,
    }
}

/// Encode an image as a 24-bit BMP (bottom-up, padded rows).
pub fn encode_bmp(img: &DecodedImage) -> Vec<u8> {
    let (w, h) = (img.width as usize, img.height as usize);
    let stride = (24 * w).div_ceil(32) * 4;
    let pixel_bytes = stride * h;
    let file_size = 54 + pixel_bytes;
    let mut out = vec![0u8; file_size];
    out[0] = b'B';
    out[1] = b'M';
    out[2..6].copy_from_slice(&(file_size as u32).to_le_bytes());
    out[10..14].copy_from_slice(&54u32.to_le_bytes());
    out[14..18].copy_from_slice(&40u32.to_le_bytes());
    out[18..22].copy_from_slice(&(img.width as i32).to_le_bytes());
    out[22..26].copy_from_slice(&(img.height as i32).to_le_bytes());
    out[26..28].copy_from_slice(&1u16.to_le_bytes());
    out[28..30].copy_from_slice(&24u16.to_le_bytes());
    out[34..38].copy_from_slice(&(pixel_bytes as u32).to_le_bytes());
    for y in 0..h {
        let src_row = h - 1 - y; // stored bottom-up
        let row_start = 54 + y * stride;
        for x in 0..w {
            let s = (src_row * w + x) * 3;
            let d = row_start + x * 3;
            out[d] = img.rgb[s + 2]; // B
            out[d + 1] = img.rgb[s + 1]; // G
            out[d + 2] = img.rgb[s]; // R
        }
    }
    out
}

/// The built-in generation path shared by the built-in
/// providers and exposed for the Rust API / tests: decode
/// the source (by extension: `bmp` or `ppm`), downscale,
/// write a BMP thumbnail to `out_path`. Returns
/// (source_width, source_height, thumb_width, thumb_height).
pub fn generate_thumbnail_file(
    source: &std::path::Path,
    out_path: &std::path::Path,
    max_w: u32,
    max_h: u32,
) -> ThResult<(u32, u32, u32, u32)> {
    let ext = source
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let bytes = std::fs::read(source)
        .map_err(|e| (ERR_NOT_FOUND, format!("cannot read source file: {e}")))?;
    let img = match ext.as_str() {
        "bmp" => decode_bmp(&bytes)?,
        "ppm" => decode_ppm(&bytes)?,
        _ => {
            return Err((
                ERR_NOT_SUPPORTED,
                format!("no built-in decoder for extension \"{ext}\""),
            ))
        }
    };
    let (sw, sh) = (img.width, img.height);
    let thumb = downscale(&img, max_w, max_h);
    let (tw, th) = (thumb.width, thumb.height);
    std::fs::write(out_path, encode_bmp(&thumb))
        .map_err(|e| (ERR_PLATFORM, format!("cannot write thumbnail file: {e}")))?;
    Ok((sw, sh, tw, th))
}

/// Source dimensions from a file's header, for formats the
/// built-ins understand; `(0, 0)` when unknown.
fn source_dimensions(path: &std::path::Path) -> (u32, u32) {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let Ok(bytes) = std::fs::read(path) else {
        return (0, 0);
    };
    match ext.as_str() {
        "bmp" => bmp_dimensions(&bytes).unwrap_or((0, 0)),
        "ppm" => ppm_dimensions(&bytes).unwrap_or((0, 0)),
        _ => (0, 0),
    }
}

/// Thumbnail dimensions of a generated file, when it is a
/// BMP; `(0, 0)` when it is some other format.
fn thumb_dimensions(path: &std::path::Path) -> (u32, u32) {
    let Ok(bytes) = std::fs::read(path) else {
        return (0, 0);
    };
    bmp_dimensions(&bytes).unwrap_or((0, 0))
}

// ---------------------------------------------------------------------------
// Providers and the cache
// ---------------------------------------------------------------------------

use std::collections::{HashMap, VecDeque};
use std::ffi::{c_void, CString};
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// A provider callback, exactly as declared in the header:
/// write a thumbnail of `source_path` (fitting
/// `max_w` x `max_h`) to `out_path`; return 0 on success,
/// any nonzero code on failure.
pub type GenerateFn =
    extern "C" fn(*const c_char, *const c_char, c_int, c_int, *mut c_void) -> c_int;

#[derive(Clone)]
enum ProviderKind {
    Builtin,
    Custom { cb: GenerateFn, user_data: usize },
}

#[derive(Clone)]
struct Provider {
    name: String,
    extensions: Vec<String>,
    kind: ProviderKind,
}

/// Parse an extension list like `"bmp, .png, JPG"` into
/// lowercase extensions without dots.
pub fn parse_extensions(csv: &str) -> ThResult<Vec<String>> {
    let mut out = Vec::new();
    for part in csv.split(',') {
        let e = part.trim().trim_start_matches('.').to_lowercase();
        if e.is_empty() {
            return Err((
                ERR_INVALID_ARG,
                "extension list contains an empty entry".to_string(),
            ));
        }
        if !e.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err((ERR_INVALID_ARG, format!("invalid extension \"{e}\"")));
        }
        if !out.contains(&e) {
            out.push(e);
        }
    }
    if out.is_empty() {
        return Err((
            ERR_INVALID_ARG,
            "extension list must not be empty".to_string(),
        ));
    }
    Ok(out)
}

/// One cache entry, as stored in its sidecar JSON.
#[derive(Clone, Debug)]
struct CacheEntry {
    key: String,
    source_path: String,
    source_size: u64,
    source_mtime_nanos: u64,
    preset_w: u32,
    preset_h: u32,
    provider: String,
    thumb_width: u32,
    thumb_height: u32,
    source_width: u32,
    source_height: u32,
    last_access: u64,
    bytes: u64,
}

impl CacheEntry {
    fn thumb_path(&self, cache_dir: &Path) -> PathBuf {
        cache_dir.join(format!("{}.thumb", self.key))
    }
    fn sidecar_path(&self, cache_dir: &Path) -> PathBuf {
        cache_dir.join(format!("{}.json", self.key))
    }
    fn to_json(&self) -> String {
        format!(
            "{{\"key\":\"{}\",\"source_path\":\"{}\",\"source_size\":{},\"source_mtime_nanos\":{},\"preset_w\":{},\"preset_h\":{},\"provider\":\"{}\",\"thumb_width\":{},\"thumb_height\":{},\"source_width\":{},\"source_height\":{},\"last_access\":{}}}",
            json_escape(&self.key),
            json_escape(&self.source_path),
            self.source_size,
            self.source_mtime_nanos,
            self.preset_w,
            self.preset_h,
            json_escape(&self.provider),
            self.thumb_width,
            self.thumb_height,
            self.source_width,
            self.source_height,
            self.last_access
        )
    }
    fn from_json(text: &str, bytes: u64) -> Option<CacheEntry> {
        let v = json::parse(text).ok()?;
        let get_s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string());
        let get_u = |k: &str| v.get(k).and_then(|x| x.as_i64()).map(|n| n as u64);
        Some(CacheEntry {
            key: get_s("key")?,
            source_path: get_s("source_path")?,
            source_size: get_u("source_size")?,
            source_mtime_nanos: get_u("source_mtime_nanos")?,
            preset_w: get_u("preset_w")? as u32,
            preset_h: get_u("preset_h")? as u32,
            provider: get_s("provider")?,
            thumb_width: get_u("thumb_width")? as u32,
            thumb_height: get_u("thumb_height")? as u32,
            source_width: get_u("source_width")? as u32,
            source_height: get_u("source_height")? as u32,
            last_access: get_u("last_access")?,
            bytes,
        })
    }
}

/// Scan the cache directory: every entry with a readable
/// sidecar and an existing thumbnail file.
fn scan_entries(cache_dir: &Path) -> Vec<CacheEntry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(cache_dir) else {
        return out;
    };
    for item in rd.flatten() {
        let path = item.path();
        if path.extension().map(|e| e == "json") != Some(true) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // The entry's byte cost is its thumbnail file's size.
        let key_guess = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let thumb = cache_dir.join(format!("{key_guess}.thumb"));
        let Ok(meta) = std::fs::metadata(&thumb) else {
            continue;
        };
        if let Some(mut e) = CacheEntry::from_json(&text, meta.len()) {
            e.key = key_guess;
            out.push(e);
        }
    }
    out
}

/// The cache key for one (source, preset, provider).
pub fn cache_key(
    canonical_source: &str,
    source_size: u64,
    source_mtime_nanos: u64,
    preset: Preset,
    provider_name: &str,
) -> String {
    sha256::sha256_hex(
        format!(
            "{canonical_source}|{source_size}|{source_mtime_nanos}|{}x{}|{provider_name}",
            preset.max_w, preset.max_h
        )
        .as_bytes(),
    )
}

fn source_stat(path: &Path) -> ThResult<(String, u64, u64)> {
    let meta = std::fs::metadata(path).map_err(|e| {
        (
            ERR_NOT_FOUND,
            format!("cannot stat source \"{}\": {e}", path.display()),
        )
    })?;
    if !meta.is_file() {
        return Err((
            ERR_INVALID_ARG,
            format!("source \"{}\" is not a file", path.display()),
        ));
    }
    let canonical = std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string());
    let mtime_nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Ok((canonical, meta.len(), mtime_nanos))
}

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReqState {
    Queued,
    Running,
    Done,
    Failed,
    Unsupported,
    Cancelled,
}

impl ReqState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReqState::Queued => "queued",
            ReqState::Running => "running",
            ReqState::Done => "done",
            ReqState::Failed => "failed",
            ReqState::Unsupported => "unsupported",
            ReqState::Cancelled => "cancelled",
        }
    }
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ReqState::Done | ReqState::Failed | ReqState::Unsupported | ReqState::Cancelled
        )
    }
}

#[derive(Clone, Debug)]
pub struct RequestStatus {
    pub id: u64,
    pub state: ReqState,
    pub thumb_path: Option<String>,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub error: Option<String>,
    pub error_code: i32,
}

impl RequestStatus {
    pub fn to_json(&self) -> String {
        let thumb = match &self.thumb_path {
            Some(p) => format!("\"{}\"", json_escape(p)),
            None => "null".to_string(),
        };
        let error = match &self.error {
            Some(e) => format!("\"{}\"", json_escape(e)),
            None => "null".to_string(),
        };
        format!(
            "{{\"id\":{},\"state\":\"{}\",\"thumb_path\":{},\"width\":{},\"height\":{},\"source_width\":{},\"source_height\":{},\"error\":{}}}",
            self.id,
            self.state.as_str(),
            thumb,
            self.width,
            self.height,
            self.source_width,
            self.source_height,
            error
        )
    }
}

struct Request {
    status: RequestStatus,
    source_path: String,
    preset: Preset,
}

struct Inner {
    providers: Vec<Provider>,
    provider_seq: u64,
    requests: HashMap<u64, Request>,
    queue: VecDeque<u64>,
    hits: u64,
    misses: u64,
    evictions: u64,
    shutdown: bool,
}

struct Shared {
    config: Config,
    inner: Mutex<Inner>,
    queue_cv: Condvar,
    done_cv: Condvar,
    cache_lock: Mutex<()>,
    next_id: AtomicU64,
}

/// A thumbnail pipeline handle (`ThPipeline` in the
/// header). Thread-safe; workers are owned by the handle
/// and joined by `shutdown` (`th_close`).
pub struct Pipeline {
    shared: Arc<Shared>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl Pipeline {
    /// Create a pipeline: create the cache directory if
    /// needed, register the built-in BMP and PPM providers,
    /// and spawn the worker pool.
    pub fn create(config: Config) -> ThResult<Arc<Pipeline>> {
        std::fs::create_dir_all(&config.cache_dir).map_err(|e| {
            (
                ERR_PLATFORM,
                format!("cannot create cache_dir \"{}\": {e}", config.cache_dir),
            )
        })?;
        let shared = Arc::new(Shared {
            config,
            inner: Mutex::new(Inner {
                providers: vec![
                    Provider {
                        name: "builtin-bmp".to_string(),
                        extensions: vec!["bmp".to_string()],
                        kind: ProviderKind::Builtin,
                    },
                    Provider {
                        name: "builtin-ppm".to_string(),
                        extensions: vec!["ppm".to_string()],
                        kind: ProviderKind::Builtin,
                    },
                ],
                provider_seq: 0,
                requests: HashMap::new(),
                queue: VecDeque::new(),
                hits: 0,
                misses: 0,
                evictions: 0,
                shutdown: false,
            }),
            queue_cv: Condvar::new(),
            done_cv: Condvar::new(),
            cache_lock: Mutex::new(()),
            next_id: AtomicU64::new(1),
        });
        let pipeline = Arc::new(Pipeline {
            shared: shared.clone(),
            workers: Mutex::new(Vec::new()),
        });
        let mut handles = Vec::new();
        for _ in 0..shared.config.worker_count {
            let s = shared.clone();
            handles.push(std::thread::spawn(move || worker_loop(s)));
        }
        *lock(&pipeline.workers) = handles;
        Ok(pipeline)
    }

    pub fn config(&self) -> &Config {
        &self.shared.config
    }

    /// Register a provider for a comma-separated extension
    /// list. Later registrations take precedence over
    /// earlier ones (including the built-ins) for the same
    /// extension. Returns the provider's name — part of
    /// every cache key it produces.
    pub fn register_provider(
        &self,
        extension_csv: &str,
        callback: GenerateFn,
        user_data: *mut c_void,
    ) -> ThResult<String> {
        let extensions = parse_extensions(extension_csv)?;
        let mut g = lock(&self.shared.inner);
        g.provider_seq += 1;
        let name = format!("custom-{}-{}", g.provider_seq, extensions.join("-"));
        g.providers.push(Provider {
            name: name.clone(),
            extensions,
            kind: ProviderKind::Custom {
                cb: callback,
                user_data: user_data as usize,
            },
        });
        Ok(name)
    }

    /// Names of all registered providers, in registration
    /// order (built-ins first).
    pub fn provider_names(&self) -> Vec<String> {
        lock(&self.shared.inner)
            .providers
            .iter()
            .map(|p| p.name.clone())
            .collect()
    }

    /// Queue a thumbnail request. Returns the request id.
    /// Fails with `ERR_QUEUE_FULL` when the number of
    /// queued (not yet running) requests has reached
    /// `queue_capacity`.
    pub fn request(&self, source_path: &str, preset_json: Option<&str>) -> ThResult<u64> {
        if source_path.is_empty() {
            return Err((ERR_INVALID_ARG, "source_path must not be empty".to_string()));
        }
        let preset = parse_preset(preset_json, &self.shared.config)?;
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        {
            let mut g = lock(&self.shared.inner);
            if g.shutdown {
                return Err((ERR_INVALID_STATE, "pipeline is shutting down".to_string()));
            }
            if g.queue.len() >= self.shared.config.queue_capacity {
                return Err((
                    ERR_QUEUE_FULL,
                    format!(
                        "request queue is full (capacity {})",
                        self.shared.config.queue_capacity
                    ),
                ));
            }
            g.requests.insert(
                id,
                Request {
                    status: RequestStatus {
                        id,
                        state: ReqState::Queued,
                        thumb_path: None,
                        width: 0,
                        height: 0,
                        source_width: 0,
                        source_height: 0,
                        error: None,
                        error_code: TH_OK,
                    },
                    source_path: source_path.to_string(),
                    preset,
                },
            );
            g.queue.push_back(id);
        }
        self.shared.queue_cv.notify_one();
        Ok(id)
    }

    /// Current status of one request.
    pub fn poll(&self, id: u64) -> ThResult<RequestStatus> {
        let g = lock(&self.shared.inner);
        g.requests
            .get(&id)
            .map(|r| r.status.clone())
            .ok_or((ERR_NOT_FOUND, format!("unknown request id {id}")))
    }

    /// Block until the request is terminal (`TH_OK`) or
    /// the timeout elapses (`ERR_TIMEOUT`; the request is
    /// untouched). `WAIT_INFINITE` waits forever, 0 polls.
    pub fn wait(&self, id: u64, timeout_ms: u32) -> ThResult<()> {
        let mut g = lock(&self.shared.inner);
        let st = g
            .requests
            .get(&id)
            .ok_or((ERR_NOT_FOUND, format!("unknown request id {id}")))?
            .status
            .state;
        if st.is_terminal() {
            return Ok(());
        }
        if timeout_ms == 0 {
            return Err((ERR_TIMEOUT, "request is not finished".to_string()));
        }
        if timeout_ms == WAIT_INFINITE {
            loop {
                g = self
                    .shared
                    .done_cv
                    .wait(g)
                    .unwrap_or_else(|e| e.into_inner());
                let st = g
                    .requests
                    .get(&id)
                    .ok_or((ERR_NOT_FOUND, format!("unknown request id {id}")))?
                    .status
                    .state;
                if st.is_terminal() {
                    return Ok(());
                }
            }
        }
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                let st = g
                    .requests
                    .get(&id)
                    .ok_or((ERR_NOT_FOUND, format!("unknown request id {id}")))?
                    .status
                    .state;
                return if st.is_terminal() {
                    Ok(())
                } else {
                    Err((
                        ERR_TIMEOUT,
                        "wait timed out; request still pending".to_string(),
                    ))
                };
            }
            let (guard, _) = self
                .shared
                .done_cv
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            g = guard;
            let st = g
                .requests
                .get(&id)
                .ok_or((ERR_NOT_FOUND, format!("unknown request id {id}")))?
                .status
                .state;
            if st.is_terminal() {
                return Ok(());
            }
        }
    }

    /// Cancel a QUEUED request. Running requests cannot be
    /// cancelled safely (the provider is arbitrary host
    /// code) and terminal ones are already finished: both
    /// return `ERR_INVALID_STATE`.
    pub fn cancel(&self, id: u64) -> ThResult<()> {
        {
            let mut g = lock(&self.shared.inner);
            let req = g
                .requests
                .get_mut(&id)
                .ok_or((ERR_NOT_FOUND, format!("unknown request id {id}")))?;
            if req.status.state != ReqState::Queued {
                return Err((
                    ERR_INVALID_STATE,
                    format!(
                        "only a queued request can be cancelled (this one is {})",
                        req.status.state.as_str()
                    ),
                ));
            }
            req.status.state = ReqState::Cancelled;
            req.status.error_code = ERR_CANCELLED;
            g.queue.retain(|&q| q != id);
        }
        self.shared.done_cv.notify_all();
        Ok(())
    }

    /// Cache statistics: on-disk entries/bytes from a scan,
    /// plus this handle's in-memory hit/miss/eviction
    /// counters.
    pub fn stats(&self) -> String {
        let entries = scan_entries(Path::new(&self.shared.config.cache_dir));
        let bytes: u64 = entries.iter().map(|e| e.bytes).sum();
        let g = lock(&self.shared.inner);
        format!(
            "{{\"entries\":{},\"bytes\":{},\"hits\":{},\"misses\":{},\"evictions\":{}}}",
            entries.len(),
            bytes,
            g.hits,
            g.misses,
            g.evictions
        )
    }

    /// Remove orphaned entries (source gone, or its size /
    /// mtime no longer matches the sidecar — which means
    /// its key is stale), remove stray files, then enforce
    /// the cache bounds by LRU eviction. Returns a JSON
    /// report: {"removed_orphans":N,"evicted":N,
    /// "entries":N,"bytes":N}.
    pub fn sweep(&self) -> String {
        let cache_dir = PathBuf::from(&self.shared.config.cache_dir);
        let _cache_guard = lock(&self.shared.cache_lock);
        let mut removed_orphans = 0u64;
        for entry in scan_entries(&cache_dir) {
            let stale = match source_stat(Path::new(&entry.source_path)) {
                Err(_) => true,
                Ok((_, size, mtime)) => {
                    size != entry.source_size || mtime != entry.source_mtime_nanos
                }
            };
            if stale {
                let _ = std::fs::remove_file(entry.thumb_path(&cache_dir));
                let _ = std::fs::remove_file(entry.sidecar_path(&cache_dir));
                removed_orphans += 1;
            }
        }
        // Stray temp files from interrupted generations.
        if let Ok(rd) = std::fs::read_dir(&cache_dir) {
            for item in rd.flatten() {
                let name = item.file_name().to_string_lossy().to_string();
                if name.contains(".tmp-") {
                    let _ = std::fs::remove_file(item.path());
                }
            }
        }
        let evicted = enforce_bounds(&self.shared, None);
        let entries = scan_entries(&cache_dir);
        let bytes: u64 = entries.iter().map(|e| e.bytes).sum();
        format!(
            "{{\"removed_orphans\":{},\"evicted\":{},\"entries\":{},\"bytes\":{}}}",
            removed_orphans,
            evicted,
            entries.len(),
            bytes
        )
    }

    /// Cancel queued work, stop the workers and join them.
    /// A request already running finishes first — a
    /// provider is arbitrary host code and cannot be
    /// interrupted safely.
    pub fn shutdown(&self) {
        {
            let mut g = lock(&self.shared.inner);
            g.shutdown = true;
            let queued: Vec<u64> = g.queue.drain(..).collect();
            for id in queued {
                if let Some(r) = g.requests.get_mut(&id) {
                    if r.status.state == ReqState::Queued {
                        r.status.state = ReqState::Cancelled;
                        r.status.error_code = ERR_CANCELLED;
                    }
                }
            }
        }
        self.shared.queue_cv.notify_all();
        self.shared.done_cv.notify_all();
        let handles: Vec<_> = lock(&self.workers).drain(..).collect();
        for h in handles {
            let _ = h.join();
        }
    }
}

/// `th_wait` timeout value meaning "wait indefinitely".
pub const WAIT_INFINITE: u32 = u32::MAX;

/// Enforce `max_cache_bytes` / `max_entries` by evicting
/// least-recently-accessed entries first. `protect` names
/// one entry (the one just generated) that is never
/// evicted by this pass. Returns the number evicted.
fn enforce_bounds(shared: &Shared, protect: Option<&str>) -> u64 {
    let cache_dir = PathBuf::from(&shared.config.cache_dir);
    let mut entries = scan_entries(&cache_dir);
    entries.sort_by(|a, b| {
        a.last_access
            .cmp(&b.last_access)
            .then_with(|| a.key.cmp(&b.key))
    });
    let mut total_bytes: u64 = entries.iter().map(|e| e.bytes).sum();
    let mut count = entries.len() as u64;
    let mut evicted = 0u64;
    for entry in &entries {
        if total_bytes <= shared.config.max_cache_bytes && count <= shared.config.max_entries {
            break;
        }
        if Some(entry.key.as_str()) == protect {
            continue;
        }
        let _ = std::fs::remove_file(entry.thumb_path(&cache_dir));
        let _ = std::fs::remove_file(entry.sidecar_path(&cache_dir));
        total_bytes -= entry.bytes;
        count -= 1;
        evicted += 1;
    }
    if evicted > 0 {
        lock(&shared.inner).evictions += evicted;
    }
    evicted
}

fn finish(shared: &Shared, id: u64, status: RequestStatus) {
    {
        let mut g = lock(&shared.inner);
        if let Some(r) = g.requests.get_mut(&id) {
            // A queued request cancelled while the worker
            // was picking it up stays cancelled.
            if r.status.state == ReqState::Cancelled {
                return;
            }
            r.status = status;
        }
    }
    shared.done_cv.notify_all();
}

fn fail_status(id: u64, state: ReqState, code: i32, msg: String) -> RequestStatus {
    RequestStatus {
        id,
        state,
        thumb_path: None,
        width: 0,
        height: 0,
        source_width: 0,
        source_height: 0,
        error: Some(msg),
        error_code: code,
    }
}

fn worker_loop(shared: Arc<Shared>) {
    loop {
        let id = {
            let mut g = lock(&shared.inner);
            loop {
                if let Some(id) = g.queue.pop_front() {
                    if let Some(r) = g.requests.get_mut(&id) {
                        if r.status.state == ReqState::Cancelled {
                            continue;
                        }
                        r.status.state = ReqState::Running;
                    }
                    break id;
                }
                if g.shutdown {
                    return;
                }
                g = shared.queue_cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            process_request(&shared, id)
        }));
        if result.is_err() {
            finish(
                &shared,
                id,
                fail_status(
                    id,
                    ReqState::Failed,
                    ERR_INTERNAL,
                    "internal panic while generating thumbnail".to_string(),
                ),
            );
        }
    }
}

fn process_request(shared: &Shared, id: u64) {
    let cache_dir = PathBuf::from(&shared.config.cache_dir);
    let (source_path, preset) = {
        let g = lock(&shared.inner);
        match g.requests.get(&id) {
            Some(r) => (r.source_path.clone(), r.preset),
            None => return,
        }
    };
    let source = PathBuf::from(&source_path);
    let ext = source
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    // Latest registration wins for an extension.
    let provider = {
        let g = lock(&shared.inner);
        g.providers
            .iter()
            .rev()
            .find(|p| p.extensions.iter().any(|e| e == &ext))
            .cloned()
    };
    let provider = match provider {
        Some(p) => p,
        None => {
            finish(
                shared,
                id,
                fail_status(
                    id,
                    ReqState::Unsupported,
                    ERR_NOT_SUPPORTED,
                    format!(
                        "no provider registered for extension \"{ext}\" \
                         (built-ins cover bmp and ppm; register a provider for other formats)"
                    ),
                ),
            );
            return;
        }
    };
    let (canonical, size, mtime) = match source_stat(&source) {
        Ok(v) => v,
        Err((code, msg)) => {
            finish(shared, id, fail_status(id, ReqState::Failed, code, msg));
            return;
        }
    };
    let key = cache_key(&canonical, size, mtime, preset, &provider.name);

    // Cache hit?
    {
        let _cache_guard = lock(&shared.cache_lock);
        let hit = scan_entries(&cache_dir).into_iter().find(|e| e.key == key);
        if let Some(mut entry) = hit {
            entry.last_access = now_millis();
            let _ = std::fs::write(entry.sidecar_path(&cache_dir), entry.to_json());
            lock(&shared.inner).hits += 1;
            finish(
                shared,
                id,
                RequestStatus {
                    id,
                    state: ReqState::Done,
                    thumb_path: Some(entry.thumb_path(&cache_dir).to_string_lossy().to_string()),
                    width: entry.thumb_width,
                    height: entry.thumb_height,
                    source_width: entry.source_width,
                    source_height: entry.source_height,
                    error: None,
                    error_code: TH_OK,
                },
            );
            return;
        }
    }
    lock(&shared.inner).misses += 1;

    // Generate to a per-request temp file, outside the
    // cache lock, so providers run in parallel.
    let tmp_path = cache_dir.join(format!("{key}.tmp-{id}"));
    let generated: ThResult<(u32, u32, u32, u32)> = match &provider.kind {
        ProviderKind::Builtin => {
            generate_thumbnail_file(&source, &tmp_path, preset.max_w, preset.max_h)
        }
        ProviderKind::Custom { cb, user_data } => {
            let src_c = CString::new(source_path.as_str());
            let out_c = CString::new(tmp_path.to_string_lossy().to_string());
            match (src_c, out_c) {
                (Ok(s), Ok(o)) => {
                    let rc = cb(
                        s.as_ptr(),
                        o.as_ptr(),
                        preset.max_w as c_int,
                        preset.max_h as c_int,
                        *user_data as *mut c_void,
                    );
                    if rc != 0 {
                        Err((
                            ERR_PLATFORM,
                            format!("provider \"{}\" returned code {rc}", provider.name),
                        ))
                    } else if !tmp_path.exists() {
                        Err((
                            ERR_PLATFORM,
                            format!(
                                "provider \"{}\" reported success but wrote no file",
                                provider.name
                            ),
                        ))
                    } else {
                        let (sw, sh) = source_dimensions(&source);
                        let (tw, th) = thumb_dimensions(&tmp_path);
                        Ok((sw, sh, tw, th))
                    }
                }
                _ => Err((
                    ERR_INVALID_ARG,
                    "source path contains a NUL byte".to_string(),
                )),
            }
        }
    };
    let (sw, sh, tw, th) = match generated {
        Ok(dims) => dims,
        Err((code, msg)) => {
            let _ = std::fs::remove_file(&tmp_path);
            let state = if code == ERR_NOT_SUPPORTED {
                ReqState::Unsupported
            } else {
                ReqState::Failed
            };
            finish(shared, id, fail_status(id, state, code, msg));
            return;
        }
    };

    // Publish into the cache and enforce the bounds.
    let entry = CacheEntry {
        key: key.clone(),
        source_path: canonical,
        source_size: size,
        source_mtime_nanos: mtime,
        preset_w: preset.max_w,
        preset_h: preset.max_h,
        provider: provider.name.clone(),
        thumb_width: tw,
        thumb_height: th,
        source_width: sw,
        source_height: sh,
        last_access: now_millis(),
        bytes: 0,
    };
    {
        let _cache_guard = lock(&shared.cache_lock);
        let final_path = entry.thumb_path(&cache_dir);
        if std::fs::rename(&tmp_path, &final_path).is_err() {
            // Rename can fail across filesystems; the temp
            // file is in the same directory, so a copy is
            // the fallback, never the plan.
            if let Err(e) = std::fs::copy(&tmp_path, &final_path) {
                finish(
                    shared,
                    id,
                    fail_status(
                        id,
                        ReqState::Failed,
                        ERR_PLATFORM,
                        format!("cannot publish thumbnail into the cache: {e}"),
                    ),
                );
                return;
            }
            let _ = std::fs::remove_file(&tmp_path);
        }
        if let Err(e) = std::fs::write(entry.sidecar_path(&cache_dir), entry.to_json()) {
            finish(
                shared,
                id,
                fail_status(
                    id,
                    ReqState::Failed,
                    ERR_PLATFORM,
                    format!("cannot write cache sidecar: {e}"),
                ),
            );
            return;
        }
        enforce_bounds(shared, Some(&key));
    }
    finish(
        shared,
        id,
        RequestStatus {
            id,
            state: ReqState::Done,
            thumb_path: Some(
                cache_dir
                    .join(format!("{key}.thumb"))
                    .to_string_lossy()
                    .to_string(),
            ),
            width: tw,
            height: th,
            source_width: sw,
            source_height: sh,
            error: None,
            error_code: TH_OK,
        },
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn temp_dir(name: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "thumbkit-test-{}-{}-{}",
            std::process::id(),
            name,
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a 24-bit BMP by hand (bottom-up BGR, padded
    /// rows), pixel colour from (x, y) -> (r, g, b).
    fn write_bmp_fixture(path: &Path, w: u32, h: u32, px: impl Fn(u32, u32) -> (u8, u8, u8)) {
        let stride = (24 * w as usize).div_ceil(32) * 4;
        let pixel_bytes = stride * h as usize;
        let mut out = vec![0u8; 54 + pixel_bytes];
        out[0] = b'B';
        out[1] = b'M';
        out[2..6].copy_from_slice(&((54 + pixel_bytes) as u32).to_le_bytes());
        out[10..14].copy_from_slice(&54u32.to_le_bytes());
        out[14..18].copy_from_slice(&40u32.to_le_bytes());
        out[18..22].copy_from_slice(&(w as i32).to_le_bytes());
        out[22..26].copy_from_slice(&(h as i32).to_le_bytes());
        out[26..28].copy_from_slice(&1u16.to_le_bytes());
        out[28..30].copy_from_slice(&24u16.to_le_bytes());
        for row in 0..h as usize {
            let y = h as usize - 1 - row; // stored bottom-up; y is top-down
            for x in 0..w as usize {
                let (r, g, b) = px(x as u32, y as u32);
                let d = 54 + row * stride + x * 3;
                out[d] = b;
                out[d + 1] = g;
                out[d + 2] = r;
            }
        }
        std::fs::write(path, out).unwrap();
    }

    fn write_ppm_fixture(path: &Path, w: u32, h: u32) {
        let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
        for y in 0..h {
            for x in 0..w {
                out.extend_from_slice(&[(x * 20) as u8, (y * 20) as u8, 99]);
            }
        }
        std::fs::write(path, out).unwrap();
    }

    fn make_pipeline(dir: &Path, extra: &str) -> Arc<Pipeline> {
        let cfg = parse_config(&format!(
            "{{\"cache_dir\":\"{}\"{extra}}}",
            dir.join("cache").to_string_lossy().replace('\\', "\\\\")
        ))
        .unwrap();
        Pipeline::create(cfg).unwrap()
    }

    // -- pure pieces --

    #[test]
    fn escapes_json() {
        assert_eq!(json_escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn config_rules() {
        let ok = parse_config(r#"{"cache_dir":"/tmp/c","max_entries":5}"#).unwrap();
        assert_eq!(ok.max_entries, 5);
        assert_eq!(ok.worker_count, 2);
        assert_eq!(ok.queue_capacity, 64);
        assert_eq!(
            parse_config(r#"{"cache_dir":"/tmp/c","bogus":1}"#)
                .unwrap_err()
                .0,
            ERR_PARSE
        );
        assert_eq!(
            parse_config(r#"{"cache_dir":"/tmp/c","cache_dir":"/x"}"#)
                .unwrap_err()
                .0,
            ERR_PARSE
        );
        assert_eq!(parse_config(r#"{}"#).unwrap_err().0, ERR_INVALID_ARG);
        assert_eq!(
            parse_config(r#"{"cache_dir":""}"#).unwrap_err().0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            parse_config(r#"{"cache_dir":"/tmp/c","worker_count":0}"#)
                .unwrap_err()
                .0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            parse_config(r#"{"cache_dir":3}"#).unwrap_err().0,
            ERR_INVALID_ARG
        );
    }

    #[test]
    fn preset_rules_and_clamp() {
        let cfg =
            parse_config(r#"{"cache_dir":"/tmp/c","max_thumb_width":100,"max_thumb_height":80}"#)
                .unwrap();
        let p = parse_preset(Some(r#"{"max_w":999,"max_h":50}"#), &cfg).unwrap();
        assert_eq!((p.max_w, p.max_h), (100, 50)); // clamped to the config caps
        assert_eq!(
            parse_preset(None, &cfg).unwrap(),
            Preset {
                max_w: 100,
                max_h: 80
            }
        );
        assert_eq!(
            parse_preset(Some(r#"{"zoom":1}"#), &cfg).unwrap_err().0,
            ERR_PARSE
        );
    }

    #[test]
    fn bmp_decode_downscale_never_upscale() {
        let dir = temp_dir("bmp");
        let src = dir.join("a.bmp");
        write_bmp_fixture(&src, 8, 4, |x, y| ((x * 30) as u8, (y * 60) as u8, 7));
        let out = dir.join("t.bmp");
        let dims = generate_thumbnail_file(&src, &out, 4, 4).unwrap();
        assert_eq!(dims, (8, 4, 4, 2));
        let thumb = decode_bmp(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!((thumb.width, thumb.height), (4, 2));
        // Nearest neighbour: thumb (0,0) samples source (0,0).
        assert_eq!(&thumb.rgb[0..3], &[0, 0, 7]);
        // Never upscale: a 2x2 source under an 8x8 preset stays 2x2.
        let small = dir.join("s.bmp");
        write_bmp_fixture(&small, 2, 2, |_, _| (1, 2, 3));
        let dims = generate_thumbnail_file(&small, &out, 8, 8).unwrap();
        assert_eq!(dims, (2, 2, 2, 2));
    }

    #[test]
    fn bmp32_and_ppm_decode() {
        let dir = temp_dir("ppm");
        // 32-bit BMP: build via decode/encode round-trip is
        // not available for 32-bit output, so hand-check the
        // decoder's rejection/acceptance on a minimal file:
        let ppm = dir.join("a.ppm");
        write_ppm_fixture(&ppm, 6, 6);
        let out = dir.join("t.bmp");
        let dims = generate_thumbnail_file(&ppm, &out, 3, 3).unwrap();
        assert_eq!(dims, (6, 6, 3, 3));
        let thumb = decode_bmp(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(thumb.rgb[2], 99); // blue channel survives
                                      // Truncated BMP is a clear error, not a panic.
        let bad = dir.join("bad.bmp");
        std::fs::write(&bad, b"BM tiny").unwrap();
        assert!(generate_thumbnail_file(&bad, &out, 4, 4).is_err());
        // Compressed / wrong-depth BMPs are explicitly unsupported.
        let weird = dir.join("w.bmp");
        write_bmp_fixture(&weird, 2, 2, |_, _| (0, 0, 0));
        let mut bytes = std::fs::read(&weird).unwrap();
        bytes[30] = 1; // BI_RLE8 compression flag
        std::fs::write(&weird, bytes).unwrap();
        assert_eq!(
            generate_thumbnail_file(&weird, &out, 4, 4).unwrap_err().0,
            ERR_NOT_SUPPORTED
        );
    }

    #[test]
    fn oversize_image_rejected_before_decode() {
        let dir = temp_dir("oversize");
        let big = dir.join("big.bmp");
        // Header claims 20000x20000 = 400 MP; the pixel cap
        // must reject it before any data-length concern.
        let mut bytes = vec![0u8; 54];
        bytes[0] = b'B';
        bytes[1] = b'M';
        bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
        bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
        bytes[18..22].copy_from_slice(&20000i32.to_le_bytes());
        bytes[22..26].copy_from_slice(&20000i32.to_le_bytes());
        bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
        bytes[28..30].copy_from_slice(&24u16.to_le_bytes());
        std::fs::write(&big, bytes).unwrap();
        let (code, msg) = generate_thumbnail_file(&big, &dir.join("t.bmp"), 64, 64).unwrap_err();
        assert_eq!(code, ERR_INVALID_ARG);
        assert!(msg.contains("exceeds the decoder cap"), "msg was {msg}");
    }

    // -- pipeline behaviour --

    static COUNT: AtomicU64 = AtomicU64::new(0);
    extern "C" fn counting_bmp(
        src: *const c_char,
        out: *const c_char,
        w: c_int,
        h: c_int,
        _ud: *mut c_void,
    ) -> c_int {
        COUNT.fetch_add(1, Ordering::SeqCst);
        let src = unsafe { std::ffi::CStr::from_ptr(src) }.to_str().unwrap();
        let out = unsafe { std::ffi::CStr::from_ptr(out) }.to_str().unwrap();
        match generate_thumbnail_file(Path::new(src), Path::new(out), w as u32, h as u32) {
            Ok(_) => 0,
            Err(_) => 9,
        }
    }

    #[test]
    fn request_wait_done_then_cache_hit() {
        let dir = temp_dir("hit");
        let src = dir.join("a.bmp");
        write_bmp_fixture(&src, 16, 8, |x, y| ((x * 10) as u8, (y * 20) as u8, 3));
        COUNT.store(0, Ordering::SeqCst);
        let pipe = make_pipeline(&dir, "");
        pipe.register_provider("bmp", counting_bmp, std::ptr::null_mut())
            .unwrap();
        let src_s = src.to_string_lossy().to_string();

        let id = pipe
            .request(&src_s, Some(r#"{"max_w":8,"max_h":8}"#))
            .unwrap();
        pipe.wait(id, 10_000).unwrap();
        let st = pipe.poll(id).unwrap();
        assert_eq!(st.state, ReqState::Done, "error was {:?}", st.error);
        assert_eq!((st.source_width, st.source_height), (16, 8));
        assert_eq!((st.width, st.height), (8, 4));
        let thumb_path = st.thumb_path.clone().unwrap();
        assert!(Path::new(&thumb_path).exists());
        assert_eq!(COUNT.load(Ordering::SeqCst), 1);

        // Second identical request: cache hit, provider NOT called again.
        let id2 = pipe
            .request(&src_s, Some(r#"{"max_w":8,"max_h":8}"#))
            .unwrap();
        pipe.wait(id2, 10_000).unwrap();
        let st2 = pipe.poll(id2).unwrap();
        assert_eq!(st2.state, ReqState::Done);
        assert_eq!(st2.thumb_path, Some(thumb_path));
        assert_eq!(
            COUNT.load(Ordering::SeqCst),
            1,
            "provider ran on a cache hit"
        );
        let stats = pipe.stats();
        let v = json::parse(&stats).unwrap();
        assert_eq!(v.get("hits").unwrap().as_i64(), Some(1));
        assert_eq!(v.get("misses").unwrap().as_i64(), Some(1));
        assert_eq!(v.get("entries").unwrap().as_i64(), Some(1));
        pipe.shutdown();
    }

    #[test]
    fn builtin_providers_without_registration() {
        let dir = temp_dir("builtin");
        let src = dir.join("a.ppm");
        write_ppm_fixture(&src, 4, 4);
        let pipe = make_pipeline(&dir, "");
        assert_eq!(pipe.provider_names(), vec!["builtin-bmp", "builtin-ppm"]);
        let id = pipe.request(&src.to_string_lossy(), None).unwrap();
        pipe.wait(id, 10_000).unwrap();
        assert_eq!(pipe.poll(id).unwrap().state, ReqState::Done);
        pipe.shutdown();
    }

    #[test]
    fn invalidation_after_source_changes_and_sweep() {
        let dir = temp_dir("invalidate");
        let src = dir.join("a.bmp");
        write_bmp_fixture(&src, 8, 8, |_, _| (10, 10, 10));
        COUNT.store(0, Ordering::SeqCst);
        let pipe = make_pipeline(&dir, "");
        pipe.register_provider("bmp", counting_bmp, std::ptr::null_mut())
            .unwrap();
        let src_s = src.to_string_lossy().to_string();
        let id = pipe.request(&src_s, None).unwrap();
        pipe.wait(id, 10_000).unwrap();
        let first_thumb = pipe.poll(id).unwrap().thumb_path.unwrap();
        assert_eq!(COUNT.load(Ordering::SeqCst), 1);

        // Change the source (different size AND content):
        // the cache key changes, so this is a miss, and the
        // first entry is now an orphan for sweep to remove.
        write_bmp_fixture(&src, 12, 8, |_, _| (20, 20, 20));
        let id2 = pipe.request(&src_s, None).unwrap();
        pipe.wait(id2, 10_000).unwrap();
        let second_thumb = pipe.poll(id2).unwrap().thumb_path.unwrap();
        assert_ne!(first_thumb, second_thumb);
        assert_eq!(COUNT.load(Ordering::SeqCst), 2);

        let report = json::parse(&pipe.sweep()).unwrap();
        assert_eq!(report.get("removed_orphans").unwrap().as_i64(), Some(1));
        assert!(
            !Path::new(&first_thumb).exists(),
            "orphan thumbnail survived sweep"
        );
        assert!(Path::new(&second_thumb).exists());
        assert_eq!(report.get("entries").unwrap().as_i64(), Some(1));
        pipe.shutdown();
    }

    #[test]
    fn lru_eviction_with_tiny_max_bytes() {
        let dir = temp_dir("lru");
        let a = dir.join("a.bmp");
        let b = dir.join("b.bmp");
        write_bmp_fixture(&a, 8, 8, |_, _| (1, 1, 1));
        write_bmp_fixture(&b, 8, 8, |_, _| (2, 2, 2));
        // First pass with room: learn one thumbnail's size.
        let pipe = make_pipeline(&dir, "");
        let id = pipe.request(&a.to_string_lossy(), None).unwrap();
        pipe.wait(id, 10_000).unwrap();
        let a_thumb = pipe.poll(id).unwrap().thumb_path.unwrap();
        let one_bytes = std::fs::metadata(&a_thumb).unwrap().len();
        pipe.shutdown();

        // Second handle over the SAME cache dir, capped at
        // exactly one thumbnail: generating B must evict A.
        let cache = json_escape(&dir.join("cache").to_string_lossy());
        let cfg = parse_config(&format!(
            "{{\"cache_dir\":\"{cache}\",\"max_cache_bytes\":{one_bytes}}}"
        ))
        .unwrap();
        let pipe2 = Pipeline::create(cfg).unwrap();
        let id = pipe2.request(&b.to_string_lossy(), None).unwrap();
        pipe2.wait(id, 10_000).unwrap();
        let b_thumb = pipe2.poll(id).unwrap().thumb_path.unwrap();
        assert!(Path::new(&b_thumb).exists());
        assert!(!Path::new(&a_thumb).exists(), "LRU entry A was not evicted");
        let stats = json::parse(&pipe2.stats()).unwrap();
        assert_eq!(stats.get("entries").unwrap().as_i64(), Some(1));
        assert!(stats.get("evictions").unwrap().as_i64().unwrap() >= 1);
        pipe2.shutdown();
    }

    #[test]
    fn unsupported_format_is_honest() {
        let dir = temp_dir("unsupported");
        let src = dir.join("photo.png");
        std::fs::write(&src, b"\x89PNG\r\n\x1a\n not really a png").unwrap();
        let pipe = make_pipeline(&dir, "");
        let id = pipe.request(&src.to_string_lossy(), None).unwrap();
        pipe.wait(id, 10_000).unwrap();
        let st = pipe.poll(id).unwrap();
        assert_eq!(st.state, ReqState::Unsupported);
        assert!(st.error.unwrap().contains("no provider registered"));
        assert!(st.thumb_path.is_none());
        pipe.shutdown();
    }

    #[test]
    fn missing_source_fails_not_found() {
        let dir = temp_dir("missing");
        let pipe = make_pipeline(&dir, "");
        let id = pipe
            .request(&dir.join("nope.bmp").to_string_lossy(), None)
            .unwrap();
        pipe.wait(id, 10_000).unwrap();
        let st = pipe.poll(id).unwrap();
        assert_eq!(st.state, ReqState::Failed);
        assert_eq!(st.error_code, ERR_NOT_FOUND);
        pipe.shutdown();
    }

    extern "C" fn slow_bmp(
        src: *const c_char,
        out: *const c_char,
        w: c_int,
        h: c_int,
        _ud: *mut c_void,
    ) -> c_int {
        std::thread::sleep(std::time::Duration::from_millis(400));
        counting_bmp(src, out, w, h, _ud)
    }

    #[test]
    fn queue_cancel_and_queue_full() {
        let dir = temp_dir("cancel");
        let src = dir.join("a.bmp");
        write_bmp_fixture(&src, 4, 4, |_, _| (5, 5, 5));
        let src_s = src.to_string_lossy().to_string();
        // One worker, room for exactly one queued request.
        let pipe = make_pipeline(&dir, r#","worker_count":1,"queue_capacity":1"#);
        pipe.register_provider("bmp", slow_bmp, std::ptr::null_mut())
            .unwrap();

        let running = pipe.request(&src_s, None).unwrap();
        // Wait until it is actually running (queue drained).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pipe.poll(running).unwrap().state != ReqState::Running {
            assert!(
                std::time::Instant::now() < deadline,
                "first request never ran"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let queued = pipe.request(&src_s, Some(r#"{"max_w":2}"#)).unwrap();
        // Queue is now full (capacity 1, one queued).
        assert_eq!(
            pipe.request(&src_s, Some(r#"{"max_w":3}"#)).unwrap_err().0,
            ERR_QUEUE_FULL
        );
        // Cancel the queued one; the running one is not cancellable.
        assert_eq!(pipe.cancel(running).unwrap_err().0, ERR_INVALID_STATE);
        pipe.cancel(queued).unwrap();
        assert_eq!(pipe.poll(queued).unwrap().state, ReqState::Cancelled);
        pipe.wait(queued, 1_000).unwrap(); // cancelled is terminal
        pipe.wait(running, 10_000).unwrap();
        assert_eq!(pipe.poll(running).unwrap().state, ReqState::Done);
        assert_eq!(pipe.cancel(99999).unwrap_err().0, ERR_NOT_FOUND);
        pipe.shutdown();
    }

    #[test]
    fn wait_timeout_leaves_request_alone() {
        let dir = temp_dir("waittimeout");
        let src = dir.join("a.bmp");
        write_bmp_fixture(&src, 4, 4, |_, _| (5, 5, 5));
        let pipe = make_pipeline(&dir, r#","worker_count":1"#);
        pipe.register_provider("bmp", slow_bmp, std::ptr::null_mut())
            .unwrap();
        let id = pipe.request(&src.to_string_lossy(), None).unwrap();
        assert_eq!(pipe.wait(id, 20).unwrap_err().0, ERR_TIMEOUT);
        pipe.wait(id, 10_000).unwrap();
        assert_eq!(pipe.poll(id).unwrap().state, ReqState::Done);
        pipe.shutdown();
    }

    #[test]
    fn oversize_request_fails_with_clear_error() {
        let dir = temp_dir("oversize-req");
        let big = dir.join("big.bmp");
        let mut bytes = vec![0u8; 54];
        bytes[0] = b'B';
        bytes[1] = b'M';
        bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
        bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
        bytes[18..22].copy_from_slice(&20000i32.to_le_bytes());
        bytes[22..26].copy_from_slice(&20000i32.to_le_bytes());
        bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
        bytes[28..30].copy_from_slice(&24u16.to_le_bytes());
        std::fs::write(&big, bytes).unwrap();
        let pipe = make_pipeline(&dir, "");
        let id = pipe.request(&big.to_string_lossy(), None).unwrap();
        pipe.wait(id, 10_000).unwrap();
        let st = pipe.poll(id).unwrap();
        assert_eq!(st.state, ReqState::Failed);
        assert!(st.error.unwrap().contains("exceeds the decoder cap"));
        pipe.shutdown();
    }
}
