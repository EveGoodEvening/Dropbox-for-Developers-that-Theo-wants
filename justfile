# fs2-devsync development tasks

# Run all workspace tests
test:
    cargo test --workspace

# Run clippy across the workspace
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Format check
fmt:
    cargo fmt --all -- --check

# Apply formatting
fmt-apply:
    cargo fmt --all

# Run the backend for local development
dev-backend:
    cargo run -p fs2-backend

# Run the CLI for local development
dev-client *ARGS:
    cargo run -p fs2-cli -- {{ARGS}}

# Run the daemon for local development
dev-daemon:
    cargo run -p fs2-daemon

# Check the whole workspace compiles
check:
    cargo check --workspace --all-targets
