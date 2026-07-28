import assert from "node:assert/strict";
import test from "node:test";

import { createChampionshipController } from "../src/app.js";

test("championship honestly rejects unsupported difficulties", () => {
  assert.throws(() => createChampionshipController("course-v1", "seed", "advanced"), /difficulty/i);
  assert.throws(() => createChampionshipController("course-v1", "seed", "adversarial"), /difficulty/i);
});

test("championship manifest registers all ten isolated stations in a deterministic order", () => {
  const controller = createChampionshipController("course-v1", "seed", "foundation");
  assert.deepEqual(controller.manifest.stations.map((station) => station.id), [
    "route", "dom-drift", "semantic-form", "validation", "iframe", "shadow-root", "popup", "file-attachment", "download", "championship",
  ]);
});

