import assert from "node:assert/strict";
import test from "node:test";

import {
  isCommandOutcome,
  isEvidence,
  isEventBatch,
  isEventGap,
  isPageState,
  isRecoveryDecision,
  isRuntimeInfo,
  isSessionState,
  isWorkflowCheckpoint,
} from "../src/validators.js";
import { isInterfaceError } from "../src/events.js";

const ID = "00000000-0000-4000-8000-000000000001";
const ID_2 = "00000000-0000-4000-8000-000000000002";
const SHA = "0123456789abcdef".repeat(4);
const TIME = "2026-07-17T12:34:56Z";

function target(): unknown {
  return {
    css: null,
    testId: "save",
    role: null,
    accessibleName: null,
    label: null,
    text: { kind: "exact", value: "Save" },
    attributes: { name: "save" },
    framePath: [],
    shadowPath: [],
    ordinal: 0,
    allowBestMatch: false,
  };
}

function evidenceFixtures(): unknown[] {
  return [
    { kind: "executionPath", path: "directHttp", reason: "eligibleStaticDocument", stateVersion: 0, elapsedMs: 1, bytes: null, sha256: null },
    { kind: "executionPath", path: "chromiumFallback", reason: "javascriptRequired", stateVersion: 2, elapsedMs: 3, bytes: 4, sha256: SHA, finalUrl: "https://example.test/", contentType: "text/html", status: 200, redirectChain: ["https://example.test/"] },
    { kind: "navigation", url: "https://example.test/", title: "Example" },
    { kind: "inspection", selector: null, url: "https://example.test/", title: "Example", text: "body", html: null },
    { kind: "element", selector: "#save", text: null },
    { kind: "upload", selector: "input", paths: ["/tmp/a"] },
    { kind: "page", pageId: ID, url: "https://example.test/", title: "Example" },
    { kind: "pages", pages: [{ pageId: ID, url: "https://example.test/", title: "Example" }] },
    { kind: "popup", openerPageId: ID, pageId: ID_2, url: "https://example.test/popup", title: "Popup" },
    { kind: "download", filename: "a.bin", path: "/tmp/a.bin", bytes: 4, sha256: SHA },
    { kind: "resolution", target: target(), fingerprint: { pageId: ID, frame: null, role: null, name: null, stableAttributes: { id: "save" } }, candidates: [{ role: null, name: "Save", score: -1, reasons: ["exact"] }], bestMatchAuthorized: false },
    { kind: "wait", condition: { kind: "element", target: target(), state: "visible" }, elapsedMs: 1, observations: 1 },
    { kind: "wait", condition: { kind: "text", target: target(), matcher: { kind: "contains", value: "Save" } }, elapsedMs: 1, observations: 1 },
    { kind: "wait", condition: { kind: "value", target: target(), matcher: { kind: "regex", value: "S.*" } }, elapsedMs: 1, observations: 1 },
    { kind: "wait", condition: { kind: "url", matcher: { kind: "exact", value: "https://example.test/" } }, elapsedMs: 1, observations: 1 },
    { kind: "wait", condition: { kind: "document", ready: "networkIdle" }, elapsedMs: 1, observations: 1 },
    { kind: "wait", condition: { kind: "networkQuiet", idleMs: 1, maxInFlight: 0 }, elapsedMs: 1, observations: 1 },
    { kind: "wait", condition: { kind: "networkQuiet", idleMs: 50, maxInFlight: 0, ignoreUrlSubstrings: ["analytics"], ignoreResourceTypes: ["Image"], ignoreLongLived: true }, elapsedMs: 10, observations: 2, excludedClasses: ["urlSubstring:analytics", "eventSource"] },
    { kind: "screenshot", artifactId: "artifact-1", mediaType: "image/png", width: 1, height: 1, bytes: 4, sha256: SHA },
    { kind: "configuration", name: "focusEmulation", value: "true" },
    { kind: "browserExecution", engine: "firefox", browserVersion: "128.0", profileId: ID, interactionPath: "engineNative" },
    { kind: "javaScriptResult", value: { answer: 42 }, truncated: false },
    {
      kind: "intentExecution",
      record: {
        intentKind: "locate",
        purpose: "Continue",
        resolutionPath: "deterministic",
        planSummary: "role=button name~Continue",
        candidates: [],
        waitElapsedMs: null,
        verification: "resolved",
        artifactIds: [],
        visionProposalSha256: null,
      },
    },
  ];
}

function recoveryDecision(): unknown {
  return { status: "restarted", checkpointId: ID, lineage: { workflowId: ID, abandonedAttemptId: ID, attemptId: ID_2, reason: "retry" } };
}

function checkpoint(): Record<string, unknown> {
  return {
    schemaVersion: 1,
    checkpointId: ID,
    workflowId: ID,
    attemptId: ID,
    sessionId: ID,
    pageId: ID,
    restartUrl: "https://example.test/",
    currentUrl: "https://example.test/current",
    cursor: null,
    boundaryCommandId: ID_2,
    recoveryClass: "boundary",
    invariants: [
      { kind: "url", value: "https://example.test/current" },
      { kind: "title", value: "Example" },
      { kind: "text", selector: "#status", value: "Saved" },
    ],
    replayableInputs: ["input"],
    evidence: evidenceFixtures(),
    recoveryHistory: [{ recordedAt: TIME, decision: recoveryDecision() }],
    createdAt: TIME,
  };
}

test("deep validators accept every exact public response variant", () => {
  assert.equal(isRuntimeInfo({ version: "1", capabilities: ["session:read"], active_sessions: 0, queued_jobs: 1, uptime_ms: Number.MAX_SAFE_INTEGER }), true);
  assert.equal(isSessionState({ id: ID, profile: "default", proxy: null, page_ids: [ID_2], created_at: TIME, last_used_at: TIME, execution_policy: { javascriptEvaluation: false, visionAssist: false } }), true);
  assert.equal(isPageState({ id: ID, session_id: ID_2, url: null, mode: "Document", ready_state: "complete", pending_requests: 0 }), true);

  for (const evidence of evidenceFixtures()) {
    assert.equal(isCommandOutcome({ status: "completed", commandId: ID, evidence: [evidence] }), true, JSON.stringify(evidence));
  }
  const commandError = { code: "internal", message: "x", layer: "workflow", retryable: false };
  for (const outcome of [
    { status: "retryableFailure", commandId: ID, error: commandError },
    { status: "needsReconciliation", commandId: ID, error: commandError, evidence: [] },
    { status: "policyDenied", commandId: ID, error: { ...commandError, code: "policyDenied" } },
    { status: "resourceExhausted", commandId: ID, error: { ...commandError, code: "resourceExhausted" }, retryAfterMs: 0 },
    { status: "restarted", commandId: ID, priorAttemptId: ID, attemptId: ID_2, reason: "retry" },
    { status: "failed", commandId: ID, error: commandError },
    {
      status: "failed",
      commandId: ID,
      error: { ...commandError, code: "intentCompileFailed" },
      evidence: [{
        kind: "intentExecution",
        record: {
          intentKind: "locate",
          purpose: "Continue",
          resolutionPath: "deterministic",
          planSummary: "miss",
          candidates: [],
          waitElapsedMs: null,
          verification: "targetNotFound",
          artifactIds: [],
          visionProposalSha256: null,
        },
      }],
    },
  ]) assert.equal(isCommandOutcome(outcome), true, JSON.stringify(outcome));

  assert.equal(isEvidence({
    kind: "intentExecution",
    record: {
      intentKind: "locate",
      purpose: null,
      resolutionPath: "visionFallback",
      planSummary: "",
      candidates: [],
      waitElapsedMs: 1,
      verification: "ok",
      artifactIds: ["a"],
      visionProposalSha256: SHA,
    },
  }), true);

  assert.equal(isWorkflowCheckpoint(checkpoint()), true);
  for (const decision of [
    { status: "resumed", checkpointId: ID, attemptId: ID_2, evidence: evidenceFixtures() },
    { status: "needsReconciliation", checkpointId: ID, attemptId: ID_2, reason: "inspect", evidence: [] },
    recoveryDecision(),
  ]) assert.equal(isRecoveryDecision(decision), true, JSON.stringify(decision));
  assert.equal(isEventBatch({ events: [{ cursor: 1, kind: "command.outcome", payload: null }], latestAvailable: 1 }, 0, 100), true);
  assert.equal(isEventGap({ reason: "historyLost", earliestAvailable: 0 }), true);
});

test("malformed nested fixtures are rejected for every public response family", () => {
  const invalidCheckpoint = checkpoint();
  invalidCheckpoint.recoveryHistory = [{ recordedAt: "not-a-time", decision: recoveryDecision() }];
  const malformed: Array<[string, boolean]> = [
    ["RuntimeInfo", isRuntimeInfo({ version: "1", capabilities: [], active_sessions: 0.5, queued_jobs: 0, uptime_ms: 0 })],
    ["SessionState", isSessionState({ id: ID, profile: "default", proxy: null, page_ids: ["not-a-uuid"], created_at: TIME, last_used_at: TIME, execution_policy: { javascriptEvaluation: false, visionAssist: false } })],
    ["PageState/PageMode", isPageState({ id: ID, session_id: ID_2, url: null, mode: "document", ready_state: "complete", pending_requests: 0 })],
    ["CommandOutcome/Evidence", isCommandOutcome({ status: "completed", commandId: ID, evidence: [{ kind: "download", filename: "a", path: "b", bytes: 1, sha256: SHA.toUpperCase() }] })],
    ["CommandOutcome/CommandError", isCommandOutcome({ status: "failed", commandId: ID, error: { code: "unknown", message: "x", layer: "workflow", retryable: false } })],
    ["WorkflowCheckpoint", isWorkflowCheckpoint(invalidCheckpoint)],
    ["RecoveryDecision", isRecoveryDecision({ status: "restarted", checkpointId: ID, lineage: { workflowId: ID, abandonedAttemptId: "bad", attemptId: ID_2, reason: "x" } })],
    ["EventBatch", isEventBatch({ events: [{ cursor: Number.MAX_SAFE_INTEGER + 1, kind: "x", payload: null }], latestAvailable: 0 }, 0, 100)],
    ["EventGap", isEventGap({ reason: "historyLost", earliestAvailable: -1 })],
  ];
  for (const [family, accepted] of malformed) assert.equal(accepted, false, family);
});

test("validators reject invalid UUID, timestamp, digest, finite-number, and optional/null shapes", () => {
  assert.equal(isSessionState({ id: "not-a-uuid", profile: "default", proxy: null, page_ids: [], created_at: TIME, last_used_at: TIME, execution_policy: { javascriptEvaluation: false, visionAssist: false } }), false);
  assert.equal(isSessionState({ id: ID, profile: "default", proxy: null, page_ids: [], created_at: "2026-07-17", last_used_at: TIME, execution_policy: { javascriptEvaluation: false, visionAssist: false } }), false);
  assert.equal(isSessionState({ id: ID, profile: "default", proxy: null, page_ids: [], created_at: "2026-02-30T12:00:00Z", last_used_at: TIME, execution_policy: { javascriptEvaluation: false, visionAssist: false } }), false);
  assert.equal(isRuntimeInfo({ version: "1", capabilities: [], active_sessions: 0, queued_jobs: 0, uptime_ms: Infinity }), false);
  assert.equal(isCommandOutcome({ status: "completed", commandId: ID, evidence: [{ kind: "executionPath", path: "directHttp", reason: "eligibleStaticDocument", stateVersion: 0, elapsedMs: 0, bytes: null, sha256: SHA }] }), false);
  assert.equal(isCommandOutcome({ status: "completed", commandId: ID, evidence: [{ kind: "executionPath", path: "directHttp", reason: "eligibleStaticDocument", stateVersion: 0, elapsedMs: 0, bytes: null, sha256: null, finalUrl: 1 }] }), false);
  assert.equal(isCommandOutcome({ status: "completed", commandId: ID, evidence: [{ kind: "resolution", target: target(), fingerprint: { pageId: ID, frame: null, role: null, name: null, stableAttributes: {} }, candidates: [{ role: null, name: null, score: 1.5, reasons: [] }], bestMatchAuthorized: false }] }), false);
  assert.equal(isEventBatch({ events: [{ cursor: 2, kind: "x", payload: null }, { cursor: 1, kind: "x", payload: null }], latestAvailable: 2 }, 0, 100), false);
  assert.equal(isEventBatch({ events: [{ cursor: 2, kind: "x", payload: null }], latestAvailable: 1 }, 0, 100), false);
});

test("validators reject unknown and variant-incompatible keys at every object layer", () => {
  const withExtra = <T extends Record<string, unknown>>(value: T): T & { unexpected: boolean } => ({ ...value, unexpected: true });
  assert.equal(isRuntimeInfo(withExtra({ version: "1", capabilities: [], active_sessions: 0, queued_jobs: 0, uptime_ms: 0 })), false);
  assert.equal(isSessionState(withExtra({ id: ID, profile: "default", proxy: null, page_ids: [], created_at: TIME, last_used_at: TIME, execution_policy: { javascriptEvaluation: false, visionAssist: false } })), false);
  assert.equal(isPageState(withExtra({ id: ID, session_id: ID_2, url: null, mode: "Document", ready_state: "complete", pending_requests: 0 })), false);

  for (const evidence of evidenceFixtures()) {
    assert.equal(isCommandOutcome({ status: "completed", commandId: ID, evidence: [withExtra(evidence as Record<string, unknown>)] }), false, `extra evidence key: ${JSON.stringify(evidence)}`);
  }
  assert.equal(isCommandOutcome({ status: "completed", commandId: ID, evidence: [], error: { code: "internal", message: "x", layer: "workflow", retryable: false } }), false);
  assert.equal(isCommandOutcome({ status: "retryableFailure", commandId: ID, error: { code: "internal", message: "x", layer: "workflow", retryable: true }, evidence: [] }), false);
  assert.equal(isCommandOutcome({ status: "failed", commandId: ID, error: withExtra({ code: "internal", message: "x", layer: "workflow", retryable: false }) }), false);

  const nestedTarget = target() as Record<string, unknown>;
  assert.equal(isCommandOutcome({ status: "completed", commandId: ID, evidence: [{ kind: "resolution", target: withExtra(nestedTarget), fingerprint: { pageId: ID, frame: null, role: null, name: null, stableAttributes: {} }, candidates: [], bestMatchAuthorized: false }] }), false);
  assert.equal(isCommandOutcome({ status: "completed", commandId: ID, evidence: [{ kind: "resolution", target: nestedTarget, fingerprint: withExtra({ pageId: ID, frame: null, role: null, name: null, stableAttributes: {} }), candidates: [], bestMatchAuthorized: false }] }), false);
  assert.equal(isCommandOutcome({ status: "completed", commandId: ID, evidence: [{ kind: "resolution", target: nestedTarget, fingerprint: { pageId: ID, frame: null, role: null, name: null, stableAttributes: {} }, candidates: [withExtra({ role: null, name: null, score: 0, reasons: [] })], bestMatchAuthorized: false }] }), false);
  assert.equal(isCommandOutcome({ status: "completed", commandId: ID, evidence: [{ kind: "pages", pages: [withExtra({ pageId: ID, url: "https://example.test/", title: "Example" })] }] }), false);
  assert.equal(isCommandOutcome({ status: "completed", commandId: ID, evidence: [{ kind: "wait", condition: withExtra({ kind: "url", matcher: { kind: "exact", value: "x" } }), elapsedMs: 0, observations: 0 }] }), false);
  assert.equal(isCommandOutcome({ status: "completed", commandId: ID, evidence: [{ kind: "wait", condition: { kind: "url", matcher: withExtra({ kind: "exact", value: "x" }) }, elapsedMs: 0, observations: 0 }] }), false);

  const extraCheckpoint = checkpoint();
  extraCheckpoint.unexpected = true;
  assert.equal(isWorkflowCheckpoint(extraCheckpoint), false);
  for (const invariant of [
    { kind: "url", value: "x", unexpected: true },
    { kind: "title", value: "x", selector: "#bad" },
    { kind: "text", selector: "#x", value: "x", title: "bad" },
  ]) {
    const value = checkpoint();
    value.invariants = [invariant];
    assert.equal(isWorkflowCheckpoint(value), false);
  }
  const extraHistory = checkpoint();
  extraHistory.recoveryHistory = [{ recordedAt: TIME, decision: recoveryDecision(), unexpected: true }];
  assert.equal(isWorkflowCheckpoint(extraHistory), false);

  assert.equal(isRecoveryDecision({ status: "resumed", checkpointId: ID, attemptId: ID_2, evidence: [], lineage: {} }), false);
  assert.equal(isRecoveryDecision({ status: "restarted", checkpointId: ID, lineage: withExtra({ workflowId: ID, abandonedAttemptId: ID, attemptId: ID_2, reason: "x" }) }), false);
  assert.equal(isRecoveryDecision({ status: "restarted", checkpointId: ID, lineage: { workflowId: ID, abandonedAttemptId: ID, attemptId: ID_2, reason: "x" }, evidence: [] }), false);

  assert.equal(isEventBatch({ events: [{ cursor: 1, kind: "x", payload: null, unexpected: true }], latestAvailable: 1 }, 0, 1), false);
  assert.equal(isEventBatch({ events: [{ cursor: 1, kind: "x", payload: null }], latestAvailable: 1, unexpected: true }, 0, 1), false);
  assert.equal(isEventGap({ reason: "historyLost", earliestAvailable: 1, unexpected: true }), false);
  assert.equal(isInterfaceError({ code: "internal", layer: "interface", message: "x", correlationId: ID, commandId: null, retryable: false, retryAfterMs: null, reconciliationRequired: false, requiredCapability: null, unexpected: true }), false);
});

test("EventBatch validation is contextual to the requested cursor and limit", () => {
  assert.equal(isEventBatch({ events: [{ cursor: 1, kind: "x", payload: null }], latestAvailable: 10 }, 10, 100), false);
  assert.equal(isEventBatch({ events: [{ cursor: 11, kind: "x", payload: null }, { cursor: 12, kind: "x", payload: null }], latestAvailable: 12 }, 10, 1), false);
  assert.equal(isEventBatch({ events: [{ cursor: 11, kind: "x", payload: null }], latestAvailable: 9 }, 10, 1), false);
  assert.equal(isEventBatch({ events: [{ cursor: 11, kind: "x", payload: null }], latestAvailable: 11 }, 10, 1), true);
});
