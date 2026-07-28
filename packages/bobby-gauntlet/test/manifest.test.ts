import assert from "node:assert/strict";
import test from "node:test";

import { createManifest } from "../src/manifest.js";
import { createFoundationController, FOUNDATION_STATIONS, type FoundationStates } from "../src/app.js";
import { GauntletController } from "../src/controller.js";

test("same version seed and difficulty produce the same immutable manifest", () => {
  const a = createManifest("course-v1", "seed-42", "foundation");
  const b = createManifest("course-v1", "seed-42", "foundation");

  assert.deepEqual(a, b);
  assert.ok(Object.isFrozen(a));
  assert.ok(Object.isFrozen(a.stations));
  assert.throws(() => {
    (a as { seed: string }).seed = "forged";
  }, TypeError);
});

test("manifest rejects an empty or oversized seed", () => {
  assert.throws(() => createManifest("course-v1", "", "foundation"), /seed/i);
  assert.throws(() => createManifest("course-v1", "x".repeat(257), "foundation"), /seed/i);
});

test("controller rejects a station whose manifest capability or mutation version differs", () => {
  const manifest = createManifest("course-v1", "seed-42", "foundation", [
    { id: "route", version: "1", mutationVersion: "unexpected", capabilities: ["navigation"] },
    { id: "dom-drift", version: "1", mutationVersion: "1", capabilities: ["dom-observation"] },
    { id: "semantic-form", version: "1", mutationVersion: "1", capabilities: ["form-fill"] },
    { id: "validation", version: "1", mutationVersion: "1", capabilities: ["form-fill", "validation"] },
  ]);

  assert.throws(() => new GauntletController<FoundationStates>(manifest, FOUNDATION_STATIONS), /manifest/i);

  const capabilityMismatch = createManifest("course-v1", "seed-42", "foundation", [
    { id: "route", version: "1", mutationVersion: "1", capabilities: ["redirect"] },
    { id: "dom-drift", version: "1", mutationVersion: "1", capabilities: ["dom-observation"] },
    { id: "semantic-form", version: "1", mutationVersion: "1", capabilities: ["form-fill"] },
    { id: "validation", version: "1", mutationVersion: "1", capabilities: ["form-fill", "validation"] },
  ]);
  assert.throws(() => new GauntletController<FoundationStates>(capabilityMismatch, FOUNDATION_STATIONS), /manifest/i);
});

test("foundation controller fails closed for unsupported difficulty and replays the supported tier", () => {
  assert.throws(() => createFoundationController("course-v1", "seed-42", "advanced"), /difficulty/i);
  assert.throws(
    () => new GauntletController<FoundationStates>(createManifest("course-v1", "seed-42", "adversarial"), FOUNDATION_STATIONS),
    /difficulty/i,
  );
  assert.deepEqual(
    createFoundationController("course-v1", "seed-42", "foundation").manifest,
    createFoundationController("course-v1", "seed-42", "foundation").manifest,
  );
});

