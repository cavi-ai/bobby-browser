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
    model: "claude-opus-5",
    pass: true,
    wallMs: 1_000,
    toolCalls: 10,
    bobbyToolCalls: 6,
    hostToolCalls: 4,
    discoveryToolCalls: 2,
    toolErrors: 0,
    inputTokens: 100,
    outputTokens: 200,
    provenance: {
      repoHead: "1111111111111111111111111111111111111111",
      repoDirty: false,
      sourceStateSha256: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
      claudeCliVersion: "2.1.219",
      nodeVersion: "v26.0.0",
      platform: "darwin-arm64",
      taskSetSha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      runnerSetSha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      bobbyBinarySha256: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
      requestedModel: "claude-opus-5",
      timeboxSeconds: 300,
      startupToolset: "explore",
      claudeIsolation: "strict-mcp,project-settings,no-skills,no-chrome,no-persistence",
    },
  };
}

function runScore(records: object[], mode?: "check") {
  const resultsDir = mkdtempSync(path.join(tmpdir(), "bobby-score-test-"));
  mkdirSync(resultsDir, { recursive: true });
  writeFileSync(
    path.join(resultsDir, "runs.jsonl"),
    records.map((row) => JSON.stringify(row)).join("\n") + "\n",
  );
  return spawnSync(
    process.execPath,
    ["--import", "tsx", scorePath, ...(mode ? [mode] : [])],
    {
      cwd: path.dirname(sourceDir),
      encoding: "utf8",
      env: { ...process.env, GAUNTLET_RESULTS_DIR: resultsDir },
    },
  );
}

function check(records: object[]) {
  return runScore(records, "check");
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

test("check rejects a latest record without overhead measurements", () => {
  const current = taskIds.map((task) => record(task, "current"));
  delete (current[0] as Partial<ReturnType<typeof record>>).hostToolCalls;

  const result = check(current);

  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stdout, /INVALID customer-update: missing call breakdown/);
});

test("score separates Bobby calls from host and discovery overhead", () => {
  const result = runScore(taskIds.map((task) => record(task, "current")));

  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  assert.match(
    result.stdout,
    /tool\truns\tpass%\ttime s\tcalls\tbobby\thost\tdiscover\terr%\ttokens/,
  );
  assert.match(result.stdout, /bobby\t5\t100\t1\.0\t10\.0\t6\.0\t4\.0\t2\.0\t0\t300/);
});

test("check rejects a latest batch without benchmark provenance", () => {
  const current = taskIds.map((task) => record(task, "current"));
  delete (current[0] as Partial<ReturnType<typeof record>>).provenance;

  const result = check(current);

  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stdout, /INVALID customer-update: missing benchmark provenance/);
});

test("check rejects unavailable reproducibility provenance", () => {
  const current = taskIds.map((task) => record(task, "current"));
  current[0].provenance.bobbyBinarySha256 = "unavailable";

  const result = check(current);

  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stdout, /INVALID customer-update: missing benchmark provenance/);
});

test("check rejects mixed provenance inside one batch", () => {
  const current = taskIds.map((task) => record(task, "current"));
  current[1].provenance.repoHead = "2222222222222222222222222222222222222222";

  const result = check(current);

  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stdout, /INVALID onboarding: provenance differs within batch/);
});

test("check rejects a run without a transcript-derived actual model", () => {
  const current = taskIds.map((task) => record(task, "current"));
  delete (current[0] as Partial<ReturnType<typeof record>>).model;

  const result = check(current);

  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stdout, /INVALID customer-update: actual model is missing/);
});

test("check rejects an actual model that differs from the requested model", () => {
  const current = taskIds.map((task) => record(task, "current"));
  current[1].model = "claude-sonnet-4-6";

  const result = check(current);

  assert.equal(result.status, 1, result.stderr);
  assert.match(
    result.stdout,
    /INVALID onboarding: actual model claude-sonnet-4-6 differs from requested model claude-opus-5/,
  );
});
