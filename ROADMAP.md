# ThumbKit Roadmap

## v0.1.0 (current)
The thumbnail pipeline behind a C ABI: a pluggable provider interface (one documented callback per extension set), real built-in BMP (24/32-bit uncompressed) and PPM (P6) decoders with nearest-neighbour downscale and BMP output, an asynchronous bounded worker pool with poll/wait/cancel-queued semantics, and a bounded sidecar-indexed cache — SHA-256 keys over path + size + mtime + preset + provider, LRU eviction against byte and entry bounds, orphan sweeping, and stats. Decoder memory is capped at 100 MP / 256 MiB. Formats without a decoder return `unsupported`; there are no placeholder thumbnails.

## Next, in order
1. **A PNG provider as a separate optional kit**: a real PNG decoder is large enough to deserve its own crate and licence review, shipped as a provider that registers itself with a ThumbKit pipeline — the pipeline interface is designed so the core never has to grow it.
2. **BMP variant widening**: 8-bit palette BMP decoding (a real decoder for the variant, with palette handling), keeping compressed BMP out unless a real RLE decoder lands with it.
3. **Per-provider concurrency limits**: let a registration declare that its provider is single-threaded, so the pool serializes only that provider's calls instead of forcing every provider to be thread-safe.
4. **Cache warm-up scan**: an optional `th_create` flag to sweep orphans at startup, for hosts that want the cleanup paid once at launch rather than on the first explicit `th_sweep`.

## Explicitly not planned
- Placeholder, icon-based, or header-only "thumbnails" for undecodable formats: the `unsupported` answer is the feature, not a gap.
- Cancelling a running provider: impossible to do safely to arbitrary host code; queued-only cancellation is the honest mechanism and it stays.
- A cache database: sidecar files scanned on demand are crash-safe by construction; a database would need to earn its complexity against that.
