---
documentedVersion: {{PRODUCT_VERSION}}
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

Unsigned permanent sideloading requires Firefox Developer Edition, Nightly,
or ESR — release Firefox refuses unsigned extensions.

### 1. Install (build + native host + profile sideload)

From a checkout:

```bash
make firefox
```

or:

```bash
pnpm --filter @cavi-ai/bobby-firefox-companion build
./target/release/bobby install --companion
```

That single step:

- copies the built extension into the bobby config dir
- installs the native messaging host (`com.bobby_browser.companion`)
- creates the Bobby Firefox profile and writes required `user.js` prefs
- permanently sideloads an **unpacked** extension into
  `$PROFILE/extensions/firefox-companion@bobby-browser.local/`
- writes `firefox-enroll-defaults.json` for popup Pair

Prefs written (appended if missing; existing custom lines are kept):

```
user_pref("xpinstall.signatures.required", false);
user_pref("extensions.autoDisableScopes", 14);
user_pref("privacy.resistFingerprinting", false);
user_pref("ui.systemUsesDarkTheme", 1);
```

Both extension prefs are required: the first permits unsigned extensions, the
second auto-enables sideloaded ones (otherwise the extension installs disabled
pending a consent click). The fingerprint prefs keep Resist Fingerprinting from
clobbering the BiDi/init-script persona and lean `prefers-color-scheme` toward
dark (init script also forces the matchMedia result).

BiDi sessions set `navigator.webdriver === true`. The init script proxies the
native getter so the value is `false` while `Function.prototype.toString` still
reports `[native code]` (required for CreepJS `webDriverIsOn` / lieProps).
Do not expect a preference alone to clear webdriver under an active BiDi session.

You do **not** need `about:debugging` → Load Temporary Add-on for the normal
path. Re-run `make firefox` after rebuilding to refresh the sideload (Firefox
picks it up on restart).

#### Manual / CI sideload (optional)

If you must assemble the profile by hand (or prefer an `.xpi`):

```bash
PROFILE="$HOME/Library/Application Support/bobby-browser/firefox-profile"
mkdir -p "$PROFILE/extensions"
# write the same user.js prefs as above, then either:
#   unpacked: copy dist/ → "$PROFILE/extensions/firefox-companion@bobby-browser.local/"
#   or pack:  (cd dist && zip -r "$PROFILE/extensions/firefox-companion@bobby-browser.local.xpi" .)
```

### 2. Native messaging host (already done by install)

`make firefox` / `bobby install --companion` installs the native host. The
manual command below is only for explicit paths:

```bash
STATE="$HOME/Library/Application Support/bobby-browser"
bobby install-firefox-native-host \
  --wrapper "$STATE/bin/firefox-native-host" \
  --manifest "$HOME/Library/Application Support/Mozilla/NativeMessagingHosts/com.bobby_browser.companion.json" \
  --cli /absolute/path/to/target/release/bobby \
  --descriptor "$STATE/firefox-native-host-descriptor.json"
```

All paths must be absolute. The installer upgrades bobby-managed wrapper and
manifest files, writes the wrapper `0700` and manifest `0600`, and rolls back
on partial failure.

### 3. Start Firefox with BiDi

```bash
make firefox-start
```

That launches the Bobby profile with `--remote-debugging-port=9224` (no
launchd). `make firefox-stop` quits it. Manual equivalent:

```bash
PROFILE="$HOME/Library/Application Support/bobby-browser/firefox-profile"
"/Applications/Firefox Developer Edition.app/Contents/MacOS/firefox" \
  --no-remote --foreground \
  --profile "$PROFILE" \
  --remote-debugging-port=9224 \
  about:blank
```

Firefox writes the BiDi endpoint to `$PROFILE/WebDriverBiDiServer.json`. A
fixed port keeps `bidiUrl` stable across restarts; `--remote-debugging-port=0`
also works if you read the port back from that file.

Not 9222: that is the port authenticated CDP binds ([`[cdp].port`](../surfaces/cdp.md)),
and whichever of the two started second failed to bind.

### 4. Enroll via popup Pair

The extension generates and persists its own profile id on first run, so
`browser-selection.json` cannot be written until enrollment completes. Use
the companion toolbar popup (yellow **Bobby Companion** badge) as the
primary enroll path:

1. With Firefox still running under the Bobby profile (step 3), click the
   toolbar badge and choose **Pair**.
2. The native host infers `profileDir` from install defaults and the BiDi
   URL from `$PROFILE/WebDriverBiDiServer.json`, runs the same enrollment
   core as the CLI, and atomically persists
   `<config-dir>/bobby-browser/browser-selection.json` (`0600` on Unix).
3. First-time **Pair** bootstraps enrollment in the native host and does
   **not** require `bobby serve`. Local agents use `bobby mcp-stdio` after
   Pair. Day-2 **Re-pair** assumes a live MCP gateway (stdio or HTTP) has
   published a descriptor when the extension needs one.

On success the popup shows a **Paired** badge with companion and profile
ids. `bobby serve`, the MCP gateway, and `bobby doctor` then resolve the
selection with no environment wiring:

```json
{"firefox":[{"attachmentTtlMs":300000,"bidiUrl":"ws://127.0.0.1:9224/session","companionBind":"127.0.0.1:9876","descriptorPath":"…/firefox-native-host-descriptor.json","pairingCodeTtlMs":300000,"profileDir":"…/firefox-profile","profileId":"…","timeoutMs":30000}],"preference":{"engine":"firefox","mode":"exact","profileId":"…"}}
```

Setting `AUTOMATION_RUNTIME_BROWSER_SELECTION` to that JSON remains
supported as an override (it wins over the persisted file), e.g. for a
one-off run against a different profile.

If **Pair** fails, the popup shows an operator-safe message (no secrets):

| Situation | Message |
|---|---|
| Re-pair while gateway is down | Start `bobby mcp-stdio` (or `bobby serve` for HTTP), then Pair again |
| Firefox not started with remote debugging | Start Firefox with remote debugging, then Pair again |
| Install defaults missing or profile path unknown | Profile path unknown — re-run bobby install (see docs) |
| Enrollment timed out | Pairing timed out |

#### CI / scripting

For headless CI or automation, keep the CLI enroll command:

```bash
bobby enroll-firefox-profile \
  --descriptor "$STATE/firefox-native-host-descriptor.json" \
  --bind 127.0.0.1:9876 \
  --bidi-url "ws://127.0.0.1:9224/session" \
  --profile-dir "$PROFILE" \
  --timeout-secs 120
```

On success it prints a single-line JSON value and writes the same
`browser-selection.json` as popup Pair.

### 5. Drive from an agent (stdio) or optional HTTP

Local agents: host spawns `bobby mcp-stdio` (wired by `bobby install`). No
`bobby serve` process is required.

Optional streamable HTTP:

```bash
bobby serve
```

Startup fails fast if the preference cannot be satisfied by the configured
registrations. The first `session_create` publishes the descriptor and waits
for the extension to pair (the extension retries the native host with
backoff, so restarts of either side self-heal).

## Operations

- Keep Firefox running while you automate (`make firefox-start`). A KeepAlive
  launchd agent is optional if you want the profile to survive logouts.
- `bobby doctor` validates selection JSON, engine satisfiability, per-profile
  `bidiUrl` syntax and TCP reachability, `profileDir`, `companionBind`, and
  browser bundle detection.
- Verify the engine in command evidence: completed commands carry
  `browserExecution` evidence with `engine: "firefox"` and
  `interactionPath: "engineNative"`.
- After rebuilding the extension, re-run `make firefox` (or
  `bobby install --companion`) so the profile sideload refreshes; Firefox
  picks it up on restart.

### Operator popup

The toolbar popup is the day-to-day operator panel for the companion:

- **Pair / Re-pair** — enroll or refresh pairing (see step 4).
- **Connection** — paired/unpaired badge, companion and profile ids when
  paired, or an unpaired reason.
- **Session** — active lease count; when the host owns fingerprint spoofing,
  session id and seed hex appear here too.
- **Fingerprint** — toggle for popup-owned spoofing; disabled and read-only
  when a Bobby worker session claims host ownership (BiDi owns spoofing).
- **Humanize** — status only; shows `Unknown — set by session policy` when not
  reported by the active session.
- **Debug** — native port connected/disconnected, protocol version, and last
  error when present.

Host-managed fingerprint cannot be flipped from the popup. After changing
popup code, rebuild and re-run `make firefox` so the profile sideload
refreshes.

## Limitations

- The companion declares `nativeInput: false` and `nativeDialogs: false`;
  JavaScript dialogs are unsupported on every engine.
- JavaScript evaluation (`evaluateJavaScript`) is Chromium-only today.
- Headless is not a companion mode; the paired Firefox is a real window.
- Chromium remains available as a managed engine in the same selection (e.g.
  `{"preference":{"mode":"prefer","engines":["firefox","chromium"]}}`).

## Notes

- Vision-assisted intents run on Firefox under the same double gate as
  Chromium: the bounded accessibility snapshot supplies the semantic
  candidates, and vision-selected coordinates execute as native BiDi pointer
  actions. See [Intent commands](intents.md#vision-double-gate).
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
