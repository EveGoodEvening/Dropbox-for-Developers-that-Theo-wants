# Project Instructions

## Lessons

- Keep `/target/` ignored before running Cargo verification; this repo initially had no `.gitignore`, and Cargo build artifacts must not be committed.
- Rust `Result` aliases in this repo must expose a defaulted error parameter, e.g. `pub type Result<T, E = LocalStoreError> = std::result::Result<T, E>;`, so call sites stay short while preserving precise-error escape hatches.
- Keep `sqlx` on the existing `0.8` resolver selection for now; pinning `0.8.6` pulls `libsqlite3-sys 0.30.x` through `sqlx-sqlite` and conflicts with `rusqlite 0.31`/`libsqlite3-sys 0.28.x`.
- `fs2 doctor` Git-internal diagnostics intentionally split confirmed concrete probe matches (`WARNING confirmed syncable rule matches .git internals`) from conservative structural reachability (`CAUTION broad syncable rule may reach .git internals`); Phase 18.2 does not attempt exhaustive glob language set-difference proofs.
- CLI auth keeps non-secret backend/user/device metadata in the JSON config path (`FS2_CONFIG_HOME` in tests, otherwise XDG/home config) and stores access/refresh tokens only in OS keyring service `fs2-devsync.cli-token.v1` accounts `default` and `default-refresh`.
- CLI workspace commands keep per-workspace local metadata in `workspaces.json` beside the CLI config, with SQLite DBs under `workspaces/<workspace-id>/metadata.sqlite`; mount lookup must prefer exact workspace IDs before name fallback and reject ambiguous duplicate names.
- Shared backend/client wire contracts for sync APIs belong in `fs2-core`; `fs2-sync::ApiClient` is a blocking HTTP/WebSocket client for the local daemon sync loops and currently targets the development direct blob endpoints.
