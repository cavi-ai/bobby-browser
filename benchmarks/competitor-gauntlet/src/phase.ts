const EASE_KEYS = ["navigate", "click", "fill", "extract"] as const;
const RANK_TOOLS = ["bobby", "playwright-mcp", "chrome-devtools-mcp", "raw-playwright", "agent-browser"] as const;

export type PhaseRun = {
  tool: string;
  task?: string;
  pass?: boolean;
  timedOut?: boolean;
  skipped?: boolean;
  skipReason?: string;
  wallMs?: number;
  toolCalls?: number;
  toolErrors?: number;
  inputTokens?: number;
  outputTokens?: number;
  selfReport?: Record<string, unknown> | null;
  batchId?: string;
  provenance?: Record<string, unknown>;
  model?: string | null;
};

export type ToolStats = {
  tool: string;
  runs: number;
  passRate: number;
  meanWallSecondsPassing: number | null;
  meanTokens: number | null;
  meanEase: number | null;
  errorRate: number | null;
  skipped?: boolean;
  skipReason?: string;
};

function mean(values: number[]): number | null {
  if (values.length === 0) return null;
  return values.reduce((sum, value) => sum + value, 0) / values.length;
}

function easeScore(report: Record<string, unknown> | null | undefined): number | null {
  if (!report) return null;
  const scored = EASE_KEYS.map((key) => Number(report[key])).filter(
    (value) => Number.isFinite(value) && value > 0,
  );
  return mean(scored);
}

export function latestBatch(runs: PhaseRun[], tool: string): PhaseRun[] {
  const matching = runs.filter((run) => run.tool === tool && !run.skipped);
  const batchId = matching.at(-1)?.batchId;
  if (!batchId) return matching;
  return matching.filter((run) => run.batchId === batchId);
}

export function statsForTool(tool: string, runs: PhaseRun[]): ToolStats {
  const skipped = runs.find((run) => run.tool === tool && run.skipped);
  if (skipped && latestBatch(runs, tool).length === 0) {
    return {
      tool,
      runs: 0,
      passRate: 0,
      meanWallSecondsPassing: null,
      meanTokens: null,
      meanEase: null,
      errorRate: null,
      skipped: true,
      skipReason: skipped.skipReason,
    };
  }
  const batch = latestBatch(runs, tool);
  const passes = batch.filter((run) => run.pass && !run.timedOut);
  const tokens = batch
    .map((run) => Number(run.inputTokens ?? 0) + Number(run.outputTokens ?? 0))
    .filter((value) => Number.isFinite(value));
  const walls = passes
    .map((run) => Number(run.wallMs) / 1000)
    .filter((value) => Number.isFinite(value) && value >= 0);
  const eases = batch
    .map((run) => easeScore(run.selfReport ?? null))
    .filter((value): value is number => value !== null);
  const calls = batch.reduce((sum, run) => sum + Number(run.toolCalls ?? 0), 0);
  const errors = batch.reduce((sum, run) => sum + Number(run.toolErrors ?? 0), 0);
  return {
    tool,
    runs: batch.length,
    passRate: batch.length ? passes.length / batch.length : 0,
    meanWallSecondsPassing: mean(walls),
    meanTokens: mean(tokens),
    meanEase: mean(eases),
    errorRate: calls > 0 ? errors / calls : null,
  };
}

export function rank(
  stats: ToolStats[],
  key: "passRate" | "meanWallSecondsPassing" | "meanTokens" | "meanEase",
  higherIsBetter: boolean,
): Array<{ tool: string; rank: number; value: number | null; deltaVsBest: number | null }> {
  const comparable = stats.filter(
    (row) => !row.skipped && row.runs > 0 && row[key] !== null,
  );
  const sorted = [...comparable].sort((left, right) => {
    const leftValue = left[key] as number;
    const rightValue = right[key] as number;
    return higherIsBetter ? rightValue - leftValue : leftValue - rightValue;
  });
  const best = sorted[0]?.[key] as number | undefined;
  return stats.map((row) => {
    const value = row[key];
    const place = sorted.findIndex((entry) => entry.tool === row.tool);
    return {
      tool: row.tool,
      rank: place === -1 ? 0 : place + 1,
      value,
      deltaVsBest:
        value === null || best === undefined ? null : value - best,
    };
  });
}

export function buildPhaseScorecard(
  runs: PhaseRun[],
  extras: {
    doctor?: { failures: number; warnings: number; checks: number };
    install?: { ok: boolean; wallMs: number; detail: string };
    opusReference?: unknown;
  } = {},
) {
  const tools = [...RANK_TOOLS, "bobby-vision"];
  const stats = tools.map((tool) => statsForTool(tool, runs));
  const ranked = stats.filter((row) => RANK_TOOLS.includes(row.tool as (typeof RANK_TOOLS)[number]));
  const provenance = latestBatch(runs, "bobby")[0]?.provenance
    ?? runs.find((run) => run.provenance)?.provenance
    ?? null;
  const grokMeasured = RANK_TOOLS.every(
    (tool) => (stats.find((row) => row.tool === tool)?.runs ?? 0) > 0,
  );
  return {
    driver: provenance?.driver ?? null,
    requestedModel: provenance?.requestedModel ?? null,
    provenance,
    tools: Object.fromEntries(stats.map((row) => [row.tool, row])),
    ranks: {
      accuracy: rank(ranked, "passRate", true),
      performance: rank(ranked, "meanWallSecondsPassing", false),
      tokens: rank(ranked, "meanTokens", false),
      ease: rank(ranked, "meanEase", true),
    },
    operator: {
      doctor: extras.doctor ?? null,
      install: extras.install ?? null,
    },
    opusReference: extras.opusReference ?? null,
    grokBatch: grokMeasured
      ? { status: "measured" as const }
      : { status: "not-measured" as const },
    note: "Ranks compare Grok-driven bobby (vision off) to competitors. bobby-vision and Opus are labeled, not ranked.",
  };
}

export function parseDoctorOutput(text: string): {
  failures: number;
  warnings: number;
  checks: number;
} {
  const lines = text.split("\n").filter(Boolean);
  const tagged = lines.filter((line) => /\[(ok|fail|warn)\]/i.test(line));
  return {
    checks: tagged.length,
    failures: tagged.filter((line) => /\[fail\]/i.test(line)).length,
    warnings: tagged.filter((line) => /\[warn\]/i.test(line)).length,
  };
}
