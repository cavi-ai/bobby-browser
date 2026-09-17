import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  isCommandOutcome,
  isContextAskResponse,
  isContextNeighborsResponse,
  isContextSiteResponse,
  isEvidence,
  isEventBatch,
  isEventGap,
  isJobStatusResponse,
  isJobSubmitResponse,
  isPageState,
  isRecoveryDecision,
  isRuntimeInfo,
  isSessionState,
  isSessionStateList,
  isWorkflowCheckpoint,
} from "../src/validators.js";
import { isInterfaceError } from "../src/events.js";
import type { Evidence, TargetSpec } from "../src/contracts.js";

const ID = "00000000-0000-4000-8000-000000000001";
const ID_2 = "00000000-0000-4000-8000-000000000002";
const SHA = "0123456789abcdef".repeat(4);
const TIME = "2026-07-17T12:34:56Z";
const JOB_ID = "job_00000000-0000-4000-8000-000000000010";

const CONTEXT_ANSWER = {
  target: { role: "textbox", accessibleName: "Email" },
  confidence: 0.95,
  observedAt: { kind: "generation", generation: 2 },
  source: "observed",
} as const;
const CONTEXT_CONTROL = {
  role: "button",
  accessibleName: "Continue",
  ordinal: 0,
  intents: { click: { successCount: 3, failureCount: 1, lastVerifiedDay: 20_000, source: "observed" } },
} as const;

const JOB_STATUS = {
  id: JOB_ID,
  name: "echo",
  priority: "high",
  status: "completed",
  payload: { hello: "world" },
  createdAt: TIME,
  startedAt: TIME,
  completedAt: TIME,
  retryCount: 0,
  maxRetries: 3,
  result: { jobId: JOB_ID, success: true, output: { hello: "world" }, error: null, completedAt: TIME },
  error: null,
  timeoutMs: 5_000,
  correlationId: ID,
} as const;

function target(): TargetSpec {
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

function evidenceFixtures(): Evidence[] {
  return [
    { kind: "executionPath", path: "directHttp", reason: "eligibleStaticDocument", stateVersion: 0, elapsedMs: 1, bytes: null, sha256: null },
    { kind: "executionPath", path: "browserFallback", reason: "javascriptRequired", stateVersion: 2, elapsedMs: 3, bytes: 4, sha256: SHA, finalUrl: "https://example.test/", contentType: "text/html", status: 200, redirectChain: ["https://example.test/"] },
    { kind: "navigation", url: "https://example.test/", title: "Example" },
    { kind: "inspection", selector: null, url: "https://example.test/", title: "Example", text: "body", html: null },
    { kind: "submitSettlement", outcome: "settled" },
    { kind: "element", selector: "#save", text: null },
    { kind: "upload", selector: "input", paths: ["/tmp/a"] },
    { kind: "page", pageId: ID, url: "https://example.test/", title: "Example" },
    { kind: "pageGeneration", pageId: ID, generation: 2 },
    { kind: "pages", pages: [{ pageId: ID, url: "https://example.test/", title: "Example" }] },
    { kind: "popup", openerPageId: ID, pageId: ID_2, url: "https://example.test/popup", title: "Popup" },
    { kind: "popupClosed", popupPageId: ID_2, openerPageId: ID },
    { kind: "download", filename: "a.bin", path: "/tmp/a.bin", bytes: 4, sha256: SHA, savedTo: "downloads/a.bin" },
    { kind: "resolution", target: target(), fingerprint: { pageId: ID, frame: null, role: null, name: null, stableAttributes: { id: "save" } }, candidates: [{ role: null, name: "Save", score: -1, reasons: ["exact"] }], bestMatchAuthorized: false },
    { kind: "wait", condition: { kind: "element", target: target(), state: "visible" }, elapsedMs: 1, observations: 1, observed: "Save" },
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
    { kind: "accessibilitySnapshot", pageId: ID, nodes: [{ role: "link", target: { role: "link", accessibleName: "Docs", framePath: [{ role: "iframe", accessibleName: "Content", ordinal: null }] }, url: "https://example.test/docs" }], truncated: false },
    { kind: "formSnapshot", snapshot: { schemaVersion: 1, pageId: ID, forms: [], unownedControls: [], truncated: false } },
    { kind: "formValidation", issues: [{ controlId: "email", controlKind: "email", accessibleName: "Email", target: null, validity: { willValidate: true, valid: false, flags: ["valueMissing"], message: "Required", describedBy: [] } }] },
    { kind: "controlAction", action: { operation: "setChecked", target: { role: "checkbox", accessibleName: "Business" }, state: { kind: "checked", checked: true }, validity: { willValidate: true, valid: true }, nodeReplaced: false, revealedControls: [{ controlKind: "text", accessibleName: "Company", target: { role: "textbox", accessibleName: "Company" } }] } },
    { kind: "structuredExtraction", pageId: ID, value: { title: "Example" }, truncated: false },
    { kind: "challengeDetection", confidence: 0.9, detection: { challenge_type: "recaptchaV2Checkbox", confidence: 0.8, region: { x: 1, y: 2, width: 3, height: 4 }, blocking: true, hints: { target_field_purpose: "Verify", instruction_text: "Check the box" } }, priorKind: "recaptchaV2Checkbox" },
    { kind: "cookieState", pageId: ID, cookies: [{ name: "session", value: "value", domain: "example.test", path: "/", secure: true, httpOnly: true, sameSite: "Lax", expiresUnix: 1 }] },
    { kind: "pdfArtifact", artifactId: "artifact-pdf", mediaType: "application/pdf", bytes: 4, sha256: SHA },
    { kind: "dialog", dialogType: "alert", message: "Saved", action: "accept" },
    { kind: "emulation", viewport: { width: 1280, height: 720 }, geolocation: { latitude: 40.7, longitude: -74, accuracy: 10 } },
    { kind: "harArtifact", artifactId: "artifact-har", mediaType: "application/json", bytes: 4, sha256: SHA, entries: 1 },
    {
      kind: "intentExecution",
      record: {
        intentKind: "locate",
        purpose: "Continue",
        resolutionPath: "visionPrefill",
        planSummary: "role=button name~Continue",
        candidates: [],
        waitElapsedMs: null,
        verification: "resolved",
        artifactIds: [],
        visionProposalSha256: null,
      },
    },
    { kind: "humanization", engine: "firefox", actions: 2, synthesizedMs: 25 },
    { kind: "extraction", field: "title", value: "Example", resolutionPath: "visionPrefill" },
    { kind: "extraction", field: "missing", resolutionPath: "deterministic", errorCode: "targetNotFound" },
  ];
}

function recoveryDecision(): unknown {
  return { status: "restarted", checkpointId: ID, lineage: { workflowId: ID, abandonedAttemptId: ID, attemptId: ID_2, reason: "retry" }, evidence: [] };
}

test("evidence fixtures cover every Rust wire variant and field", () => {
  const source = readFileSync(new URL("../../../../crates/bobby-browser-client/src/outcomes.rs", import.meta.url), "utf8");
  const start = source.indexOf("pub enum Evidence {");
  const body = source.slice(start, source.indexOf("\n}\n", start));
  const variants = [...body.matchAll(/^    ([A-Z][A-Za-z0-9_]*)\s*\{\n([\s\S]*?)^    \},?$/gm)];
  const rustKinds = variants.map((match) => `${match[1]![0]!.toLowerCase()}${match[1]!.slice(1)}`).sort();
  const fixtureKinds = [...new Set(evidenceFixtures().map((evidence) => evidence.kind))].sort();
  assert.notEqual(start, -1);
  assert.deepEqual(fixtureKinds, rustKinds);
  for (const variant of variants) {
    const kind = `${variant[1]![0]!.toLowerCase()}${variant[1]!.slice(1)}`;
    const expected = ["kind", ...[...variant[2]!.matchAll(/^        ([a-z][a-z0-9_]*):/gm)].map((field) => field[1]!.replace(/_([a-z])/g, (_, letter: string) => letter.toUpperCase()))].sort();
    const actual = [...new Set(evidenceFixtures().filter((evidence) => evidence.kind === kind).flatMap((evidence) => Object.keys(evidence)))].sort();
    assert.deepEqual(actual, expected, kind);
  }
});

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
    recoveryReceipts: [],
    createdAt: TIME,
  };
}

test("deep validators accept every exact public response variant", () => {
  assert.equal(isRuntimeInfo({ version: "1", capabilities: ["session:read"], active_sessions: 0, queued_jobs: 1, uptime_ms: Number.MAX_SAFE_INTEGER }), true);
  assert.equal(isRuntimeInfo({
    version: "1", capabilities: [], active_sessions: 0, queued_jobs: 0, uptime_ms: 0,
    visionProposeBudgetMs: 1500,
    operationalMetrics: { observationWindowMs: 10 },
    providerHealth: [{ providerMode: "http", status: "degraded", successes: 8, failures: 2, consecutiveFailures: 0, budgetViolations: 4, lastLatencyMs: 1900, latencyBudgetMs: 1500, failureThreshold: 3 }],
  }), true);
  assert.equal(isSessionState({ id: ID, profile: "default", proxy: null, page_ids: [ID_2], created_at: TIME, last_used_at: TIME, execution_policy: { javascriptEvaluation: false, visionAssist: false, fingerprint: false, humanize: false } }), true);
  assert.equal(isPageState({ id: ID, session_id: ID_2, url: null, mode: "Document", ready_state: "complete", pending_requests: 0 }), true);

  assert.equal(new Set(evidenceFixtures().map((evidence) => evidence.kind)).size, 32);
  for (const evidence of evidenceFixtures()) {
    assert.equal(isCommandOutcome({ status: "completed", commandId: ID, evidence: [evidence] }), true, JSON.stringify(evidence));
  }
  const commandError = { code: "internal", message: "x", layer: "workflow", retryable: false };
  for (const outcome of [
    { status: "retryableFailure", commandId: ID, error: commandError },
    { status: "needsReconciliation", commandId: ID, error: commandError, evidence: [] },
    { status: "policyDenied", commandId: ID, error: { ...commandError, code: "policyDenied" } },
    { status: "resourceExhausted", commandId: ID, error: { ...commandError, code: "resourceExhausted" }, retryAfterMs: 0 },
    { status: "restarted", commandId: ID, priorAttemptId: ID, attemptId: ID_2, reason: "retry", evidence: [] },
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
  const withReceipts = checkpoint();
  withReceipts.recoveryReceipts = [{ recordedAt: TIME, opaque: "receipt payloads are passed through" }];
  assert.equal(isWorkflowCheckpoint(withReceipts), true);
  assert.equal(isSessionStateList([{ id: ID, profile: "default", proxy: null, page_ids: [ID_2], created_at: TIME, last_used_at: TIME, execution_policy: { javascriptEvaluation: false, visionAssist: false, fingerprint: false, humanize: false } }]), true);
  assert.equal(isSessionStateList([]), true);
  assert.equal(isSessionStateList([{ id: "bad" }]), false);
  for (const decision of [
    { status: "resumed", checkpointId: ID, attemptId: ID_2, evidence: evidenceFixtures() },
    { status: "needsReconciliation", checkpointId: ID, attemptId: ID_2, reason: "inspect", evidence: [] },
    recoveryDecision(),
  ]) assert.equal(isRecoveryDecision(decision), true, JSON.stringify(decision));
  assert.equal(isEvidence({
    kind: "accessibilitySnapshot",
    pageId: ID,
    nodes: [{ role: "main", children: [{ role: "textbox", name: "Email", target: { role: "textbox", accessibleName: "Email", ordinal: 0 }, value: "broken", description: "Use a work address", required: true, disabled: false, readOnly: false, invalid: true, checked: false, autocomplete: "email", valueMin: "1", valueMax: "10" }] }],
    truncated: false,
  }), true);
  assert.equal(isEvidence({ kind: "accessibilitySnapshot", pageId: ID, nodes: [{ role: 3 }], truncated: false }), false);
  assert.equal(isEvidence({ kind: "accessibilitySnapshot", pageId: ID, nodes: [{ role: "textbox", name: "Email", target: { role: "textbox", accessibleName: "Email", ordinal: 2048 } }], truncated: false }), false);
  assert.equal(isEventBatch({ events: [{ cursor: 1, kind: "command.outcome", payload: null }], latestAvailable: 1 }, 0, 100), true);
  assert.equal(isEventGap({ reason: "historyLost", earliestAvailable: 0 }), true);
});

test("context response validators accept hits and misses and reject malformed nested data", () => {
  assert.equal(isContextAskResponse({ answer: CONTEXT_ANSWER, hit: true }), true);
  assert.equal(isContextAskResponse({ answer: null, hit: false, reason: "notRemembered", nextStep: "a11y_snapshot" }), true);
  assert.equal(isContextNeighborsResponse({ neighbors: { answer: CONTEXT_ANSWER, form: "signup", pagePattern: "/join", controls: [CONTEXT_CONTROL] }, hit: true }), true);
  assert.equal(isContextNeighborsResponse({ neighbors: null, hit: false, reason: "notRemembered", nextStep: "a11y_snapshot" }), true);
  assert.equal(isContextSiteResponse({ site: { siteKey: "https://example.test", pages: { "/join": { signup: [CONTEXT_CONTROL] } } } }), true);
  assert.equal(isContextSiteResponse({ site: null }), true);

  assert.equal(isContextAskResponse({ answer: { ...CONTEXT_ANSWER, confidence: 2 }, hit: true }), false);
  assert.equal(isContextAskResponse({ answer: CONTEXT_ANSWER, hit: false }), false);
  assert.equal(isContextNeighborsResponse({ neighbors: { answer: CONTEXT_ANSWER, form: "signup", pagePattern: "/join", controls: [{ ...CONTEXT_CONTROL, unexpected: true }] }, hit: true }), false);
  assert.equal(isContextSiteResponse({ site: { siteKey: "https://example.test", pages: { "/join": { signup: [{ ...CONTEXT_CONTROL, intents: { click: { successCount: -1, failureCount: 0 } } }] } } } }), false);
});

test("job response validators enforce exact lifecycle and nested result contracts", () => {
  assert.equal(isJobSubmitResponse({ jobId: JOB_ID, status: "pending" }), true);
  assert.equal(isJobStatusResponse(JOB_STATUS), true);
  assert.equal(isJobStatusResponse({ ...JOB_STATUS, status: "failed", result: null, error: "handler failed" }), true);
  assert.equal(isJobStatusResponse({ ...JOB_STATUS, status: "cancelled", result: null, error: null }), true);

  assert.equal(isJobSubmitResponse({ jobId: "bad", status: "pending" }), false);
  assert.equal(isJobSubmitResponse({ jobId: JOB_ID, status: "unknown" }), false);
  assert.equal(isJobStatusResponse({ ...JOB_STATUS, result: { ...JOB_STATUS.result, jobId: "bad" } }), false);
  assert.equal(isJobStatusResponse({ ...JOB_STATUS, result: { ...JOB_STATUS.result, unexpected: true } }), false);
  assert.equal(isJobStatusResponse({ ...JOB_STATUS, retryCount: 4, maxRetries: 3 }), false);
  assert.equal(isJobStatusResponse({ ...JOB_STATUS, status: "running", completedAt: TIME, result: null }), false);
  assert.equal(isJobStatusResponse({ ...JOB_STATUS, unexpected: true }), false);
});

test("session state accepts the selected vision node returned by the runtime", () => {
  const session = {
    id: ID,
    profile: "default",
    proxy: null,
    page_ids: [],
    created_at: TIME,
    last_used_at: TIME,
    execution_policy: {
      javascriptEvaluation: false,
      visionAssist: true,
      fingerprint: false,
      humanize: false,
      visionNode: "acp-codex",
    },
  };
  assert.equal(isSessionState(session), true);
  assert.equal(isSessionState({
    ...session,
    execution_policy: { ...session.execution_policy, visionNode: "" },
  }), false);
  assert.equal(isSessionState({
    ...session,
    execution_policy: { ...session.execution_policy, visionNode: "x".repeat(129) },
  }), false);
});

test("malformed nested fixtures are rejected for every public response family", () => {
  const invalidCheckpoint = checkpoint();
  invalidCheckpoint.recoveryHistory = [{ recordedAt: "not-a-time", decision: recoveryDecision() }];
  const malformed: Array<[string, boolean]> = [
    ["RuntimeInfo", isRuntimeInfo({ version: "1", capabilities: [], active_sessions: 0.5, queued_jobs: 0, uptime_ms: 0 })],
    ["SessionState", isSessionState({ id: ID, profile: "default", proxy: null, page_ids: ["not-a-uuid"], created_at: TIME, last_used_at: TIME, execution_policy: { javascriptEvaluation: false, visionAssist: false, fingerprint: false, humanize: false } })],
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
  assert.equal(isSessionState({ id: "not-a-uuid", profile: "default", proxy: null, page_ids: [], created_at: TIME, last_used_at: TIME, execution_policy: { javascriptEvaluation: false, visionAssist: false, fingerprint: false, humanize: false } }), false);
  assert.equal(isSessionState({ id: ID, profile: "default", proxy: null, page_ids: [], created_at: "2026-07-17", last_used_at: TIME, execution_policy: { javascriptEvaluation: false, visionAssist: false, fingerprint: false, humanize: false } }), false);
  assert.equal(isSessionState({ id: ID, profile: "default", proxy: null, page_ids: [], created_at: "2026-02-30T12:00:00Z", last_used_at: TIME, execution_policy: { javascriptEvaluation: false, visionAssist: false, fingerprint: false, humanize: false } }), false);
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
  assert.equal(isRuntimeInfo({ version: "1", capabilities: [], active_sessions: 0, queued_jobs: 0, uptime_ms: 0, providerHealth: [{ providerMode: "http", status: "broken", successes: 0, failures: 0, consecutiveFailures: 0, budgetViolations: 0, failureThreshold: 3 }] }), false);
  assert.equal(isRuntimeInfo({ version: "1", capabilities: [], active_sessions: 0, queued_jobs: 0, uptime_ms: 0, providerHealth: "http" }), false);
  assert.equal(isSessionState(withExtra({ id: ID, profile: "default", proxy: null, page_ids: [], created_at: TIME, last_used_at: TIME, execution_policy: { javascriptEvaluation: false, visionAssist: false, fingerprint: false, humanize: false } })), false);
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
  assert.equal(isRecoveryDecision({ status: "restarted", checkpointId: ID, lineage: withExtra({ workflowId: ID, abandonedAttemptId: ID, attemptId: ID_2, reason: "x" }), evidence: [] }), false);
  assert.equal(isRecoveryDecision({ status: "restarted", checkpointId: ID, lineage: { workflowId: ID, abandonedAttemptId: ID, attemptId: ID_2, reason: "x" } }), false);

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
