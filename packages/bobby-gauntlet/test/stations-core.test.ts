import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { createFoundationController } from "../src/app.js";

test("approved upload fixture matches the browser-side byte contract", async () => {
  const fixture = await readFile(new URL("../fixtures/approved-upload.txt", import.meta.url), "utf8");

  assert.equal(fixture, "approved upload for Bobby\n");
});

test("route station accepts only the seeded canonical route", () => {
  const controller = createFoundationController("course-v1", "seed-42", "foundation");
  const state = controller.stateFor("route");

  assert.equal(controller.verify("route", { url: state.canonicalUrl }).passed, true);
  assert.equal(controller.verify("route", { url: `${state.canonicalUrl}/decoy` }).passed, false);
});

test("dom drift station rejects the stale target then accepts the seeded replacement", () => {
  const controller = createFoundationController("course-v1", "seed-42", "foundation");
  const state = controller.stateFor("dom-drift");

  assert.equal(controller.verify("dom-drift", { targetId: state.initialTargetId }).passed, false);
  assert.equal(controller.verify("dom-drift", { targetId: state.replacementTargetId }).passed, true);
});

test("semantic form accepts required meaningfully named values and rejects a claimed pass", () => {
  const controller = createFoundationController("course-v1", "seed-42", "foundation");
  const state = controller.stateFor("semantic-form");

  assert.equal(controller.verify("semantic-form", { values: { [state.fields.name]: "Bobby", [state.fields.email]: "bobby@example.test", [state.fields.plan]: "pro", "accept-terms": true } }).passed, true);
  assert.equal(controller.verify("semantic-form", { claimedPass: true, values: {} }).passed, false);
});

test("validation correction preserves valid input while correcting the seeded rejected field", () => {
  const controller = createFoundationController("course-v1", "seed-42", "foundation");
  const state = controller.stateFor("validation");

  assert.equal(controller.verify("validation", { values: { [state.validField]: state.validValue, [state.invalidField]: state.invalidValue } }).passed, false);
  assert.equal(controller.verify("validation", { values: { [state.validField]: state.validValue, [state.invalidField]: state.correctedValue } }).passed, true);
});

test("station state is isolated by direct route and reset is deterministic", () => {
  const controller = createFoundationController("course-v1", "seed-42", "foundation");
  const routeBefore = controller.stateFor("route");
  const formBefore = controller.stateFor("semantic-form");

  controller.reset("route");

  assert.deepEqual(controller.stateFor("route"), routeBefore);
  assert.deepEqual(controller.stateFor("semantic-form"), formBefore);
});
