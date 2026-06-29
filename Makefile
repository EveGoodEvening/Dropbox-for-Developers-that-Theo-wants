# FS2 devsync makefile.
# Mirrors the `justfile` recipes from the design doc for environments without `just`.

CARGO ?= cargo
TARGET_DIR ?= target

.PHONY: all test clippy fmt fmt-check dev-backend dev-client clean

all: ## Build everything
	$(CARGO) build --workspace

test: ## Run workspace tests
	$(CARGO) test --workspace

clippy: ## Run clippy
	$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt: ## Format code
	$(CARGO) fmt --all

fmt-check: ## Check formatting without writing
	$(CARGO) fmt --all -- --check

dev-backend: ## Run backend dev server
	$(CARGO) run -p fs2-backend

dev-client: ## Run CLI
	$(CARGO) run -p fs2-cli --

clean: ## Clean build artifacts
	$(CARGO) clean
