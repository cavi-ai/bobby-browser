# OSS Docs and Packages (Phases 1–2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a professional OSS front door (README + metadata + `bobby` CLI with `init`/loopback first-run) and publish `@bobby-browser/sdk` to npm.

**Architecture:** Keep the existing `StartupCredential` env contract. Add a small `bootstrap_local` module in the `cli` crate that generates/loads a dotenv secret file, rename the binary to `bobby`, update docs to dual CTAs, then add a tag-driven npm publish workflow and publish the SDK.

**Tech Stack:** Rust workspace (`cli` crate), Tokio, chrono, getrandom, dirs, TypeScript/pnpm monorepo, GitHub Actions, npm.

**Spec:** `docs/superpowers/specs/2026-07-28-oss-docs-and-packages-design.md`

## Global Constraints

- CLI invocation name is `bobby`; product/release name remains `bobby-browser`.
- Bootstrap uses only `AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN`, `AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL`, `AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES`, `AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT`.
- Default bootstrap TTL is 30 days; bearer entropy ≥256 bits; bearer must satisfy broker bearer rules (32–480 printable ASCII bytes).
- Secret file: dotenv under OS config dir (`…/bobby-browser/bootstrap.env`), mode `0600` where supported; never write plaintext into `config.toml`.
- Serve bootstrap order: process env → local secret file → loopback auto-init → error.
- Auto-generate only when `server.host` is loopback (`127.0.0.1` or `::1`).
- Root `package.json` stays `private: true`. Publish only `@bobby-browser/sdk` in this plan.
- Do not publish gauntlet, firefox-companion, or interface-conformance.
- Do not implement crates.io or GitHub Releases (phases 3–4).
- Public docs source lives under `docs/bobby-browser/source/`; regenerate `v0.2.0` only via `pnpm docs:build`.
- No secrets in git, URLs, logs, or committed fixtures.

## File Structure

| File | Responsibility |
|---|---|
| `crates/cli/src/bootstrap_local.rs` | Generate/load/write bootstrap secret; loopback host check; path helpers |
| `crates/cli/src/main.rs` | Wire `init` / `serve` resolution; keep existing serve path |
| `crates/cli/src/bin/cli.rs` | Binary entry (unchanged logic) |
| `crates/cli/Cargo.toml` | Rename `[[bin]]` to `bobby`; add deps |
| `.gitignore` | Ignore local bootstrap override paths |
| `README.md` | Dual CTAs + alpha + links |
| `CONTRIBUTING.md`, `Makefile`, `config.toml` comments | `bobby` naming |
| `docs/bobby-browser/source/pages/**` | Install/quickstart/auth/run updates |
| `package.json` | Root OSS metadata |
| `packages/typescript-sdk/package.json` | Publish metadata gaps |
| `packages/typescript-sdk/CHANGELOG.md` | 0.2.0 notes |
| `packages/typescript-sdk/README.md` | Short install blurb |
| `.github/workflows/publish-npm.yml` | Dry-run + publish `@bobby-browser/sdk` |

---

### Task 1: Bootstrap local module (TDD)

**Files:**
- Create: `crates/cli/src/bootstrap_local.rs`
- Modify: `crates/cli/src/main.rs` (module declare)
- Modify: `crates/cli/Cargo.toml` (deps: `chrono`, `dirs`, `getrandom`, `hex`)
- Modify: `Cargo.toml` (add `dirs` under `[workspace.dependencies]` if missing)
- Test: unit tests inside `bootstrap_local.rs` under `#[cfg(test)]`

**Interfaces:**
- Consumes: `broker::StartupCredential`, `chrono::{Duration, Utc}`, `uuid::Uuid`, `types::Capability`
- Produces:
  - `pub struct BootstrapMaterial` with Debug redacting the bearer; accessors `bearer()`, `principal_id()`, `capabilities_csv()`, `expires_at()`
  - `pub fn default_bootstrap_path() -> anyhow::Result<PathBuf>`
  - `pub fn generate_bootstrap(ttl: Duration) -> anyhow::Result<BootstrapMaterial>`
  - `pub fn write_bootstrap_env(path: &Path, material: &BootstrapMaterial, force: bool) -> anyhow::Result<()>`
  - `pub fn load_startup_from_env_file(path: &Path) -> anyhow::Result<StartupCredential>`
  - `pub fn is_loopback_host(host: &str) -> bool`
  - `pub const DEFAULT_TTL_DAYS: i64 = 30`
  - Default capabilities CSV: every `Capability` wire string including `authority:admin`, comma-separated with no spaces

- [ ] **Step 1: Add dependencies**

In root `Cargo.toml` under `[workspace.dependencies]`:

```toml
dirs = "6"
```

In `crates/cli/Cargo.toml` dependencies:

```toml
chrono.workspace = true
dirs.workspace = true
getrandom.workspace = true
hex.workspace = true
```

In `crates/cli/Cargo.toml` dev-dependencies:

```toml
tempfile.workspace = true
```

- [ ] **Step 2: Create module with failing tests**

Create `crates/cli/src/bootstrap_local.rs`. Declare `mod bootstrap_local;` near the top of `crates/cli/src/main.rs`.

Include these tests (implementation may still be stubs that fail to compile or panic until Step 4):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn generate_bootstrap_meets_bearer_rules() {
        let material = generate_bootstrap(chrono::Duration::days(DEFAULT_TTL_DAYS)).unwrap();
        assert!(material.bearer().len() >= 32);
        assert!(material.capabilities_csv().contains("authority:admin"));
        assert!(material.capabilities_csv().contains("session:read"));
    }

    #[test]
    fn write_refuses_existing_without_force() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        let material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        write_bootstrap_env(&path, &material, false).unwrap();
        let err = write_bootstrap_env(&path, &material, false).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn write_force_overwrites_and_load_succeeds() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        let first = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        write_bootstrap_env(&path, &first, false).unwrap();
        let second = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        write_bootstrap_env(&path, &second, true).unwrap();
        load_startup_from_env_file(&path).unwrap();
    }

    #[test]
    fn loopback_hosts() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("192.168.1.1"));
    }

    #[test]
    fn debug_redacts_bearer() {
        let material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        let rendered = format!("{material:?}");
        assert!(!rendered.contains(material.bearer()));
        assert!(rendered.contains("REDACTED"));
    }

    #[test]
    fn corrupt_file_errors_with_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        std::fs::write(&path, "NOT_A_VALID=file\n").unwrap();
        let err = load_startup_from_env_file(&path).unwrap_err();
        assert!(err.to_string().contains(path.display().to_string().as_str()));
    }
}
```

- [ ] **Step 3: Run tests to confirm failure**

```bash
cargo test -p cli bootstrap_local -- --nocapture
```

Expected: compile failure or test failure until implementation exists.

- [ ] **Step 4: Implement `bootstrap_local.rs`**

1. `generate_bootstrap`: 32 random bytes via `getrandom::fill`, encode with `hex::encode` (64 hex chars). Principal: `Uuid::new_v4()`. Capabilities: collect every `Capability` variant via an explicit list of `Capability::*` values and join `as_str()` with `,`. Expiry: `Utc::now() + ttl`.
2. `write_bootstrap_env`: create parent dirs; if path exists and `!force`, return error containing `already exists` and instruct `--force`. Write dotenv lines for the four `AUTOMATION_RUNTIME_BOOTSTRAP_*` keys. `EXPIRES_AT` is RFC3339. On Unix set mode `0o600`.
3. `load_startup_from_env_file`: parse KEY=VALUE (skip blank/`#` lines); require all four keys; map capabilities CSV to `Vec<Capability>` (same strings as broker); call `StartupCredential::new`. Errors must include the path.
4. `default_bootstrap_path`: `dirs::config_dir()?.join("bobby-browser").join("bootstrap.env")`.
5. `is_loopback_host`: exact match `127.0.0.1` or `::1`.
6. `BootstrapMaterial` Debug must redact the bearer.

Capability list for default CSV (explicit match arms / array — do not rely on reflection):

```rust
const DEFAULT_CAPABILITIES: &[Capability] = &[
    Capability::SessionRead,
    Capability::SessionWrite,
    Capability::PageRead,
    Capability::PageWrite,
    Capability::BrowserMutate,
    Capability::FileUpload,
    Capability::FileDownload,
    Capability::JavascriptEvaluate,
    Capability::IntentExecute,
    Capability::VisionAssist,
    Capability::ArtifactRead,
    Capability::ArtifactCapture,
    Capability::RecoveryRead,
    Capability::RecoveryWrite,
    Capability::AuthorityAdmin,
];
```

- [ ] **Step 5: Re-run unit tests**

```bash
cargo test -p cli bootstrap_local -- --nocapture
```

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/cli/Cargo.toml crates/cli/src/bootstrap_local.rs crates/cli/src/main.rs
git commit -m "$(cat <<'EOF'
feat(cli): add local bootstrap env generate and load

EOF
)"
```

---

### Task 2: Wire `init` and serve bootstrap resolution

**Files:**
- Modify: `crates/cli/src/bootstrap_local.rs` (add `ResolveOutcome` + `resolve_startup_credential_with`)
- Modify: `crates/cli/src/main.rs` (`run` match arms)

**Interfaces:**
- Consumes: Task 1 APIs, `AppConfig::load`, `broker::StartupCredential::from_env`
- Produces:
  - `pub enum ResolveOutcome { FromEnv(StartupCredential), FromFile(StartupCredential), Generated { credential: StartupCredential, material: BootstrapMaterial } }`
  - `pub fn resolve_startup_credential_with(host: &str, bootstrap_path: &Path, from_env: F) -> anyhow::Result<ResolveOutcome>` where `F: FnOnce() -> Result<StartupCredential, broker::StartupCredentialError>`
  - `init` args: `--force`, `--ttl-days <u32>` (default 30), `--path <path>` (default `default_bootstrap_path()`)

Resolution algorithm:

1. If `from_env()` returns `Ok`, return `FromEnv`.
2. Else if `bootstrap_path` exists, `load_startup_from_env_file` → `FromFile` (propagate corrupt-file errors; do not fall through).
3. Else if `is_loopback_host(host)`, generate + write (`force=false`) → `Generated`.
4. Else error: message must contain `bobby init` and mention required `AUTOMATION_RUNTIME_BOOTSTRAP_*` env vars.

- [ ] **Step 1: Write failing resolution tests**

```rust
#[test]
fn resolve_prefers_process_env() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bootstrap.env");
    let file_material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
    write_bootstrap_env(&path, &file_material, false).unwrap();
    let env_material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
    let env_cred = broker::StartupCredential::new(
        env_material.bearer().to_string(),
        types::PrincipalId::from_uuid(env_material.principal_id()),
        DEFAULT_CAPABILITIES.to_vec(),
        env_material.expires_at(),
    ).unwrap();
    let outcome = resolve_startup_credential_with("127.0.0.1", &path, || Ok(env_cred)).unwrap();
    assert!(matches!(outcome, ResolveOutcome::FromEnv(_)));
}

#[test]
fn resolve_loads_file_when_env_missing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bootstrap.env");
    let material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
    write_bootstrap_env(&path, &material, false).unwrap();
    let outcome = resolve_startup_credential_with("127.0.0.1", &path, || {
        Err(broker::StartupCredentialError::MissingInput)
    }).unwrap();
    assert!(matches!(outcome, ResolveOutcome::FromFile(_)));
}

#[test]
fn resolve_autogens_on_loopback_when_missing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bootstrap.env");
    assert!(!path.exists());
    let outcome = resolve_startup_credential_with("127.0.0.1", &path, || {
        Err(broker::StartupCredentialError::MissingInput)
    }).unwrap();
    assert!(matches!(outcome, ResolveOutcome::Generated { .. }));
    assert!(path.exists());
}

#[test]
fn resolve_errors_on_non_loopback_when_missing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bootstrap.env");
    let err = resolve_startup_credential_with("0.0.0.0", &path, || {
        Err(broker::StartupCredentialError::MissingInput)
    }).unwrap_err();
    assert!(err.to_string().contains("bobby init"));
}
```

Export `DEFAULT_CAPABILITIES` as `pub(crate)` or `pub` for tests, or rebuild the capability vec inside the test via `generate_bootstrap` + parse.

- [ ] **Step 2: Run tests — expect FAIL**

```bash
cargo test -p cli resolve_ -- --nocapture
```

- [ ] **Step 3: Implement resolution + wire `run()`**

Add `init` arm before `serve`. Parse flags from `std::env::args().skip(2)` with simple chunk scanning (same style as `install-firefox-native-host`).

For `serve`:

```rust
let bootstrap_path = std::env::var("BOBBY_BROWSER_BOOTSTRAP_ENV")
    .map(PathBuf::from)
    .unwrap_or(default_bootstrap_path()?);
let resolved = resolve_startup_credential_with(
    &config.server.host,
    &bootstrap_path,
    broker::StartupCredential::from_env,
)?;
let startup = match resolved {
    ResolveOutcome::FromEnv(c) | ResolveOutcome::FromFile(c) => c,
    ResolveOutcome::Generated { credential, material } => {
        eprintln!(
            "Generated loopback bootstrap at {}",
            bootstrap_path.display()
        );
        eprintln!("Bootstrap bearer (copy now; will not be shown again):");
        eprintln!("{}", material.bearer());
        credential
    }
};
```

`init` prints the bearer once to stdout after a successful write, plus the path and SDK mapping note (`AUTOMATION_RUNTIME_TOKEN` / Authorization bearer). Document `--force` regenerates and invalidates the previous bearer for new enrollment.

- [ ] **Step 4: Run cli lib tests**

```bash
cargo test -p cli --lib
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/main.rs crates/cli/src/bootstrap_local.rs
git commit -m "$(cat <<'EOF'
feat(cli): wire init and loopback bootstrap resolution

EOF
)"
```

---

### Task 3: Rename binary to `bobby`

**Files:**
- Modify: `crates/cli/Cargo.toml`
- Modify: `Makefile` (help text mentioning `./target/release/bobby`)
- Modify: `scripts/dev/*.sh` if they invoke a binary named `cli`

**Interfaces:**
- Consumes: unchanged cargo package name `cli`
- Produces: `target/debug/bobby` and `target/release/bobby`

- [ ] **Step 1: Rename bin**

```toml
[[bin]]
name = "bobby"
path = "src/bin/cli.rs"
```

- [ ] **Step 2: Build and smoke**

```bash
cargo build -p cli
./target/debug/bobby doctor
```

Expected: prints `ok`.

- [ ] **Step 3: Fix references to the old binary filename**

```bash
rg -n 'target/.*/cli\b|/--bin cli\b' --glob '!target/**' --glob '!vendor/**'
```

Update hits that mean the binary file. Keep `-p cli` (package name). Prefer:

```bash
cargo run -p cli -- doctor
```

- [ ] **Step 4: Commit**

```bash
git add crates/cli/Cargo.toml Makefile scripts/dev
git commit -m "$(cat <<'EOF'
feat(cli): rename binary to bobby

EOF
)"
```

---

### Task 4: Docs, README, CONTRIBUTING, gitignore

**Files:**
- Modify: `README.md`
- Modify: `CONTRIBUTING.md`
- Modify: `docs/bobby-browser/source/pages/introduction/installation.md`
- Modify: `docs/bobby-browser/source/pages/introduction/quickstart.md`
- Modify: `docs/bobby-browser/source/pages/guides/auth.md`
- Modify: `docs/bobby-browser/source/pages/guides/run.md`
- Modify: `docs/bobby-browser/source/pages/guides/configuration.md`
- Modify: `config.toml` (header comments only)
- Modify: `.gitignore`
- Regenerate: `docs/bobby-browser/v0.2.0/**` via `pnpm docs:build`

**Interfaces:**
- Consumes: Task 2/3 command names and bootstrap paths
- Produces: dual-CTA docs consistent with CLI behavior

- [ ] **Step 1: Rewrite README**

Required sections in order:

1. `# bobby-browser` + short pitch.
2. Dual CTAs:

```markdown
## Run the runtime

```bash
cargo build -p cli --release
./target/release/bobby init
./target/release/bobby serve
```

Then open `http://127.0.0.1:7777/healthz`.

## Use from TypeScript

```bash
npm install @bobby-browser/sdk
```

```ts
import { BrowserRuntimeClient } from "@bobby-browser/sdk";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});
```
3. Alpha banner linking `SECURITY.md`.
4. Links to hosted docs, CONTRIBUTING, key guides.
5. Do not put binary Releases URLs (phase 4).

- [ ] **Step 2: Update source docs**

- `installation.md`: build `cli` package, run `bobby init`, secret path, env contract.
- `quickstart.md`: `bobby init` then `bobby serve` (also show `cargo run -p cli --`).
- `auth.md`: init flow, resolution order, `--force` warning, no tokens in URLs/config.
- `run.md` / `configuration.md`: `bobby serve`; mention `BOBBY_BROWSER_BOOTSTRAP_ENV`.

- [ ] **Step 3: CONTRIBUTING + Makefile**

Document:

```bash
cargo build -p cli
./target/debug/bobby doctor
```

- [ ] **Step 4: Gitignore**

```gitignore
bootstrap.env
**/.bobby-browser/
```

- [ ] **Step 5: Rebuild docs artifact**

```bash
pnpm docs:build
pnpm docs:verify
pnpm docs:test
```

Expected: PASS. Commit generated `v0.2.0` with source.

- [ ] **Step 6: Commit**

```bash
git add README.md CONTRIBUTING.md Makefile config.toml .gitignore \
  docs/bobby-browser/source docs/bobby-browser/v0.2.0
git commit -m "$(cat <<'EOF'
docs: dual CTAs, bobby CLI, and init bootstrap flow

EOF
)"
```

---

### Task 5: Root and SDK package metadata

**Files:**
- Modify: `package.json`
- Modify: `packages/typescript-sdk/package.json`
- Create: `packages/typescript-sdk/CHANGELOG.md`
- Create: `packages/typescript-sdk/README.md`

**Interfaces:**
- Consumes: existing SDK tests
- Produces: publish-ready `@bobby-browser/sdk@0.2.0` metadata

- [ ] **Step 1: Root `package.json`**

Keep `private: true`. Set `name` to `bobby-browser`, plus `description`, `license`, `repository`, `homepage`, `bugs`, `engines`. Keep `packageManager` and docs scripts.

```json
{
  "name": "bobby-browser",
  "private": true,
  "description": "Browser automation runtime with authenticated, capability-scoped control surfaces",
  "license": "MIT",
  "repository": {
    "type": "git",
    "url": "git+https://github.com/cavi-ai/bobby-browser.git"
  },
  "homepage": "https://cavi-ai.xyz/docs/bobby-browser",
  "bugs": {
    "url": "https://github.com/cavi-ai/bobby-browser/issues"
  }
}
```

- [ ] **Step 2: SDK package.json gaps**

Add `homepage`, `bugs`, and `keywords`:

```json
"homepage": "https://cavi-ai.xyz/docs/bobby-browser",
"bugs": { "url": "https://github.com/cavi-ai/bobby-browser/issues" },
"keywords": [
  "browser-automation",
  "cdp",
  "mcp",
  "playwright",
  "puppeteer",
  "bobby-browser"
]
```

Keep version `0.2.0` and `publishConfig.access: "public"`.

- [ ] **Step 3: SDK README + CHANGELOG**

`CHANGELOG.md`:

```markdown
# Changelog

## 0.2.0

- Initial public npm release of the typed Bobby Browser runtime client.
```

`README.md`: `npm install @bobby-browser/sdk` plus links to the GitHub repo and hosted docs.

- [ ] **Step 4: Test SDK**

```bash
pnpm --filter @bobby-browser/sdk test
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add package.json packages/typescript-sdk
git commit -m "$(cat <<'EOF'
chore: add OSS package metadata for root and SDK

EOF
)"
```

---

### Task 6: npm publish workflow and live publish

**Files:**
- Create: `.github/workflows/publish-npm.yml`

**Interfaces:**
- Consumes: GitHub secret `NPM_TOKEN` with publish rights to `@bobby-browser`
- Produces: live `@bobby-browser/sdk@0.2.0`

- [ ] **Step 1: Add workflow**

```yaml
name: Publish npm

on:
  workflow_dispatch:
  push:
    tags:
      - "sdk-v*"

concurrency:
  group: publish-npm-${{ github.ref }}
  cancel-in-progress: false

jobs:
  publish-sdk:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      id-token: write
    steps:
      - uses: actions/checkout@v7
      - uses: pnpm/action-setup@v4
      - uses: actions/setup-node@v7
        with:
          node-version: 22
          cache: pnpm
          registry-url: https://registry.npmjs.org
      - run: pnpm install --frozen-lockfile
      - run: pnpm --filter @bobby-browser/sdk test
      - name: Dry-run pack
        working-directory: packages/typescript-sdk
        run: pnpm build && npm pack --dry-run
      - name: Publish
        working-directory: packages/typescript-sdk
        env:
          NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}
        run: npm publish --access public
```

- [ ] **Step 2: Human gate — verify publish prerequisites**

Stop and confirm with the maintainer before publishing:

1. npm scope `@bobby-browser` is owned / accessible.
2. Repo secret `NPM_TOKEN` exists (or local token for a one-shot publish).
3. `npm view @bobby-browser/sdk version` is empty or older than `0.2.0`.

If any check fails, report and wait — do not invent credentials.

- [ ] **Step 3: Local dry-run**

```bash
pnpm --filter @bobby-browser/sdk build
cd packages/typescript-sdk && npm pack --dry-run
```

Expected: tarball contains `dist/src/**` and package metadata; no `.env` or secrets.

- [ ] **Step 4: Publish**

```bash
git add .github/workflows/publish-npm.yml
git commit -m "$(cat <<'EOF'
ci: add npm publish workflow for @bobby-browser/sdk

EOF
)"
git tag sdk-v0.2.0
git push origin HEAD
git push origin sdk-v0.2.0
```

Or `workflow_dispatch` after the workflow is on `main`. Only push tags/remotes when the user has approved pushing.

Alternative with explicit approval: `npm publish --access public` from `packages/typescript-sdk` using a provided token.

- [ ] **Step 5: Verify**

```bash
npm view @bobby-browser/sdk version
npm view @bobby-browser/sdk repository.url
```

Expected: `0.2.0` and `git+https://github.com/cavi-ai/bobby-browser.git`.

---

## Spec coverage checklist

| Spec requirement (phases 1–2) | Task |
|---|---|
| Dual CTAs in README | 4 |
| Root + SDK package metadata | 5 |
| `bobby` binary name | 3 |
| `bobby init` + secret file + TTL/force | 1–2 |
| Serve env → file → loopback autogen → error | 2 |
| Docs source + generated artifact | 4 |
| CONTRIBUTING/Makefile updates | 4 |
| gitignore for secrets | 4 |
| npm publish workflow + live SDK | 6 |
| Fail-closed / no parallel auth | 1–2 |
| No gauntlet/companion/conformance publish | 6 (SDK only) |
| Phases 3–4 deferred | Global Constraints |

## Out of scope (follow-on plans)

- crates.io `bobby-browser` + curated Rust libs
- cargo-dist / GitHub Releases multi-platform binaries
- Renaming the cargo package from `cli` to `bobby-browser` for crates.io
