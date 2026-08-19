import assert from "node:assert/strict";
import test from "node:test";

import type {
  ClickCommand,
  ClickModifier,
  CommandOutcome,
  CompleteFormField,
  ControlAction,
  ControlActionEvidence,
  Evidence,
  FillIntent,
  InterfaceError,
  InterfaceEvent,
  OpenPageRequest,
  RuntimeInfo,
  TargetSpec,
} from "../src/index.js";

test("ClickCommand exposes the cross-engine modifier contract", () => {
  const modifiers: ClickModifier[] = ["shift", "ctrl", "alt", "meta"];
  const click: ClickCommand = {
    selector: "#range-end",
    target: null,
    boundary: false,
    expectedUrl: null,
    modifiers,
  };
  assert.deepEqual(click.modifiers, modifiers);
});

test("TargetSpec accepts the minimal semantic target supported by the wire schema", () => {
  const target: TargetSpec = {
    role: "textbox",
    accessibleName: "Phone",
    ordinal: 1,
  };
  assert.deepEqual(target, { role: "textbox", accessibleName: "Phone", ordinal: 1 });
});

test("control action contracts preserve the closed tagged wire shape", () => {
  const actions: ControlAction[] = [
    { kind: "setText", value: "Ada" },
    { kind: "setText", value: "Ada", clearFirst: false },
    { kind: "setChecked", checked: true },
    { kind: "selectOne", value: "pro" },
    { kind: "selectMany", values: ["one", "two"] },
    { kind: "setFiles", paths: ["/input/resume.pdf"] },
    { kind: "clear" },
    { kind: "activate" },
  ];
  const evidence: ControlActionEvidence = {
    operation: "setChecked",
    target: { role: "checkbox", accessibleName: "Terms", ordinal: null, framePath: [], shadowPath: [] },
    state: { kind: "checked", checked: true },
    validity: { willValidate: true, valid: true, flags: [], message: null, describedBy: [] },
    nodeReplaced: false,
  };
  assert.equal(actions.length, 8);
  assert.equal(evidence.operation, "setChecked");
});

test("FillIntent and CompleteFormField carry the unified ControlAction vocabulary", () => {
  const fill: FillIntent = { purpose: "Email", value: { kind: "setText", value: "a@b.co" } };
  const field: CompleteFormField = {
    name: "toppings",
    purpose: "Toppings",
    value: { kind: "selectMany", values: ["ham", "cheese"] },
  };
  assert.deepEqual(fill.value, { kind: "setText", value: "a@b.co" });
  assert.deepEqual(field.value, { kind: "selectMany", values: ["ham", "cheese"] });
});

test("legacy FillValue shapes no longer type-check as ControlAction", () => {
  // @ts-expect-error legacy `text`/`text` kind was replaced by `setText`/`value`
  const legacyText: ControlAction = { kind: "text", text: "a@b.co" };
  // @ts-expect-error legacy `select`/`option` kind was replaced by `selectOne`/`value`
  const legacySelect: ControlAction = { kind: "select", option: "Pro" };
  // @ts-expect-error legacy `checked` kind was replaced by `setChecked`
  const legacyChecked: ControlAction = { kind: "checked", checked: true };
  // @ts-expect-error legacy `files` kind was replaced by `setFiles`
  const legacyFiles: ControlAction = { kind: "files", paths: ["cv.pdf"] };
  assert.ok(legacyText && legacySelect && legacyChecked && legacyFiles);
});

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
