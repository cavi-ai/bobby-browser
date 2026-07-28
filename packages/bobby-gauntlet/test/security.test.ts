import assert from "node:assert/strict";
import test from "node:test";

import { createFoundationController } from "../src/app.js";
import { GauntletController } from "../src/controller.js";
import { createManifest } from "../src/manifest.js";
import type { GauntletStation } from "../src/station.js";
import { sha256Hex } from "../src/station.js";

test("integrity digest is canonical SHA-256 and changes for a one-byte tamper", () => {
  assert.equal(sha256Hex("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
  assert.notEqual(sha256Hex("approved upload for Bobby\n"), sha256Hex("approved upload for Bobby!\n"));
});

test("page input cannot mark a station successful", () => {
  const controller = createFoundationController("course-v1", "seed-42", "foundation");

  const result = controller.verify("semantic-form", { claimedPass: true, values: {} });

  assert.equal(result.passed, false);
});

test("unknown stations fail closed without creating page-controlled state", () => {
  const controller = createFoundationController("course-v1", "seed-42", "foundation");

  const result = controller.verify("forged-station", { claimedPass: true });

  assert.equal(result.passed, false);
  if (result.passed) {
    throw new Error("forged station unexpectedly passed");
  }
  assert.equal(result.failure.code, "configurationConflict");
});

test("verify is total and fails closed for malformed station IDs and submissions", () => {
  const controller = createFoundationController("course-v1", "seed-42", "foundation");
  const malformedStationIds: unknown[] = [undefined, null, [], {}, "", "x".repeat(97)];
  const malformedSubmissions: unknown[] = [undefined, null, [], "claimed", 1];

  for (const stationId of malformedStationIds) {
    assert.doesNotThrow(() => controller.verify(stationId, {}));
    assert.equal(controller.verify(stationId, {}).passed, false);
  }
  for (const submission of malformedSubmissions) {
    assert.doesNotThrow(() => controller.verify("semantic-form", submission));
    assert.equal(controller.verify("semantic-form", submission).passed, false);
  }
});

test("scorecards contain only controller-verified immutable results", () => {
  const controller = createFoundationController("course-v1", "seed-42", "foundation");
  const beforeVerification = controller.scorecard();
  assert.equal(beforeVerification.passed, false);

  const state = controller.stateFor("route");
  controller.verify("route", { url: state.canonicalUrl });
  const scorecard = controller.scorecard();
  assert.equal(scorecard.results.route?.passed, true);
  assert.ok(Object.isFrozen(scorecard));
  assert.ok(Object.isFrozen(scorecard.results));
  assert.throws(() => {
    (scorecard.results.route?.evidence[0] as { id: string }).id = "mutated";
  }, TypeError);
});

test("controller rejects invalid station evidence before it can enter the run ledger", () => {
  const maliciousStation: GauntletStation<{ value: string }, unknown> = {
    id: "malicious",
    version: "1",
    mutationVersion: "1",
    supportedDifficulties: ["foundation"],
    title: "malicious",
    capabilities: ["test"],
    setup: () => ({ value: "state" }),
    verify: () => ({ passed: true, postconditions: ["forged"], evidence: [{ id: "/private/secret" }] }),
    reset: () => {},
  };
  const manifest = createManifest("course-v1", "seed-42", "foundation", [{ id: "malicious", version: "1", mutationVersion: "1", capabilities: ["test"] }]);
  const controller = new GauntletController<{ malicious: { value: string } }>(manifest, [maliciousStation]);

  const result = controller.verify("malicious", {});
  assert.equal(result.passed, false);
  assert.equal(controller.scorecard().passed, false);
});

test("controller rejects arbitrary failure guidance before persisting its scorecard", () => {
  const maliciousStation: GauntletStation<{ value: string }, unknown> = {
    id: "guidance-fixture",
    version: "1",
    mutationVersion: "1",
    supportedDifficulties: ["foundation"],
    title: "guidance fixture",
    capabilities: ["test"],
    setup: () => ({ value: "state" }),
    verify: () => ({
      passed: false,
      failure: { code: "postconditionFailed", layer: "station", retryable: false, guidance: "Bearer secret-token /private/host" as never },
      evidence: [{ id: "fixture:failure" }],
    }),
    reset: () => {},
  };
  const manifest = createManifest("course-v1", "seed-42", "foundation", [{ id: "guidance-fixture", version: "1", mutationVersion: "1", capabilities: ["test"] }]);
  const controller = new GauntletController<{ "guidance-fixture": { value: string } }>(manifest, [maliciousStation]);

  controller.verify("guidance-fixture", {});
  assert.doesNotMatch(JSON.stringify(controller.scorecard()), /secret-token|\/private\/host/);
});

