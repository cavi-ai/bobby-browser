import { readFileSync, readdirSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const harnessDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const resultsDir = path.resolve(harnessDir, "../results");
const runsFile = path.join(resultsDir, "runs.jsonl");

if (!existsSync(runsFile)) {
  console.error("no results yet — run src/run.ts first");
  process.exit(1);
}

const runs = readFileSync(runsFile, "utf8")
  .split("\n")
  .filter(Boolean)
  .map((line) => JSON.parse(line));

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
