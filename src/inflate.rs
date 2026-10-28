//! DEFLATE (RFC 1951) and zlib (RFC 1950) decompression,
//! written from scratch — the crate is zero-dependency, so
//! PNG's compression arrives here.
//!
//! Scope: stored, fixed-Huffman and dynamic-Huffman
//! blocks. The zlib wrapper's Adler-32 is verified.
//! Every entry point takes a hard output cap so a hostile
//! stream cannot expand without bound; every malformed
//! input produces an `Err(String)`, never a panic.

/// Errors are plain messages; callers wrap them in their
/// own error type with format context.
type IResult<T> = Result<T, String>;

fn err<T>(msg: &str) -> IResult<T> {
    Err(msg.to_string())
}

// ---------------------------------------------------------------------------
// Bit reader (DEFLATE packs bits LSB-first)
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bitbuf: u32,
    bitcnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            bitbuf: 0,
            bitcnt: 0,
        }
    }

    fn bits(&mut self, n: u32) -> IResult<u32> {
        while self.bitcnt < n {
            let byte = match self.data.get(self.pos) {
                Some(b) => *b,
                None => return err("deflate stream is truncated"),
            };
            self.pos += 1;
            self.bitbuf |= (byte as u32) << self.bitcnt;
            self.bitcnt += 8;
        }
        let v = self.bitbuf & ((1u32 << n) - 1);
        self.bitbuf >>= n;
        self.bitcnt -= n;
        Ok(v)
    }

    fn byte(&mut self) -> IResult<u8> {
        Ok(self.bits(8)? as u8)
    }

    fn align_byte(&mut self) {
        let drop = self.bitcnt % 8;
        self.bitbuf >>= drop;
        self.bitcnt -= drop;
    }

    /// Offset of the first unconsumed byte, counting the
    /// whole bytes still buffered in the bit reader back
    /// out. Locates the zlib Adler-32 trailer.
    fn consumed_bytes(&self) -> usize {
        self.pos - (self.bitcnt / 8) as usize
    }
}

// ---------------------------------------------------------------------------
// Canonical Huffman decoding
// ---------------------------------------------------------------------------

const MAX_BITS: usize = 15;

struct Huffman {
    /// counts[len] = number of codes of that length.
    counts: [u16; MAX_BITS + 1],
    /// Symbols sorted by (length, symbol) — canonical order.
    symbols: Vec<u16>,
}

impl Huffman {
    /// Build a decoder from per-symbol code lengths.
    /// Oversubscribed length sets are an error.
    /// Incomplete sets are ACCEPTED — real encoders emit
    /// them (the fixed distance table itself uses only
    /// 30 of its 32 five-bit codes, and zlib's dynamic
    /// tables for small alphabets are routinely
    /// incomplete). A bit pattern that lands in the
    /// unassigned space simply fails to decode, which is
    /// the correct clean error if a stream uses it.
    fn build(lengths: &[u8]) -> IResult<Huffman> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &l in lengths {
            if l as usize > MAX_BITS {
                return err("huffman code length over 15 bits");
            }
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let total: u32 = counts[1..].iter().map(|&c| c as u32).sum();
        if total == 0 {
            return Ok(Huffman {
                counts,
                symbols: Vec::new(),
            });
        }
        let mut left: i32 = 1;
        for &count in counts.iter().take(MAX_BITS + 1).skip(1) {
            left <<= 1;
            left -= count as i32;
            if left < 0 {
                return err("oversubscribed huffman code lengths");
            }
        }
        let mut offs = [0u16; MAX_BITS + 1];
        for len in 1..MAX_BITS {
            offs[len + 1] = offs[len] + counts[len];
        }
        let mut symbols = vec![0u16; total as usize];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    fn decode(&self, br: &mut BitReader) -> IResult<u16> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAX_BITS {
            code |= br.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        err("invalid huffman code in deflate stream")
    }
}

// ---------------------------------------------------------------------------
// DEFLATE proper
// ---------------------------------------------------------------------------

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn fixed_tables() -> IResult<(Huffman, Huffman)> {
    let mut lit_lengths = [0u8; 288];
    for (i, l) in lit_lengths.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let lit = Huffman::build(&lit_lengths)?;
    let dist = Huffman::build(&[5u8; 30])?;
    Ok((lit, dist))
}

fn inflate_block(
    br: &mut BitReader,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
    max_out: usize,
) -> IResult<()> {
    loop {
        let sym = lit.decode(br)?;
        match sym {
            0..=255 => {
                if out.len() >= max_out {
                    return err("deflate output exceeds the size limit");
                }
                out.push(sym as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let idx = (sym - 257) as usize;
                let len = LENGTH_BASE[idx] as usize + br.bits(LENGTH_EXTRA[idx])? as usize;
                let dsym = dist.decode(br)?;
                if dsym as usize >= DIST_BASE.len() {
                    return err("deflate distance code 30/31 is not valid");
                }
                let distance = DIST_BASE[dsym as usize] as usize
                    + br.bits(DIST_EXTRA[dsym as usize])? as usize;
                if distance == 0 || distance > out.len() {
                    return err("deflate distance reaches before the output start");
                }
                if out.len() + len > max_out {
                    return err("deflate output exceeds the size limit");
                }
                let start = out.len() - distance;
                for i in 0..len {
                    let b = out[start + i];
                    out.push(b);
                }
            }
            _ => return err("deflate literal/length code out of range"),
        }
    }
}

fn inflate_blocks(br: &mut BitReader, max_out: usize) -> IResult<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let bfinal = br.bits(1)?;
        match br.bits(2)? {
            0 => {
                // Stored block: skip to a byte boundary,
                // then LEN (u16le), NLEN (one's complement),
                // then LEN raw bytes.
                br.align_byte();
                let len = u16::from_le_bytes([br.byte()?, br.byte()?]) as usize;
                let nlen = u16::from_le_bytes([br.byte()?, br.byte()?]) as usize;
                if len != (!nlen & 0xFFFF) {
                    return err("stored block LEN/NLEN mismatch");
                }
                if out.len() + len > max_out {
                    return err("deflate output exceeds the size limit");
                }
                for _ in 0..len {
                    let b = br.byte()?;
                    out.push(b);
                }
            }
            1 => {
                let (lit, dist) = fixed_tables()?;
                inflate_block(br, &mut out, &lit, &dist, max_out)?;
            }
            2 => {
                let hlit = br.bits(5)? as usize + 257;
                let hdist = br.bits(5)? as usize + 1;
                let hclen = br.bits(4)? as usize + 4;
                const ORDER: [usize; 19] = [
                    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
                ];
                let mut cl_lengths = [0u8; 19];
                for i in 0..hclen {
                    cl_lengths[ORDER[i]] = br.bits(3)? as u8;
                }
                let cl = Huffman::build(&cl_lengths)?;
                let total = hlit + hdist;
                let mut lengths = vec![0u8; total];
                let mut i = 0;
                while i < total {
                    let sym = cl.decode(br)?;
                    match sym {
                        0..=15 => {
                            lengths[i] = sym as u8;
                            i += 1;
                        }
                        16 => {
                            if i == 0 {
                                return err("deflate repeat with no previous length");
                            }
                            let prev = lengths[i - 1];
                            let rep = 3 + br.bits(2)? as usize;
                            if i + rep > total {
                                return err("deflate length repeat overflows the tables");
                            }
                            for _ in 0..rep {
                                lengths[i] = prev;
                                i += 1;
                            }
                        }
                        17 => {
                            let rep = 3 + br.bits(3)? as usize;
                            if i + rep > total {
                                return err("deflate zero repeat overflows the tables");
                            }
                            i += rep;
                        }
                        18 => {
                            let rep = 11 + br.bits(7)? as usize;
                            if i + rep > total {
                                return err("deflate zero repeat overflows the tables");
                            }
                            i += rep;
                        }
                        _ => return err("deflate code-length symbol out of range"),
                    }
                }
                if lengths[256] == 0 {
                    return err("deflate dynamic block has no end-of-block code");
                }
                let lit = Huffman::build(&lengths[..hlit])?;
                let dist = Huffman::build(&lengths[hlit..])?;
                inflate_block(br, &mut out, &lit, &dist, max_out)?;
            }
            _ => return err("deflate block type 3 is reserved"),
        }
        if bfinal == 1 {
            return Ok(out);
        }
    }
}

/// Inflate a raw DEFLATE stream. `max_out` bounds the
/// output; exceeding it is an error, not a truncation.
pub fn inflate(data: &[u8], max_out: usize) -> IResult<Vec<u8>> {
    inflate_blocks(&mut BitReader::new(data), max_out)
}

/// Adler-32 (RFC 1950), used to verify zlib streams.
pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// Decompress a complete zlib stream: two header bytes,
/// a DEFLATE body, and a big-endian Adler-32 of the
/// output, which is verified. `max_out` bounds the
/// output exactly as in `inflate`.
pub fn zlib_decompress(data: &[u8], max_out: usize) -> IResult<Vec<u8>> {
    if data.len() < 2 {
        return err("zlib stream is shorter than its 2-byte header");
    }
    let cmf = data[0];
    let flg = data[1];
    if cmf & 0x0F != 8 {
        return err("zlib stream does not use the deflate method");
    }
    if (cmf as u32 * 256 + flg as u32) % 31 != 0 {
        return err("zlib header check bits are wrong");
    }
    if flg & 0x20 != 0 {
        return err("zlib preset dictionaries are not supported");
    }
    let mut br = BitReader::new(&data[2..]);
    let out = inflate_blocks(&mut br, max_out)?;
    let used = 2 + br.consumed_bytes();
    if used + 4 > data.len() {
        return err("zlib stream is missing its Adler-32 trailer");
    }
    let stored = u32::from_be_bytes([data[used], data[used + 1], data[used + 2], data[used + 3]]);
    if stored != adler32(&out) {
        return err("zlib Adler-32 mismatch (data is corrupt)");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vectors generated with Python's zlib module; the
    // set covers stored, fixed-Huffman and
    // dynamic-Huffman blocks, back-references, an empty
    // input and a full-range binary input.
    include!("inflate_vectors.rs");

    fn roundtrip(zlib: &[u8], orig: &[u8]) {
        let out = zlib_decompress(zlib, orig.len()).expect("decompress failed");
        assert_eq!(out, orig);
        // The cap is enforced: one byte less must refuse.
        if !orig.is_empty() {
            assert!(zlib_decompress(zlib, orig.len() - 1).is_err());
        }
    }

    #[test]
    fn stored_block_vector() {
        roundtrip(STORED_ZLIB, STORED_ORIG);
    }

    #[test]
    fn fixed_huffman_vector() {
        roundtrip(FIXED_ZLIB, FIXED_ORIG);
    }

    #[test]
    fn dynamic_huffman_vector() {
        // The original is rebuilt byte-for-byte here (it
        // is too large to embed); DYNAMIC_ZLIB is the
        // literal zlib stream Python produced from it,
        // and its first block is a dynamic-Huffman block.
        let mut orig = Vec::new();
        for _ in 0..60 {
            orig.extend_from_slice(b"It is a truth universally acknowledged, that a single man in possession of a good fortune, must be in want of a wife. ");
        }
        for _ in 0..4 {
            orig.extend(0u8..=255);
        }
        for _ in 0..3000 {
            orig.extend_from_slice(b"xyz");
        }
        roundtrip(DYNAMIC_ZLIB, &orig);
    }

    #[test]
    fn back_reference_vector() {
        roundtrip(BACKREF_ZLIB, BACKREF_ORIG);
    }

    #[test]
    fn empty_and_binary_vectors() {
        roundtrip(EMPTY_ZLIB, EMPTY_ORIG);
        roundtrip(BINARY_ZLIB, BINARY_ORIG);
    }

    #[test]
    fn adler32_known_value() {
        // Published reference value (the "Wikipedia"
        // example from the Adler-32 literature).
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(b""), 1);
    }

    #[test]
    fn bad_adler_is_rejected() {
        let mut bad = STORED_ZLIB.to_vec();
        let n = bad.len();
        bad[n - 1] ^= 0xFF;
        assert!(zlib_decompress(&bad, 1 << 20).is_err());
    }

    #[test]
    fn bad_headers_are_rejected() {
        assert!(zlib_decompress(&[], 10).is_err());
        assert!(zlib_decompress(&[0x78], 10).is_err());
        // Check bits wrong:
        assert!(zlib_decompress(&[0x78, 0x00, 0x03, 0x00, 0, 0, 0, 1], 10).is_err());
        // Compression method 7 (not deflate):
        assert!(zlib_decompress(&[0x77, 0x09, 0x03, 0x00, 0, 0, 0, 1], 10).is_err());
        // Preset-dictionary flag set (0x78 0xBB has FDICT):
        assert!(zlib_decompress(&[0x78, 0xBB, 0x03, 0x00, 0, 0, 0, 1], 10).is_err());
    }

    #[test]
    fn truncations_never_panic_and_always_error() {
        for vector in [
            STORED_ZLIB,
            FIXED_ZLIB,
            DYNAMIC_ZLIB,
            BACKREF_ZLIB,
            BINARY_ZLIB,
        ] {
            for cut in 0..vector.len() {
                assert!(
                    zlib_decompress(&vector[..cut], 1 << 20).is_err(),
                    "truncation at {cut} must error"
                );
            }
        }
    }

    #[test]
    fn bit_flips_never_panic() {
        // DEFLATE itself has no checksum, but the zlib
        // Adler-32 catches the overwhelming majority of
        // single-bit corruption; every flip must at worst
        // decode differently, never panic.
        let mut caught = 0;
        let mut total = 0;
        for vector in [FIXED_ZLIB, DYNAMIC_ZLIB] {
            for i in 0..vector.len() {
                total += 1;
                let mut bad = vector.to_vec();
                bad[i] ^= 0x40;
                if zlib_decompress(&bad, 1 << 20).is_err() {
                    caught += 1;
                }
            }
        }
        assert!(caught > total / 2, "adler caught only {caught}/{total}");
    }
}
