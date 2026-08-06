# Upstream Regression Hardening Design

## Scope

Repair six verified regressions on `d49d1fa` and add tests that fail on the
current implementations. The changes are limited to durable context identity,
context retention and locking, scheduler compaction, and Firefox native-host
installation ownership and rollback.

## Context identity

Site identity must follow the Public Suffix List, including its private
domains section. Unrelated tenants such as `alice.github.io` and
`bob.github.io` must never share a site key, while ordinary subdomains such as
`app.example.com` and `www.example.com` should continue to share
`example.com`. IP literals and single-label hosts remain keyed as-is.

Profile directory names must use an injective filesystem encoding. Distinct
profile IDs, including `a/b` and `a_b`, must resolve to distinct directories.
The original profile ID is not persisted in context records.

## Context locking and retention

The store will use an advisory lock held by an open file descriptor rather
than treating lockfile existence as ownership. Concurrent writers for the same
profile remain rejected, but an uncleanly terminated writer cannot block later
processes. Lock acquisition must reject unsafe non-file lock paths and avoid
following symbolic links on Unix.

Opening a context store will apply `context.ttl_days` before promotion becomes
available. The runtime builder supplies the configured TTL and the store sweep
uses the current UTC day. Sweep failures disable durable promotion for that
runtime rather than allowing an unbounded-retention configuration to appear
active.

## Scheduler compaction

Journal compaction must preserve every pending or running job and the newest
terminal jobs up to the retention bound. Terminal recency is ordered by
`completed_at`, falling back to `created_at` for malformed or legacy records.
Tests assert the exact retained and removed job IDs after reopening the
compacted journal.

## Native-host ownership and rollback

A wrapper is Bobby-managed only when it parses as the exact generated wrapper:
one `#!/bin/sh` line followed by one `exec <quoted-cli> firefox-native-host
--descriptor <quoted-descriptor>` line and a final newline. Comments, extra
commands, trailing content, missing `exec`, and malformed quoting are
operator-owned and must never be overwritten.

Replacement rollback records the original bytes and original Unix permission
mode. Rollback returns a result. If manifest installation and wrapper rollback
both fail, the returned error reports both failures; it must never claim only
the manifest failure while silently leaving a mutated wrapper.

## Regression matrix

- Public/private suffix tenant isolation and ordinary-subdomain collapsing.
- Collision-free profile directory encoding.
- Concurrent writer rejection and recovery with a pre-existing stale lockfile.
- Runtime construction applying configured TTL to persisted records.
- Compaction retaining exact newest terminal job IDs and all active jobs.
- Bobby wrapper acceptance plus rejection of comment and lookalike scripts.
- Rollback restoring exact bytes and permissions.
- Rollback failure surfaced together with the primary installation failure.

Every production change follows a red-green cycle: its regression test must
fail for the verified current behavior before implementation and pass
afterward. Focused crate suites run after each slice; final verification runs
formatting, linting, and the applicable workspace tests.
