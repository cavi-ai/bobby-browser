# OSS Distribution Rail Design

## Objective

Finish the remaining OSS distribution program so a new visitor can install
bobby-browser without cloning: TypeScript via npm, the `bobby` CLI via crates.io
and/or GitHub Releases. This continues
[`2026-07-28-oss-docs-and-packages-design.md`](2026-07-28-oss-docs-and-packages-design.md)
phases 2–4 under a staged release train.

## Decisions (locked)

| Decision | Choice |
|---|---|
| Gap focus | Distribution (not skills API or CLI doctor depth) |
| Scope | npm + crates.io + GitHub Release binaries |
| Publish readiness | **Hybrid:** live npm is required; crates.io and Releases may land as green workflows first, then one maintainer publish pass |
| crates.io shape | **Binary now, libraries later** — supported consumer path is `cargo install bobby-browser` → `bobby`; do not document workspace crates as stable public libraries yet |
| Approach | Staged release train (npm → crates binary → Releases), not a mega-PR |

## Non-goals

- Public Bobby skills on HTTP/MCP/TS SDK
- Publishing `@bobby-browser/gauntlet`, firefox-companion, or interface-conformance to npm
- Stable, documented Rust library APIs (`types` / `sdk-core` / `interface-core` as curated libs)
- Redesigning auth, multi-principal, or the hosted docs product
- Claiming install URLs or registry versions in README/docs before the corresponding publish succeeds

## Current state

- Product version **0.2.1**; CLI binary already named `bobby`; `bobby init` / loopback bootstrap exist
- `@bobby-browser/sdk@0.2.1` is publish-ready; `.github/workflows/publish-npm.yml` exists; live publish previously failed on invalid `NPM_TOKEN`
- Every workspace Rust crate is `publish = false`; no crates.io package; no multi-platform Release binaries
- README still mentions `docs/bobby-browser/v0.2.0` in places while live docs line is 0.2.1

## Architecture

```text
Visitor CTAs
├── TypeScript  → npm i @bobby-browser/sdk          [Stage 1 — live required]
├── cargo       → cargo install bobby-browser       [Stage 2 — workflow then publish]
└── Binary      → GitHub Release asset → bobby      [Stage 3 — primary binary UX]
```

Shared rules:

- Version alignment: npm package, Cargo workspace version, and Release tags stay
  coherent (`0.2.1` / `v0.2.1` / `sdk-v0.2.1` as applicable)
- Security: no tokens in git; publish secrets only in CI/maintainer env; dry-run
  before live publish
- Docs: each CTA becomes “real” only after its success criterion passes

## Stage 1 — Live npm (`@bobby-browser/sdk`)

### Work

1. Maintainer sets a valid GitHub Actions secret `NPM_TOKEN` with publish rights
   to `@bobby-browser` (or performs an equivalent authenticated local publish).
2. Run the existing publish path: dry-run pack already in
   `.github/workflows/publish-npm.yml`, then publish via `sdk-v0.2.1` tag or
   `workflow_dispatch`.
3. Verify: `npm view @bobby-browser/sdk version` returns `0.2.1` (or the version
   intentionally published).
4. Confirm README / installation docs use `npm install @bobby-browser/sdk`
   without “when available” hedges.

### Success criterion

`npm view @bobby-browser/sdk` succeeds for the published version. **This stage
blocks calling the overall program done.**

### Failure handling

If the token is still invalid, stop Stage 1; do not advance README claims; do not
pretend Stages 2–3 complete the TS CTA.

## Stage 2 — crates.io binary (`bobby-browser` → `bobby`)

### Problem

The `cli` crate depends on a large path-dep graph. `cargo publish` of a single
binary crate cannot succeed while those path deps remain unpublished.

### Resolution (binary now, libs later)

1. Rename the **published package name** for the CLI surface to `bobby-browser`
   (directory may remain `crates/cli`; binary name stays `bobby`).
2. Set `publish = true` on the **install closure** — every path dependency
   required to build that binary — so `cargo install bobby-browser` compiles
   from crates.io.
3. Resolve the **vendored `chromiumoxide` patch** (`[patch.crates-io]` →
   `vendor/chromiumoxide`) so the published graph does not rely on an
   unpublished path patch. Preferred options in order: (a) publish the fork
   under a distinct crates.io name and depend on it, or (b) upstream the
   needed changes and drop the patch. Do not leave `cargo install` dependent
   on a git checkout of this monorepo.
4. Explicitly **do not** document those transitive crates as a supported public
   library API. README and docs state: use `cargo install bobby-browser` or
   Release binaries; depending on internal crates is unsupported until a later
   curated-libs program.
5. Add CI workflow: `cargo publish --dry-run` (ordered) for the closure on tag
   or `workflow_dispatch`. Live publish is the hybrid **manual/maintainer pass**
   after merge (crates.io API token).
6. Keep gauntlet/test-only/publish-never crates `publish = false` when they are
   not in the binary closure.

### Success criteria

- Dry-run publish of the closure succeeds in CI
- After the maintainer publish pass: `cargo install bobby-browser` yields a
  working `bobby --help` (and can run `bobby init` / `bobby serve` with normal
  browser prerequisites)

### Explicit deferral

Curated, documented library publishes (`types`, `sdk-core`, `interface-core` as
stable APIs) remain a follow-on design.

## Stage 3 — GitHub Release binaries

### Work

1. Adopt **cargo-dist** (preferred) or an equivalent thin matrix that builds
   `bobby` for:
   - macOS arm64, macOS x64
   - Linux arm64, Linux x64
   - Windows arm64, Windows x64
2. Tag convention: `v0.2.1` (or matching workspace version) produces Release
   assets named consistently with the OSS design, e.g.
   `bobby-browser-<version>-<os>-<arch>`, archive containing the `bobby`
   binary.
3. CI smoke where feasible: unpack asset → `bobby --help` (full
   `init`/`serve`/`/healthz` on runners that have a browser is optional, not
   blocking for asset publish).
4. README dual CTA: add “Download release binary” pointing at the Releases page
   **only after** at least one successful multi-platform Release exists.

### Success criterion

A visitor can download a Release asset for their platform, run `bobby init` and
`bobby serve`, and hit `/healthz` without cloning (browser binary still required
on the machine as today).

## Docs and CTA gates

| Surface | When README/docs may claim it |
|---|---|
| `npm install @bobby-browser/sdk` | After Stage 1 `npm view` succeeds |
| `cargo install bobby-browser` | After Stage 2 live crates.io publish |
| Download Release binary | After Stage 3 assets exist for the advertised platforms |

Also:

- Fix stale `v0.2.0` artifact path references to the current docs line (`0.2.1`
  / `v0.2.1` as appropriate)
- Installation + quickstart pages list all three install paths with the gates
  above
- Do not invent new auth; keep `bobby init` / bootstrap env contract

## Testing

| Stage | Gate |
|---|---|
| 1 | SDK unit tests + `npm pack --dry-run`; post-publish `npm view` |
| 2 | `cargo check -p cli` / workspace CI; ordered `cargo publish --dry-run`; post-publish `cargo install` smoke |
| 3 | cargo-dist plan/build on CI; per-asset `bobby --help` smoke |
| Docs | `pnpm docs:build && pnpm docs:verify && pnpm docs:test` after CTA/path edits |

## Release mechanics summary

| Artifact | Trigger | Secret | Live when |
|---|---|---|---|
| `@bobby-browser/sdk` | `sdk-v*` tag or `workflow_dispatch` | `NPM_TOKEN` | **Required in this program** |
| `bobby-browser` + install closure | tag / `workflow_dispatch` dry-run; maintainer publish | crates.io token | Manual pass after workflows green |
| GitHub Release binaries | `v*` tag via cargo-dist | `GITHUB_TOKEN` / Release permissions | Workflow first; announce when assets exist |

## Implementation order

1. Stage 1 npm live (blocker for “done”)
2. Stage 2 package rename + publish-closure + dry-run workflow
3. Stage 3 cargo-dist + Release smoke
4. Docs/README CTA updates gated on each stage’s success
5. Maintainer publish pass for crates.io (and Release tag if not already cut)

## Success criteria (program)

- `npm view @bobby-browser/sdk` works
- Green crates.io dry-run for the binary install closure; after publish pass,
  `cargo install bobby-browser` works
- Multi-platform GitHub Release assets install a working `bobby` binary
- README dual CTAs are truthful for npm and for at least one real binary path
  (Releases and/or crates.io)
- Security model unchanged: fail closed; no secrets in git, URLs, or logs
