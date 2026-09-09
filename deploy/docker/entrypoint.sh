#!/usr/bin/env bash
# Entrypoint for the bobby-browser Docker image. Generates the loopback
# bootstrap credential once (bobby-browser/docs guides/auth.md: non-loopback
# binds never auto-generate one, and this image binds 0.0.0.0), then execs
# `bobby serve` as PID 1's child under tini.
set -euo pipefail

CONFIG_PATH="${BOBBY_BROWSER_CONFIG:-/etc/bobby/config.toml}"
BOOTSTRAP_PATH="${BOBBY_BROWSER_BOOTSTRAP_ENV:-/var/lib/bobby/bootstrap.env}"

# Not every storage path in config.toml is auto-created by `bobby serve`
# (authority_path's parent is; the rest are not verified to be) — create
# them all up front so a fresh named volume never fails on a missing dir.
mkdir -p \
    /var/lib/bobby/data/profiles \
    /var/lib/bobby/data/uploads \
    /var/lib/bobby/data/downloads \
    /var/lib/bobby/data/artifacts \
    /var/lib/bobby/data/storage

if [ ! -f "$BOOTSTRAP_PATH" ]; then
    echo "bobby-browser: generating bootstrap credential at $BOOTSTRAP_PATH" >&2
    # `bobby init` prints the plaintext bearer once on success; captured and
    # discarded rather than left in `docker logs` (auth.md: never put a
    # token in a log). Retrieve it after startup with:
    #   docker compose exec bobby bobby token --stdout
    if ! init_output=$(bobby init --path "$BOOTSTRAP_PATH" 2>&1); then
        echo "$init_output" >&2
        echo "bobby-browser: bobby init failed" >&2
        exit 1
    fi
    echo "bobby-browser: bootstrap credential generated; retrieve the bearer with: docker compose exec bobby bobby token --stdout" >&2
fi

exec bobby serve --config "$CONFIG_PATH" --bootstrap-env "$BOOTSTRAP_PATH"
