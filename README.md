# FS2 Devsync

FS2 Devsync is an experimental developer-focused sync layer for making a `~/code` workspace available across macOS and Linux machines with metadata-first sync, lazy file hydration, encrypted environment-variable sync, Git-aware safety, and generated-directory rules for paths such as `node_modules` and `target`. It is not production-ready, Theo has not endorsed it, and it must not be trusted with irreplaceable private code or secrets until the safety checklist in `plan/todo.md` is complete.

## Current status

This repository is in early implementation. The design lives in `plan/design.md`; the executable checklist lives in `plan/todo.md`.

## MVP boundaries

Read `docs/mvp.md` and `docs/non-goals.md` before adding features. The MVP is single-user, macOS/Linux only, FUSE-backed for desktop use, Postgres-backed for metadata, S3-compatible for blobs, SQLite-backed locally, encrypted for file blobs and env values, and Git-aware without blind `.git` directory sync.
