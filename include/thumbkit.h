/* ThumbKit.dll — a thumbnail and preview pipeline:
 * pluggable generators, a bounded on-disk cache, and
 * asynchronous retrieval behind one small C ABI.
 *
 *   ThPipeline *pipe;
 *   th_create("{\"cache_dir\":\"C:/app/thumbcache\"}", &pipe);
 *   uint64_t id;
 *   th_request(pipe, "C:/photos/a.bmp", "{\"max_w\":128}", &id);
 *   th_wait(pipe, id, TH_WAIT_INFINITE);
 *   char *s; th_poll(pipe, id, &s);   // {"state":"done",...}
 *   printf("%s\n", s); th_free_string(s);
 *   th_close(pipe);
 *
 * CONFIG (th_create, one JSON object):
 *
 *   {"cache_dir": "...", "max_cache_bytes": 67108864,
 *    "max_entries": 1024, "max_thumb_width": 256,
 *    "max_thumb_height": 256, "worker_count": 2,
 *    "queue_capacity": 64}
 *
 *   cache_dir        (string, required, non-empty) the
 *                    cache directory; created if missing.
 *   max_cache_bytes  (uint, optional, default 67108864)
 *                    total thumbnail bytes the cache may
 *                    hold before LRU eviction.
 *   max_entries      (uint, optional, default 1024,
 *                    1..=1000000) entry count bound.
 *   max_thumb_width  (uint, optional, default 256,
 *                    1..=8192) hard cap on thumbnail width;
 *                    presets are clamped to it.
 *   max_thumb_height (uint, optional, default 256,
 *                    1..=8192) same for height.
 *   worker_count     (uint, optional, default 2, 1..=16)
 *                    generation worker threads.
 *   queue_capacity   (uint, optional, default 64,
 *                    1..=4096) maximum QUEUED (not yet
 *                    running) requests; th_request beyond
 *                    it returns TH_ERR_QUEUE_FULL.
 *
 * Unknown keys are an error (TH_ERR_PARSE), never
 * ignored; duplicate keys in one object are a parse
 * error too.
 *
 * PROVIDERS. A provider generates thumbnails for a set
 * of extensions. Callback signature (exact):
 *
 *   typedef int (*ThGenerateFn)(const char *source_path,
 *                               const char *out_path,
 *                               int max_w, int max_h,
 *                               void *user_data);
 *
 * The provider writes the thumbnail file to out_path
 * (a path inside the cache directory) and returns 0 on
 * success; any nonzero return fails the request with
 * "provider ... returned code N". Providers run on
 * worker threads, possibly in parallel, so a provider
 * and its user_data must be thread-safe. user_data is
 * handed back verbatim.
 *
 * th_register_provider(handle, extension_csv, cb, user)
 * registers one for a comma-separated extension list
 * such as "png, .jpg, webp" (dots optional, case
 * ignored). A later registration takes precedence over
 * earlier ones — including the built-ins — for the same
 * extension. Each registration gets a generated name
 * (custom-<n>-<extensions>) that becomes part of the
 * cache keys it produces, so re-registering a changed
 * provider never serves the old provider's thumbnails.
 *
 * BUILT-IN PROVIDERS (registered by th_create):
 *
 *   bmp  Real decoder for uncompressed 24/32-bit BMP.
 *   ppm  Real decoder for binary PPM (P6, maxval 255).
 *
 * Both decode, downscale with nearest-neighbour sampling
 * (NEVER upscale: a smaller source keeps its size), and
 * write a 24-bit BMP thumbnail. Every other format —
 * PNG, JPEG, GIF included — has NO built-in decoder:
 * requests for it end in state "unsupported" unless a
 * provider is registered. ThumbKit never fabricates a
 * placeholder thumbnail.
 *
 * DECODER MEMORY BOUND. The built-in decoders reject any
 * image over 100 megapixels, or whose decoded RGB buffer
 * would exceed 256 MiB, with TH_ERR_INVALID_ARG and a
 * message naming the cap — before allocating the buffer.
 *
 * REQUESTS. th_request(handle, source_path, preset_json,
 * out_id) queues work and returns a request id (uint64,
 * starting at 1, unique per handle). preset_json is NULL
 * or one JSON object {"max_w":N, "max_h":N} (either key
 * optional; values 1..=8192, clamped to the config
 * maximums). Unknown preset keys are TH_ERR_PARSE.
 *
 * th_poll(handle, id, &json) returns one JSON object:
 *
 *   {"id":1,"state":"done","thumb_path":".../ab12….thumb",
 *    "width":128,"height":96,"source_width":1024,
 *    "source_height":768,"error":null}
 *
 *   state         queued | running | done | failed |
 *                 unsupported | cancelled
 *   thumb_path    the cached thumbnail file when done,
 *                 else null. The file is owned by the
 *                 cache: copy it if you need it to
 *                 survive eviction or source changes.
 *   width/height  thumbnail dimensions (0 when unknown —
 *                 possible for a custom provider whose
 *                 output is not a BMP).
 *   source_width / source_height
 *                 source dimensions for formats the
 *                 built-ins understand, else 0.
 *   error         null, or the failure message.
 *
 * th_wait(handle, id, timeout_ms) returns TH_OK once the
 * request is terminal (done, failed, unsupported or
 * cancelled), or TH_ERR_TIMEOUT when the wait elapsed
 * (the request is untouched). TH_WAIT_INFINITE waits
 * forever; 0 polls once.
 *
 * th_cancel(handle, id) cancels a QUEUED request only.
 * A running request cannot be cancelled safely — the
 * provider is arbitrary host code — and cancelling a
 * running or terminal request returns
 * TH_ERR_INVALID_STATE. Unknown ids return
 * TH_ERR_NOT_FOUND (as do th_poll / th_wait).
 *
 * CACHE. One entry is two files in cache_dir:
 * <key>.thumb (the thumbnail) and <key>.json (a sidecar
 * with source path, size, mtime, preset, provider,
 * dimensions and last-access time). key is the SHA-256
 * of the canonical source path, source size, source
 * mtime, preset dimensions and provider name — so when
 * a source changes, the next request misses and
 * generates fresh; the stale entry becomes an orphan.
 * The index is rebuilt by scanning sidecars, so the
 * cache survives process restarts and crashes.
 *
 * Eviction is LRU by sidecar last-access and is enforced
 * after every completed generation and by th_sweep,
 * against both bounds. The entry just generated is never
 * evicted by its own pass (a single thumbnail larger
 * than max_cache_bytes is kept and reported honestly by
 * th_stats rather than generated-then-deleted).
 *
 * th_stats(handle, &json):
 *   {"entries":N,"bytes":N,"hits":N,"misses":N,
 *    "evictions":N}
 * entries/bytes describe the cache on disk right now;
 * hits/misses/evictions are counters for this handle
 * since th_create.
 *
 * th_sweep(handle, &json) removes orphaned entries
 * (source deleted, or size/mtime no longer matching the
 * sidecar), deletes leftover temp files, enforces the
 * bounds, and reports:
 *   {"removed_orphans":N,"evicted":N,"entries":N,
 *    "bytes":N}
 *
 * th_close(handle) cancels queued requests, lets a
 * running generation finish, JOINS every worker, and
 * frees the handle. NULL is a no-op. Anything through
 * the handle after th_close is use-after-close.
 *
 * THREADING. The handle is thread-safe; every call is
 * serialized inside. Provider callbacks run on worker
 * threads, never under the pipeline lock.
 *
 * All strings are UTF-8, NUL-terminated. char* results
 * are caller-owned: free with th_free_string(). Errors
 * set a thread-local th_last_error() (message + code).
 */
#ifndef THUMBKIT_H
#define THUMBKIT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32) && defined(TH_BUILD)
#define TH_API __declspec(dllexport)
#elif defined(_WIN32)
#define TH_API __declspec(dllimport)
#else
#define TH_API
#endif

enum {
    TH_OK = 0,
    TH_ERR_INVALID_ARG = 1,   /* null pointers, missing/empty/wrong-typed values */
    TH_ERR_NOT_SUPPORTED = 2, /* no provider for the format, or unsupported variant */
    TH_ERR_NOT_FOUND = 3,     /* unknown request id, or source file missing */
    TH_ERR_ACCESS_DENIED = 4,
    TH_ERR_PLATFORM = 5,      /* an OS/thread/file call failed; see th_last_error() */
    TH_ERR_INTERNAL = 6,
    TH_ERR_PARSE = 7,         /* JSON is malformed, or has an unknown/duplicate key */
    TH_ERR_CANCELLED = 8,
    TH_ERR_INVALID_STATE = 9, /* e.g. cancelling a running or terminal request */
    TH_ERR_TIMEOUT = 10,      /* th_wait elapsed; the request is untouched */
    TH_ERR_QUEUE_FULL = 11    /* queued requests reached queue_capacity */
};

typedef struct ThPipeline ThPipeline;

/* A thumbnail provider. Write the thumbnail of
 * source_path (fitting max_w x max_h) to out_path and
 * return 0 on success, nonzero on failure. Runs on a
 * worker thread; see the provider notes above. */
typedef int (*ThGenerateFn)(const char *source_path,
                            const char *out_path,
                            int max_w, int max_h,
                            void *user_data);

TH_API const char *th_version(void);
TH_API const char *th_last_error(void);
TH_API int th_last_error_code(void);
TH_API void th_free_string(char *p);

/* Create a pipeline from a config JSON object (see
 * above). Returns TH_OK and a handle in *out_handle, or
 * a TH_ERR_* code and *out_handle is NULL. */
TH_API int th_create(const char *config_json, ThPipeline **out_handle);

/* Register a provider for a comma-separated extension
 * list ("png, .jpg"). generate_cb must not be NULL.
 * Later registrations win for the same extension. */
TH_API int th_register_provider(ThPipeline *handle,
                                const char *extension_csv,
                                ThGenerateFn generate_cb,
                                void *user_data);

/* Queue a thumbnail request for source_path.
 * preset_json is NULL or {"max_w":N,"max_h":N}. On
 * TH_OK, *out_id holds the request id. */
TH_API int th_request(ThPipeline *handle, const char *source_path,
                      const char *preset_json, uint64_t *out_id);

/* Status of one request as one JSON object
 * (caller-owned; free with th_free_string()). Shape and
 * field rules: see above. */
TH_API int th_poll(ThPipeline *handle, uint64_t id, char **out_json);

/* Block until the request is terminal (TH_OK) or the
 * wait elapses (TH_ERR_TIMEOUT; request untouched). */
#define TH_WAIT_INFINITE 0xFFFFFFFFu
TH_API int th_wait(ThPipeline *handle, uint64_t id, uint32_t timeout_ms);

/* Cancel a QUEUED request. Running or terminal requests
 * return TH_ERR_INVALID_STATE; unknown ids return
 * TH_ERR_NOT_FOUND. */
TH_API int th_cancel(ThPipeline *handle, uint64_t id);

/* Cache statistics as one JSON object (caller-owned):
 * {"entries":N,"bytes":N,"hits":N,"misses":N,
 *  "evictions":N}. */
TH_API int th_stats(ThPipeline *handle, char **out_json);

/* Remove orphaned entries, clear temp files, enforce
 * the cache bounds; report as one JSON object
 * (caller-owned): {"removed_orphans":N,"evicted":N,
 * "entries":N,"bytes":N}. */
TH_API int th_sweep(ThPipeline *handle, char **out_json);

/* Close and free the handle: queued requests are
 * cancelled, a running generation finishes, and all
 * workers are joined before this returns. NULL is a
 * no-op. */
TH_API void th_close(ThPipeline *handle);

#ifdef __cplusplus
}
#endif
#endif /* THUMBKIT_H */
