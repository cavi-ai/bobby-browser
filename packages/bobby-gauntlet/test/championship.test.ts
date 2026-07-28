import assert from "node:assert/strict";
import test from "node:test";

import { createChampionshipController } from "../src/app.js";
import { finalizeScorecard, manifestDigest, type ChampionshipTelemetry } from "../src/scorecard.js";
import { failed, passed, type GauntletStation } from "../src/station.js";

function telemetry(): ChampionshipTelemetry {
  return { engine: "firefox", activeSkills: [{ id: "SkillGhost", version: "1" }], recoveryCount: 1, strategyChanges: ["observe"], durationMs: 25 };
}

test("championship fails when any mandatory station fails", () => {
  const controller = createChampionshipController("course-v1", "championship-seed", "foundation");
  const results = recordFailureForEveryStation(controller);

  const scorecard = finalizeScorecard(controller.manifest, results, telemetry());

  assert.equal(scorecard.passed, false);
  assert.equal(scorecard.terminalFailure?.code, "postconditionFailed");
});

test("championship rejects a missing, duplicate, unknown, or version-mismatched station result", () => {
  const controller = createChampionshipController("course-v1", "championship-seed", "foundation");
  const all = recordFailureForEveryStation(controller);

  assert.throws(() => finalizeScorecard(controller.manifest, all.slice(1), telemetry()), /missing/i);
  assert.throws(() => finalizeScorecard(controller.manifest, [...all, all[0]!], telemetry()), /duplicate/i);
  assert.throws(() => finalizeScorecard(controller.manifest, [...all, { ...all[0]!, id: "unknown" }], telemetry()), /unknown/i);
  assert.throws(() => finalizeScorecard(controller.manifest, [{ ...all[0]!, version: "forged" }, ...all.slice(1)], telemetry()), /version/i);
});

test("championship scorecards are deterministic, deeply immutable, and bind a manifest digest", () => {
  const a = createChampionshipController("course-v1", "replay-seed", "foundation");
  const b = createChampionshipController("course-v1", "replay-seed", "foundation");
  const resultsA = recordFailureForEveryStation(a);
  const resultsB = recordFailureForEveryStation(b);
  const first = finalizeScorecard(a.manifest, resultsA, telemetry());
  const second = finalizeScorecard(b.manifest, resultsB, telemetry());

  assert.deepEqual(first, second);
  assert.ok(Object.isFrozen(first));
  assert.ok(Object.isFrozen(first.stations));
  assert.throws(() => {
    (first.stations[0]!.evidence[0] as { id: string }).id = "forged";
  }, TypeError);
  assert.throws(() => finalizeScorecard({ ...a.manifest, seed: "tampered" }, resultsA, telemetry()), /manifest/i);
});

test("championship finalization rejects a structurally invalid manifest even when entries use its digest", () => {
  const controller = createChampionshipController("course-v1", "manifest-seed", "foundation");
  const results = recordFailureForEveryStation(controller);
  const malformed = { ...controller.manifest, stations: [] };
  const retagged = results.map((entry) => ({ ...entry, manifestDigest: manifestDigest(malformed) }));

  assert.throws(() => finalizeScorecard(malformed, retagged, telemetry()), /manifest/i);
});

test("championship refuses a coherently retagged manifest that omits, reorders, or substitutes a mandatory station", () => {
  const controller = createChampionshipController("course-v1", "course-authority-seed", "foundation");
  const results = recordFailureForEveryStation(controller);
  const retag = (manifest: typeof controller.manifest) => results
    .filter((entry) => manifest.stations.some((station) => station.id === entry.id))
    .map((entry) => ({ ...entry, manifestDigest: manifestDigest(manifest) }));
  const omitted = { ...controller.manifest, stations: controller.manifest.stations.slice(1) };
  const reordered = { ...controller.manifest, stations: [...controller.manifest.stations].reverse() };
  const substituted = { ...controller.manifest, stations: controller.manifest.stations.map((station) => station.id === "route" ? { ...station, id: "forged-route" } : station) };

  assert.throws(() => finalizeScorecard(omitted, retag(omitted), telemetry()), /course/i);
  assert.throws(() => finalizeScorecard(reordered, retag(reordered), telemetry()), /course/i);
  assert.throws(() => finalizeScorecard(substituted, retag(substituted), telemetry()), /course/i);
});

function recordFailureForEveryStation(controller: ReturnType<typeof createChampionshipController>) {
  for (const station of controller.manifest.stations) controller.verify(station.id, {});
  return controller.verifiedResults();
}

test("a station module registers without controller changes", () => {
  const controller = createChampionshipController("course-v1", "extensible-seed", "foundation");
  const custom: GauntletStation<{ value: string }, { value?: unknown }> = {
    id: "adversarial-fixture",
    version: "1",
    mutationVersion: "1",
    supportedDifficulties: ["foundation"],
    title: "Adversarial fixture",
    capabilities: ["fixture"],
    setup: () => ({ value: "expected" }),
    verify: (state, submission) => submission.value === state.value ? passed("fixture-verified", "fixture:verified") : failed("postconditionFailed", "station", "inspect-canonical-route", "fixture:rejected"),
    reset: () => {},
  };

  const extended = controller.withStation(custom);
  assert.equal(extended.manifest.stations.at(-1)?.id, "adversarial-fixture");
  assert.equal(extended.verify("adversarial-fixture", { value: "expected" }).passed, true);
});

