import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { fileURLToPath } from "node:url";

const sourceDir = path.dirname(fileURLToPath(import.meta.url));
const scorePath = path.join(sourceDir, "score.ts");
const taskIds = [
  "customer-update",
  "onboarding",
  "documents",
  "authorization",
  "report-recovery",
];

function record(task: string, batchId: string) {
  return {
    batchId,
    tool: "bobby",
    task,
    pass: true,
    wallMs: 1_000,
    toolErrors: 0,
  };
}

function check(records: object[]) {
  const resultsDir = mkdtempSync(path.join(tmpdir(), "bobby-score-test-"));
  mkdirSync(resultsDir, { recursive: true });
  writeFileSync(
    path.join(resultsDir, "runs.jsonl"),
    records.map((row) => JSON.stringify(row)).join("\n") + "\n",
  );
  return spawnSync(
    process.execPath,
    ["--import", "tsx", scorePath, "check"],
    {
      cwd: path.dirname(sourceDir),
      encoding: "utf8",
      env: { ...process.env, GAUNTLET_RESULTS_DIR: resultsDir },
    },
  );
}

test("check rejects an incomplete latest invocation instead of borrowing stale tasks", () => {
  const oldComplete = taskIds.map((task) => record(task, "old"));
  const currentPartial = [record(taskIds[0], "current")];

  const result = check([...oldComplete, ...currentPartial]);

  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stdout, /MISS onboarding: no bobby run recorded/);
});

test("check accepts a complete latest invocation", () => {
  const oldComplete = taskIds.map((task) => record(task, "old"));
  const currentComplete = taskIds.map((task) => record(task, "current"));

  const result = check([...oldComplete, ...currentComplete]);

  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
});

test("check rejects a passing record with missing measurements", () => {
  const current = taskIds.map((task) => record(task, "current"));
  delete (current[0] as Partial<ReturnType<typeof record>>).wallMs;

  const result = check(current);

  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stdout, /INVALID customer-update: missing numeric wallMs or toolErrors/);
});
