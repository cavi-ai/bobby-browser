# Changelog

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
