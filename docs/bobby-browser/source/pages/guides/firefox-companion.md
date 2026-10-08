---
documentedVersion: {{PRODUCT_VERSION}}
---

# Firefox companion

The Firefox engine drives a real, headed Firefox through WebDriver BiDi plus a companion extension that observes pages and binds tabs. bobby never launches Firefox on its own; you start the dedicated Bobby profile and pair it once.

## Set up

```bash
bobby install --companion
bobby firefox-start
```

`bobby install --companion` installs the extension, the native messaging host and the Bobby Firefox profile. `bobby firefox-start` opens that profile with remote debugging enabled.

In Firefox, click the **Bobby Companion** toolbar badge and choose **Pair**. A **Paired** badge with the companion and profile IDs confirms it. Pairing saves `browser-selection.json` in the config directory, and `bobby serve`, `bobby mcp-stdio` and `bobby doctor` pick it up with no environment variables.

Release builds carry a Mozilla-signed extension that Firefox 128 or later accepts. An unsigned build from source needs Firefox Developer Edition, Nightly or ESR.

Run `bobby doctor` to confirm. It checks the selection, the BiDi URL, the profile directory and that Firefox is reachable.

### If Pair fails

| Message | Fix |
|---|---|
| Start `bobby mcp-stdio` (or `bobby serve`), then Pair again | Re-pairing needs a running gateway |
| Start Firefox with remote debugging, then Pair again | Use `bobby firefox-start` |
| Profile path unknown, re-run bobby install | Run `bobby install --companion` again |
| Pairing timed out | Click Pair again |

### Pair from a script

```bash
bobby enroll-firefox-profile \
  --descriptor /abs/path/firefox-native-host-descriptor.json \
  --bidi-url ws://127.0.0.1:9224/session \
  --profile-dir /abs/path/firefox-profile \
  --timeout-secs 120
```

It prints the selection JSON and writes the same `browser-selection.json` as the popup. `bobby install-firefox-native-host` and `bobby firefox-native-host` are the lower-level commands behind the installer; their flags are in the [CLI reference](cli.md).

## Engine selection

The runtime reads the browser selection from, in order:

1. `AUTOMATION_RUNTIME_BROWSER_SELECTION` (JSON), which overrides everything.
2. The paired selection in `browser-selection.json`.
3. The default: Firefox. With no paired profile, startup fails with an actionable error.

To allow Chromium as well, use `{"preference":{"mode":"prefer","engines":["firefox","chromium"]}}`, or `{"preference":{"mode":"managedChromium"}}` for Chromium only.

## Operating it

- Keep the paired Firefox running while you automate.
- Completed commands carry `browserExecution` evidence with `engine: "firefox"` and `interactionPath: "engineNative"`.
- After you rebuild or reinstall the extension, the next session start compares the running build with the installed one. A build that reports an ID is asked to reload. An older build is handled by restarting the enrolled Firefox. One attempt is made per installed build. If the old build still runs, the session fails with a non-retryable error naming both IDs.
- All runtime sessions on a profile share one BiDi connection, because Firefox accepts one BiDi session per browser. Page attachments stay per session and renew before they expire.
- If pairing is interrupted by a restart or a rotated descriptor, the extension retries with a bounded cooldown.

### Toolbar popup

| Section | Shows |
|---|---|
| Pair / Re-pair | Enroll or refresh pairing |
| Connection | Paired state and the companion and profile IDs |
| Session | Active lease count, plus session ID and seed when the host owns fingerprint spoofing |
| Fingerprint | Spoofing toggle. Read-only while a bobby session owns it |
| Humanize | Status set by session policy |
| Debug | Native port state, protocol version, last error |

## Errors

| Failure | Code | Retryable |
|---|---|---|
| Action deadline, response timeout, expired page binding | `deadlineExceeded` | yes |
| Connection or queue closed, no active connection, no target discovery, no attachment grant | `browserCommandFailed` | yes |
| Invalid companion event | `browserCommandFailed` | no |
| Attachment or profile does not match the grant or connection | `policyDenied` | no |
| Page does not match the attachment grant | `notFound` | no |
| Pending command or page-binding capacity exhausted | `resourceExhausted` | yes |
| Pairing code invalid or expired, profile mismatch, companion revoked, lease expired, credential invalid | `policyDenied` | no |
| Paired profile not found | `notFound` | no |
| Running build still differs from the installed one after a reload or restart | `browserLaunchFailed` | no |

## Limitations

- JavaScript dialogs are not supported on Firefox, and `evaluate_javascript` needs Chromium.
- There is no headless mode. The paired Firefox is a real window.
- Vision-assisted intents work on Firefox under the same gates as Chromium. See [Intent commands](intents.md#vision-assist).
