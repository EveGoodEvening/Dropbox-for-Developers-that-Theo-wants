# FS2 non-goals and deferred work

Work explicitly deferred beyond the MVP. Each item lists why it is deferred so
future contributors know whether a given PR belongs in MVP or post-MVP.

## Git replacement

- **Deferred:** a snapshot/tag/source-control model that replaces Git.
- **Why:** the MVP must solve cross-machine code sync first. Git remains the
  source-control tool inside synced workspaces. The data model (node revisions
  and operation logs) is designed so a future source-control layer can evolve
  from it, but the MVP does not depend on solving that problem.

## Automatic code merge

- **Deferred:** automatic semantic or LLM-assisted merge of conflicting edits.
- **Why:** FS2 is a sync system, not a merge engine. Concurrent edits produce
  explicit conflict artifacts in the MVP. A future product may integrate merge
  tools.

## End-to-end encrypted metadata

- **Deferred:** encrypting filenames, paths, sizes, and directory structure
  against the service operator.
- **Why:** encrypted metadata complicates conflict detection, listing, and
  web/CLI management. The MVP protects file content and secret values only.
  `SECURITY.md` documents this clearly.

## Artifact cache

- **Deferred:** remote caching of `node_modules`, `target/`, `.venv/`, etc. by
  `PlatformKey` + lockfile hash.
- **Why:** substantially increases complexity and storage cost. Generated
  outputs are treated as local cache in the MVP and rebuilt per machine.

## Web dashboard

- **Deferred:** browser UI for workspaces, devices, conflicts, and env vars.
- **Why:** the CLI and `fs2 status --json` are the MVP surface. A web UI can
  consume the same backend API later.

## Editor integrations

- **Deferred:** VS Code extension, language-server hydration awareness,
  in-editor conflict UI.
- **Why:** the MVP ships a stable CLI and daemon RPC. Editor integrations can
  build on the RPC interface after MVP.

## Other deferred items

- Team workspaces, roles, per-project permissions, audit log.
- Cloud agent checkout/push helpers (a materialized mode is allowed for tests,
  but the polished agent UX is post-MVP).
- Windows support.
- Mobile clients.
- Cross-user global deduplication.
- Multi-tenant enterprise ACLs.
