#!/usr/bin/env python3
"""Fail if publishable workspace crates use path deps without a version."""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CLOSURE = {
    "artifact-store",
    "bobby-browser",
    "broker",
    "checkpoint-store",
    "companion-core",
    "companion-protocol",
    "config",
    "dom-engine",
    "firefox-companion",
    "intent-engine",
    "interface-core",
    "js-engine",
    "mcp-gateway",
    "network-engine",
    "observability",
    "page-runtime",
    "sdk-core",
    "session-manager",
    "skill-runtime",
    "test-site",
    "types",
    "worker-pool",
    "workflow-journal",
}

PATH_DEP = re.compile(
    r'\{[^{}]*path\s*=\s*"[^"]+"[^{}]*\}',
    re.M,
)


def main() -> int:
    errors: list[str] = []
    for name in sorted(CLOSURE):
        if name == "bobby-browser":
            path = ROOT / "crates" / "cli" / "Cargo.toml"
        else:
            path = ROOT / "crates" / name / "Cargo.toml"
        text = path.read_text()
        if "publish = true" not in text and "publish = false" not in text:
            # default publish true — treat as publishable if in closure
            pass
        if re.search(r"^publish\s*=\s*false\s*$", text, re.M):
            errors.append(f"{path}: expected publish = true")
            continue
        for match in PATH_DEP.finditer(text):
            block = match.group(0)
            if "version" not in block:
                errors.append(f"{path}: path dep missing version: {block}")
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"ok: {len(CLOSURE)} publishable crates have versioned path deps")
    return 0


if __name__ == "__main__":
    sys.exit(main())
