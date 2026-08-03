SHELL := /bin/bash
REPO_ROOT := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
SERVICE := $(REPO_ROOT)scripts/dev/service.sh

.PHONY: help build install reload verify status fmt lint test fingerprint-dogfood fingerprint-collectors fingerprint-collectors-headed fingerprint-collectors-firefox behavioral-benchmark behavioral-e2e behavioral-dogfood

help:
	@echo "build   - cargo build --release -p bobby-browser (produces ./target/release/bobby)"
	@echo "reload  - build, restart the launchd service, verify the MCP handshake"
	@echo "verify  - restart and verify without rebuilding"
	@echo "status  - launchd state, port health, binary freshness"
	@echo "fmt     - cargo fmt --all"
	@echo "lint    - cargo clippy --workspace --all-targets -- -D warnings"
	@echo "test    - cargo test --workspace"
	@echo "fingerprint-dogfood - live Chromium collector probe (requires Chrome; --ignored)"
	@echo "fingerprint-collectors - production site dogfood (BrowserLeaks/CreepJS/FingerprintJS; --ignored)"
	@echo "fingerprint-collectors-headed - same as collectors but headed Chrome (requires GUI display)"
	@echo "fingerprint-collectors-firefox - live Firefox collectors (requires BOBBY_FIREFOX_*; --ignored)"
	@echo "behavioral-benchmark - offline interaction biometric scores (CreepJS-analogue for behavior)"
	@echo "behavioral-e2e - multi-seed gates + Firefox companion BiDi wiring (FakeBidi; no browser)"
	@echo "behavioral-dogfood - live Firefox behavioral probe (requires BOBBY_FIREFOX_*; --ignored)"
	@echo
	@echo "Set BOBBY_BROWSER_TOKEN to include the MCP handshake check in reload/verify."
	@echo
	@echo "Local CLI after build:"
	@echo "  cargo build -p bobby-browser && ./target/debug/bobby doctor"

build:
	cargo build --release --manifest-path $(REPO_ROOT)Cargo.toml -p bobby-browser

# Build bobby + the gateways and run the interactive agent-host installer
# (credential, MCP config merge, agent skill). Non-interactive:
#   ./target/release/bobby install --host claude --skill --yes
install:
	cargo build --release --manifest-path $(REPO_ROOT)Cargo.toml -p bobby-browser -p mcp-gateway
	pnpm --filter @bobby-browser/firefox-companion build
	$(REPO_ROOT)target/release/bobby install

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

fingerprint-dogfood:
	cargo test -p worker-pool --test fingerprint_conformance -- --ignored --nocapture

fingerprint-collectors:
	cargo test -p worker-pool --test fingerprint_conformance chromium_production_collector_dogfood -- --ignored --nocapture

fingerprint-collectors-headed:
	BOBBY_FP_HEADED=1 cargo test -p worker-pool --test fingerprint_conformance chromium_production_collector_dogfood -- --ignored --nocapture

fingerprint-collectors-firefox:
	@$(REPO_ROOT)scripts/dev/fingerprint-firefox.sh

behavioral-benchmark:
	cargo test -p behavioral-engine --test benchmark -- --nocapture

behavioral-e2e:
	cargo test -p behavioral-engine --test e2e -- --nocapture
	cargo test -p firefox-companion --test behavioral_e2e -- --nocapture

behavioral-dogfood:
	@$(REPO_ROOT)scripts/dev/behavioral-firefox.sh
