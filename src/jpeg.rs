//! JPEG decoder, written from scratch (the crate is
//! zero-dependency).
//!
//! Scope (a real decoder for these variants, per the
//! kit's no-placeholder rule):
//! - Sequential DCT-based JPEG: SOF0 (baseline) and
//!   SOF1 (extended sequential), 8-bit samples.
//! - Huffman entropy coding (DHT, multiple tables),
//!   8- and 16-bit quantisation tables (DQT), restart
//!   intervals (DRI), APPn/COM segments skipped.
//! - Grayscale (1 component) and 3-component images:
//!   YCbCr (the JFIF default) and RGB component ids.
//! - Chroma upsampling by nearest neighbour for any
//!   sampling factors 1..=4 per axis — which covers
//!   4:4:4 (1x1), 4:2:2 (2x1), 4:4:0 (1x2) and 4:2:0
//!   (2x2). The IDCT is a straightforward separable
//!   float IDCT; decoded values land within a couple of
//!   units of a reference decoder.
//!
//! Explicitly NOT supported, each with a clean
//! `unsupported` error (never a wrong image):
//! - Progressive JPEG (SOF2) and the other process
//!   markers (lossless, differential, arithmetic).
//! - 4-component images (CMYK/YCCK).
//! - 12-bit samples.
//!
//! The decoder memory cap (100 MP / 256 MiB) is checked
//! from the SOF header before any scan data is decoded.
//! Malformed input is a clean error, never a panic.

use crate::{check_pixel_cap, DecodedImage, ThResult, ERR_INVALID_ARG, ERR_NOT_SUPPORTED};

fn err<T>(msg: &str) -> ThResult<T> {
    Err((ERR_INVALID_ARG, format!("invalid JPEG: {msg}")))
}

fn unsupported<T>(msg: String) -> ThResult<T> {
    Err((ERR_NOT_SUPPORTED, msg))
}

fn u16be(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

/// Zigzag scan order: index in zigzag order -> index in
/// natural (row-major) order.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

// ---------------------------------------------------------------------------
// Huffman tables (JPEG flavour: lengths then symbols)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Huff {
    counts: [u16; 16],
    symbols: Vec<u8>,
    mincode: [i32; 17],
    valptr: [usize; 17],
    has: [bool; 17],
}

impl Huff {
    fn build(counts: [u16; 16], symbols: Vec<u8>) -> ThResult<Huff> {
        let total: usize = counts.iter().map(|&c| c as usize).sum();
        if total != symbols.len() || total == 0 || total > 256 {
            return err("DHT symbol count does not match its length counts");
        }
        // A canonical code must not be oversubscribed.
        let mut left: i32 = 1;
        for len in 1..=16 {
            left <<= 1;
            left -= counts[len - 1] as i32;
            if left < 0 {
                return err("DHT table is oversubscribed");
            }
        }
        let mut mincode = [0i32; 17];
        let mut valptr = [0usize; 17];
        let mut has = [false; 17];
        let mut code: i32 = 0;
        let mut idx: usize = 0;
        for (i, &count) in counts.iter().enumerate() {
            let len = i + 1;
            if count > 0 {
                has[len] = true;
                valptr[len] = idx;
                mincode[len] = code;
                idx += count as usize;
                code += count as i32;
            }
            code <<= 1;
        }
        Ok(Huff {
            counts,
            symbols,
            mincode,
            valptr,
            has,
        })
    }

    fn decode(&self, r: &mut ScanReader) -> ThResult<u8> {
        let mut code: i32 = r.bit()? as i32;
        for len in 1..=16usize {
            if self.has[len] {
                let rel = code - self.mincode[len];
                if rel >= 0 && rel < self.counts[len - 1] as i32 {
                    return Ok(self.symbols[self.valptr[len] + rel as usize]);
                }
            }
            code = (code << 1) | r.bit()? as i32;
        }
        err("invalid Huffman code in JPEG scan data")
    }
}

// ---------------------------------------------------------------------------
// Entropy bit reader over the scan data
// ---------------------------------------------------------------------------

/// Reads the entropy-coded segment. A 0xFF byte in the
/// data is stuffed as 0xFF 0x00; a 0xFF followed by
/// anything else is the next marker, which the reader
/// records instead of consuming as data.
struct ScanReader<'a> {
    data: &'a [u8],
    pos: usize,
    bitbuf: u32,
    bitcnt: u32,
    /// Marker code (the byte after 0xFF) seen while
    /// fetching; `pos` then indexes that code byte.
    marker: Option<u8>,
}

impl<'a> ScanReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        ScanReader {
            data,
            pos,
            bitbuf: 0,
            bitcnt: 0,
            marker: None,
        }
    }

    fn fetch(&mut self) -> Option<u8> {
        if self.marker.is_some() {
            return None;
        }
        let b = *self.data.get(self.pos)?;
        if b != 0xFF {
            self.pos += 1;
            return Some(b);
        }
        match self.data.get(self.pos + 1) {
            Some(0x00) => {
                self.pos += 2;
                Some(0xFF)
            }
            Some(&m) => {
                self.marker = Some(m);
                self.pos += 1; // now indexes the marker code
                None
            }
            None => {
                self.pos += 1;
                None
            }
        }
    }

    fn bits(&mut self, n: u32) -> ThResult<u32> {
        while self.bitcnt < n {
            match self.fetch() {
                Some(b) => {
                    self.bitbuf = (self.bitbuf << 8) | b as u32;
                    self.bitcnt += 8;
                }
                None => return err("scan data ends mid-value (truncated or hit a marker)"),
            }
        }
        let shift = self.bitcnt - n;
        let v = if n == 0 {
            0
        } else {
            (self.bitbuf >> shift) & ((1u32 << n) - 1)
        };
        self.bitcnt -= n;
        self.bitbuf &= (1u32 << self.bitcnt) - 1;
        Ok(v)
    }

    fn bit(&mut self) -> ThResult<u32> {
        self.bits(1)
    }

    /// Receive `s` bits and sign-extend them the JPEG way
    /// (values below the halfway mark are negative).
    fn receive_extend(&mut self, s: u8) -> ThResult<i32> {
        if s == 0 {
            return Ok(0);
        }
        let v = self.bits(s as u32)? as i32;
        if v < (1 << (s - 1)) {
            Ok(v - ((1 << s) - 1))
        } else {
            Ok(v)
        }
    }

    /// Byte-align after a restart interval and consume
    /// the restart marker. The marker may already have
    /// been recorded by `fetch`, or may still sit in the
    /// stream (possibly after 0xFF fill bytes).
    fn restart(&mut self) -> ThResult<()> {
        self.bitbuf = 0;
        self.bitcnt = 0;
        if let Some(m) = self.marker.take() {
            if !(0xD0..=0xD7).contains(&m) {
                return err("expected a restart marker in the scan data");
            }
            self.pos += 1; // consume the marker code byte
            return Ok(());
        }
        while self.data.get(self.pos) == Some(&0xFF) && self.data.get(self.pos + 1) == Some(&0xFF) {
            self.pos += 1;
        }
        if self.data.get(self.pos) == Some(&0xFF) {
            if let Some(&m) = self.data.get(self.pos + 1) {
                if (0xD0..=0xD7).contains(&m) {
                    self.pos += 2;
                    return Ok(());
                }
            }
        }
        err("expected a restart marker in the scan data")
    }

    /// Where the outer segment loop should resume: at
    /// the 0xFF of the marker that ended the scan.
    fn resume_pos(&self) -> usize {
        if self.marker.is_some() {
            self.pos - 1
        } else {
            self.pos
        }
    }
}

// ---------------------------------------------------------------------------
// Frame state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Component {
    id: u8,
    h: usize,
    v: usize,
    qtable: usize,
    // Plane geometry (samples, not blocks) and samples.
    width: usize,
    height: usize,
    plane: Vec<i32>,
    scanned: bool,
}

struct Decoder {
    quant: [Option<[i32; 64]>; 16],
    huff_dc: [Option<Huff>; 16],
    huff_ac: [Option<Huff>; 16],
    restart_interval: usize,
    width: u32,
    height: u32,
    components: Vec<Component>,
    hmax: usize,
    vmax: usize,
    app14_transform: Option<u8>,
    cos: [[f64; 8]; 8],
}

impl Decoder {
    fn new() -> Decoder {
        // IDCT cosine factors, computed once per decode:
        // cos_t[x][u] = cos((2x+1) * u * pi / 16).
        let mut cos = [[0.0f64; 8]; 8];
        for (x, row) in cos.iter_mut().enumerate() {
            for (u, v) in row.iter_mut().enumerate() {
                *v = ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos();
            }
        }
        Decoder {
            quant: Default::default(),
            huff_dc: Default::default(),
            huff_ac: Default::default(),
            restart_interval: 0,
            width: 0,
            height: 0,
            components: Vec::new(),
            hmax: 0,
            vmax: 0,
            app14_transform: None,
            cos,
        }
    }

    fn parse_dqt(&mut self, mut seg: &[u8]) -> ThResult<()> {
        while !seg.is_empty() {
            let pq = seg[0] >> 4;
            let tq = (seg[0] & 0x0F) as usize;
            seg = &seg[1..];
            let mut table = [0i32; 64];
            match pq {
                0 => {
                    if seg.len() < 64 {
                        return err("DQT segment is truncated");
                    }
                    for (i, t) in table.iter_mut().enumerate() {
                        *t = seg[i] as i32;
                    }
                    seg = &seg[64..];
                }
                1 => {
                    if seg.len() < 128 {
                        return err("DQT segment is truncated");
                    }
                    for (i, t) in table.iter_mut().enumerate() {
                        *t = u16be(seg, i * 2) as i32;
                    }
                    seg = &seg[128..];
                }
                _ => return err("DQT precision must be 0 (8-bit) or 1 (16-bit)"),
            }
            self.quant[tq] = Some(table);
        }
        Ok(())
    }

    fn parse_dht(&mut self, mut seg: &[u8]) -> ThResult<()> {
        while !seg.is_empty() {
            if seg.len() < 17 {
                return err("DHT segment is truncated");
            }
            let tc = seg[0] >> 4;
            let th = (seg[0] & 0x0F) as usize;
            let mut counts = [0u16; 16];
            let mut total = 0usize;
            for i in 0..16 {
                counts[i] = seg[1 + i] as u16;
                total += counts[i] as usize;
            }
            seg = &seg[17..];
            if seg.len() < total {
                return err("DHT segment is truncated");
            }
            let table = Huff::build(counts, seg[..total].to_vec())?;
            seg = &seg[total..];
            match tc {
                0 => self.huff_dc[th] = Some(table),
                1 => self.huff_ac[th] = Some(table),
                _ => return err("DHT table class must be 0 (DC) or 1 (AC)"),
            }
        }
        Ok(())
    }

    fn parse_sof(&mut self, seg: &[u8]) -> ThResult<()> {
        if !self.components.is_empty() {
            return err("file has more than one SOF marker");
        }
        if seg.len() < 6 {
            return err("SOF segment is truncated");
        }
        let precision = seg[0];
        if precision != 8 {
            return unsupported(format!(
                "unsupported JPEG sample precision {precision} (only 8-bit is supported)"
            ));
        }
        self.height = u16be(seg, 1) as u32;
        self.width = u16be(seg, 3) as u32;
        if self.width == 0 || self.height == 0 {
            return err("zero image dimension in SOF");
        }
        check_pixel_cap(self.width, self.height, "JPEG")?;
        let ncomp = seg[5] as usize;
        if ncomp == 4 {
            return unsupported(
                "unsupported JPEG: 4-component images (CMYK/YCCK) are not supported".to_string(),
            );
        }
        if ncomp != 1 && ncomp != 3 {
            return unsupported(format!(
                "unsupported JPEG: {ncomp} components (only 1 and 3 are supported)"
            ));
        }
        if seg.len() < 6 + 3 * ncomp {
            return err("SOF segment is truncated");
        }
        let mut components = Vec::new();
        let (mut hmax, mut vmax) = (1usize, 1usize);
        for i in 0..ncomp {
            let b = &seg[6 + 3 * i..];
            let (h, v) = ((b[1] >> 4) as usize, (b[1] & 0x0F) as usize);
            if h == 0 || h > 4 || v == 0 || v > 4 {
                return unsupported(format!(
                    "unsupported JPEG sampling factor {h}x{v} (only 1..=4 per axis is supported)"
                ));
            }
            hmax = hmax.max(h);
            vmax = vmax.max(v);
            components.push(Component {
                id: b[0],
                h,
                v,
                qtable: b[2] as usize,
                width: 0,
                height: 0,
                plane: Vec::new(),
                scanned: false,
            });
        }
        let total_blocks: usize = components.iter().map(|c| c.h * c.v).sum();
        if total_blocks > 10 {
            return unsupported(format!(
                "unsupported JPEG: {total_blocks} blocks per MCU exceeds the baseline limit of 10"
            ));
        }
        for c in components.iter_mut() {
            c.width = (self.width as usize * c.h).div_ceil(hmax);
            c.height = (self.height as usize * c.v).div_ceil(vmax);
            c.plane = vec![0i32; c.width * c.height];
        }
        self.components = components;
        self.hmax = hmax;
        self.vmax = vmax;
        Ok(())
    }

    /// Decode one 8x8 block into `out` (natural order,
    /// level-shifted, clamped 0..=255 samples).
    fn decode_block(
        &self,
        r: &mut ScanReader,
        comp: &Component,
        dc: &Huff,
        ac: &Huff,
        pred: &mut i32,
        out: &mut [i32; 64],
    ) -> ThResult<()> {
        let quant = self.quant[comp.qtable].ok_or((
            ERR_INVALID_ARG,
            "invalid JPEG: scan uses an undefined quantisation table".to_string(),
        ))?;
        let mut coef = [0i32; 64];
        let s = dc.decode(r)?;
        if s > 15 {
            return err("DC coefficient size over 15 bits");
        }
        let diff = if s == 0 { 0 } else { r.receive_extend(s)? };
        *pred += diff;
        coef[0] = *pred * quant[0];
        let mut k = 1usize;
        while k < 64 {
            let rs = ac.decode(r)?;
            let run = (rs >> 4) as usize;
            let size = rs & 0x0F;
            if size == 0 {
                if run == 15 {
                    k += 16;
                    continue;
                }
                break; // EOB
            }
            if size > 10 {
                return err("AC coefficient size over 10 bits");
            }
            k += run;
            if k > 63 {
                return err("AC run-length overruns the block");
            }
            let v = r.receive_extend(size)?;
            coef[ZIGZAG[k]] = v * quant[k];
            k += 1;
        }
        self.idct(&coef, out);
        Ok(())
    }

    /// Separable float IDCT (the JFIF form), with the
    /// +128 level shift and 0..=255 clamp applied.
    fn idct(&self, coef: &[i32; 64], out: &mut [i32; 64]) {
        let alpha = |u: usize| {
            if u == 0 {
                std::f64::consts::FRAC_1_SQRT_2
            } else {
                1.0
            }
        };
        let mut tmp = [0f64; 64];
        for y in 0..8 {
            for x in 0..8 {
                let mut s = 0.0f64;
                for u in 0..8 {
                    s += alpha(u) * coef[y * 8 + u] as f64 * self.cos[x][u];
                }
                tmp[y * 8 + x] = s * 0.5;
            }
        }
        for y in 0..8 {
            for x in 0..8 {
                let mut s = 0.0f64;
                for v in 0..8 {
                    s += alpha(v) * tmp[v * 8 + x] * self.cos[y][v];
                }
                let val = (s * 0.5 + 128.0).round();
                out[y * 8 + x] = val.clamp(0.0, 255.0) as i32;
            }
        }
    }

    fn place_block(comp: &mut Component, block_x: usize, block_y: usize, px: &[i32; 64]) {
        let (ox, oy) = (block_x * 8, block_y * 8);
        for y in 0..8 {
            let py = oy + y;
            if py >= comp.height {
                break;
            }
            for x in 0..8 {
                let px_x = ox + x;
                if px_x >= comp.width {
                    break;
                }
                comp.plane[py * comp.width + px_x] = px[y * 8 + x];
            }
        }
    }

    /// Decode one scan (the entropy data after an SOS
    /// header). Returns the position where the segment
    /// scan should resume.
    fn decode_scan(&mut self, bytes: &[u8], seg: &[u8], data_start: usize) -> ThResult<usize> {
        if self.components.is_empty() {
            return err("SOS appears before any SOF");
        }
        if seg.is_empty() {
            return err("SOS segment is truncated");
        }
        let ns = seg[0] as usize;
        if ns == 0 || ns > self.components.len() {
            return err("SOS component count does not match the frame");
        }
        if seg.len() < 1 + 2 * ns + 3 {
            return err("SOS segment is truncated");
        }
        // (component index, dc table, ac table) in scan order.
        let mut scan: Vec<(usize, usize, usize)> = Vec::new();
        for i in 0..ns {
            let id = seg[1 + 2 * i];
            let tt = seg[2 + 2 * i];
            let ci = self.components.iter().position(|c| c.id == id).ok_or((
                ERR_INVALID_ARG,
                format!("invalid JPEG: SOS names unknown component id {id}"),
            ))?;
            scan.push((ci, (tt >> 4) as usize, (tt & 0x0F) as usize));
        }
        for &(ci, _, _) in &scan {
            if self.quant[self.components[ci].qtable].is_none() {
                return err("SOS component uses an undefined quantisation table");
            }
        }
        if ns > 1 && ns != self.components.len() {
            return unsupported(format!(
                "unsupported JPEG: scan interleaves {ns} of {} components (partial interleaving)",
                self.components.len()
            ));
        }
        // Resolve (and clone out of the table slots) the
        // Huffman tables once per scan.
        let mut tables: Vec<(usize, Huff, Huff)> = Vec::new();
        for &(ci, td, ta) in &scan {
            let dc = self.huff_dc[td].clone().ok_or((
                ERR_INVALID_ARG,
                "invalid JPEG: scan uses an undefined DC table".to_string(),
            ))?;
            let ac = self.huff_ac[ta].clone().ok_or((
                ERR_INVALID_ARG,
                "invalid JPEG: scan uses an undefined AC table".to_string(),
            ))?;
            tables.push((ci, dc, ac));
        }
        let mut reader = ScanReader::new(bytes, data_start);
        let mut block = [0i32; 64];
        let ri = self.restart_interval;
        if ns == self.components.len() && ns > 1 {
            // Interleaved scan: MCUs in raster order.
            let mcus_x = (self.width as usize).div_ceil(8 * self.hmax);
            let mcus_y = (self.height as usize).div_ceil(8 * self.vmax);
            let mut preds = vec![0i32; self.components.len()];
            let mut mcu = 0usize;
            for my in 0..mcus_y {
                for mx in 0..mcus_x {
                    if ri > 0 && mcu > 0 && mcu % ri == 0 {
                        reader.restart()?;
                        preds.iter_mut().for_each(|p| *p = 0);
                    }
                    for (ci, dc, ac) in &tables {
                        let ci = *ci;
                        let (h, v) = (self.components[ci].h, self.components[ci].v);
                        for by in 0..v {
                            for bx in 0..h {
                                self.decode_block(
                                    &mut reader,
                                    &self.components[ci],
                                    dc,
                                    ac,
                                    &mut preds[ci],
                                    &mut block,
                                )?;
                                let comp = &mut self.components[ci];
                                Self::place_block(comp, mx * h + bx, my * v + by, &block);
                            }
                        }
                    }
                    mcu += 1;
                }
            }
        } else {
            // Non-interleaved scan (one component, or a
            // one-component frame): blocks in raster order
            // over the component's own block grid.
            for (ci, dc, ac) in &tables {
                let ci = *ci;
                let bw = self.components[ci].width.div_ceil(8);
                let bh = self.components[ci].height.div_ceil(8);
                let mut pred = 0i32;
                let mut n = 0usize;
                for by in 0..bh {
                    for bx in 0..bw {
                        if ri > 0 && n > 0 && n % ri == 0 {
                            reader.restart()?;
                            pred = 0;
                        }
                        self.decode_block(
                            &mut reader,
                            &self.components[ci],
                            dc,
                            ac,
                            &mut pred,
                            &mut block,
                        )?;
                        let comp = &mut self.components[ci];
                        Self::place_block(comp, bx, by, &block);
                        n += 1;
                    }
                }
            }
        }
        for &(ci, _, _) in &scan {
            self.components[ci].scanned = true;
        }
        Ok(reader.resume_pos())
    }

    fn to_image(&self) -> ThResult<DecodedImage> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut rgb = vec![0u8; w * h * 3];
        let ids: Vec<u8> = self.components.iter().map(|c| c.id).collect();
        let rgb_ids = ids.len() == 3 && ids[0] == b'R' && ids[1] == b'G' && ids[2] == b'B';
        let as_rgb = rgb_ids || (self.app14_transform == Some(0) && ids != [1, 2, 3]);
        for y in 0..h {
            for x in 0..w {
                let mut s = [0i32; 3];
                for (i, c) in self.components.iter().enumerate() {
                    let sx = ((x * c.h) / self.hmax).min(c.width - 1);
                    let sy = ((y * c.v) / self.vmax).min(c.height - 1);
                    s[i] = c.plane[sy * c.width + sx];
                }
                let d = (y * w + x) * 3;
                if self.components.len() == 1 {
                    rgb[d] = s[0] as u8;
                    rgb[d + 1] = s[0] as u8;
                    rgb[d + 2] = s[0] as u8;
                } else if as_rgb {
                    rgb[d] = s[0] as u8;
                    rgb[d + 1] = s[1] as u8;
                    rgb[d + 2] = s[2] as u8;
                } else {
                    let (yy, cb, cr) = (s[0] as f64, s[1] as f64 - 128.0, s[2] as f64 - 128.0);
                    rgb[d] = (yy + 1.402 * cr).round().clamp(0.0, 255.0) as u8;
                    rgb[d + 1] = (yy - 0.34414 * cb - 0.71414 * cr).round().clamp(0.0, 255.0) as u8;
                    rgb[d + 2] = (yy + 1.772 * cb).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        Ok(DecodedImage {
            width: self.width,
            height: self.height,
            rgb,
        })
    }
}

/// Find the next marker at or after `pos`: returns
/// (marker code, position just after the marker code).
/// Fill 0xFF bytes before a marker are skipped, per spec.
fn next_marker(bytes: &[u8], mut pos: usize) -> ThResult<(u8, usize)> {
    loop {
        if pos + 1 >= bytes.len() {
            return err("ran out of data looking for a marker");
        }
        if bytes[pos] != 0xFF {
            return err("expected a marker (0xFF) between segments");
        }
        let m = bytes[pos + 1];
        if m == 0xFF {
            pos += 1;
            continue;
        }
        if m == 0x00 {
            return err("found stuffed 0xFF00 outside scan data");
        }
        return Ok((m, pos + 2));
    }
}

/// Decode a JPEG file to the kit's RGB image form.
pub fn decode_jpeg(bytes: &[u8]) -> ThResult<DecodedImage> {
    if bytes.len() < 2 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return err("missing the SOI marker (FF D8)");
    }
    let mut dec = Decoder::new();
    let mut pos = 2usize;
    while pos < bytes.len() {
        let (marker, after) = next_marker(bytes, pos)?;
        match marker {
            0xD9 => break, // EOI: done (a missing EOI after complete scans is tolerated below)
            0xD8 => return err("duplicate SOI marker"),
            0x01 | 0xD0..=0xD7 => {
                pos = after; // standalone markers carry no payload
            }
            _ => {
                if after + 2 > bytes.len() {
                    return err("segment length field is truncated");
                }
                let len = u16be(bytes, after) as usize;
                if len < 2 || after + len > bytes.len() {
                    return err("segment length runs past the end of the file");
                }
                let seg = &bytes[after + 2..after + len];
                match marker {
                    0xDB => dec.parse_dqt(seg)?,
                    0xC4 => dec.parse_dht(seg)?,
                    0xDD => {
                        if seg.len() != 2 {
                            return err("DRI segment must be exactly 2 bytes");
                        }
                        dec.restart_interval = u16be(seg, 0) as usize;
                    }
                    0xC0 | 0xC1 => dec.parse_sof(seg)?,
                    0xC2 => {
                        return unsupported(
                            "unsupported JPEG: progressive (SOF2) files are not supported; baseline/sequential only"
                                .to_string(),
                        )
                    }
                    0xC3 | 0xC5 | 0xC6 | 0xC7 | 0xC9 | 0xCA | 0xCB | 0xCD | 0xCE | 0xCF => {
                        return unsupported(format!(
                            "unsupported JPEG: SOF marker 0xFF{marker:02X} (lossless/differential process) is not supported"
                        ))
                    }
                    0xCC => {
                        return unsupported(
                            "unsupported JPEG: arithmetic coding (DAC) is not supported".to_string(),
                        )
                    }
                    0xEE => {
                        // APP14 (Adobe): byte 11 is the
                        // colour transform when present.
                        if seg.len() >= 12 {
                            dec.app14_transform = Some(seg[11]);
                        }
                    }
                    0xDA => {
                        pos = dec.decode_scan(bytes, seg, after + len)?;
                        continue;
                    }
                    _ => {} // APPn / COM / DNL / ...: skipped
                }
                pos = after + len;
            }
        }
    }
    if dec.components.is_empty() {
        return err("no SOF marker found");
    }
    if !dec.components.iter().all(|c| c.scanned) {
        return err("file ended before every component was scanned");
    }
    dec.to_image()
}

/// Header-only JPEG dimensions: walk the markers to the
/// first SOF and report its frame size.
pub fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 2 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut pos = 2usize;
    while pos + 1 < bytes.len() {
        if bytes[pos] != 0xFF {
            return None;
        }
        let marker = bytes[pos + 1];
        if marker == 0xFF {
            pos += 1;
            continue;
        }
        pos += 2;
        match marker {
            0xD9 => return None,
            0x01 | 0xD0..=0xD8 => {}
            0xC0..=0xCF => {
                if marker == 0xC4 || marker == 0xC8 || marker == 0xCC {
                    // Not SOF markers: they carry lengths.
                    if pos + 2 > bytes.len() {
                        return None;
                    }
                    let len = u16be(bytes, pos) as usize;
                    if len < 2 {
                        return None;
                    }
                    pos += len;
                    continue;
                }
                if pos + 7 > bytes.len() {
                    return None;
                }
                let len = u16be(bytes, pos) as usize;
                if len < 7 {
                    return None;
                }
                let h = u16be(bytes, pos + 3) as u32;
                let w = u16be(bytes, pos + 5) as u32;
                if w == 0 || h == 0 {
                    return None;
                }
                return Some((w, h));
            }
            _ => {
                if pos + 2 > bytes.len() {
                    return None;
                }
                let len = u16be(bytes, pos) as usize;
                if len < 2 || pos + len > bytes.len() {
                    return None;
                }
                if marker == 0xDA {
                    return None; // scan data before any SOF
                }
                pos += len;
            }
        }
    }
    None
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

    fn at(img: &DecodedImage, x: u32, y: u32) -> [i32; 3] {
        let d = (y as usize * img.width as usize + x as usize) * 3;
        [
            img.rgb[d] as i32,
            img.rgb[d + 1] as i32,
            img.rgb[d + 2] as i32,
        ]
    }

    fn close(got: [i32; 3], want: [i32; 3], tol: i32, what: &str) {
        for i in 0..3 {
            assert!(
                (got[i] - want[i]).abs() <= tol,
                "{what}: channel {i}: got {}, want {} (tol {tol})",
                got[i],
                want[i]
            );
        }
    }

    #[test]
    fn fixture_420_decodes_quadrants() {
        let img = decode_jpeg(&fixture("jpg_420.jpg")).unwrap();
        assert_eq!((img.width, img.height), (16, 16));
        // Quadrant centres, well away from the DCT edges.
        close(at(&img, 4, 4), [255, 0, 0], 8, "red quadrant");
        close(at(&img, 12, 4), [0, 255, 0], 8, "green quadrant");
        close(at(&img, 4, 12), [0, 0, 255], 8, "blue quadrant");
        close(at(&img, 12, 12), [255, 255, 255], 8, "white quadrant");
    }

    #[test]
    fn fixture_444_decodes_halves() {
        let img = decode_jpeg(&fixture("jpg_444.jpg")).unwrap();
        assert_eq!((img.width, img.height), (8, 8));
        close(at(&img, 2, 4), [200, 30, 60], 8, "left half");
        close(at(&img, 6, 4), [20, 180, 90], 8, "right half");
    }

    #[test]
    fn fixture_grayscale_decodes_gradient() {
        let img = decode_jpeg(&fixture("jpg_gray.jpg")).unwrap();
        assert_eq!((img.width, img.height), (8, 8));
        // Every row is the gradient x*36; sample the
        // middle row at each column (block-centre-ish).
        for x in 0..8u32 {
            let v = (x * 36) as i32;
            close(at(&img, x, 4), [v, v, v], 8, "gradient");
        }
    }

    // The 8x8 grayscale fixture, embedded as bytes so
    // core decode coverage needs no fixture file.
    const EMBEDDED_JPEG: &[u8] = &[
        0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x03, 0x02, 0x02, 0x02, 0x02,
        0x02, 0x03, 0x02, 0x02, 0x02, 0x03, 0x03, 0x03, 0x03, 0x04, 0x06, 0x04, 0x04, 0x04, 0x04,
        0x04, 0x08, 0x06, 0x06, 0x05, 0x06, 0x09, 0x08, 0x0a, 0x0a, 0x09, 0x08, 0x09, 0x09, 0x0a,
        0x0c, 0x0f, 0x0c, 0x0a, 0x0b, 0x0e, 0x0b, 0x09, 0x09, 0x0d, 0x11, 0x0d, 0x0e, 0x0f, 0x10,
        0x10, 0x11, 0x10, 0x0a, 0x0c, 0x12, 0x13, 0x12, 0x10, 0x13, 0x0f, 0x10, 0x10, 0x10, 0xff,
        0xc0, 0x00, 0x0b, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01, 0x01, 0x11, 0x00, 0xff, 0xc4, 0x00,
        0x1f, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
        0xff, 0xc4, 0x00, 0xb5, 0x10, 0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05,
        0x04, 0x04, 0x00, 0x00, 0x01, 0x7d, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21,
        0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08,
        0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a,
        0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37,
        0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56,
        0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75,
        0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93,
        0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9,
        0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6,
        0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
        0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7,
        0xf8, 0xf9, 0xfa, 0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00, 0x8b, 0xfe,
        0x09, 0x6f, 0xff, 0x00, 0x30, 0x8f, 0xf8, 0x07, 0xf4, 0xaf, 0xff, 0xd9,
    ];

    #[test]
    fn embedded_jpeg_decodes() {
        let img = decode_jpeg(EMBEDDED_JPEG).unwrap();
        assert_eq!((img.width, img.height), (8, 8));
        for x in 0..8u32 {
            let v = (x * 36) as i32;
            close(at(&img, x, 4), [v, v, v], 8, "embedded gradient");
        }
    }

    #[test]
    fn fixture_restart_intervals_decode() {
        // jpg_dri.jpg: 48x36, 4:2:0, restart marker every
        // 2 MCUs (generated with PIL's
        // restart_marker_blocks=2). The expected pixels
        // are ffmpeg's decoding of the same file: the two
        // independent decoders agreed within 4 units on
        // every pixel of this file during development, so
        // ±6 against ffmpeg samples is a tight check that
        // the restart machinery (byte alignment, DC
        // prediction reset) is right.
        let img = decode_jpeg(&fixture("jpg_dri.jpg")).unwrap();
        assert_eq!((img.width, img.height), (48, 36));
        let samples: [((u32, u32), [i32; 3]); 6] = [
            ((0, 0), [139, 241, 3]),
            ((10, 10), [132, 12, 131]),
            ((24, 18), [237, 209, 194]),
            ((47, 35), [255, 49, 227]),
            ((30, 5), [162, 25, 187]),
            ((5, 30), [250, 198, 248]),
        ];
        for ((x, y), want) in samples {
            close(at(&img, x, y), want, 6, "restart-interval pixel");
        }
    }

    #[test]
    fn dimensions_from_markers() {
        assert_eq!(jpeg_dimensions(&fixture("jpg_420.jpg")), Some((16, 16)));
        assert_eq!(jpeg_dimensions(&fixture("jpg_gray.jpg")), Some((8, 8)));
        assert_eq!(jpeg_dimensions(b"\xff\xd8 garbage"), None);
        assert_eq!(jpeg_dimensions(&[]), None);
    }

    #[test]
    fn truncations_never_panic() {
        for name in ["jpg_420.jpg", "jpg_444.jpg", "jpg_gray.jpg"] {
            let good = fixture(name);
            for cut in 0..good.len() {
                let _ = decode_jpeg(&good[..cut]); // must not panic; may Err
            }
            // Sanity: the full file still decodes.
            assert!(decode_jpeg(&good).is_ok());
        }
        assert!(decode_jpeg(&[]).is_err());
        assert!(decode_jpeg(b"\xff\xd8").is_err());
        assert!(decode_jpeg(b"not a jpeg at all").is_err());
    }

    #[test]
    fn progressive_is_unsupported_not_wrong() {
        // Take the grayscale fixture and rewrite its SOF0
        // marker (FF C0) to SOF2 (FF C2): the decoder
        // must answer `unsupported`, not decode garbage.
        let mut bytes = fixture("jpg_gray.jpg");
        let pos = bytes
            .windows(2)
            .position(|w| w == [0xFF, 0xC0])
            .expect("fixture has an SOF0");
        bytes[pos + 1] = 0xC2;
        let (code, msg) = decode_jpeg(&bytes).unwrap_err();
        assert_eq!(code, ERR_NOT_SUPPORTED);
        assert!(msg.contains("progressive"), "{msg}");
    }

    #[test]
    fn oversize_header_is_rejected_before_decode() {
        // Rewrite the SOF dimensions of the gray fixture
        // to 20000x20000 (400 MP): the cap error must win
        // over any later truncation problem.
        let mut bytes = fixture("jpg_gray.jpg");
        let pos = bytes
            .windows(2)
            .position(|w| w == [0xFF, 0xC0])
            .expect("fixture has an SOF0");
        // SOF payload: len(2) precision(1) height(2) width(2)
        bytes[pos + 5] = 0x4E;
        bytes[pos + 6] = 0x20; // height 20000
        bytes[pos + 7] = 0x4E;
        bytes[pos + 8] = 0x20; // width 20000
        let (code, msg) = decode_jpeg(&bytes).unwrap_err();
        assert_eq!(code, ERR_INVALID_ARG);
        assert!(msg.contains("exceeds the decoder cap"), "{msg}");
    }
}
