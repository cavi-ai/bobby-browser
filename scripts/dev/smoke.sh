#!/usr/bin/env bash
set -euo pipefail

cargo run -p cli -- serve &
PID=$!
trap 'kill $PID' EXIT
sleep 2
curl -s http://127.0.0.1:7777/healthz
printf '\n'
curl -s http://127.0.0.1:7777/runtime
printf '\n'
