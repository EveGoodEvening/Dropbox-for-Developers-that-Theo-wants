# Install Guide — fs2-devsync

**Status:** Experimental. Not yet ready for production use.

## Prerequisites

- **Rust** 1.75 or later (for building from source)
- **Git** (for Git-aware mode)
- **macOS:** macFUSE (for FUSE mount)
- **Linux:** FUSE3 (for FUSE mount)

## Building from source

```bash
git clone https://github.com/EveGoodEvening/Dropbox-for-Developers-that-Theo-wants.git
cd Dropbox-for-Developers-that-Theo-wants
cargo build --release
```

The binaries will be in `target/release/`:
- `fs2-cli` — the CLI (rename to `fs2` and add to PATH)
- `fs2-backend` — the backend server

## Quick start

1. **Start the backend:**
   ```bash
   ./target/release/fs2-backend
   ```
   The backend listens on `127.0.0.1:8787` by default.

2. **Log in:**
   ```bash
   fs2 login --backend http://localhost:8787
   ```

3. **Create a workspace:**
   ```bash
   fs2 workspace create my-code
   ```

4. **Check status:**
   ```bash
   fs2 status
   ```

5. **Run diagnostics:**
   ```bash
   fs2 doctor
   ```

## Configuration

The CLI stores its config in `~/.fs2/config.json`:
- Backend URL
- JWT token
- User ID
- Device ID

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `FS2_BACKEND_BIND` | `127.0.0.1:8787` | Backend bind address |
| `FS2_DATABASE_URL` | (empty) | Postgres URL (empty = in-memory) |
| `FS2_JWT_SECRET` | `dev-only-secret-change-me` | JWT signing secret |
| `FS2_DEV_AUTH` | `true` | Enable dev-only auth endpoints |

## Self-hosting the backend

The backend can run without Postgres using an in-memory store. For production,
set `FS2_DATABASE_URL` to a Postgres connection string and apply the migrations
in `migrations/postgres/001_initial_schema.sql`.

## FUSE mount

FUSE mount support requires platform-specific FUSE libraries:

### macOS
Install [macFUSE](https://macfuse.io/).

### Linux
Install FUSE3:
```bash
# Ubuntu/Debian
sudo apt install libfuse3-dev

# Fedora
sudo dnf install fuse3-devel
```

## Troubleshooting

- **`fs2 login` fails:** Ensure the backend is running (`fs2-backend`) and
  reachable at the specified URL.
- **`fs2 doctor` reports failures:** Follow the suggestions in the output.
- **Backend won't start:** Check that port 8787 is not in use.
