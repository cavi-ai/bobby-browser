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
    cacheReadTokens: 100_000,
    cacheCreationTokens: 5_000,
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
      engine: "chromium",
      providerMode: "off",
    },
  };
}

function runScore(records: object[], mode?: "check" | "phase", baselinePath?: string) {
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
      env: {
        ...process.env,
        GAUNTLET_RESULTS_DIR: resultsDir,
        BOBBY_MCP_COMMAND: "false",
        OPUS_RESULTS_DIR: path.join(resultsDir, "missing-opus"),
        ...(baselinePath ? { GAUNTLET_BASELINE_PATH: baselinePath } : {}),
      },
    },
  );
}

function check(records: object[], baselinePath?: string) {
  return runScore(records, "check", baselinePath);
}

// A complete baseline over the five tasks with caller-controlled budgets, so
// dimension tests do not depend on the committed baseline's numbers.
function writeBaseline(overrides: {
  budget?: Record<string, number>;
  dimensions?: Record<string, unknown>;
}): string {
  const dir = mkdtempSync(path.join(tmpdir(), "bobby-baseline-test-"));
  const baselinePath = path.join(dir, "baseline.json");
  const tasks = Object.fromEntries(
    taskIds.map((task) => [task, { pass: true, wallSeconds: 60, toolErrors: 0 }]),
  );
  writeFileSync(
    baselinePath,
    JSON.stringify({
      tasks,
      budget: overrides.budget ?? {
        perTaskCacheReadTokens: 1_000_000,
        perTaskCacheCreationTokens: 100_000,
        perTaskToolCalls: 50,
      },
      ...(overrides.dimensions ? { dimensions: overrides.dimensions } : {}),
    }),
  );
  return baselinePath;
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
    /tool\truns\tpass%\ttime s\tcalls\tbobby\thost\tdiscover\terr%\tin tok\tcache read tok\tcache create tok\tout tok/,
  );
  assert.match(result.stdout, /bobby\t5\t100\t1\.0\t10\.0\t6\.0\t4\.0\t2\.0\t0\t100\t100000\t5000\t200/);
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

test("check aggregates a multi-run batch: every run must pass, means face thresholds", () => {
  const runs = taskIds.flatMap((task) => [
    record(task, "current"),
    { ...record(task, "current"), wallMs: 3_000 },
  ]);

  const result = check(runs);

  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  // Mean wall of 1s+3s = 2s per task; the OK line names the run count.
  assert.match(result.stdout, /OK   customer-update: 2s errors=0 cacheR=100000 cacheC=5000 calls=10\.0 \(n=2\)/);
});

test("check fails a multi-run batch when any single run fails", () => {
  const runs = taskIds.flatMap((task) => [
    record(task, "current"),
    { ...record(task, "current"), pass: task !== "documents" },
  ]);

  const result = check(runs);

  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stdout, /FAIL documents: baseline passes, 1\/2 run\(s\) did not/);
});

test("check enforces the baseline token budget on mean cache-read", () => {
  const runs = taskIds.map((task) => ({
    ...record(task, "current"),
    cacheReadTokens: 900_000,
  }));

  const result = check(runs);

  assert.equal(result.status, 1, result.stderr);
  assert.match(
    result.stdout,
    /BUDGET customer-update: cacheRead 900000 > 700000/,
  );
});

test("check enforces the call budget on the mean across runs", () => {
  const runs = taskIds.flatMap((task) => [
    { ...record(task, "current"), toolCalls: 20, bobbyToolCalls: 12, hostToolCalls: 8 },
    record(task, "current"),
  ]);

  const result = check(runs);

  // Mean (20+10)/2 = 15 stays under the 16-call cap: the budget faces the
  // batch mean, not the worst run, so no breach fires.
  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  assert.equal(result.stdout.includes("BUDGET"), false, result.stdout);
  assert.match(result.stdout, /calls=15\.0 \(n=2\)/);
});

test("phase emits ranks without mixing skipped vision into competitor order", () => {
  const bobby = taskIds.map((task) => ({
    ...record(task, "g"),
    tool: "bobby",
    model: "grok-4.6",
    provenance: { ...record(task, "g").provenance, requestedModel: "grok-4.6", driver: "cursor" },
  }));
  const playwright = taskIds.map((task) => ({
    ...record(task, "g"),
    tool: "playwright-mcp",
    wallMs: 500,
    model: "grok-4.6",
  }));
  const result = runScore(
    [
      ...bobby,
      ...playwright,
      { tool: "bobby-vision", skipped: true, skipReason: "mlx-unreachable" },
    ],
    "phase",
  );
  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  const scorecard = JSON.parse(result.stdout);
  assert.equal(scorecard.tools.bobby.passRate, 1);
  assert.equal(scorecard.tools["bobby-vision"].skipped, true);
  assert.equal(
    scorecard.ranks.performance.find((row: { tool: string }) => row.tool === "playwright-mcp")
      .rank,
    1,
  );
  assert.equal(scorecard.operator.install, null);
});

test("check applies the matching engine/providerMode dimension's thresholds", () => {
  // The chromium/off overlay tightens the cache-read budget below the run's
  // 100k; the top-level budget would pass it.
  const baselinePath = writeBaseline({
    budget: {
      perTaskCacheReadTokens: 1_000_000,
      perTaskCacheCreationTokens: 100_000,
      perTaskToolCalls: 50,
    },
    dimensions: {
      "chromium/off": {
        tasks: Object.fromEntries(
          taskIds.map((task) => [
            task,
            { pass: true, wallSeconds: 60, toolErrors: 0 },
          ]),
        ),
        budget: {
          perTaskCacheReadTokens: 50_000,
          perTaskCacheCreationTokens: 100_000,
          perTaskToolCalls: 50,
        },
      },
    },
  });
  const runs = taskIds.map((task) => record(task, "current"));

  const result = check(runs, baselinePath);

  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stdout, /baseline dimension: chromium\/off/);
  assert.match(result.stdout, /BUDGET customer-update: cacheRead 100000 > 50000/);
});

test("check falls back to the top-level baseline when no dimension matches", () => {
  const baselinePath = writeBaseline({
    dimensions: {
      "firefox/off": {
        tasks: Object.fromEntries(
          taskIds.map((task) => [
            task,
            { pass: true, wallSeconds: 60, toolErrors: 0 },
          ]),
        ),
        budget: {
          perTaskCacheReadTokens: 50_000,
          perTaskCacheCreationTokens: 100_000,
          perTaskToolCalls: 50,
        },
      },
    },
  });
  // The batch ran chromium/off; only firefox/off has an overlay, so the
  // top-level budget (1M cache-read) applies and 100k passes.
  const runs = taskIds.map((task) => record(task, "current"));

  const result = check(runs, baselinePath);

  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  assert.equal(result.stdout.includes("baseline dimension:"), false, result.stdout);
});

test("check rejects a dimension entry without tasks", () => {
  const baselinePath = writeBaseline({
    dimensions: { "firefox/off": { budget: {} } },
  });
  const runs = taskIds.map((task) => record(task, "current"));

  const result = check(runs, baselinePath);

  assert.equal(result.status, 1);
  assert.match(result.stderr, /dimensions\.firefox\/off must carry tasks/);
});
