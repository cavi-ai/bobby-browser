# Firefox companion icon + popup enroll — design

**Status:** approved for planning (brainstorm 2026-08-04)  
**Scope:** Firefox companion toolbar icon (C1 mark) and popup-triggered
profile enroll / re-pair mediated by the native host  
**Out of scope:** starting `bobby serve` from the extension; Chrome MV3
companion; pasting pairing codes into the popup; changing fingerprint /
humanize enforcement

**Related:** `2026-08-03-firefox-companion-popup-panel-design.md` (operator
status panel). This spec extends that panel with Pair and branding assets.

## Goal

Operators enroll and re-pair the Firefox companion from the toolbar popup
instead of running `bobby enroll-firefox-profile`. Pairing secrets never
enter the extension. v1 requires a live companion listener (`bobby serve` or
the MCP gateway path that publishes the descriptor).

## Decisions

| Topic | Choice |
|---|---|
| Approach | Popup → native host enroll API (not CLI shell-out, not protocol bump) |
| First-time + re-pair | Both from the popup |
| Secrets | Native host only; extension never sees pairing code / endpoint / descriptor body |
| Serve already up | Required for v1; document it |
| `profileDir` / `bidiUrl` | Infer on host: profile path + `$profileDir/WebDriverBiDiServer.json` |
| CLI enroll | Keep for CI/scripts; human docs prefer the popup |
| Icon | C1: yellow badge, bee stripes, white serif **B**, Chinese **鲍比** under B inside the circle; no staff/wand |
| Toolbar 16px | Omit 鲍比; keep B + stripes |
| Transliteration | **鲍比** (Bào bǐ) |

## Icon

- Ship SVG source plus PNGs for Firefox `browser_action` at 16 / 32 / 48 / 96.
- Assets under `packages/firefox-companion/icons/`.
- Wire `manifest.json` `icons` and `browser_action.default_icon`.
- Large surfaces (popup header / about) may use the full C1 mark including
  鲍比; 16px toolbar uses the simplified crop.

## Architecture

```
Popup (Pair) → Background → Native host → Runtime companion listener
                     │            │
                     │            ├─ read descriptor (host-local)
                     │            ├─ infer profileDir + bidiUrl
                     │            ├─ enroll + write browser-selection.json
                     │            └─ continue existing pair handshake
                     └─ popupStatus (paired / errors; no secrets)
```

- **Popup** — Connection section gains **Pair** / **Re-pair**; shows progress
  and operator-safe errors. Existing paired/unpaired badge and fingerprint /
  humanize rows stay as in the popup-panel design. Pair click is a
  `runtime.sendMessage` only (never talks to the host directly).
- **Background** — On click, sends a host control message (working name:
  `enrollProfile`). Must not receive or log pairing codes, endpoints, or
  descriptor contents. Continues to own pair state for `popupStatus`.
- **Native host** — Reads the local descriptor, infers `profileDir` and
  `bidiUrl`, runs the same enroll path as today’s CLI, writes
  `browser-selection.json` (atomic, owner-only `0600` on Unix), then the
  existing pair handshake proceeds.
- **Runtime** — No v1 change to how serve publishes the descriptor / listener.
- **CLI** — `bobby enroll-firefox-profile` remains for automation.

Prefer extracting shared enroll logic used by both CLI and native host rather
than duplicating selection-write rules. Avoid a companion-protocol version
bump for this feature if the control message stays on the native-messaging
channel only (extension ↔ host).

## Popup UX

1. Unpaired / never enrolled → primary **Pair**.
2. Enroll in flight → button disabled; short status (“Pairing…”).
3. Success → **Paired** badge + truncated `companionId` / `profileId`.
4. Already paired but operator wants refresh → **Re-pair** (same host path).
5. Failure → operator-facing reason from the table below; debug may show a
   safe error code only.

## Inference (host)

| Field | Source |
|---|---|
| `profileDir` | Profile this companion install is bound to (install / native-host sidecar paths). Fail closed if ambiguous. |
| `bidiUrl` | Read `$profileDir/WebDriverBiDiServer.json`; build WebSocket URL. Fail if missing (Firefox not started with BiDi). |
| Selection file | Same path and shape as current `bobby enroll-firefox-profile` output. |

## Error handling

| Case | Popup shows |
|---|---|
| Serve/gateway not up / no live descriptor | Start bobby serve, then Pair again |
| BiDi file missing / unreadable | Start Firefox with remote debugging, then Pair again |
| Profile path ambiguous / unknown | Profile path unknown — re-run bobby install (see docs) |
| Pairing timeout | Pairing timed out |
| Native host disconnected | Existing disconnected / unpaired reason |
| Success | Paired + truncated ids |

Never surface pairing codes, bearer material, or raw descriptor JSON in the
popup, status strings, or extension logs.

## Testing

- **Extension unit:** Pair / Re-pair button states; `enrollProfile` request
  shape contains no secret fields; popup status after success and each failure
  class.
- **Host / Rust:** Inference from fixture `WebDriverBiDiServer.json`; enroll
  writes selection; existing fail-closed rules (non-loopback, bad descriptor)
  still hold.
- **CLI:** Existing `bobby enroll-firefox-profile` tests stay green; shared
  enroll path covered once.
- **Assets:** Manifest references resolve; 16px asset is the B+stripes crop
  without 鲍比 (assert via dedicated file or build step).

## Docs

Update the Firefox companion guide: human path is popup **Pair** with serve
already up; CLI enroll remains for CI. Note icon branding only as needed for
operators (no design essay).

## Non-goals (explicit)

- Auto-starting serve or Firefox from the extension.
- Multi-profile picker UI in v1 (single inferred profile; ambiguous → fail).
- Changing BiDi attach, fingerprint ownership, or lease protocol.
