# fs2-devsync — Dropbox for Developers

**Experimental.** A developer-focused sync layer that makes a `~/code` directory behave like Dropbox across machines while respecting the realities of software projects: Git repositories, secrets, generated directories, platform-specific dependency folders, large workspaces, and offline edits.

> Idea from [Theo](https://x.com/theo/status/2069621429189161350). Theo has no endorsement on this project.

## Warning

This is an experimental project. Do not use it with sensitive production data. See [SECURITY.md](SECURITY.md) and [docs/mvp.md](docs/mvp.md).

## What it does

- Same project tree everywhere, synced metadata-first.
- On-demand file hydration: read a file and its bytes download on first access.
- Developer-specific ignore semantics via `.fs2ignore` and `.fs2/config.toml`.
- Environment variable sync with client-side encryption.
- Special handling for generated/platform-specific directories (`node_modules`, `target`, `.venv`, ...).
- Safe conflict handling with explicit conflict artifacts and tombstoned deletes.
- Git-aware: syncs the working tree, never `.git` internals.

## Status

Early implementation. See `plan/design.md` for the detailed design and `plan/todo.md` for the implementation tracker.
