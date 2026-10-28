# ThumbKit — Parity

What the brief asked for, and where each piece stands in v0.1.0. Status words: **Done** (implemented and test-covered), **Partial**, **Not done** (with the reason).

| Brief requirement | Status | Notes |
|---|---|---|
| Reusable thumbnail/preview pipeline as a DLL kit (WinDevKit family pattern) | Done | Rust, zero dependencies, `cdylib`+`rlib`+`staticlib`, handle-based C ABI (`th_create`/`th_close`), thread-local last error, caller-owned strings freed by `th_free_string`, JSON in/out with duplicate keys as parse errors. |
| Pluggable provider interface | Done | `th_register_provider(handle, extension_csv, generate_cb, user_data)`; callback signature documented exactly in the header; later registrations override earlier ones per extension; providers run on worker threads without the pipeline lock held. |
| Built-in providers | Done (scoped) | BMP 24/32-bit uncompressed and PPM P6/maxval 255: real decode, nearest-neighbour downscale (never upscale), 24-bit BMP output — all test-covered with hand-written fixtures. |
| No fake thumbnails for undecodable formats | Done | PNG/JPEG/GIF and unsupported variants return state `unsupported` with a message naming the remedy (register a provider). No placeholder or header-only output path exists. A built-in PNG decoder was judged too large for this kit and is roadmap item 1 as a separate provider crate. |
| Async retrieval | Done | `th_request` returns a u64 id; bounded worker pool (default 2, `worker_count` 1–16) and bounded queue (default 64, `TH_ERR_QUEUE_FULL` when full); `th_poll` JSON status, `th_wait` with timeout/`TH_WAIT_INFINITE`, `th_cancel` for queued requests. |
| Bounded cache with invalidation | Done | SHA-256 (hand-rolled, known-vector tested) over canonical path + size + mtime + preset + provider name; `<key>.thumb` + `<key>.json` sidecar per entry; index rebuilt by scanning sidecars; changed sources miss automatically and leave orphans. |
| Eviction and sweeping | Done | LRU by sidecar `last_access`, enforced against `max_cache_bytes` and `max_entries` after each generation and on `th_sweep`, which also removes orphans and temp files and returns a JSON report. One documented exception: the just-generated entry is not evicted by its own pass. |
| Stats | Done | `th_stats` JSON: on-disk `entries`/`bytes` plus per-handle `hits`/`misses`/`evictions`. A second identical request is a proven hit (provider call count stays 1 in tests). |
| Predictable memory limits | Done | Decoders reject >100 MP or >256 MiB decoded RGB before allocating, with the cap named in the error; covered by direct and end-to-end oversize tests. |
| Tests with synthetic fixtures | Done | 22 tests: hand-written BMP/PPM fixtures in temp dirs, downscale/never-upscale, cache hit, invalidation + sweep, LRU eviction, unsupported format, missing source, queue cancel, queue full, wait timeout, oversize rejection, config/preset rules, JSON and SHA-256 suites, FFI end-to-end. |
| Docs | Done | Header documents every export; README (TL;DR, features, C example, build, tests, honest limitations, MIT); AGENTS.md, ROADMAP.md, QUALITY.md, this file; LICENSE copied from the family. |

## Deviations from the brief, stated plainly

- **PNG decoder not built in.** The brief allowed this explicitly ("Too big. Keep BMP+PPM real; plugins cover the rest"); PNG/JPEG/GIF are `unsupported` until a provider registers. Recorded here so the scope is a decision, not an omission.
- **Cancel is queued-only.** The brief asked for `th_cancel` for queued items; running providers are arbitrary host code and are not interrupted. `th_close` joins running work instead.
- **Cache-hit access-time updates rewrite the sidecar on every hit.** Simpler and crash-safe; a high-read workload pays one small file write per hit. Noted as a known cost, not hidden.
