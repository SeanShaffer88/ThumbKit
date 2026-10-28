# ThumbKit Roadmap

## v0.2.0 (current)
Built-in PNG and JPEG decoders, written from scratch in the crate (zero dependencies kept): a DEFLATE/zlib inflater (stored, fixed-Huffman and dynamic-Huffman blocks, Adler-32 verified); a PNG decoder covering color types 0/2/3/4/6, bit depths 1/2/4/8/16, all five scanline filters and Adam7 interlacing; a JPEG decoder covering sequential SOF0/SOF1 with Huffman entropy, restart intervals, grayscale and 3-component images at any 1..=4 sampling factors. Progressive JPEG (SOF2) and 4-component CMYK stay `unsupported`, by name. The same 100 MP / 256 MiB decoder cap guards both new formats, checked from the header before any pixel data is decoded. This supersedes the old "PNG as a separate provider kit" plan (see Next item 1): the decoders landed in-core instead, behind the same built-in provider mechanism.

## v0.1.0
The thumbnail pipeline behind a C ABI: a pluggable provider interface (one documented callback per extension set), real built-in BMP (24/32-bit uncompressed) and PPM (P6) decoders with nearest-neighbour downscale and BMP output, an asynchronous bounded worker pool with poll/wait/cancel-queued semantics, and a bounded sidecar-indexed cache — SHA-256 keys over path + size + mtime + preset + provider, LRU eviction against byte and entry bounds, orphan sweeping, and stats. Decoder memory is capped at 100 MP / 256 MiB. Formats without a decoder return `unsupported`; there are no placeholder thumbnails.

## Next, in order
1. **Progressive JPEG (SOF2), if it ever earns its complexity**: spectral-selection / successive-approximation decoding is a second entropy engine; it lands only as a real decoder, never as a "render the first scan and call it done" shortcut. Until then SOF2 stays honestly `unsupported`. (The previous item 1 — a separate optional PNG provider kit — is **superseded**: v0.2.0 built real PNG and JPEG decoders into the core instead, so there is nothing left for that kit to provide.)
2. **BMP variant widening**: 8-bit palette BMP decoding (a real decoder for the variant, with palette handling), keeping compressed BMP out unless a real RLE decoder lands with it.
3. **Per-provider concurrency limits**: let a registration declare that its provider is single-threaded, so the pool serializes only that provider's calls instead of forcing every provider to be thread-safe.
4. **Cache warm-up scan**: an optional `th_create` flag to sweep orphans at startup, for hosts that want the cleanup paid once at launch rather than on the first explicit `th_sweep`.

## Explicitly not planned
- Placeholder, icon-based, or header-only "thumbnails" for undecodable formats: the `unsupported` answer is the feature, not a gap.
- Cancelling a running provider: impossible to do safely to arbitrary host code; queued-only cancellation is the honest mechanism and it stays.
- A cache database: sidecar files scanned on demand are crash-safe by construction; a database would need to earn its complexity against that.
