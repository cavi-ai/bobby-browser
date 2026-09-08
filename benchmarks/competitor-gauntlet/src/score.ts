import { mkdtempSync, readFileSync, readdirSync, existsSync, writeFileSync } from "node:fs";
import path from "node:path";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { buildPhaseScorecard, parseDoctorOutput, statsForTool } from "./phase.js";

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

const PROVENANCE_KEYS = [
  "repoHead",
  "repoDirty",
  "sourceStateSha256",
  "claudeCliVersion",
  "nodeVersion",
  "platform",
  "taskSetSha256",
  "runnerSetSha256",
  "bobbyBinarySha256",
  "requestedModel",
  "timeboxSeconds",
  "startupToolset",
  "claudeIsolation",
  "engine",
  "providerMode",
] as const;

function validProvenance(value: unknown): value is Record<string, unknown> {
  if (value === null || typeof value !== "object") return false;
  const record = value as Record<string, unknown>;
  return PROVENANCE_KEYS.every((key) => {
    const field = record[key];
    if (key === "timeboxSeconds") {
      return Number.isFinite(field) && Number(field) > 0;
    }
    if (key === "repoDirty") return typeof field === "boolean";
    return typeof field === "string" && field.length > 0 && field !== "unavailable";
  });
}

function validCallBreakdown(run: Record<string, unknown>): boolean {
  const keys = [
    "toolCalls",
    "bobbyToolCalls",
    "hostToolCalls",
    "discoveryToolCalls",
  ];
  if (!keys.every((key) => Number.isFinite(run[key]) && Number(run[key]) >= 0)) {
    return false;
  }
  return (
    Number(run.toolCalls) ===
      Number(run.bobbyToolCalls) + Number(run.hostToolCalls) &&
    Number(run.discoveryToolCalls) <= Number(run.hostToolCalls)
  );
}

function provenanceKey(value: Record<string, unknown>): string {
  return JSON.stringify(PROVENANCE_KEYS.map((key) => value[key]));
}

function probeDoctor() {
  const bobby = process.env.BOBBY_MCP_COMMAND ?? "bobby";
  const result = spawnSync(bobby, ["doctor"], {
    encoding: "utf8",
    timeout: 30_000,
  });
  return parseDoctorOutput(`${result.stdout}\n${result.stderr}`);
}

function probeInstall() {
  if (process.env.GAUNTLET_PROBE_INSTALL !== "1") return null;
  const bobby = process.env.BOBBY_MCP_COMMAND ?? "bobby";
  const home = mkdtempSync(path.join(tmpdir(), "bobby-install-probe-"));
  const started = Date.now();
  const result = spawnSync(bobby, ["install", "--host", "claude", "--skill", "--yes"], {
    encoding: "utf8",
    timeout: 120_000,
    env: { ...process.env, HOME: home },
  });
  const detail = String(result.stderr || result.stdout || `exit ${result.status}`).slice(
    0,
    400,
  );
  return {
    ok: result.status === 0,
    wallMs: Date.now() - started,
    detail: result.status === 0 ? "ok" : detail,
  };
}

function loadOpusReference() {
  const directory =
    process.env.OPUS_RESULTS_DIR ??
    path.resolve(harnessDir, "../results/post-merge-5e5d0d9-opus-4-8");
  const file = path.join(directory, "runs.jsonl");
  if (!existsSync(file)) return null;
  const opusRuns = readFileSync(file, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
  return {
    head: "5e5d0d9",
    label: "Opus Bobby freeze — labeled only, not ranked",
    bobby: statsForTool("bobby", opusRuns),
  };
}

if (process.argv[2] === "phase") {
  const doctor = probeDoctor();
  const scorecard = buildPhaseScorecard(runs, {
    doctor,
    install: probeInstall() ?? undefined,
    opusReference: loadOpusReference(),
  });
  const out = path.join(resultsDir, "phase-scorecard.json");
  writeFileSync(out, JSON.stringify(scorecard, null, 2) + "\n");
  console.log(JSON.stringify(scorecard, null, 2));
  console.error(`wrote ${out}`);
  process.exit(0);
}

if (process.argv[2] === "check") {
  // Regression gate: one complete bobby invocation against the committed
  // baseline. Pass must hold; wall time <= 2x baseline; errors <= +3;
  // token/call budgets (when the baseline carries them) must hold. A batch
  // may carry several runs per task (`--runs N`): every run must pass and
  // the aggregates (mean wall/tokens, worst errors) face the thresholds, so
  // a lucky single run cannot green the gate and one flaky run cannot hide
  // behind two good ones.
  //
  // Thresholds split by engine and provider mode: baseline.dimensions
  // keys on "<engine>/<providerMode>" from the batch's provenance (e.g.
  // "firefox/off"). A batch whose dimension has no entry falls back to the
  // top-level tasks/budget, so the default matrix cell needs no duplicate.
  const baseline = JSON.parse(
    readFileSync(
      process.env.GAUNTLET_BASELINE_PATH ?? path.join(harnessDir, "baseline.json"),
      "utf8",
    ),
  );
  const validateBudget = (budget: unknown, label: string): void => {
    if (budget === null || budget === undefined) return;
    const keys = [
      "perTaskCacheReadTokens",
      "perTaskCacheCreationTokens",
      "perTaskToolCalls",
    ];
    for (const key of keys) {
      const value = (budget as Record<string, unknown>)[key];
      if (!Number.isFinite(value) || Number(value) <= 0) {
        console.error(`baseline ${label}.${key} must be a positive number`);
        process.exit(1);
      }
    }
  };
  validateBudget(baseline.budget, "budget");
  const dimensions = (baseline.dimensions ?? {}) as Record<string, any>;
  for (const [key, overlay] of Object.entries(dimensions)) {
    if (!overlay || typeof overlay !== "object" || !overlay.tasks) {
      console.error(`baseline dimensions.${key} must carry tasks`);
      process.exit(1);
    }
    validateBudget(overlay.budget, `dimensions.${key}.budget`);
  }
  const bobbyRuns = runs.filter((run) => run.tool === "bobby");
  const latestBatchId = bobbyRuns.at(-1)?.batchId;
  if (typeof latestBatchId !== "string" || latestBatchId.length === 0) {
    console.error("latest bobby result has no batchId — rerun src/run.ts");
    process.exit(1);
  }
  const latest = new Map<string, any[]>();
  const latestRuns = bobbyRuns.filter((run) => run.batchId === latestBatchId);
  for (const run of latestRuns) {
    const list = latest.get(run.task) ?? [];
    list.push(run);
    latest.set(run.task, list);
  }
  const batchProvenance = latestRuns.find((run) =>
    validProvenance(run.provenance),
  )?.provenance;
  const batchProvenanceKey = validProvenance(batchProvenance)
    ? provenanceKey(batchProvenance)
    : null;
  const dimension = validProvenance(batchProvenance)
    ? `${batchProvenance.engine}/${batchProvenance.providerMode}`
    : null;
  const overlay = dimension !== null ? dimensions[dimension] : undefined;
  if (overlay) {
    console.log(`baseline dimension: ${dimension}`);
  }
  const selectedTasks = (overlay?.tasks ?? baseline.tasks) as Record<string, any>;
  const budget = (overlay?.budget ?? baseline.budget ?? null) as Record<
    string,
    number
  > | null;
  let failures = 0;
  for (const [task, base] of Object.entries(selectedTasks)) {
    const taskRuns = latest.get(task);
    if (!taskRuns || taskRuns.length === 0) {
      console.log(`MISS ${task}: no bobby run recorded`);
      failures += 1;
      continue;
    }
    for (const run of taskRuns) {
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
      if (!validCallBreakdown(run)) {
        console.log(`INVALID ${task}: missing call breakdown`);
        failures += 1;
        continue;
      }
      if (!validProvenance(run.provenance)) {
        console.log(`INVALID ${task}: missing benchmark provenance`);
        failures += 1;
        continue;
      }
      if (typeof run.model !== "string" || run.model.length === 0) {
        console.log(`INVALID ${task}: actual model is missing`);
        failures += 1;
        continue;
      }
      if (run.model !== run.provenance.requestedModel) {
        console.log(
          `INVALID ${task}: actual model ${run.model} differs from requested model ${run.provenance.requestedModel}`,
        );
        failures += 1;
        continue;
      }
      if (
        batchProvenanceKey === null ||
        provenanceKey(run.provenance) !== batchProvenanceKey
      ) {
        console.log(`INVALID ${task}: provenance differs within batch`);
        failures += 1;
        continue;
      }
    }
    if (taskRuns.some((run) => !run.pass)) {
      const failed = taskRuns.filter((run) => !run.pass).length;
      console.log(
        `FAIL ${task}: baseline passes, ${failed}/${taskRuns.length} run(s) did not`,
      );
      failures += 1;
      continue;
    }
    const wallMean =
      taskRuns.reduce((sum, run) => sum + run.wallMs / 1000, 0) /
      taskRuns.length;
    const errorsWorst = Math.max(...taskRuns.map((run) => run.toolErrors));
    if (wallMean > base.wallSeconds * 2) {
      console.log(
        `SLOW ${task}: mean ${wallMean.toFixed(0)}s over ${taskRuns.length} run(s) vs baseline ${base.wallSeconds}s (>2x)`,
      );
      failures += 1;
    } else if (errorsWorst > base.toolErrors + 3) {
      console.log(
        `ERRORS ${task}: worst ${errorsWorst} vs baseline ${base.toolErrors} (+3 slack)`,
      );
      failures += 1;
    } else if (budget && !Number.isFinite(taskRuns[0].cacheReadTokens)) {
      console.log(
        `INVALID ${task}: budget gate set but cacheReadTokens missing — rerun with the current run.ts`,
      );
      failures += 1;
    } else if (budget) {
      const cacheReadMean =
        taskRuns.reduce((sum, run) => sum + Number(run.cacheReadTokens), 0) /
        taskRuns.length;
      const cacheCreationMean =
        taskRuns.reduce(
          (sum, run) => sum + Number(run.cacheCreationTokens ?? 0),
          0,
        ) / taskRuns.length;
      const callsMean =
        taskRuns.reduce((sum, run) => sum + Number(run.toolCalls), 0) /
        taskRuns.length;
      const breaches: string[] = [];
      if (cacheReadMean > budget.perTaskCacheReadTokens) {
        breaches.push(
          `cacheRead ${cacheReadMean.toFixed(0)} > ${budget.perTaskCacheReadTokens}`,
        );
      }
      if (cacheCreationMean > budget.perTaskCacheCreationTokens) {
        breaches.push(
          `cacheCreate ${cacheCreationMean.toFixed(0)} > ${budget.perTaskCacheCreationTokens}`,
        );
      }
      if (callsMean > budget.perTaskToolCalls) {
        breaches.push(`calls ${callsMean.toFixed(1)} > ${budget.perTaskToolCalls}`);
      }
      if (breaches.length > 0) {
        console.log(
          `BUDGET ${task}: ${breaches.join("; ")} (mean over ${taskRuns.length} run(s))`,
        );
        failures += 1;
      } else {
        console.log(
          `OK   ${task}: ${wallMean.toFixed(0)}s errors=${errorsWorst} cacheR=${cacheReadMean.toFixed(0)} cacheC=${cacheCreationMean.toFixed(0)} calls=${callsMean.toFixed(1)} (n=${taskRuns.length})`,
        );
      }
    } else {
      console.log(
        `OK   ${task}: ${wallMean.toFixed(0)}s errors=${errorsWorst} (n=${taskRuns.length})`,
      );
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

const completeMean = (list: any[], key: string): string => {
  const values = list
    .map((item) => Number(item[key]))
    .filter((value) => Number.isFinite(value) && value >= 0);
  return values.length === list.length ? mean(values).toFixed(1) : "-";
};

const EASE_KEYS = ["navigate", "click", "fill", "extract"] as const;

console.log(
  [
    "tool",
    "runs",
    "pass%",
    "time s",
    "calls",
    "bobby",
    "host",
    "discover",
    "err%",
    "in tok",
    "cache read tok",
    "cache create tok",
    "out tok",
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
      completeMean(list, "bobbyToolCalls"),
      completeMean(list, "hostToolCalls"),
      completeMean(list, "discoveryToolCalls"),
      (
        (list.reduce((a, r) => a + r.toolErrors, 0) /
          Math.max(
            1,
            list.reduce((a, r) => a + r.toolCalls, 0),
          )) *
        100
      ).toFixed(0),
      mean(list.map((r) => r.inputTokens ?? 0)).toFixed(0),
      mean(list.map((r) => r.cacheReadTokens ?? 0)).toFixed(0),
      mean(list.map((r) => r.cacheCreationTokens ?? 0)).toFixed(0),
      mean(list.map((r) => r.outputTokens ?? 0)).toFixed(0),
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
