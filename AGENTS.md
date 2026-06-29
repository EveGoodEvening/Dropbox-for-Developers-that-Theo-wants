# FS2 dev-sync — Agent Notes

## Lessons

- **FUSE in this sandbox**: `/dev/fuse` and `fusermount3` are present and the `fuser` crate compiles, but live FUSE mounts do NOT service kernel requests — `ls` on a mounted dir hangs then returns `ECONNABORTED`/`Transport endpoint is not connected`. Implement and unit-test the `Filesystem` trait methods directly (testable `*_core` helpers that don't need a `fuser::Request`); record live-mount acceptance as blocked.
- **`fuser` 0.14**: use `default-features = false` (the `libfuse` feature needs `pkg-config` + dev headers which aren't installed). `mount2` works without libfuse; `spawn` does not. `ReplyDirectory::add` returns `bool` (not `Result`).
- **`fuser::Request::new` is `pub(crate)`** — cannot be constructed in tests. Extract filesystem logic into `*_core` methods returning `Result<FileAttr, i32>` etc. and test those directly.
- **`parking_lot::Mutex`** is preferred over `std::sync::Mutex` when the lock is immediately unwrapped (no poisoning). Use `tokio::sync::Mutex` only when a guard is held across `.await`.
- **Content-addressed blobs are shared across nodes**: cache pruning must protect a blob while *any* referencing node is pinned. Use `blob_cache.pinned_ref_count` (a ref count on the `blob_cache` row), NOT a `LEFT JOIN local_state ON local_blob_path = blob_cache.path` (which multiplies rows and lets one unpinned node evict a pinned blob). `mark_blob_cached` must use `ON CONFLICT DO UPDATE` (not `INSERT OR REPLACE`, which wipes `pinned_ref_count`).
- **`set_pinned` must upsert** the `local_state` row so the pin sticks for not-yet-hydrated nodes; a bare `UPDATE` affects 0 rows and silently loses the pin.
- **`apply_operation` for `CreateNode` assigns a new `NodeId`** internally (different from the `node_id` in the op's `initial_revision`). Revisions are looked up by `revision_id`, not by `node_id`, so `get_revision` works, but the revision's `blob_id` is whatever was in the op — key test blobs by the revision's actual `blob_id`, not by `sha256:{returned_node_id}`.
- **Postgres is not available** in this sandbox (no `psql`, no `pg_ctl`, no docker). The backend uses an in-memory `MemoryStore`; Postgres migration tests and S3/MinIO blob tests are blocked.
- **`tracing-subscriber` needs the `env-filter` feature** for `EnvFilter`/`with_env_filter`.
