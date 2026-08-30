import assert from "node:assert/strict";
import test from "node:test";

import { isFormSnapshot } from "../src/validators.js";

const ID = "00000000-0000-4000-8000-000000000001";

function control(kind = "email", state: unknown = { kind: "text", value: "agent@example.test" }): Record<string, unknown> {
  return {
    id: `${kind}-control`, formId: "account", groupId: "identity", target: { role: "textbox", accessibleName: "Email", ordinal: 0, framePath: [], shadowPath: [] },
    controlKind: kind, accessibleName: "Email", label: "Email", description: null, placeholder: null, autocomplete: "email", state,
    constraints: { required: true, readOnly: false, disabled: false, pattern: null, minLength: null, maxLength: 254, min: null, max: null, step: null, multiple: false, accept: [] },
    validity: { willValidate: true, valid: true, flags: [], message: null, describedBy: [] }, options: [], supportedOperations: ["setText", "clear"],
  };
}

function snapshot(): Record<string, unknown> {
  const email = control();
  return {
    schemaVersion: 1, pageId: ID, truncated: false, unownedControls: [], forms: [{
      id: "account", target: null, accessibleName: "Account", description: null,
      groups: [{ id: "identity", label: "Identity", description: null, controlIds: [email.id] }], controls: [email], submitControlIds: [], resetControlIds: [], validity: { valid: true, invalidControlIds: [] },
    }],
  };
}

test("canonical form snapshot accepts bounded exact wire data", () => {
  assert.equal(isFormSnapshot(snapshot()), true);
});

test("canonical form snapshot rejects unknown versions, fields, and dangling references", () => {
  assert.equal(isFormSnapshot({ ...snapshot(), schemaVersion: 2 }), false);
  assert.equal(isFormSnapshot({ ...snapshot(), selector: "#account" }), false);
  const dangling = snapshot();
  (dangling.forms as Array<Record<string, unknown>>)[0]!.submitControlIds = ["missing"];
  assert.equal(isFormSnapshot(dangling), false);
  const oneSided = snapshot();
  ((oneSided.forms as Array<Record<string, unknown>>)[0]!.groups as Array<Record<string, unknown>>)[0]!.controlIds = [];
  assert.equal(isFormSnapshot(oneSided), false);
});

test("canonical form snapshot never accepts exposed password text", () => {
  const value = snapshot();
  const form = (value.forms as Array<Record<string, unknown>>)[0]!;
  const password = control("password", { kind: "text", value: "secret" });
  password.id = "password-control";
  form.controls = [password];
  form.groups = [{ id: "identity", label: null, description: null, controlIds: [password.id] }];
  assert.equal(isFormSnapshot(value), false);
  password.state = { kind: "redacted", present: true };
  assert.equal(isFormSnapshot(value), true);
});

test("canonical form snapshot enforces byte and collection bounds", () => {
  const oversized = snapshot();
  const form = (oversized.forms as Array<Record<string, unknown>>)[0]!;
  const item = (form.controls as Array<Record<string, unknown>>)[0]!;
  item.label = "x".repeat(2049);
  assert.equal(isFormSnapshot(oversized), false);
  assert.equal(isFormSnapshot({ ...snapshot(), forms: Array.from({ length: 65 }, () => (snapshot().forms as unknown[])[0]) }), false);
});

test("slim wire shape omits default fields and still validates", () => {
  // What the runtime emits after the form-snapshot slimming: default-value
  // fields (label equal to accessibleName, null placeholder, empty options,
  // unconstrained constraints, validity bits at their default) are absent.
  const slimControl = {
    id: "email-control",
    formId: "account",
    groupId: "identity",
    target: { role: "textbox", accessibleName: "Email" },
    controlKind: "email",
    accessibleName: "Email",
    autocomplete: "email",
    state: { kind: "empty" },
    constraints: { required: true, maxLength: 254 },
    validity: { valid: true },
    supportedOperations: ["setText", "clear"],
  };
  const slimSnapshot = {
    schemaVersion: 1, pageId: ID, truncated: false, unownedControls: [], forms: [{
      id: "account", target: null, accessibleName: "Account", description: null,
      groups: [{ id: "identity", label: null, description: null, controlIds: ["email-control"] }], controls: [slimControl], submitControlIds: [], resetControlIds: [], validity: { valid: true, invalidControlIds: [] },
    }],
  };
  assert.equal(isFormSnapshot(slimSnapshot), true);
  // The slim shape must not open the door to garbage: unknown keys stay
  // rejected, and a missing always-required field is still fatal.
  assert.equal(isFormSnapshot({ ...slimSnapshot, forms: [{ ...(slimSnapshot.forms as Array<Record<string, unknown>>)[0]!, controls: [{ ...slimControl, mystery: true }] }] }), false);
  assert.equal(
    isFormSnapshot({ ...slimSnapshot, forms: [{ ...(slimSnapshot.forms as Array<Record<string, unknown>>)[0]!, controls: [{ id: "x", controlKind: "email", state: { kind: "empty" } }] }] }),
    false,
    "validity and supportedOperations stay required",
  );
});
