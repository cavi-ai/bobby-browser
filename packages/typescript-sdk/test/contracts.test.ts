import assert from "node:assert/strict";
import test from "node:test";

import type {
  CommandOutcome,
  Evidence,
  InterfaceError,
  InterfaceEvent,
  OpenPageRequest,
  RuntimeInfo,
} from "../src/index.js";

test("contracts preserve every discriminated wire variant", () => {
  const evidence: Evidence[] = [
    { kind: "navigation", url: "https://example.test", title: "Example" },
    { kind: "screenshot", artifactId: "artifact", mediaType: "image/png", width: 1, height: 1, bytes: 1, sha256: "00" },
  ];
  const outcome: CommandOutcome = {
    status: "needsReconciliation",
    commandId: "command-id",
    error: { code: "httpStateConflict", message: "state changed", layer: "workflow", retryable: false },
    evidence,
  };
  const event: InterfaceEvent = { kind: "command.outcome", cursor: 1, payload: outcome };
  const error: InterfaceError = {
    code: "idempotencyConflict",
    layer: "interface",
    message: "conflict",
    correlationId: "correlation-id",
    commandId: "command-id",
    retryable: false,
    retryAfterMs: null,
    reconciliationRequired: true,
    requiredCapability: null,
  };
  const runtime: RuntimeInfo = { version: "v", capabilities: [], active_sessions: 0, queued_jobs: 0, uptime_ms: 0 };
  const openPage: OpenPageRequest = { session_id: "session-id" };

  assert.equal(event.kind, "command.outcome");
  assert.equal(error.reconciliationRequired, true);
  assert.equal(runtime.active_sessions, 0);
  assert.equal(openPage.session_id, "session-id");
});
