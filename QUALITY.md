# ThumbKit — Quality

Scope assessed: the whole v0.1.0 crate (core pipeline, built-in BMP/PPM decoders, cache, SHA-256, JSON engine, C ABI, header, docs) as built and gated locally on Linux. Windows runtime behaviour is **Unverified** until the Windows CI build/link and an on-machine run exist — this file does not claim them.

## Scorecard

| Dimension | Score (0–5) | Evidence |
|---|---|---|
| Correctness | 4 | 22 tests pass, including cache-hit provider call-count, invalidation + orphan sweep, LRU eviction under a one-thumbnail byte cap, and queue cancel/full with a slow provider. |
| Robustness | 4 | Truncated/corrupt BMP fixtures fail with clear errors; decoder pixel cap is checked before allocation; provider panics are caught at the worker and FFI boundaries (`catch_unwind`). |
| Honesty of scope | 5 | Formats without a decoder return `unsupported`; no placeholder output exists anywhere in the crate; narrow BMP/PPM variants are named in error messages and docs. |
| Bounds & resources | 4 | Queue capacity, worker count, cache bytes, cache entries, and decoder pixels/bytes are all bounded and tested; the single-oversized-entry exception is documented in the header and README. |
| API clarity | 4 | Header documents every export, the provider callback signature, all JSON shapes, states, and error codes; FFI tests exercise create/request/poll/wait/stats/sweep end to end. |
| Documentation | 4 | README has TL;DR, a C example, API table, limitations, and test description; ROADMAP and docs/PARITY.md included. Screenshots do not apply to a DLL. |
| Portability | 3 | Pure `std` logic, no OS APIs, no `#[link]` blocks; host gate green including the windows-msvc check/clippy targets. A real Windows link is CI's job and is Unverified locally. |

Total: 28/35. No dimension below 3; no Fail.

## Gate

`~/workspace/windevkit/tools/gate.sh` — format check, clippy on host and `x86_64-pc-windows-msvc` targets with `-D warnings`, full test suite (never a filtered subset). Result is recorded in the delivery report for this kit; a green gate here is the claim, and CI's real Windows build remains the link gate.

## Known limitations (also in README)

- Only BMP and PPM decode built-in; PNG/JPEG/GIF need a registered provider.
- Running requests cannot be cancelled; `th_close` waits for a running provider to return.
- Hit/miss/eviction counters are per-handle; cache entries and bytes are scanned from disk.
- Thumbnail/source dimensions report 0 when a custom provider's files are not BMP-readable.
