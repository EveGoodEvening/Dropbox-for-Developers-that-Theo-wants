# Project Instructions

## Lessons

- Keep `/target/` ignored before running Cargo verification; this repo initially had no `.gitignore`, and Cargo build artifacts must not be committed.
- Rust `Result` aliases in this repo must expose a defaulted error parameter, e.g. `pub type Result<T, E = LocalStoreError> = std::result::Result<T, E>;`, so call sites stay short while preserving precise-error escape hatches.
- Keep `sqlx` on the existing `0.8` resolver selection for now; pinning `0.8.6` pulls `libsqlite3-sys 0.30.x` through `sqlx-sqlite` and conflicts with `rusqlite 0.31`/`libsqlite3-sys 0.28.x`.
