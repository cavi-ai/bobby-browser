SHELL := /bin/bash
REPO_ROOT := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
SERVICE := $(REPO_ROOT)scripts/dev/service.sh

.PHONY: help build reload verify status fmt lint test

help:
	@echo "build   - cargo build --release -p bobby-browser (produces ./target/release/bobby)"
	@echo "reload  - build, restart the launchd service, verify the MCP handshake"
	@echo "verify  - restart and verify without rebuilding"
	@echo "status  - launchd state, port health, binary freshness"
	@echo "fmt     - cargo fmt --all"
	@echo "lint    - cargo clippy --workspace --all-targets -- -D warnings"
	@echo "test    - cargo test --workspace"
	@echo
	@echo "Set BOBBY_BROWSER_TOKEN to include the MCP handshake check in reload/verify."
	@echo
	@echo "Local CLI after build:"
	@echo "  cargo build -p bobby-browser && ./target/debug/bobby doctor"

build:
	cargo build --release --manifest-path $(REPO_ROOT)Cargo.toml -p bobby-browser

# The service keeps serving whatever binary existed when launchd last started
# it, so a rebuild alone changes nothing. Always go through reload/verify.
reload:
	@$(SERVICE) reload

verify:
	@$(SERVICE) verify

status:
	@$(SERVICE) status

fmt:
	cargo fmt --all --manifest-path $(REPO_ROOT)Cargo.toml

lint:
	cargo clippy --manifest-path $(REPO_ROOT)Cargo.toml --workspace --all-targets -- -D warnings

test:
	cargo test --manifest-path $(REPO_ROOT)Cargo.toml --workspace
