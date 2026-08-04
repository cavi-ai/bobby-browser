#!/usr/bin/env python3
"""Print one version's section from CHANGELOG.md.

The release body is the changelog section, not a hand-written summary: the
notes a reader gets are the notes the repo already reviewed. `gh release
create --notes-file` consumes this.

Exits non-zero when the version has no section, so a tag cannot publish a
release with an empty or wrong body.

    scripts/changelog-section.py 0.5.0
"""
from __future__ import annotations

import pathlib
import re
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent
HEADING = re.compile(r"^## +(?P<version>[^ \n]+)")


def section(text: str, version: str) -> str | None:
    """The body under `## <version>`, up to the next `##` heading."""
    lines = text.splitlines()
    start = None
    for index, line in enumerate(lines):
        match = HEADING.match(line)
        if not match:
            continue
        if start is not None:
            return "\n".join(lines[start:index]).strip()
        if match.group("version") == version:
            start = index + 1
    if start is None:
        return None
    return "\n".join(lines[start:]).strip()


def main() -> int:
    if len(sys.argv) != 2:
        sys.exit("usage: changelog-section.py <version>")
    version = sys.argv[1].removeprefix("v")
    body = section((REPO / "CHANGELOG.md").read_text(), version)
    if not body:
        sys.exit(f"CHANGELOG.md has no section for {version}")
    print(body)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
