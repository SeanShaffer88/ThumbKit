# ThumbKit

## TL;DR

One thumbnail pipeline for every app, behind a tiny zero-dependency DLL: register generator plugins per file extension, request a thumbnail, and get it back asynchronously — served from a bounded on-disk cache that notices when the source file changes. Built-in generators really decode BMP and PPM; every other format is honestly `unsupported` until you register a provider for it. No placeholder thumbnails, ever.

```c
#include "thumbkit.h"

int main(void) {
    ThPipeline *pipe;
    th_create("{\"cache_dir\":\"C:/app/thumbcache\",\"max_cache_bytes\":67108864}", &pipe);

    uint64_t id;
    th_request(pipe, "C:/photos/team.bmp", "{\"max_w\":128,\"max_h\":128}", &id);
    th_wait(pipe, id, TH_WAIT_INFINITE);

    char *s; th_poll(pipe, id, &s);
    printf("%s\n", s);   /* {"state":"done","thumb_path":".../….thumb",...} */
    th_free_string(s);

    th_close(pipe);
}
```

## What it does

- **A pluggable provider interface.** A provider is one callback — `int (*)(const char *source_path, const char *out_path, int max_w, int max_h, void *user_data)` — registered for a comma-separated extension list with `th_register_provider`. It writes the thumbnail file to `out_path` and returns 0 on success. Later registrations win for the same extension, including over the built-ins, and each registration gets its own cache-key namespace, so swapping a provider never serves the old one's output.
- **Real built-in generators for BMP and PPM.** Uncompressed 24/32-bit BMP and binary PPM (P6, maxval 255) are decoded for real, downscaled with nearest-neighbour sampling — never upscaled — and written out as 24-bit BMP thumbnails.
- **Honest unsupported formats.** PNG, JPEG, GIF and everything else have no built-in decoder. Requests for them end in state `unsupported` with a message saying to register a provider. ThumbKit does not render placeholder or "info" images and pretend they are thumbnails.
- **A bounded, self-healing cache.** Each entry is a `<sha256>.thumb` file plus a `<sha256>.json` sidecar in `cache_dir`. The key covers the canonical source path, the source's size and modification time, the preset dimensions, and the provider name — so editing a source automatically misses and regenerates, and the stale entry becomes an orphan. The index is rebuilt by scanning sidecars, so the cache survives restarts and crashes with no separate database to corrupt.
- **LRU eviction against two bounds.** After every completed generation, and on `th_sweep`, the least-recently-used entries (by sidecar last-access time) are evicted until both `max_cache_bytes` and `max_entries` hold. `th_sweep` also removes orphans (source deleted, or size/mtime no longer matching) and leftover temp files, and reports what it did.
- **Asynchronous retrieval on a bounded pool.** Requests run on `worker_count` worker threads (default 2) with a bounded queue (default 64 queued; beyond that `th_request` returns `TH_ERR_QUEUE_FULL` instead of growing without limit). Callers `th_poll` for status JSON, `th_wait` with a timeout, or `th_cancel` a still-queued request.
- **A decoder memory bound.** The built-in decoders reject any image over 100 megapixels, or whose decoded RGB buffer would exceed 256 MiB, before allocating it — with an error that names the cap.

## Request states

`queued → running → done`, or ending in `failed` (missing source, decode error, provider failure), `unsupported` (no provider for the extension, or an unsupported variant of a format), or `cancelled` (cancelled while queued). Only queued requests can be cancelled: a running provider is arbitrary host code and cannot be interrupted safely, and ThumbKit does not pretend otherwise — cancelling a running or finished request returns `TH_ERR_INVALID_STATE`.

`th_poll` returns one JSON object: `id`, `state`, `thumb_path` (null until done), thumbnail `width`/`height`, `source_width`/`source_height`, and `error`. The thumbnail file belongs to the cache — copy it if it must survive eviction or a source change.

## Cache statistics and sweeping

```c
char *s;
th_stats(pipe, &s);  /* {"entries":12,"bytes":40211,"hits":30,"misses":12,"evictions":3} */
th_free_string(s);
th_sweep(pipe, &s);  /* {"removed_orphans":1,"evicted":0,"entries":11,"bytes":37100} */
th_free_string(s);
```

`entries`/`bytes` describe the cache on disk right now; `hits`/`misses`/`evictions` are counters for this handle since `th_create`.

## The API in one screen

| Area | Calls |
|---|---|
| Lifecycle | `th_create` (config JSON), `th_close` (cancels queued work, joins all workers) |
| Providers | `th_register_provider` (extension list, `ThGenerateFn` callback, user_data) |
| Requests | `th_request` (source path, preset JSON, out request id), `th_poll`, `th_wait`, `th_cancel` |
| Cache | `th_stats`, `th_sweep` — both return one JSON object |
| Errors | Integer `TH_*` codes + thread-local `th_last_error()`; strings freed with `th_free_string()` |

Config keys: `cache_dir` (required), `max_cache_bytes`, `max_entries`, `max_thumb_width`, `max_thumb_height`, `worker_count`, `queue_capacity`. A preset is `{"max_w":N,"max_h":N}`, clamped to the config maximums. Unknown keys — in config or preset — are errors, never silently ignored, and duplicate keys in one JSON object are a parse error.

## Honest limitations

- **Only BMP and PPM decode out of the box.** That is deliberate: a real decoder or an honest `unsupported`, never a fake thumbnail. PNG/JPEG/GIF support arrives by registering a provider (your decoder, your licence call), which the pipeline then caches and bounds exactly like the built-ins.
- **Built-in variants are narrow.** BMP: uncompressed 24/32-bit only (compressed and 8-bit BMPs return `unsupported`). PPM: P6 with maxval 255 only. Both limits are stated in the error message, not hidden.
- **Thumbnail dimensions can be 0 in status.** For a custom provider whose output file is not a BMP, and for sources in formats the built-ins do not understand, the pipeline cannot read dimensions back — it reports 0 rather than inventing a number.
- **One thumbnail larger than `max_cache_bytes` is kept.** The entry just generated is never evicted by its own pass; otherwise an oversized preset would generate-then-delete forever. `th_stats` shows the true bytes either way.
- **`th_close` can block on a running provider.** Queued work is cancelled instantly, but a generation already running finishes first — there is no safe way to stop arbitrary host code mid-call.
- **Hit/miss counters are per-handle.** Entries and bytes survive restarts (they are scanned from disk); the counters start at zero for each `th_create`.

## Building and tests

```sh
cargo build --release   # -> thumbkit.dll / libthumbkit.so / libthumbkit.a
cargo test --locked     # full suite
```

The crate is Rust, edition 2021, zero dependencies, and builds as a `cdylib` + `rlib` + `staticlib`. The 22 tests use synthetic fixtures generated in temp directories (hand-written BMP and PPM files): decoder and downscale rules including never-upscale, the 100-megapixel rejection, config and preset validation, request → wait → done, a cache hit that provably does not call the provider a second time (a counting provider in the Rust API), invalidation after the source changes plus an orphan sweep, LRU eviction with a one-thumbnail byte cap, the honest `unsupported` result for PNG, queue-full and queued-request cancellation with a slow provider, wait timeouts, and FFI end-to-end status/stats/sweep checks.

License: MIT. Part of the WinDevKit family — see `AGENTS.md` for build discipline and `ROADMAP.md` for what is next.
