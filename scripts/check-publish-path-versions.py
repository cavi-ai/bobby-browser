#!/usr/bin/env python3
"""Fail if the crates.io SDK crate uses path deps or is marked publish=false."""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
# Sole Rust library published to crates.io (CLI `bobby-browser` is separate).
SDK = "bobby-browser-client"

PATH_DEP = re.compile(
    r'\{[^{}]*path\s*=\s*"[^"]+"[^{}]*\}',
    re.M,
)


def main() -> int:
    path = ROOT / "crates" / SDK / "Cargo.toml"
    text = path.read_text()
    errors: list[str] = []
    if re.search(r"^publish\s*=\s*false\s*$", text, re.M):
        errors.append(f"{path}: expected publish = true")
    for match in PATH_DEP.finditer(text):
        errors.append(f"{path}: crates.io SDK must not use path deps: {match.group(0)}")
    types_toml = ROOT / "crates" / "types" / "Cargo.toml"
    types_text = types_toml.read_text()
    if not re.search(r"^publish\s*=\s*false\s*$", types_text, re.M):
        errors.append(f"{types_toml}: expected publish = false (wire types live in {SDK})")
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"ok: {SDK} is path-dep-free and types is publish = false")
    return 0


if __name__ == "__main__":
    sys.exit(main())
