# Developer Workflow Guide — fs2-devsync

This guide explains the key concepts and daily workflow for using fs2-devsync.

## Metadata-only files

When you sync a workspace, the directory structure (file names, sizes, mtimes,
executable bits, symlink targets) appears on all machines immediately. File
**content** is not downloaded until you access it.

This means:
- `ls` and `find` work instantly on a cold machine.
- `cat file.txt` triggers a download on first access (hydration).
- Subsequent reads come from the local cache.

## Hydration

Hydration is the process of downloading file content on first access.

- **Metadata-only:** File exists in the tree but bytes are not cached.
- **Hydrated:** Bytes are present and verified.
- **Pinned:** Bytes are present and will not be evicted.

To pre-download files before going offline:
```bash
fs2 hydrate apps/web --recursive --pin
```

## Pinning

Pinning keeps files available offline. Pinned files are never evicted by cache
pruning.

```bash
fs2 pin apps/web --recursive
fs2 unpin apps/web --recursive
```

## Generated directories

Directories like `node_modules`, `target`, `.venv`, and `.next` are **not
synced** by default. The rule engine marks them as `generated` or
`dependency-cache`, which means:

- Their content is never uploaded.
- Their content is never downloaded.
- They appear in the tree only if they exist locally.

To install dependencies on a new machine:
```bash
cd ~/code/apps/web
pnpm install  # or npm install, cargo build, etc.
```

Run `fs2 doctor` to see which package manager is detected and what install
command to run.

## `.fs2ignore` and `.fs2/config.toml`

Create a `.fs2ignore` file in your workspace root to customize sync behavior:

```gitignore
# Ignore files
.DS_Store
*.swp

# Generated directories
:generated node_modules/
:generated target/

# Local-only files
:local-only .env.local

# Pin important files
:pin package.json
:pin pnpm-lock.yaml

# Secret materialization
:secret .env
```

Create a `.fs2/config.toml` for structured configuration:

```toml
version = 1
workspace_name = "personal-code"
default_file_policy = "lazy"

[cache]
max_bytes = "50GiB"
min_free_bytes = "20GiB"

[git]
mode = "aware"
sync_git_dir = false
```

## Environment variables

fs2-devsync can sync environment variables encrypted across machines:

```bash
fs2 env set STRIPE_SECRET_KEY --project apps/web --env dev --secret
fs2 env set NEXT_PUBLIC_API_URL https://api.local --project apps/web --env dev --plain
fs2 env list --project apps/web --env dev
fs2 env exec --project apps/web --env dev -- pnpm dev
```

Secrets are encrypted client-side. The backend never sees plaintext values.

## Git-aware mode

fs2-devsync detects Git repositories and records metadata (remote URL, branch,
HEAD commit, dirty status) without syncing `.git` internals.

On a new machine, the synced working tree appears. To set up a functional Git
repo:
```bash
fs2 git materialize apps/web
```

This clones the remote, moves `.git` into place, and overlays the synced
working tree.

## Conflicts

When two machines edit the same file concurrently, fs2-devsync creates a
conflict artifact instead of silently overwriting:

```
src/app.conflict.mac-mini-2.2026-06-28T13-04-11.ts
```

Both versions are preserved. Resolve conflicts with:
```bash
fs2 conflicts list
fs2 conflicts resolve <id> --use-local
```

## Offline mode

When the backend is unreachable:
- You can still read and edit hydrated files.
- New files and edits are queued as pending operations.
- On reconnect, pending operations are submitted in order.
- Conflicts are created if the base revision changed.
