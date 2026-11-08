# Changelog

## 0.24.0

- `ExecutionPolicy.engine` runs a session on `chromium` or `firefox`.
- Evidence types carry the `screenshot` session, workflow, page, URL and capture time, the `upload` `sha256` list, `outcomeReading` and the file control's `names`.
- `SubmitAndVerifyIntent.expectedState` is optional, `CaptureScreenshotCommand` takes `maxEdge`, and intent hints take `attributes`.
- Align the client package with the Bobby Browser 0.24.0 runtime release.

## 0.23.0

- `ControlAction` includes `{ kind: "credential"; name }` for configured credentials.
- Align the client package with the Bobby Browser 0.23.0 runtime release.

## 0.22.1

- Form-control targets support structural attributes for unnamed file inputs.
- Align the client package with the Bobby Browser 0.22.1 runtime release.

## 0.3.1

- Align package version with the Bobby Browser 0.3.1 runtime / docs release.

## 0.3.0

- Align package version with the Bobby Browser 0.3.0 runtime release.
- Publish under `@cavi-ai/bobby-browser` (was `@bobby-browser/sdk`).
- Document session teardown (`deleteSession`), page activation, and
  accessibility-snapshot helpers including `intentHintsFromAccessibilityTarget`.
- Add `recoveryStatus(workflowId)` for `GET /v1/recovery/{id}`.
- Intent envelope helpers remain the HTTP/TypeScript path; MCP clients can use
  dedicated `intent_*` tools when available.

## 0.2.1

- Align the client package with the Firefox-default runtime release.

## 0.2.0

- Initial public npm release of the typed Bobby Browser runtime client.
