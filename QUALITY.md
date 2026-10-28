# ThumbKit — Quality

Scope assessed: the whole v0.2.0 crate (core pipeline, built-in BMP/PPM/PNG/JPEG decoders, DEFLATE inflater, cache, SHA-256, JSON engine, C ABI, header, docs) as built and gated locally on Linux. Windows runtime behaviour is **Unverified** until the Windows CI build/link and an on-machine run exist — this file does not claim them.

## Scorecard

| Dimension | Score (0–5) | Evidence |
|---|---|---|
| Correctness | 4 | 53 tests pass, including cache-hit provider call-count, invalidation + orphan sweep, LRU eviction under a one-thumbnail byte cap, and queue cancel/full with a slow provider. The v0.2.0 decoders were cross-checked during development against PIL (byte-exact on every PNG variant tested, incl. 16-bit, sub-8-bit, palette+tRNS and an Adam7 file) and against ffmpeg's MJPEG decoder (within 4 units on every pixel of 4:2:0/4:2:2/4:4:4/grayscale and restart-interval files). |
| Robustness | 4 | Truncated/corrupt fixtures for all four formats fail with clear errors (every truncation of the zlib vectors and of the PNG/JPEG fixtures is required to error, and bit-flip sweeps must never panic); decoder pixel cap is checked from the header before allocation in all decoders; provider panics are caught at the worker and FFI boundaries (`catch_unwind`). |
| Honesty of scope | 5 | Formats without a decoder return `unsupported`; no placeholder output exists anywhere in the crate; the excluded JPEG families (progressive, CMYK, 12-bit, arithmetic) are named in error messages and docs, and a progressive file is test-proven to answer `unsupported`, not a wrong image. |
| Bounds & resources | 4 | Queue capacity, worker count, cache bytes, cache entries, and decoder pixels/bytes are all bounded and tested; the inflater takes a hard output cap so a zip-bomb IDAT cannot expand past the expected scanline size; the single-oversized-entry exception is documented in the header and README. |
| API clarity | 4 | Header documents every export, the provider callback signature, all JSON shapes, states, and error codes; FFI tests exercise create/request/poll/wait/stats/sweep end to end. |
| Documentation | 4 | README has TL;DR, a C example, API table, limitations, and test description; ROADMAP and docs/PARITY.md included. Screenshots do not apply to a DLL. |
| Portability | 3 | Pure `std` logic, no OS APIs, no `#[link]` blocks; host gate green including the windows-msvc check/clippy targets. A real Windows link is CI's job and is Unverified locally. |

Total: 28/35. No dimension below 3; no Fail.

## Gate

`~/workspace/windevkit/tools/gate.sh` — format check, clippy on host and `x86_64-pc-windows-msvc` targets with `-D warnings`, full test suite (never a filtered subset). Result is recorded in the delivery report for this kit; a green gate here is the claim, and CI's real Windows build remains the link gate.

## Known limitations (also in README)

- BMP, PPM, PNG and JPEG (sequential) decode built-in; progressive JPEG, CMYK JPEG and all other formats need a registered provider.
- JPEG chroma is upsampled by nearest neighbour; a "fancy upsampling" decoder can differ by a few units around hard chroma edges (both behaviours decode the same image; the difference is interpolation, measured during development against ffmpeg/PIL).
- Running requests cannot be cancelled; `th_close` waits for a running provider to return.
- Hit/miss/eviction counters are per-handle; cache entries and bytes are scanned from disk.
- Thumbnail/source dimensions report 0 when a custom provider's files are not BMP-readable.
