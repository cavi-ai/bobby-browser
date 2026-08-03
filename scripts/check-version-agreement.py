#!/usr/bin/env python3
"""Every artifact in this repo ships under one version. Prove it.

One tag (`v*`) publishes the binaries, the npm SDK, and the crates.io SDK, so a
version that disagrees between manifests means the tag ships a mislabelled
artifact. That is not hypothetical here: npm reached 0.3.1 while the last
`sdk-v*` tag was 0.3.0, because nothing checked.

Same failure shape as the five capability parse tables that drifted twice in a
week — one concept, many copies, no gate. This is the gate.

Exits non-zero and names every disagreement.
"""
from __future__ import annotations

import json
import pathlib
import re
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent

# Immutable published docs carry the version they were published at, forever.
# Rewriting them would falsify a release artifact.
SKIP_DIRS = ("node_modules", "target", ".git", "docs/bobby-browser/v")


def skipped(path: pathlib.Path) -> bool:
    text = str(path.relative_to(REPO))
    return any(part in text for part in SKIP_DIRS)


def workspace_version() -> str:
    manifest = (REPO / "Cargo.toml").read_text()
    match = re.search(r'^\[workspace\.package\][^\[]*?^version = "([^"]+)"', manifest, re.M | re.S)
    if not match:
        sys.exit("could not read [workspace.package] version from Cargo.toml")
    return match.group(1)


def crate_versions(expected: str) -> list[str]:
    """Crates pin sibling path dependencies by version; all must match."""
    problems = []
    for manifest in sorted((REPO / "crates").glob("*/Cargo.toml")):
        if skipped(manifest):
            continue
        text = manifest.read_text()
        name = next(
            (line.split('"')[1] for line in text.splitlines() if line.startswith("name = ")),
            manifest.parent.name,
        )
        # `version.workspace = true` inherits and is always correct.
        own = re.search(r'^version = "([^"]+)"', text, re.M)
        if own and own.group(1) != expected:
            problems.append(f"{name}: package version {own.group(1)} != {expected}")
        for pinned in re.findall(r'path = "\.\./[^"]+", version = "([^"]+)"', text):
            if pinned != expected:
                problems.append(f"{name}: path dependency pinned at {pinned} != {expected}")
    return problems


def package_versions(expected: str) -> list[str]:
    problems = []
    for manifest in sorted((REPO / "packages").glob("*/package.json")):
        if skipped(manifest):
            continue
        data = json.loads(manifest.read_text())
        if data.get("version") != expected:
            problems.append(f"{data.get('name')}: {data.get('version')} != {expected}")
    return problems


def npm_scope() -> list[str]:
    """One scope. `@bobby-browser` is not an org we own; `@cavi-ai` is."""
    problems = []
    for manifest in sorted((REPO / "packages").glob("*/package.json")):
        name = json.loads(manifest.read_text()).get("name", "")
        if name.startswith("@") and not name.startswith("@cavi-ai/"):
            problems.append(f"{name}: not under the @cavi-ai scope")
    return problems


def publishable_crates() -> list[str]:
    """Only products go to crates.io. Publishing `types` or `config` under a
    generic name claims it permanently and leaks internal structure."""
    allowed = {"bobby-browser-client", "bobby-browser"}
    problems = []
    for manifest in sorted((REPO / "crates").glob("*/Cargo.toml")):
        text = manifest.read_text()
        name = next(
            (line.split('"')[1] for line in text.splitlines() if line.startswith("name = ")),
            manifest.parent.name,
        )
        publishes = "publish = false" not in text
        if publishes and name not in allowed:
            problems.append(f"{name}: publishable but not a published product")
    return problems


def main() -> int:
    expected = workspace_version()
    problems = (
        crate_versions(expected)
        + package_versions(expected)
        + npm_scope()
        + publishable_crates()
    )
    if problems:
        print(f"version/naming disagreement (workspace is {expected}):", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    print(f"all artifacts agree: {expected}, @cavi-ai scope, 2 publishable crates")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
