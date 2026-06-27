CARGO ?= cargo

.PHONY: build test lint run version info migration-info clean

build:
	$(CARGO) build --locked

test:
	$(CARGO) test --locked

lint:
	$(CARGO) clippy --locked --all-targets -- -D warnings

run:
	$(CARGO) run --locked -- $(ARGS)

version:
	$(CARGO) run --locked -- version

info:
	$(CARGO) run --locked -- info

migration-info:
	$(CARGO) run --locked -- info

clean:
	$(CARGO) clean
