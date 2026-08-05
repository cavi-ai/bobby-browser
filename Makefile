SHELL := /bin/bash
REPO_ROOT := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
SERVICE := $(REPO_ROOT)scripts/dev/service.sh

.DEFAULT_GOAL := help

.PHONY: help \
	build install firefox cli \
	start stop firefox-start firefox-stop \
	reload verify status \
	fmt lint test \
	fingerprint-dogfood fingerprint-collectors fingerprint-collectors-headed fingerprint-collectors-firefox \
	behavioral-benchmark behavioral-e2e behavioral-dogfood

help:
	@echo "bobby-browser make targets"
	@echo
	@echo "Setup"
	@echo "  build      release-build bobby (./target/release/bobby)"
	@echo "  install    build runtime + companion, then interactive agent setup"
	@echo "  firefox    build + install Firefox companion only"
	@echo "  cli        build + install bobby (+ mcp-gateway) onto PATH"
	@echo
	@echo "Service (launchd)"
	@echo "  start          start bobby serve MCP HTTP (com.bobby_browser.serve)"
	@echo "  stop           stop it for real (bootout; kill alone respawns)"
	@echo "  firefox-start  start KeepAlive Firefox companion agent"
	@echo "  firefox-stop   stop KeepAlive Firefox companion agent"
	@echo "  reload         build, restart bobby serve, verify MCP handshake"
	@echo "  verify         restart and verify without rebuilding"
	@echo "  status         launchd state, port health, binary freshness"
	@echo
	@echo "Quality"
	@echo "  fmt        cargo fmt --all"
	@echo "  lint       cargo clippy --workspace --all-targets -- -D warnings"
	@echo "  test       cargo test --workspace"
	@echo
	@echo "Fingerprint dogfood"
	@echo "  fingerprint-dogfood              live Chromium collector probe (needs Chrome)"
	@echo "  fingerprint-collectors           BrowserLeaks/CreepJS/FingerprintJS (needs Chrome)"
	@echo "  fingerprint-collectors-headed    same, headed Chrome (needs GUI)"
	@echo "  fingerprint-collectors-firefox   live Firefox collectors (needs BOBBY_FIREFOX_*)"
	@echo
	@echo "Behavioral dogfood"
	@echo "  behavioral-benchmark   offline interaction biometric scores"
	@echo "  behavioral-e2e         multi-seed gates + companion BiDi (FakeBidi; no browser)"
	@echo "  behavioral-dogfood     live Firefox behavioral probe (needs BOBBY_FIREFOX_*)"
	@echo
	@echo "Notes"
	@echo "  Set BOBBY_BROWSER_TOKEN to include the MCP handshake in reload/verify."
	@echo "  Non-interactive full setup:"
	@echo "    ./target/release/bobby install --host claude --skill --cli --yes"
	@echo "  Companion only:  make firefox"
	@echo "  CLI on PATH:     make cli"

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

build:
	cargo build --release --manifest-path $(REPO_ROOT)Cargo.toml -p bobby-browser

# Build bobby + the gateways and run the interactive agent-host installer
# (credential, MCP config merge, agent skill). Non-interactive:
#   ./target/release/bobby install --host claude --skill --yes
install:
	cargo build --release --manifest-path $(REPO_ROOT)Cargo.toml -p bobby-browser -p mcp-gateway
	pnpm --filter @cavi-ai/bobby-firefox-companion build
	$(REPO_ROOT)target/release/bobby install

# Build the companion extension and install it (native host + extension copy).
# Does not touch host MCP configs or the bootstrap credential.
firefox:
	cargo build --release --manifest-path $(REPO_ROOT)Cargo.toml -p bobby-browser
	pnpm --filter @cavi-ai/bobby-firefox-companion build
	$(REPO_ROOT)target/release/bobby install --companion

# Install bobby (+ mcp-gateway) onto PATH (~/.cargo/bin when present, else ~/.local/bin).
cli:
	cargo build --release --manifest-path $(REPO_ROOT)Cargo.toml -p bobby-browser -p mcp-gateway
	$(REPO_ROOT)target/release/bobby install --cli

# ---------------------------------------------------------------------------
# Service
# ---------------------------------------------------------------------------

FIREFOX_SERVICE_LABEL := com.bobby_browser.firefox
FIREFOX_SERVICE_PLIST := $(HOME)/Library/LaunchAgents/$(FIREFOX_SERVICE_LABEL).plist

# The service keeps serving whatever binary existed when launchd last started
# it, so a rebuild alone changes nothing. Always go through reload/verify.
# KeepAlive=true: `kill` respawns — use `make stop` (bootout) to shut down.
start:
	@$(SERVICE) start

stop:
	@$(SERVICE) stop

firefox-start:
	@if launchctl print "gui/$$(id -u)/$(FIREFOX_SERVICE_LABEL)" >/dev/null 2>&1; then \
		echo "==> $(FIREFOX_SERVICE_LABEL) already loaded"; \
	else \
		test -f "$(FIREFOX_SERVICE_PLIST)" || { echo "error: no plist at $(FIREFOX_SERVICE_PLIST)" >&2; exit 1; }; \
		echo "==> starting $(FIREFOX_SERVICE_LABEL)"; \
		launchctl bootstrap "gui/$$(id -u)" "$(FIREFOX_SERVICE_PLIST)"; \
	fi

firefox-stop:
	@if ! launchctl print "gui/$$(id -u)/$(FIREFOX_SERVICE_LABEL)" >/dev/null 2>&1; then \
		echo "==> $(FIREFOX_SERVICE_LABEL) not loaded"; \
	else \
		echo "==> stopping $(FIREFOX_SERVICE_LABEL) (bootout — KeepAlive will not respawn)"; \
		launchctl bootout "gui/$$(id -u)/$(FIREFOX_SERVICE_LABEL)"; \
	fi

reload:
	@$(SERVICE) reload

verify:
	@$(SERVICE) verify

status:
	@$(SERVICE) status

# ---------------------------------------------------------------------------
# Quality
# ---------------------------------------------------------------------------

fmt:
	cargo fmt --all --manifest-path $(REPO_ROOT)Cargo.toml

lint:
	cargo clippy --manifest-path $(REPO_ROOT)Cargo.toml --workspace --all-targets -- -D warnings

test:
	cargo test --manifest-path $(REPO_ROOT)Cargo.toml --workspace

# ---------------------------------------------------------------------------
# Dogfood
# ---------------------------------------------------------------------------

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
