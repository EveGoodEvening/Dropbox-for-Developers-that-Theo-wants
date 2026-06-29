# FS2 Non-goals and Deferred Work

These items are explicitly deferred from the MVP. Do not spend implementation effort on them until the vertical slice in `plan/todo.md` is complete and stable.

## Deferred product capabilities

- Git replacement: FS2 syncs working trees and records Git metadata, but it does not replace commits, branches, tags, or remotes.
- Automatic code merge: concurrent edits produce conflicts and preserve both versions. FS2 does not attempt semantic merges in MVP.
- End-to-end encrypted metadata: MVP protects file content and secrets, but metadata is plaintext on the backend.
- Artifact cache: generated outputs may be suppressed or rebuilt locally, but FS2 does not upload reusable platform-specific build artifacts in MVP.
- Web dashboard: all MVP administration happens through CLI/API.
- Editor integrations: editor status UI and conflict tools are future work.

## Deferred platform and team scope

- Team workspaces, members, roles, and project ACLs.
- Windows support through WinFsp or Windows Credential Manager.
- Native macOS File Provider replacement for FUSE.

## Deferred polish

- Installer packaging and notarization beyond developer builds.
- Public release dogfood polish after the core safety and sync tests pass.
