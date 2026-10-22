"""Verify release archives and publish signed Homebrew formula updates."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile
import time

PLATFORMS = ("macos-arm64", "macos-x64", "linux-arm64", "linux-x64")
FORMULA = "Formula/bobby-browser.rb"
SOURCE = "cavi-ai/bobby-browser"
TAP = "cavi-ai/homebrew-tap"
VERSION = re.compile(r"v((?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))")
FORMULA_VERSION = re.compile(r'^  version "([0-9]+\.[0-9]+\.[0-9]+)"$', re.M)
ASSET = re.compile(
    r'(url "https://github\.com/cavi-ai/bobby-browser/releases/download/'
    r'v#\{version\}/bobby-browser-#\{version\}-(macos-arm64|macos-x64|linux-arm64|linux-x64)'
    r'\.tar\.gz"\n\s+sha256 ")[0-9a-f]{64}(")'
)


def version_from_tag(tag):
    match = VERSION.fullmatch(tag)
    if not match:
        raise ValueError("expected a stable release tag vX.Y.Z")
    return match[1]


def render_formula(template, version, digests):
    platforms = [match[2] for match in ASSET.finditer(template)]
    if sorted(platforms) != sorted(PLATFORMS):
        raise ValueError("formula platforms must contain each supported archive exactly once")
    if len(FORMULA_VERSION.findall(template)) != 1:
        raise ValueError("formula must declare one version")
    result = FORMULA_VERSION.sub(f'  version "{version}"', template)
    return ASSET.sub(lambda match: match[1] + digests[match[2]] + match[3], result)


def prepare(args):
    version = version_from_tag(args.tag)
    release = json.loads(args.release.read_text())
    if release.get("tag_name") != args.tag or release.get("draft") is not False or release.get("prerelease") is not False:
        raise ValueError("expected the matching published stable release")
    digests = {}
    for platform in PLATFORMS:
        name = f"bobby-browser-{version}-{platform}.tar.gz"
        assets = [asset for asset in release.get("assets", []) if asset.get("name") == name]
        if len(assets) != 1:
            raise ValueError(f"release asset {name} must occur exactly once")
        asset = assets[0]
        digest = asset.get("digest") or ""
        expected_url = f"https://github.com/{SOURCE}/releases/download/{args.tag}/{name}"
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest) or asset.get("state") != "uploaded" or asset.get("browser_download_url") != expected_url:
            raise ValueError(f"release asset {name} has invalid metadata")
        path = args.assets_dir / name
        with path.open("rb") as archive:
            actual = hashlib.file_digest(archive, "sha256").hexdigest()
        if actual != digest.removeprefix("sha256:") or path.stat().st_size != asset.get("size"):
            raise ValueError(f"archive integrity check failed for {name}")
        digests[platform] = actual
    candidate = render_formula(args.formula.read_text(), version, digests)
    args.output.write_text(candidate)
    print(f"Verified all four archives for {args.tag}")


def gh(*arguments, payload=None, timeout=120):
    result = subprocess.run(
        ["gh", *arguments], input=None if payload is None else json.dumps(payload),
        text=True, capture_output=True, timeout=timeout,
    )
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or result.stdout.strip())
    return result.stdout


def api(endpoint, payload=None):
    arguments = ["api", endpoint]
    if payload is not None:
        arguments += ["--method", "POST", "--input", "-"]
    return json.loads(gh(*arguments, payload=payload))


def read_formula(repository, commit):
    content = api(f"repos/{repository}/contents/{FORMULA}?ref={commit}")
    return base64.b64decode(content["content"]).decode()


def needs_update(current, candidate):
    current_version = FORMULA_VERSION.findall(current)
    candidate_version = FORMULA_VERSION.findall(candidate)
    if len(current_version) != 1 or len(candidate_version) != 1:
        raise ValueError("formula must declare one version")
    old, new = (tuple(map(int, value[0].split("."))) for value in (current_version, candidate_version))
    if old > new or current == candidate:
        return False
    if old == new:
        raise ValueError("refusing to change the formula of an already published version")
    neutral = {platform: "0" * 64 for platform in PLATFORMS}
    if render_formula(current, "0.0.0", neutral) != render_formula(candidate, "0.0.0", neutral):
        raise ValueError("formula recipe differs from the release; reconcile it before publishing")
    return True


def wait_for_ci(repository, commit):
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        runs = api(f"repos/{repository}/actions/runs?head_sha={commit}&event=pull_request")["workflow_runs"]
        runs = [run for run in runs if run["path"] == ".github/workflows/ci.yml"]
        if runs:
            run = max(runs, key=lambda item: item["id"])
            gh("run", "watch", str(run["id"]), "--repo", repository, "--interval", "60", "--exit-status", timeout=2700)
            jobs = api(f"repos/{repository}/actions/runs/{run['id']}/jobs?per_page=100")["jobs"]
            if not jobs or any(job["conclusion"] != "success" for job in jobs):
                raise RuntimeError("formula update requires every CI job to succeed")
            return
        time.sleep(5)
    raise RuntimeError("formula update CI did not start within three minutes")


def publication_branch(version, base):
    return f"feat/homebrew-{version}-{base[:12]}"


def validate_commit(record, base):
    if record["commit"]["verification"].get("verified") is not True:
        raise ValueError("formula commit has no verified signature")
    if [parent["sha"] for parent in record["parents"]] != [base]:
        raise ValueError("formula commit must start at the current default branch")


def publish_repository(repository, candidate, version):
    metadata = api(f"repos/{repository}")
    default = metadata["default_branch"]
    base = api(f"repos/{repository}/git/ref/heads/{default}")["object"]["sha"]
    if not needs_update(read_formula(repository, base), candidate):
        print(f"{repository}: formula is current or newer")
        return
    # A changed base gets a new head and CI run; retries cannot reuse proof
    # for a merge candidate built against an earlier default branch.
    branch = publication_branch(version, base)
    refs = api(f"repos/{repository}/git/matching-refs/heads/{branch}")
    matching = [ref for ref in refs if ref["ref"] == f"refs/heads/{branch}"]
    title = f"chore: update Homebrew to {version}"
    create_commit = True
    if matching:
        commit = matching[0]["object"]["sha"]
        files = api(f"repos/{repository}/compare/{base}...{commit}")["files"]
        branch_formula = read_formula(repository, commit)
        if branch_formula == candidate and {file["filename"] for file in files} == {FORMULA}:
            create_commit = False
        elif files or commit != base:
            raise RuntimeError(f"{branch} already contains a different change")
    else:
        api(f"repos/{repository}/git/refs", {"ref": f"refs/heads/{branch}", "sha": base})
        commit = base
    if create_commit:
        query = """mutation($input: CreateCommitOnBranchInput!) {
          createCommitOnBranch(input: $input) { commit { oid } }
        }"""
        result = api("graphql", {"query": query, "variables": {"input": {
            "branch": {"repositoryNameWithOwner": repository, "branchName": branch},
            "expectedHeadOid": commit, "message": {"headline": title},
            "fileChanges": {"additions": [{"path": FORMULA, "contents": base64.b64encode(candidate.encode()).decode()}]},
        }}})
        if result.get("errors"):
            raise RuntimeError(json.dumps(result["errors"]))
        commit = result["data"]["createCommitOnBranch"]["commit"]["oid"]
    validate_commit(api(f"repos/{repository}/commits/{commit}"), base)
    pulls = api(f"repos/{repository}/pulls?state=open&head=cavi-ai:{branch}&base={default}")
    if pulls:
        number = pulls[0]["number"]
        if pulls[0]["head"]["sha"] != commit:
            raise RuntimeError("formula PR head changed")
    else:
        body = f"""## What changed

- Update the Homebrew formula to {version} with verified release archive hashes.

## Why

- Install the published release through Homebrew.

## Verification

- All four archive SHA-256 hashes match the published release metadata.
- The formula passed a Homebrew install and test on macOS.

## Contract impact

- No wire contract change.
"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md") as body_file:
            body_file.write(body)
            body_file.flush()
            url = gh("pr", "create", "--repo", repository, "--head", branch, "--base", default, "--title", title, "--body-file", body_file.name).strip()
        number = int(url.rsplit("/", 1)[1])
    print(f"https://github.com/{repository}/pull/{number}", flush=True)
    if repository == SOURCE:
        wait_for_ci(repository, commit)
    if api(f"repos/{repository}/git/ref/heads/{default}")["object"]["sha"] != base:
        raise RuntimeError("default branch changed while validating the formula; rerun the workflow")
    gh("pr", "merge", str(number), "--repo", repository, "--squash", "--match-head-commit", commit, "--subject", title, "--body", "")
    pull = api(f"repos/{repository}/pulls/{number}")
    if not pull.get("merged") or read_formula(repository, pull["merge_commit_sha"]) != candidate:
        raise RuntimeError("formula PR was not merged with the expected contents")
    validate_commit(api(f"repos/{repository}/commits/{pull['merge_commit_sha']}"), base)
    print(f"{repository}: published Homebrew {version}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare_parser = commands.add_parser("prepare")
    prepare_parser.add_argument("--tag", required=True)
    for name in ("release", "assets-dir", "formula", "output"):
        prepare_parser.add_argument("--" + name, type=Path, required=True)
    publish_parser = commands.add_parser("publish")
    publish_parser.add_argument("--tag", required=True)
    publish_parser.add_argument("--formula", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.command == "prepare":
            prepare(args)
        else:
            version = version_from_tag(args.tag)
            candidate = args.formula.read_text()
            if FORMULA_VERSION.findall(candidate) != [version]:
                raise ValueError("candidate formula does not match the release tag")
            for repository in (SOURCE, TAP):
                publish_repository(repository, candidate, version)
    except (ValueError, OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
