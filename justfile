set dotenv-load := false

fmt:
    cargo fmt --all

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

dev-backend:
    cargo run -p fs2-backend

dev-client:
    cargo run -p fs2-cli -- --help
