import { readFileSync, readdirSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const harnessDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const resultsDir =
  process.env.GAUNTLET_RESULTS_DIR ??
  path.resolve(harnessDir, "../results");
const runsFile = path.join(resultsDir, "runs.jsonl");

if (!existsSync(runsFile)) {
  console.error("no results yet — run src/run.ts first");
  process.exit(1);
}

const runs = readFileSync(runsFile, "utf8")
  .split("\n")
  .filter(Boolean)
  .map((line) => JSON.parse(line));

if (process.argv[2] === "check") {
  // Regression gate: one complete bobby invocation against the committed
  // baseline. Pass must hold; wall time <= 2x baseline; errors <= +3.
  const baseline = JSON.parse(
    readFileSync(path.join(harnessDir, "baseline.json"), "utf8"),
  );
  const bobbyRuns = runs.filter((run) => run.tool === "bobby");
  const latestBatchId = bobbyRuns.at(-1)?.batchId;
  if (typeof latestBatchId !== "string" || latestBatchId.length === 0) {
    console.error("latest bobby result has no batchId — rerun src/run.ts");
    process.exit(1);
  }
  const latest = new Map<string, any>();
  for (const run of bobbyRuns.filter((run) => run.batchId === latestBatchId)) {
    latest.set(run.task, run);
  }
  let failures = 0;
  for (const [task, base] of Object.entries(
    baseline.tasks as Record<string, any>,
  )) {
    const run = latest.get(task);
    if (!run) {
      console.log(`MISS ${task}: no bobby run recorded`);
      failures += 1;
      continue;
    }
    if (
      !Number.isFinite(run.wallMs) ||
      run.wallMs < 0 ||
      !Number.isFinite(run.toolErrors) ||
      run.toolErrors < 0
    ) {
      console.log(`INVALID ${task}: missing numeric wallMs or toolErrors`);
      failures += 1;
      continue;
    }
    const wall = run.wallMs / 1000;
    if (!run.pass) {
      console.log(`FAIL ${task}: baseline passes, this run did not`);
      failures += 1;
    } else if (wall > base.wallSeconds * 2) {
      console.log(
        `SLOW ${task}: ${wall.toFixed(0)}s vs baseline ${base.wallSeconds}s (>2x)`,
      );
      failures += 1;
    } else if (run.toolErrors > base.toolErrors + 3) {
      console.log(
        `ERRORS ${task}: ${run.toolErrors} vs baseline ${base.toolErrors} (+3 slack)`,
      );
      failures += 1;
    } else {
      console.log(`OK   ${task}: ${wall.toFixed(0)}s errors=${run.toolErrors}`);
    }
  }
  process.exit(failures === 0 ? 0 : 1);
}

const byTool = new Map<string, any[]>();
for (const run of runs) {
  const list = byTool.get(run.tool) ?? [];
  list.push(run);
  byTool.set(run.tool, list);
}

const mean = (xs: number[]) =>
  xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : 0;

const EASE_KEYS = ["navigate", "click", "fill", "extract"] as const;

console.log(
  [
    "tool",
    "runs",
    "pass%",
    "time s",
    "calls",
    "err%",
    "tokens",
    ...EASE_KEYS,
  ].join("\t"),
);
for (const [tool, list] of [...byTool.entries()].sort()) {
  const passes = list.filter((r) => r.pass).length;
  const reports = list.map((r) => r.selfReport).filter(Boolean);
  const ease = EASE_KEYS.map((key) => {
    const scored = reports
      .map((r) => Number(r[key]))
      .filter((n) => Number.isFinite(n) && n > 0);
    return scored.length ? mean(scored).toFixed(1) : "-";
  });
  console.log(
    [
      tool,
      String(list.length),
      ((passes / list.length) * 100).toFixed(0),
      mean(list.map((r) => r.wallMs / 1000)).toFixed(1),
      mean(list.map((r) => r.toolCalls)).toFixed(1),
      (
        (list.reduce((a, r) => a + r.toolErrors, 0) /
          Math.max(
            1,
            list.reduce((a, r) => a + r.toolCalls, 0),
          )) *
        100
      ).toFixed(0),
      mean(list.map((r) => r.inputTokens + r.outputTokens)).toFixed(0),
      ...ease,
    ].join("\t"),
  );
}

console.log("\nblockers:");
for (const [tool, list] of [...byTool.entries()].sort()) {
  for (const run of list) {
    if (run.selfReport?.blockers) {
      console.log(`  ${tool}/${run.task}#${run.run}: ${run.selfReport.blockers}`);
    }
    if (run.selfReport?.bottlenecks) {
      console.log(
        `  ${tool}/${run.task}#${run.run} (bottleneck): ${run.selfReport.bottlenecks}`,
      );
    }
    for (const failure of run.failures ?? []) {
      console.log(`  ${tool}/${run.task}#${run.run} FAIL: ${failure}`);
    }
  }
}
