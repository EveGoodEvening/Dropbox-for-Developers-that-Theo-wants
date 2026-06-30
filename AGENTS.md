# Project Instructions

## Lessons

- Keep `/target/` ignored before running Cargo verification; this repo initially had no `.gitignore`, and Cargo build artifacts must not be committed.
- Rust `Result` aliases in this repo must expose a defaulted error parameter, e.g. `pub type Result<T, E = LocalStoreError> = std::result::Result<T, E>;`, so call sites stay short while preserving precise-error escape hatches.
