#!/usr/bin/env python3
"""Fail closed unless the exact release tag passed CI and has release assets."""
import argparse
import json
import os
import re
import subprocess
import sys
import time

REQUIRED_JOBS = {'rust · fmt · clippy', 'rust · test', 'node · typescript · docs'}


def api(path):
    return json.loads(subprocess.check_output(['gh', 'api', path], text=True))


def ci_result(runs, tag, sha, jobs_for):
    matches = [r for r in runs if r['event'] == 'push' and r['head_branch'] == tag]
    if not matches:
        return 'pending'
    run = max(matches, key=lambda r: (r['run_number'], r['run_attempt']))
    if run['head_sha'] != sha:
        return 'failed'
    if run['status'] != 'completed':
        return 'pending'
    if run['conclusion'] != 'success':
        return 'failed'
    jobs = jobs_for(run['id'])
    # The latest attempt must include each required job, all successful.
    by_name = {job['name']: job for job in jobs}
    return 'success' if REQUIRED_JOBS <= by_name.keys() and all(by_name[n]['conclusion'] == 'success' for n in REQUIRED_JOBS) else 'failed'


def check_assets(release, tag):
    if release['draft'] or release['prerelease'] or release['tag_name'] != tag:
        return False
    v = tag[1:]
    names = {a['name'] for a in release['assets'] if a['state'] == 'uploaded'}
    expected = {f'bobby-browser-{v}-{platform}.{ext}' for platform, ext in [
        ('linux-x64', 'tar.gz'), ('linux-arm64', 'tar.gz'), ('macos-arm64', 'tar.gz'),
        ('macos-x64', 'tar.gz'), ('windows-x64', 'zip')]}
    expected |= {f'bobby-browser-docs-{tag}.tar.gz', f'bobby-browser-docs-{tag}.tar.gz.sha256'}
    return expected <= names


def tag_commit(repo, tag, get=api):
    obj = get(f'repos/{repo}/git/ref/tags/{tag}')['object']
    # Annotated tags may point to another annotated tag. Bound traversal.
    for _ in range(10):
        if obj['type'] == 'commit':
            return obj['sha']
        if obj['type'] != 'tag':
            break
        obj = get(f"repos/{repo}/git/tags/{obj['sha']}")['object']
    raise ValueError(f'{tag} does not resolve to a commit')


def require_checkout(tag, sha, checkout_sha):
    if checkout_sha != sha:
        raise ValueError(f'checkout {checkout_sha} does not match {tag} at {sha}')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('tag')
    parser.add_argument('--assets', action='store_true')
    parser.add_argument('--output-sha', action='store_true')
    parser.add_argument('--timeout', type=int, default=1800)
    args = parser.parse_args()
    if not re.fullmatch(r'v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)', args.tag):
        sys.exit('invalid release tag')
    repo = os.environ['GITHUB_REPOSITORY']
    sha = tag_commit(repo, args.tag)
    checkout_sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    require_checkout(args.tag, sha, checkout_sha)
    deadline = time.monotonic() + args.timeout
    while True:
        # Query by workflow and tag, and check the full commit and event explicitly.
        runs = api(f'repos/{repo}/actions/workflows/ci.yml/runs?branch={args.tag}&event=push&per_page=100')['workflow_runs']
        result = ci_result(runs, args.tag, sha, lambda run: api(f'repos/{repo}/actions/runs/{run}/jobs?per_page=100')['jobs'])
        if result == 'success':
            break
        if result == 'failed' or time.monotonic() >= deadline:
            sys.exit(f'exact-tag CI {result} for {args.tag} at {sha}')
        time.sleep(15)
    if args.assets and not check_assets(api(f'repos/{repo}/releases/tags/{args.tag}'), args.tag):
        sys.exit(f'release assets incomplete for {args.tag}')
    if args.output_sha:
        with open(os.environ['GITHUB_OUTPUT'], 'a', encoding='utf-8') as output:
            output.write(f'sha={sha}\n')
    print(f'exact-tag CI passed for {args.tag} at {sha}' + ('; release assets present' if args.assets else ''))


if __name__ == '__main__':
    main()
