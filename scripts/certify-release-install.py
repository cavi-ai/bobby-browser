#!/usr/bin/env python3
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import zipfile


PROFILE_NAMES = {"desktop", "headless-ci", "openshell", "remote"}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--asset-os", choices=("linux", "macos", "windows"), required=True)
    parser.add_argument("--version", required=True)
    return parser.parse_args()


def safe_member(name: str) -> bool:
    path = Path(name)
    return not path.is_absolute() and ".." not in path.parts


def extract_archive(archive: Path, destination: Path) -> None:
    if archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as bundle:
            if not all(safe_member(member.filename) for member in bundle.infolist()):
                raise RuntimeError("archive contains an unsafe path")
            bundle.extractall(destination)
        return
    with tarfile.open(archive, "r:gz") as bundle:
        members = bundle.getmembers()
        if not all(
            safe_member(member.name) and (member.isfile() or member.isdir())
            for member in members
        ):
            raise RuntimeError("archive contains an unsafe entry")
        bundle.extractall(destination, filter="data")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def tree_hashes(root: Path) -> dict[str, str]:
    return {
        path.relative_to(root).as_posix(): sha256(path)
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def run_installer(repo_root: Path, asset_os: str, archive: Path, version: str, root: Path) -> None:
    env = os.environ.copy()
    env.update(
        {
            "BOBBY_ARCHIVE": str(archive.resolve()),
            "BOBBY_VERSION": version,
            "INSTALL_DIR": str(root / "bin"),
            "BOBBY_SHARE_DIR": str(root / "share" / "bobby-browser"),
        }
    )
    if asset_os == "windows":
        command = [
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            str(repo_root / "scripts" / "install.ps1"),
        ]
    else:
        command = ["bash", str(repo_root / "scripts" / "install.sh")]
    subprocess.run(command, env=env, check=True)


def verify_install(stage: Path, asset_os: str, version: str, root: Path) -> None:
    suffix = ".exe" if asset_os == "windows" else ""
    for name in ("bobby", "mcp-gateway", "acp-gateway"):
        source = stage / f"{name}{suffix}"
        installed = root / "bin" / f"{name}{suffix}"
        if not installed.is_file() or sha256(installed) != sha256(source):
            raise RuntimeError(f"installed binary mismatch: {name}{suffix}")
        if asset_os != "windows" and not os.access(installed, os.X_OK):
            raise RuntimeError(f"installed binary is not executable: {name}")

    share = root / "share" / "bobby-browser"
    pairs = (
        (stage / "scripts" / "vision-mlx", share / "scripts" / "vision-mlx"),
        (stage / "firefox-companion", share / "firefox-companion"),
    )
    for source, installed in pairs:
        expected = tree_hashes(source)
        actual = tree_hashes(installed)
        if actual != expected:
            raise RuntimeError(
                f"installed tree mismatch: {installed}; expected={sorted(expected)} actual={sorted(actual)}"
            )

    bobby = root / "bin" / f"bobby{suffix}"
    version_output = subprocess.run(
        [str(bobby), "--version"], check=True, capture_output=True, text=True
    ).stdout
    if version not in version_output:
        raise RuntimeError("installed bobby reports the wrong version")
    profile_output = subprocess.run(
        [str(bobby), "profiles", "--json"], check=True, capture_output=True, text=True
    ).stdout
    names = {profile["name"] for profile in json.loads(profile_output)}
    if names != PROFILE_NAMES:
        raise RuntimeError("installed bobby reports an incomplete deployment profile catalog")


def main() -> None:
    args = parse_args()
    archive = args.archive.resolve()
    repo_root = Path(__file__).resolve().parent.parent
    suffix = "zip" if args.asset_os == "windows" else "tar.gz"
    expected_name = f"bobby-browser-{args.version}-{args.asset_os}-"
    if not archive.name.startswith(expected_name) or not archive.name.endswith(f".{suffix}"):
        raise RuntimeError("archive name does not match version and operating system")

    with tempfile.TemporaryDirectory(prefix="bobby-release-cert-") as temp:
        root = Path(temp)
        extracted = root / "archive"
        extracted.mkdir()
        extract_archive(archive, extracted)
        stage_dirs = [path for path in extracted.iterdir() if path.is_dir()]
        if len(stage_dirs) != 1 or not stage_dirs[0].name.startswith(expected_name):
            raise RuntimeError("archive must contain one versioned stage directory")
        stage = stage_dirs[0]

        clean = root / "clean"
        run_installer(repo_root, args.asset_os, archive, args.version, clean)
        verify_install(stage, args.asset_os, args.version, clean)

        upgrade = root / "upgrade"
        binary_suffix = ".exe" if args.asset_os == "windows" else ""
        for name in ("bobby", "mcp-gateway", "acp-gateway"):
            old_binary = upgrade / "bin" / f"{name}{binary_suffix}"
            old_binary.parent.mkdir(parents=True, exist_ok=True)
            old_binary.write_bytes(b"old")
        stale_vision = upgrade / "share" / "bobby-browser" / "scripts" / "vision-mlx" / "stale.py"
        stale_companion = upgrade / "share" / "bobby-browser" / "firefox-companion" / "stale.js"
        operator_file = upgrade / "share" / "bobby-browser" / "operator-owned.txt"
        for path in (stale_vision, stale_companion, operator_file):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("old", encoding="utf-8")
        run_installer(repo_root, args.asset_os, archive, args.version, upgrade)
        verify_install(stage, args.asset_os, args.version, upgrade)
        if stale_vision.exists() or stale_companion.exists():
            raise RuntimeError("upgrade retained stale managed files")
        if operator_file.read_text(encoding="utf-8") != "old":
            raise RuntimeError("upgrade changed an operator-owned file")

    print(f"certified {archive.name}: clean install and upgrade")


if __name__ == "__main__":
    main()
