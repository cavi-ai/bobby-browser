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


def python_package_version(expected: str) -> list[str]:
    """`packages/python-sdk` ships as `pyproject.toml`, not `package.json`,
    so `package_versions()` above never sees it. Same one-tag-many-artifacts
    failure mode applies: check it here explicitly."""
    path = REPO / "packages" / "python-sdk" / "pyproject.toml"
    if not path.is_file():
        return [f"{path.relative_to(REPO)}: missing"]
    match = re.search(r'^version = "([^"]+)"', path.read_text(), re.M)
    if not match:
        return [f"{path.relative_to(REPO)}: no [project] version declared"]
    if match.group(1) != expected:
        return [f"bobby-browser (python): {match.group(1)} != {expected}"]
    init = REPO / "packages" / "python-sdk" / "bobby_browser" / "__init__.py"
    if not init.is_file():
        return [f"{init.relative_to(REPO)}: missing"]
    match = re.search(r'^__version__ = "([^"]+)"', init.read_text(), re.M)
    if not match:
        return [f"{init.relative_to(REPO)}: no __version__ declared"]
    if match.group(1) != expected:
        return [f"bobby_browser.__version__: {match.group(1)} != {expected}"]
    return []


def firefox_companion_extension_version(expected: str) -> list[str]:
    """Firefox about:addons shows packages/firefox-companion/manifest.json,
    not package.json. Keep them locked together."""
    path = REPO / "packages" / "firefox-companion" / "manifest.json"
    if not path.is_file():
        return [f"{path.relative_to(REPO)}: missing"]
    data = json.loads(path.read_text())
    version = data.get("version")
    if version != expected:
        return [f"firefox-companion manifest.json: {version} != {expected}"]
    return []


def formula_may_trail(formula_version: str, expected: str) -> bool:
    """The formula is at the release or at an earlier one no older than the
    previous minor line: 0.19.1 accepts 0.19.0, and 0.20.0 accepts 0.19.1."""
    try:
        formula = tuple(int(part) for part in formula_version.split("."))
        release = tuple(int(part) for part in expected.split("."))
    except ValueError:
        return False
    if len(formula) != 3 or len(release) != 3:
        return False
    return formula == release or (
        formula < release and formula[0] == release[0] and formula[1] >= release[1] - 1
    )


def homebrew_formula_version(expected: str) -> list[str]:
    """The formula may trail one minor release until its binary hashes exist.

    Binaries are built from the tag. Hashes guessed before that build would
    make the formula uninstallable, so the previous release remains available
    until the new assets are published and the formula is updated.
    """
    path = REPO / "Formula" / "bobby-browser.rb"
    if not path.is_file():
        return [f"{path.relative_to(REPO)}: missing"]
    formula = path.read_text()
    found = re.search(r'^\s*version "([^"]+)"', formula, re.M)
    if not found:
        return ["Formula/bobby-browser.rb: no version declared"]
    formula_version = found.group(1)
    try:
        major, minor, _ = (int(part) for part in expected.split("."))
    except ValueError:
        return [f"workspace version {expected} is not a stable version"]
    if not formula_may_trail(formula_version, expected):
        return [f"Formula/bobby-browser.rb: {formula_version} must be {expected} or an earlier release since {major}.{max(minor - 1, 0)}.0"]
    digests = re.findall(r'^\s*sha256 "([^"]+)"', formula, re.M)
    if len(digests) != 4 or any(not re.fullmatch(r"[a-f0-9]{64}", digest) for digest in digests):
        return ["Formula/bobby-browser.rb: expected four SHA-256 digests"]
    workspace_license = re.search(r'^license = "([^"]+)"', (REPO / "Cargo.toml").read_text(), re.M)
    formula_license = re.search(r'^\s*license "([^"]+)"', formula, re.M)
    if not workspace_license or not formula_license or formula_license.group(1) != workspace_license.group(1):
        return ["Formula/bobby-browser.rb: license must match the Cargo workspace license"]
    return []


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
        + python_package_version(expected)
        + firefox_companion_extension_version(expected)
        + homebrew_formula_version(expected)
        + npm_scope()
        + publishable_crates()
    )
    if problems:
        print(f"version/naming disagreement (workspace is {expected}):", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    print(f"release artifacts agree: {expected}; Homebrew may trail one minor until published binaries have verified hashes")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
