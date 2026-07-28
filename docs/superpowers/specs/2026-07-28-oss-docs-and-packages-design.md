# OSS Docs and Packages Design

## Objective

Make bobby-browser a professional public OSS project that is neat, easy to
understand, and easy to download or install. The end state is fully live across
GitHub (docs + Releases), npm, and crates.io, delivered as one phased program.

## Approach

Docs-first launch rail:

1. Polish README, package metadata, install/auth docs, and CLI first-run UX.
2. Publish `@bobby-browser/sdk` to npm.
3. Publish `bobby-browser` on crates.io (`cargo install` → `bobby`) plus curated
   public Rust libraries.
4. Ship multi-platform GitHub Release binaries.

The first implementation plan covers phases 1–2. Phases 3–4 remain part of this
design and get follow-on plans from the same roadmap.

## Goals

- Dual primary CTAs: run the runtime (`bobby`) and use TypeScript
  (`@bobby-browser/sdk`).
- Registry-curated publishes: public source does not imply every package lands
  on npm/crates.io.
- First-run auth that stays fail-closed: `bobby init` plus loopback-oriented
  local bootstrap generation.
- Consistent naming: product/release/crate `bobby-browser`; CLI invocation
  `bobby`.

## Non-Goals

- Redesigning the multi-principal security model.
- Publishing gauntlet, firefox-companion, or interface-conformance to npm until
  they have a clear consumer install story.
- Broad cleanup of private planning or repo noise beyond keeping secrets and
  private paths out of the public install story.
- Redesigning the hosted docs product at `cavi-ai.xyz` beyond keeping README and
  source-doc links accurate.

## Naming

| Role | Name |
|---|---|
| Product / GitHub / release assets / crates.io package | `bobby-browser` |
| CLI binary invocation | `bobby` (e.g. `bobby serve`, `bobby init`) |
| npm scope | `@bobby-browser/*` |
| Release asset pattern | `bobby-browser-<version>-<os>-<arch>` (or cargo-dist equivalent); archive contains the `bobby` binary |

The current workspace crate named `cli` with `publish = false` is an internal
packaging detail. Phase 3/4 introduces a publishable `bobby-browser` surface
that installs the `bobby` binary; phase 1 renames the invoked binary to `bobby`
for local builds and docs.

## Phases

| Phase | Outcome |
|---|---|
| 1 | README dual CTAs; root + SDK metadata; install/auth docs; CLI `bobby`; `bobby init` + loopback first-run secret path |
| 2 | `@bobby-browser/sdk` published to npm; README install commands are real |
| 3 | `bobby-browser` on crates.io + curated public Rust libs |
| 4 | GitHub Releases for macOS, Linux, and Windows (arm64 + x64) |

## Surfaces

| Surface | Consumer | Phase |
|---|---|---|
| README + community files | Humans on GitHub | 1 |
| Root `package.json` | Monorepo identity (`private: true` + OSS metadata) | 1 |
| Docs source under `docs/bobby-browser/` | Hosted docs + CONTRIBUTING | 1 |
| `@bobby-browser/sdk` | TypeScript clients via npm | 2 |
| `bobby-browser` crate | Rust via `cargo install bobby-browser` | 3 |
| Curated Rust libs | Embedders; internals stay `publish = false` | 3 |
| GitHub Releases | Binary downloaders | 4 |

### Registry policy

Publish only packages with a clear consumer install path.

**npm now (phase 2):** `@bobby-browser/sdk`.

**npm later (only when install story exists):** gauntlet, firefox-companion,
interface-conformance remain public in git and documented as build-from-source.

**crates.io (phase 3):** publishable `bobby-browser` binary crate plus a curated
library set. Default curated set unless a later plan narrows it: `sdk-core`,
`types`, and `interface-core` (only if each has a stable public API and no
private path deps that block publish). All other workspace crates remain
`publish = false`.

## CLI auth

Bootstrap continues to use the existing `StartupCredential` contract
(`AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN`, `AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL`,
`AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES`,
`AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT`). No parallel auth system.

### `bobby init`

1. Creates a local data directory under the OS/XDG app convention.
2. Generates a high-entropy bearer (≥256 bits), principal UUID, admin capability
   set including `authority:admin`, and RFC3339 expiry. Default TTL is 30 days
   from generation unless overridden by a CLI flag.
3. Writes a local secret file under the OS config/data dir
   (`$XDG_CONFIG_HOME/bobby-browser/bootstrap.env` on Linux, equivalent
   Application Support / known folder paths on macOS/Windows), mode `0600` where
   supported. File format is dotenv-compatible key=value for the four
   `AUTOMATION_RUNTIME_BOOTSTRAP_*` variables. Never writes plaintext into
   `config.toml`. Any repo-relative override path is gitignored.
4. Prints the plaintext bearer once, plus env export instructions. Documents how
   client examples map (`Authorization` bearer /
   `AUTOMATION_RUNTIME_TOKEN` for SDK clients — same bearer value when the
   client uses the bootstrap principal directly).

### First-run on `bobby serve`

If bootstrap env is missing and a local secret file already exists, load it. If
neither exists and the configured bind is loopback only (`127.0.0.1` /
`::1`), run the same generation path as `bobby init` automatically, print the
bearer once, then continue serve. If the bind is not loopback-only, refuse with
a clear error pointing to `bobby init` / required env vars — never auto-generate
for non-loopback binds.

`bobby serve` resolves bootstrap in order: process env (`StartupCredential::from_env`)
→ local secret file → loopback auto-init → error.

### Error handling

- Missing bootstrap on non-safe bind → fail closed; point to `bobby init`.
- Corrupt or unreadable secret file → fail closed; name the path; no empty-auth
  fallback.
- Re-`init` when secrets already exist → refuse by default; require `--force`.
  Document that regeneration invalidates the previous bearer for new enrollment
  and what happens to any existing authority store.
- Registry publish failure → stop the release; do not claim published in docs
  until the registry confirms.

Security invariants unchanged: fail closed; no credentials in URLs, query
strings, committed config, or logs; plaintext bearer shown at most once at
generation.

## README and docs (phase 1)

README structure:

1. Short product pitch.
2. Dual CTAs: run runtime (`bobby`) and use TypeScript (`@bobby-browser/sdk`).
3. Alpha banner with link to `SECURITY.md`.
4. Links to hosted docs, CONTRIBUTING, and key guides.

Until phase 4 ships, the runtime CTA documents the real path available at that
moment (local `bobby` / `cargo` build). Phase 4 flips the CTA to GitHub Releases
URLs without rewriting the whole README.

Update docs source pages (`installation`, `quickstart`, auth as needed) and
CONTRIBUTING/Makefile examples to `bobby` naming and the init flow. Keep hosted
docs links accurate (`https://cavi-ai.xyz/docs/bobby-browser`).

## Package metadata

### Root `package.json`

Remain `private: true`. Add description, license, repository, homepage, bugs,
and engines as needed for a professional monorepo root. Keep packageManager and
docs scripts.

### `@bobby-browser/sdk`

Ensure publish-ready fields: description, license, author, repository,
homepage, bugs, keywords, engines, files, exports, `publishConfig.access`.
Align version with the agreed release (`0.2.0` unless intentionally bumped).
Add changelog if missing.

## Testing

- `bobby init`: writes secret with expected permissions; refuses overwrite
  without `--force`; prints bearer once in controlled tests without leaking into
  CI logs as a committed fixture.
- `bobby serve`: fails without credentials on non-loopback; succeeds after init
  on loopback with generated secret.
- Docs: `pnpm docs:verify` and `pnpm docs:test` pass after command/path updates.
- npm: existing SDK tests gate CI; release workflow includes publish dry-run
  before live publish.
- Phase 4: smoke each Release asset with `bobby --help` / `bobby serve --help`
  on CI matrix runners where feasible.

## Release mechanics

### Phase 2

Tag- or manually-driven workflow publishes `@bobby-browser/sdk` only after CI
green. Success criterion: `npm view @bobby-browser/sdk` returns the published
version.

### Phase 3

Publish `bobby-browser` crate and curated libraries. Success criterion:
`cargo install bobby-browser` yields a working `bobby` binary.

### Phase 4

cargo-dist (or equivalent) builds macOS, Linux, and Windows for arm64 and x64.
README binary CTA points at Releases. Success criterion: a new visitor can
download, run `bobby init` / `bobby serve`, and hit `/healthz` without cloning.

## Phase 1–2 deliverables

- `README.md` dual CTAs and link hygiene.
- Root and SDK `package.json` metadata.
- Docs source + CONTRIBUTING + Makefile command updates.
- CLI binary name `bobby`; `init` + loopback first-run secret loader.
- `.gitignore` for local secret paths.
- CI gate + npm publish workflow for `@bobby-browser/sdk`.
- Live publish of `@bobby-browser/sdk`.

## Success criteria

- A new visitor can start the runtime or install the SDK from the README without
  tribal knowledge.
- After phase 2, `npm view @bobby-browser/sdk` works.
- After phase 3, `cargo install bobby-browser` works.
- After phase 4, multi-platform Releases install a working `bobby` binary.
- Security model remains fail-closed; no secrets in git, URLs, or logs.
