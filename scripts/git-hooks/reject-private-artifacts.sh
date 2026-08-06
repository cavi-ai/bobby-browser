#!/usr/bin/env bash
# Reject staged private planning/agent docs.
set -euo pipefail

banned_regex='^(docs/superpowers/|docs/plans/|docs/specs/|docs/brainstorms/|plans/|\.superpowers/|\.claude/plans/|\.remember/)'

staged="$(git diff --cached --name-only --diff-filter=ACMR || true)"
[[ -z "${staged}" ]] && exit 0

bad="$(printf '%s\n' "${staged}" | grep -E "${banned_regex}" || true)"
if [[ -n "${bad}" ]]; then
  echo "REJECTED: private planning/agent docs:" >&2
  printf '%s\n' "${bad}" >&2
  exit 1
fi
exit 0
