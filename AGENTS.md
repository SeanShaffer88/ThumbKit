# ThumbKit — agent notes

- The no-placeholder rule is load-bearing: a format without a real decoder returns `unsupported`. Never add a provider that renders a placeholder, an icon, or a header-only "preview" and reports it as a thumbnail.
- Built-in decoder scope is narrow on purpose: BMP uncompressed 24/32-bit and PPM P6/maxval 255 only. Other variants return `TH_ERR_NOT_SUPPORTED` with the reason in the message. Widening scope means a real decoder for the new variant, not a looser check.
- The decoder memory cap (100 MP / 256 MiB, checked before allocation or any data-length validation that could mask it) is a safety path: the oversize tests assert the cap error wins over truncation errors.
- Cache keys must cover canonical path + size + mtime + preset + provider name. Dropping any component reintroduces stale-thumbnails-served-as-current. Provider names include the registration sequence number for exactly this reason.
- The entry just generated is never evicted by its own bound-enforcement pass (documented in the header). "Fixing" that makes an oversized single thumbnail generate-then-delete on every request.
- Cache state lives in sidecar files, scanned on demand — there is no in-memory index to keep in sync. Do not add one without a crash-consistency story at least as good.
- Cancellation is queued-only by design: a running provider is arbitrary host code and cannot be interrupted safely. Do not add a path that claims to cancel running work.
- Providers run WITHOUT the pipeline lock held and possibly in parallel; generation happens to a per-request temp file outside the cache lock, and only publish/sidecar/eviction take `cache_lock`. Holding `inner` or `cache_lock` across a provider call serializes the pool and can deadlock a provider that calls back into status APIs.
- `th_close` cancels queued work and JOINS all workers. A close that detaches breaks the no-leaked-threads guarantee.
- The JSON engine (src/json.rs) is the family design — duplicate keys in one object are a PARSE ERROR by design; do not "fix" that into last-wins.
- Zero dependencies; SHA-256 is hand-rolled in src/sha256.rs for cache keys only — it is not used for any security decision.
- Gate: WinDevKit tools/gate.sh (fmt + clippy both targets + full tests). Check/clippy do not link — CI's real Windows build is the link gate.
