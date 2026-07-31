---
documentedVersion: 0.3.0
---

# Firefox companion

The Firefox engine drives a real, headed Firefox through two channels: a
WebDriver BiDi connection for engine-native interaction, and a companion Web
extension (`packages/firefox-companion`, MV2) paired over native messaging for
page observation, tab discovery, and page binding. The runtime never launches
Firefox itself; it pairs with a Firefox you start.

Trust chain: the runtime publishes a native-host descriptor (endpoint +
one-time pairing code) → the extension connects to the native host
`com.bobby_browser.companion` → the host reads the descriptor and pairs with
the runtime's loopback companion server → the runtime grants page attachments
with TTL-bound leases.

## One-time setup

All steps run on the machine that hosts Firefox. Paths below use the macOS
state dir `~/Library/Application Support/bobby-browser`; adjust as needed.

### 1. Build the extension

```bash
cd packages/firefox-companion
tsc -p tsconfig.json --noEmit
esbuild src/background.ts src/content.ts --bundle --format=iife \
  --platform=browser --outdir=dist
cp manifest.json dist/manifest.json
```

(or `pnpm --filter @bobby-browser/firefox-companion build`)

### 2. Create a dedicated profile and sideload the extension

Unsigned sideloading requires Firefox Developer Edition, Nightly, or ESR —
release Firefox refuses unsigned permanent extensions.

```bash
PROFILE="$HOME/Library/Application Support/bobby-browser/firefox-profile"
mkdir -p "$PROFILE/extensions"
cat > "$PROFILE/user.js" <<'EOF'
user_pref("xpinstall.signatures.required", false);
user_pref("extensions.autoDisableScopes", 14);
EOF
(cd dist && zip -r "$PROFILE/extensions/firefox-companion@bobby-browser.local.xpi" .)
```

Both prefs are required: the first permits unsigned extensions, the second
auto-enables sideloaded ones (otherwise the extension installs disabled
pending a consent click).

### 3. Install the native messaging host

```bash
STATE="$HOME/Library/Application Support/bobby-browser"
bobby install-firefox-native-host \
  --wrapper "$STATE/bin/firefox-native-host" \
  --manifest "$HOME/Library/Application Support/Mozilla/NativeMessagingHosts/com.bobby_browser.companion.json" \
  --cli /absolute/path/to/target/release/bobby \
  --descriptor "$STATE/firefox-native-host-descriptor.json"
```

All paths must be absolute. The installer refuses to clobber, writes the
wrapper `0700` and manifest `0600`, and rolls back on partial failure.

### 4. Start Firefox with BiDi

```bash
"/Applications/Firefox Developer Edition.app/Contents/MacOS/firefox" \
  --no-remote --foreground \
  --profile "$PROFILE" \
  --remote-debugging-port=9222 \
  about:blank
```

Firefox writes the BiDi endpoint to `$PROFILE/WebDriverBiDiServer.json`. A
fixed port keeps `bidiUrl` stable across restarts; `--remote-debugging-port=0`
also works if you read the port back from that file.

### 5. Enroll the profile

The extension generates and persists its own profile id on first run, so the
selection config cannot be written before the first pairing. Enrollment
discovers it:

```bash
bobby enroll-firefox-profile \
  --descriptor "$STATE/firefox-native-host-descriptor.json" \
  --bind 127.0.0.1:9876 \
  --bidi-url "ws://127.0.0.1:9222/session" \
  --profile-dir "$PROFILE" \
  --timeout-secs 120
```

On success it prints a single-line JSON value. Export it as
`AUTOMATION_RUNTIME_BROWSER_SELECTION` for `bobby serve`:

```json
{"firefox":[{"attachmentTtlMs":300000,"bidiUrl":"ws://127.0.0.1:9222/session","companionBind":"127.0.0.1:9876","descriptorPath":"…/firefox-native-host-descriptor.json","pairingCodeTtlMs":300000,"profileDir":"…/firefox-profile","profileId":"…","timeoutMs":30000}],"preference":{"engine":"firefox","mode":"exact","profileId":"…"}}
```

### 6. Serve

```bash
bobby serve
```

Startup fails fast if the preference cannot be satisfied by the configured
registrations. The first `session_create` publishes the descriptor and waits
for the extension to pair (the extension retries the native host with
backoff, so restarts of either side self-heal).

## Operations

- Keep Firefox running under a supervisor (e.g. a launchd agent with
  `KeepAlive`) so the BiDi endpoint and extension stay up.
- `bobby doctor` validates selection JSON, engine satisfiability, per-profile
  `bidiUrl` syntax and TCP reachability, `profileDir`, `companionBind`, and
  browser bundle detection.
- Verify the engine in command evidence: completed commands carry
  `browserExecution` evidence with `engine: "firefox"` and
  `interactionPath: "engineNative"`.
- Re-sideload a new extension xpi after rebuilding `dist/`; Firefox picks it
  up on restart.

## Limitations

- The companion declares `nativeInput: false` and `nativeDialogs: false`;
  JavaScript dialogs are unsupported on every engine.
- JavaScript evaluation (`evaluateJavaScript`) is Chromium-only today.
- Headless is not a companion mode; the paired Firefox is a real window.
- Chromium remains available as a managed engine in the same selection (e.g.
  `{"preference":{"mode":"prefer","engines":["firefox","chromium"]}}`).

## Notes

- Firefox's RemoteAgent accepts a single WebDriver BiDi session per browser,
  so all runtime sessions on a profile share one BiDi connection; page
  attachments stay per-session and renew automatically before their TTL.
  New attachment grants merge with prior grants instead of replacing them.
- If pairing is interrupted (serve restart, rotated descriptor), the
  extension retries with a bounded cooldown instead of giving up until a
  browser restart. A killed serve cannot brick later pairing: descriptor
  publication recovers its own leftover files.
- The native host treats a companion server silent for ~45s as dead and
  reconnects, recovering half-open connections left by killed processes.
