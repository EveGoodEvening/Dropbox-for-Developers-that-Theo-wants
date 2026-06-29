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

## Manual FUSE test

After installing the FUSE prerequisites and building, verify the mount works
on a real macOS or Linux host (the read-only adapter is also unit-tested via
`cargo test -p fs2-fuse`, but a live mount confirms the kernel path).

### Linux

```bash
# Start the backend in one terminal.
./target/release/fs2-backend

# In another terminal, log in and create a workspace.
fs2 login --backend http://localhost:8787
fs2 workspace create test-ws

# Create a mount point and mount.
mkdir ~/fs2-mnt
fs2 mount test-ws ~/fs2-mnt

# Verify the mount.
ls -la ~/fs2-mnt          # should show the (empty) workspace root
stat ~/fs2-mnt            # should report a directory

# Unmount when done.
fusermount3 -u ~/fs2-mnt
```

### macOS

```bash
# Same backend/login steps as above.
mkdir ~/fs2-mnt
fs2 mount test-ws ~/fs2-mnt
ls -la ~/fs2-mnt
# Unmount:
umount ~/fs2-mnt
```

If `ls` hangs or reports "Transport endpoint is not connected", the FUSE
daemon is not receiving kernel requests — check that macFUSE/FUSE3 is
installed and that the user has mount permission.

## Troubleshooting

- **`fs2 login` fails:** Ensure the backend is running (`fs2-backend`) and
  reachable at the specified URL.
- **`fs2 doctor` reports failures:** Follow the suggestions in the output.
- **Backend won't start:** Check that port 8787 is not in use.
