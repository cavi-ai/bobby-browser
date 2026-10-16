import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { commandVerificationStatus, isCommandOutcome, type CommandOutcome, type VerificationStatus } from "../src/index.js";

test("verification status matches the shared SDK contract", () => {
  const cases = JSON.parse(readFileSync(new URL("../../../../tests/fixtures/command-verification.json", import.meta.url), "utf8")) as Array<{ name: string; outcome: CommandOutcome; expected: VerificationStatus }>;
  for (const entry of cases) {
    const { postStateStatus: _observation, ...wire } = entry.outcome as CommandOutcome & { postStateStatus?: unknown };
    assert.ok(isCommandOutcome(wire), entry.name);
    assert.equal(commandVerificationStatus(entry.outcome), entry.expected, entry.name);
  }
});
