# Firefox companion icon + popup enroll — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the C1 toolbar icon (B + 鲍比) and let operators first-time enroll / re-pair from the Firefox companion popup via a native-host enroll API that never exposes pairing secrets to the extension.

**Architecture:** Popup sends `enrollPair` to the background; background sends a secret-free `{ kind: "enrollProfile" }` native message. The native host resolves `profileDir` + BiDi URL, runs the same enrollment core as `bobby enroll-firefox-profile` (`start_firefox_profile_enrollment` + `persist_browser_selection`), then completes the existing Pair handshake on the same native port. Icons are static assets under `packages/firefox-companion/icons/`.

**Tech Stack:** Firefox MV2 WebExtension (TypeScript, esbuild), native messaging host in Rust (`companion-core` + `bobby` CLI), shared enroll helpers in `firefox-companion::selection`, node:test + `cargo test`.

**Spec:** `docs/superpowers/specs/2026-08-04-firefox-companion-icon-and-popup-enroll-design.md`  
**Worktree:** `.worktrees/firefox-companion-icon-enroll` on branch `docs/firefox-companion-icon-and-popup-enroll` (rename branch to `feat/…` when implementation starts if preferred).  
**Note:** `docs/superpowers/` is gitignored — always `git add -f` for specs/plans.

## Global Constraints

- Secrets never enter the extension: no `pairingCode`, `endpoint`, descriptor body, bearer, or reconnect credential in popup/background/native outbound from the extension (`reject_extension_secrets` must keep failing closed).
- First-time **Pair** bootstraps enrollment in the native host (same as CLI): starts a temporary companion listener, writes the descriptor, waits for pair, persists `browser-selection.json`. This does **not** require `bobby serve` (serve needs selection first). Day-2 **Re-pair** assumes serve/gateway (or a fresh enroll) has a live descriptor.
- Infer `bidiUrl` from `$profileDir/WebDriverBiDiServer.json` (`ws_host` + `ws_port` → `ws://…/session`); fail closed if missing/non-loopback.
- Infer `profileDir` from install defaults file (Task 4); fail closed if ambiguous/missing.
- Operator-facing error copy must match the spec table (serve/descriptor, BiDi missing, profile unknown, timeout) — never dump raw errors with secrets.
- Icon C1: yellow badge, bee stripes, white serif **B**, **鲍比** under B inside the circle; no wand. 16px asset is B+stripes only (no Chinese).
- Keep `bobby enroll-firefox-profile` for CI; thin CLI over shared helpers.
- Prefer no companion-protocol version bump; control stays on extension ↔ native-host framing (`NativeRequest`).
- No Chrome companion, no auto-start serve/Firefox, no multi-profile picker in v1.

## File map

| Path | Responsibility |
|---|---|
| `packages/firefox-companion/icons/*.svg`, `*.png` | C1 brand assets (full + 16px crop) |
| `packages/firefox-companion/manifest.json` | `icons` + `browser_action.default_icon` |
| `packages/firefox-companion/package.json` | `build` copies `icons/` into `dist/` |
| `packages/firefox-companion/popup.html` / `src/popup.ts` | Pair / Re-pair button + status |
| `packages/firefox-companion/src/popup-status.ts` | Enroll phase fields on `PopupStatus` |
| `packages/firefox-companion/src/background.ts` | `enrollPair` runtime message → native `enrollProfile` |
| `packages/firefox-companion/src/native-transport.ts` | Validate/send `enrollProfile`; parse enroll status |
| `crates/firefox-companion/src/selection.rs` | Shared selection builder + BiDi file URL helper + enroll defaults path |
| `crates/cli/src/main.rs` | Thin CLI enroll; native-host entry handles enroll-first |
| `crates/cli/src/onboarding.rs` | Write enroll defaults (`profileDir`, bind, descriptor) at companion install |
| `crates/companion-core/src/native_host.rs` | `NativeRequest::EnrollProfile` + enroll-then-pair loop |
| `docs/bobby-browser/source/pages/guides/firefox-companion.md` | Human path = popup Pair |

---

### Task 1: Toolbar icon assets (C1)

**Files:**
- Create: `packages/firefox-companion/icons/icon.svg` (full C1 with 鲍比)
- Create: `packages/firefox-companion/icons/icon-16.svg` (B + stripes only)
- Create: `packages/firefox-companion/icons/icon-16.png`, `icon-32.png`, `icon-48.png`, `icon-96.png` (rasterize from the appropriate SVG; 16 from `icon-16.svg`, others from `icon.svg`)
- Modify: `packages/firefox-companion/manifest.json`
- Modify: `packages/firefox-companion/package.json` (`build` copies icons)
- Test: `packages/firefox-companion/test/icons.test.ts`

**Interfaces:**
- Consumes: none
- Produces: manifest icon paths `icons/icon-16.png` … `icons/icon-96.png`; build copies `icons/` → `dist/icons/`

- [ ] **Step 1: Write the failing test**

```ts
// packages/firefox-companion/test/icons.test.ts
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

test("manifest wires browser_action icons at 16/32/48/96", () => {
  const manifest = JSON.parse(readFileSync(join(root, "manifest.json"), "utf8"));
  for (const size of ["16", "32", "48", "96"]) {
    const path = manifest.browser_action.default_icon[size];
    assert.equal(path, `icons/icon-${size}.png`);
    assert.ok(existsSync(join(root, path)), `missing ${path}`);
  }
});

test("16px SVG source has no Chinese glyphs", () => {
  const svg = readFileSync(join(root, "icons/icon-16.svg"), "utf8");
  assert.equal(svg.includes("鲍"), false);
  assert.equal(svg.includes("比"), false);
});

test("full SVG source includes 鲍比", () => {
  const svg = readFileSync(join(root, "icons/icon.svg"), "utf8");
  assert.ok(svg.includes("鲍比"));
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd packages/firefox-companion && node --import tsx --test test/icons.test.ts`  
Expected: FAIL (missing icons / manifest keys)

- [ ] **Step 3: Add SVG sources + PNGs + manifest + build copy**

Create `icons/icon.svg` matching C1 (yellow circle, black stripes, white **B**, **鲍比** under B; no staff). Create `icons/icon-16.svg` as B+stripes only. Rasterize PNGs (e.g. `rsvg-convert` or `magick`) at 16/32/48/96.

Update `manifest.json`:

```json
"icons": {
  "16": "icons/icon-16.png",
  "32": "icons/icon-32.png",
  "48": "icons/icon-48.png",
  "96": "icons/icon-96.png"
},
"browser_action": {
  "default_title": "Bobby Companion",
  "default_popup": "popup.html",
  "default_icon": {
    "16": "icons/icon-16.png",
    "32": "icons/icon-32.png",
    "48": "icons/icon-48.png",
    "96": "icons/icon-96.png"
  }
}
```

Update `package.json` `build` to append: `&& mkdir -p dist/icons && cp -R icons/. dist/icons/`

- [ ] **Step 4: Run test to verify it passes**

Run: `cd packages/firefox-companion && node --import tsx --test test/icons.test.ts && npm run build`  
Expected: PASS; `dist/icons/icon-16.png` exists

- [ ] **Step 5: Commit**

```bash
git add -f packages/firefox-companion/icons packages/firefox-companion/manifest.json \
  packages/firefox-companion/package.json packages/firefox-companion/test/icons.test.ts
git commit -m "feat(firefox-companion): add C1 toolbar icons with 鲍比"
```

---

### Task 2: Shared BiDi endpoint file parser

**Files:**
- Modify: `crates/firefox-companion/src/selection.rs` (or new `crates/firefox-companion/src/bidi_endpoint.rs` re-exported from `lib.rs`)
- Modify: `crates/firefox-companion/src/lib.rs` if new module
- Test: unit tests in `crates/firefox-companion/src/selection.rs` (`#[cfg(test)]`) or `crates/firefox-companion/tests/bidi_endpoint.rs`

**Interfaces:**
- Consumes: none
- Produces:
  - `pub fn bidi_url_from_endpoint_file(bytes: &[u8]) -> Result<Url, String>`
  - `pub fn read_bidi_url_from_profile_dir(profile_dir: &Path) -> Result<Url, CommandError>`  
    Reads `profile_dir.join("WebDriverBiDiServer.json")` with the same bounds/loopback rules as `runtime-tests` (`ws_host` + `ws_port` only, ≤4096 bytes, loopback only) → `ws://{authority}/session`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn bidi_url_from_endpoint_file_accepts_loopback_port() {
    let bytes = br#"{"ws_host":"127.0.0.1","ws_port":9222}"#;
    let url = bidi_url_from_endpoint_file(bytes).expect("parse");
    assert_eq!(url.as_str(), "ws://127.0.0.1:9222/session");
}

#[test]
fn bidi_url_from_endpoint_file_rejects_non_loopback() {
    let bytes = br#"{"ws_host":"8.8.8.8","ws_port":9222}"#;
    assert!(bidi_url_from_endpoint_file(bytes).is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p firefox-companion bidi_url_from_endpoint_file -- --nocapture`  
Expected: FAIL (symbol not found / compile error)

- [ ] **Step 3: Implement parser**

Port logic from `crates/runtime-tests/src/lib.rs` `bidi_endpoint_file_url` / `read_bidi_endpoint_file` into `firefox-companion` (do not create a dependency on `runtime-tests`). Keep fail-closed checks identical.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p firefox-companion bidi_url_from_endpoint_file`  
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/firefox-companion/src/selection.rs crates/firefox-companion/src/lib.rs \
  crates/firefox-companion/src/bidi_endpoint.rs crates/firefox-companion/tests/bidi_endpoint.rs
git commit -m "feat(firefox-companion): parse WebDriverBiDiServer.json into bidiUrl"
```

---

### Task 3: Shared enrolled selection builder + thin CLI

**Files:**
- Modify: `crates/firefox-companion/src/selection.rs`
- Modify: `crates/cli/src/main.rs` (`run_firefox_profile_enroll`)
- Test: unit tests next to the new helper in `selection.rs`

**Interfaces:**
- Consumes: `persist_browser_selection`, `default_selection_path`, `ProfileId`
- Produces:
  ```rust
  pub fn build_enrolled_browser_selection(
      profile_id: &ProfileId,
      bidi_url: &str,
      profile_dir: &Path,
      companion_bind: SocketAddr,
      descriptor_path: &Path,
  ) -> BrowserSelectionConfig
  ```
  Exact JSON shape must match current CLI output (preference `exact`/`firefox` + firefox entry with `timeoutMs: 30000`, `pairingCodeTtlMs: 300000`, `attachmentTtlMs: 300000`).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn build_enrolled_browser_selection_matches_wire_shape() {
    let profile_id = ProfileId::from_uuid(uuid::Uuid::nil()); // use real constructor used in crate
    let selection = build_enrolled_browser_selection(
        &profile_id,
        "ws://127.0.0.1:9222/session",
        Path::new("/tmp/firefox-profile"),
        "127.0.0.1:9876".parse().unwrap(),
        Path::new("/tmp/descriptor.json"),
    );
    let value = serde_json::to_value(&selection).unwrap();
    assert_eq!(value["preference"]["engine"], "firefox");
    assert_eq!(value["firefox"][0]["bidiUrl"], "ws://127.0.0.1:9222/session");
    assert_eq!(value["firefox"][0]["timeoutMs"], 30_000);
}
```

(Adjust `ProfileId` construction to the crate’s real API.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p firefox-companion build_enrolled_browser_selection -- --nocapture`  
Expected: FAIL

- [ ] **Step 3: Implement helper; switch CLI to use it**

Replace inline `serde_json::json!({...})` in `run_firefox_profile_enroll` (`crates/cli/src/main.rs` ~682–694) with `build_enrolled_browser_selection` + `persist_browser_selection`. Keep stdout print of the selection JSON for CI.

- [ ] **Step 4: Run tests**

Run:
```bash
cargo test -p firefox-companion build_enrolled_browser_selection
cargo test -p bobby-browser enroll
```
Expected: PASS (existing enroll paths still green)

- [ ] **Step 5: Commit**

```bash
git add crates/firefox-companion/src/selection.rs crates/cli/src/main.rs
git commit -m "refactor: share enrolled browser-selection builder with CLI"
```

---

### Task 4: Persist enroll defaults at companion install

**Files:**
- Modify: `crates/cli/src/onboarding.rs`
- Modify: `crates/firefox-companion/src/selection.rs` (defaults path + load/save)
- Modify: docs only in Task 8; here code + tests
- Test: `crates/cli` unit test or `onboarding` test module

**Interfaces:**
- Consumes: `bobby_config_dir()` / config dir layout from install
- Produces:
  ```rust
  #[derive(Debug, Clone, Serialize, Deserialize)]
  pub struct FirefoxEnrollDefaults {
      pub profile_dir: PathBuf,
      pub companion_bind: SocketAddr, // default 127.0.0.1:9876
      pub descriptor_path: PathBuf,
  }

  pub fn enroll_defaults_path(config_dir: &Path) -> PathBuf {
      config_dir.join("firefox-enroll-defaults.json")
  }

  pub fn write_enroll_defaults(path: &Path, defaults: &FirefoxEnrollDefaults) -> Result<()>
  pub fn read_enroll_defaults(path: &Path) -> Result<FirefoxEnrollDefaults>
  ```
  Install writes defaults with `profile_dir = config_dir.join("firefox-profile")` (create dir if missing), `descriptor_path` from `CompanionInstall`, bind `127.0.0.1:9876`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn install_writes_enroll_defaults_next_to_descriptor() {
    // temp config dir; call write_enroll_defaults / install helper
    let defaults = read_enroll_defaults(&path).unwrap();
    assert!(defaults.profile_dir.ends_with("firefox-profile"));
    assert_eq!(defaults.companion_bind.to_string(), "127.0.0.1:9876");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p bobby-browser install_writes_enroll_defaults -- --nocapture`  
Expected: FAIL

- [ ] **Step 3: Implement load/save + call from `install_firefox_companion`**

After native host install succeeds, write `firefox-enroll-defaults.json` (`0600` on Unix, atomic write matching selection persist style if available). Ensure `firefox-profile` directory exists (`create_dir_all`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p bobby-browser install_writes_enroll_defaults`  
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/onboarding.rs crates/firefox-companion/src/selection.rs
git commit -m "feat: persist firefox enroll defaults at companion install"
```

---

### Task 5: Native host `enrollProfile` control path

**Files:**
- Modify: `crates/companion-core/src/native_host.rs`
- Modify: `crates/cli/src/main.rs` (`run_configured_native_host` / `run_native_host` wiring — pass config dir / defaults path / allow enroll without pre-existing descriptor)
- Modify: `crates/companion-core/src/lib.rs` exports as needed
- Test: `crates/companion-core/tests/native_host.rs`

**Interfaces:**
- Consumes: `start_firefox_profile_enrollment`, `build_enrolled_browser_selection`, `persist_browser_selection`, `read_enroll_defaults`, `read_bidi_url_from_profile_dir`
- Produces: `NativeRequest` gains:
  ```rust
  enum NativeRequest {
      Pair(NativeConnectRequest),
      EnrollProfile(EnrollProfileRequest), // empty input object `{}` with deny_unknown_fields
  }
  ```
  Host replies with secret-free status, e.g.:
  ```json
  { "kind": "nativeStatus", "output": { "state": "enrollOk" } }
  ```
  or `{ "state": "enrollFailed", "code": "bidiMissing" | "defaultsMissing" | "timeout" | "listenerUnavailable" }`  
  (exact code strings must match extension mapping in Task 6).

**Behavior:**
1. First native message may be `enrollProfile` **or** `pair` (existing).
2. On `enrollProfile`: load defaults → read BiDi URL → `start_firefox_profile_enrollment` using defaults’ bind + descriptor path → wait for the **same** connection’s subsequent `pair` (or accept pair already queued) → persist selection → emit `enrollOk` → continue normal relay **or** exit cleanly after enroll (prefer: complete pair, send `paired` through as today, then relay).
3. If defaults/BiDi missing: write `enrollFailed` status to extension and return (no secrets in message).
4. If descriptor missing and first message is `pair` (not enroll): keep today’s failure mode (invalid pairing material / missing descriptor).

Dependency note: `companion-core` may need a new dependency on `firefox-companion` **or** enroll orchestration stays in `bobby` CLI (`run_configured_native_host`) while `companion-core` only decodes `EnrollProfile` and invokes a callback/`NativeHostEnroll` trait supplied by CLI. Prefer **callback/trait injected from CLI** to avoid crate cycles (`firefox-companion` already depends on `companion-core`).

```rust
// companion-core
pub trait NativeHostEnroll: Send + Sync {
    fn enroll_and_wait_for_pair(
        &self,
        pair: NativeConnectRequest,
    ) -> impl Future<Output = Result<NativeHostConfig, EnrollHostError>> + Send;
}
```

CLI implements this by reading defaults, BiDi file, starting enrollment, bridging pair with returned `NativeHostConfig`.

- [ ] **Step 1: Write the failing test**

In `crates/companion-core/tests/native_host.rs`, add a test that first frame `{ "kind": "enrollProfile", "input": {} }` is accepted by `decode` / request enum (and that `{ "kind": "enrollProfile", "input": { "pairingCode": "x" } }` is rejected).

```rust
#[test]
fn enroll_profile_request_decodes_empty_input() {
    let value = serde_json::json!({ "kind": "enrollProfile", "input": {} });
    // assert decode succeeds as NativeRequest::EnrollProfile
}

#[test]
fn enroll_profile_request_rejects_secret_fields() {
    let value = serde_json::json!({
        "kind": "enrollProfile",
        "input": { "pairingCode": "nope" }
    });
    // assert decode / reject_extension_secrets fails
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p companion-core enroll_profile_request -- --nocapture`  
Expected: FAIL

- [ ] **Step 3: Implement enum + CLI enroll bridge**

Minimal path that satisfies tests and a host integration test with a fake enroll trait returning a loopback `NativeHostConfig` after pair.

- [ ] **Step 4: Run tests**

Run:
```bash
cargo test -p companion-core
cargo test -p bobby-browser
cargo test -p firefox-companion
```
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/companion-core crates/cli/src/main.rs crates/firefox-companion
git commit -m "feat: native-host enrollProfile control path"
```

---

### Task 6: Extension native transport + background `enrollPair`

**Files:**
- Modify: `packages/firefox-companion/src/native-transport.ts`
- Modify: `packages/firefox-companion/src/background.ts`
- Modify: `packages/firefox-companion/src/popup-status.ts`
- Test: `packages/firefox-companion/test/native-transport.test.ts`, `background.test.ts`, `popup-status.test.ts`

**Interfaces:**
- Consumes: host `enrollProfile` / `nativeStatus` codes from Task 5
- Produces:
  ```ts
  // outbound (secret-free)
  type NativeEnrollProfileRequest = { kind: "enrollProfile"; input: Record<string, never> };

  // runtime message
  // { type: "enrollPair" } → Promise<{ ok: true } | { ok: false; code: string; message: string }>

  // PopupStatus additions
  enrollPhase?: "idle" | "pairing" | "failed";
  enrollError?: { code: string; message: string };
  ```
  Map host codes → operator messages:
  - `listenerUnavailable` → `Start bobby serve, then Pair again` (also used when enroll bootstrap cannot bind / no companion)
  - `bidiMissing` → `Start Firefox with remote debugging, then Pair again`
  - `defaultsMissing` → `Profile path unknown — re-run bobby install (see docs)`
  - `timeout` → `Pairing timed out`

- [ ] **Step 1: Write the failing tests**

```ts
test("enrollProfile outbound is exact and secret free", () => {
  const msg = { kind: "enrollProfile", input: {} };
  assert.deepEqual(Object.keys(msg.input), []);
  assert.equal(JSON.stringify(msg).includes("pairing"), false);
});

test("getPopupStatus surfaces enroll failed operator copy", async () => {
  // background stub with last enroll error code bidiMissing
  const status = await background.getPopupStatus(storage);
  assert.equal(status.enrollPhase, "failed");
  assert.match(status.enrollError?.message ?? "", /remote debugging/i);
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd packages/firefox-companion && npm test`  
Expected: FAIL on new assertions

- [ ] **Step 3: Implement transport validation, background handler, status fields**

Wire `receiveRuntimeMessage` for `{ type: "enrollPair" }`: set phase pairing → `transport.send(enrollProfile)` then ensure `connect(...)` / pair request is sent → wait for `paired` or timeout → update status. Never log response bodies that might contain secrets (strip via existing redaction helpers if any).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd packages/firefox-companion && npm test`  
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add packages/firefox-companion/src packages/firefox-companion/test
git commit -m "feat(firefox-companion): enrollPair via native enrollProfile"
```

---

### Task 7: Popup Pair / Re-pair UI

**Files:**
- Modify: `packages/firefox-companion/popup.html`
- Modify: `packages/firefox-companion/src/popup.ts`
- Test: `packages/firefox-companion/test/popup.test.ts`

**Interfaces:**
- Consumes: `PopupStatus.enrollPhase`, `enrollError`, `paired` from Task 6
- Produces: Connection section button **Pair** when unpaired; **Re-pair** when paired; disabled + “Pairing…” while `enrollPhase === "pairing"`; shows `enrollError.message` on failure

- [ ] **Step 1: Write the failing test**

```ts
test("renderPopup shows Pair when unpaired", () => {
  renderPopup(document, {
    paired: false,
    leaseCount: 0,
    nativeConnected: true,
    fingerprint: { enabled: false, owner: "popup" },
    humanize: "unknown",
    protocolVersion: 1,
    enrollPhase: "idle",
  });
  const button = document.querySelector("#pair-button");
  assert.ok(button);
  assert.equal(button.textContent, "Pair");
  assert.equal(button.disabled, false);
});

test("renderPopup disables button while pairing", () => {
  renderPopup(document, { /* … */ enrollPhase: "pairing", paired: false, /* … */ });
  assert.equal(document.querySelector("#pair-button").disabled, true);
  assert.match(document.querySelector("#connection .status")?.textContent ?? "", /Pairing/i);
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd packages/firefox-companion && node --import tsx --test test/popup.test.ts`  
Expected: FAIL

- [ ] **Step 3: Implement button in HTML + click handler**

```html
<button id="pair-button" type="button">Pair</button>
```

On click: `browser.runtime.sendMessage({ type: "enrollPair" })`, then reload `popupStatus`.

- [ ] **Step 4: Run tests**

Run: `cd packages/firefox-companion && npm test`  
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add packages/firefox-companion/popup.html packages/firefox-companion/src/popup.ts \
  packages/firefox-companion/test/popup.test.ts
git commit -m "feat(firefox-companion): Pair and Re-pair controls in popup"
```

---

### Task 8: Firefox companion guide docs

**Files:**
- Modify: `docs/bobby-browser/source/pages/guides/firefox-companion.md`
- Optionally mirror versioned copies under `docs/bobby-browser/v*/` only if the repo’s docs publish step requires it (follow existing pattern for the active version)

**Interfaces:** none (docs only)

- [ ] **Step 1: Update the human enroll section**

Replace “must run `bobby enroll-firefox-profile`” as the primary path with:

1. Install companion / native host (`bobby install` / existing steps).
2. Start Firefox with the Bobby profile + `--remote-debugging-port`.
3. Open the companion toolbar popup → **Pair** (writes `browser-selection.json`).
4. Then `bobby serve` / MCP.

Keep a short “CI / scripting” subsection with the existing CLI enroll command.

Mention toolbar icon only in one line (Bobby companion badge).

- [ ] **Step 2: Sanity-check copy against spec error strings**

Ensure documented failures match Task 6 operator messages.

- [ ] **Step 3: Commit**

```bash
git add docs/bobby-browser/source/pages/guides/firefox-companion.md
# plus versioned mirrors if touched
git commit -m "docs: prefer popup Pair for Firefox companion enroll"
```

---

## Spec coverage self-review

| Spec requirement | Task |
|---|---|
| C1 icon + 鲍比; no wand; 16px without Chinese | Task 1 |
| Popup Pair / Re-pair; progress; safe errors | Tasks 6–7 |
| Background → native `enrollProfile`; no secrets | Tasks 5–6 |
| Native host enroll + write `browser-selection.json` | Tasks 3–5 |
| Infer BiDi from `WebDriverBiDiServer.json` | Task 2 |
| Infer profileDir (install defaults) | Task 4 |
| Keep CLI enroll | Task 3 |
| Docs human path = popup | Task 8 |
| Extension + Rust tests | Tasks 1–7 |
| First-time without serve (bootstrap) vs day-2 | Global Constraints + Task 5 |

**Clarification vs brainstorm “serve must be up”:** first-time Pair bootstraps enrollment in-host (required because serve needs `browser-selection.json`). Documented in Global Constraints; docs Task 8 states the order explicitly.

**Placeholder scan:** none intentional. Implementers must use real `ProfileId` constructors and exact `NativeStatus` code strings agreed in Task 5 when writing Task 6 mappings.

---

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-08-04-firefox-companion-icon-and-popup-enroll.md`.

**Two execution options:**

1. **Subagent-Driven (recommended)** — fresh subagent per task, review between tasks  
2. **Inline Execution** — execute tasks in this session with executing-plans checkpoints  

Which approach?
