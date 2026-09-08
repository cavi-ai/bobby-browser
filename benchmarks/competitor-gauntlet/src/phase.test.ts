import assert from "node:assert/strict";
import test from "node:test";
import {
  buildPhaseScorecard,
  parseDoctorOutput,
  statsForTool,
} from "./phase.js";

const tasks = [
  "customer-update",
  "onboarding",
  "documents",
  "authorization",
  "report-recovery",
];

function run(
  tool: string,
  task: string,
  extra: Partial<{
    pass: boolean;
    wallMs: number;
    inputTokens: number;
    outputTokens: number;
    toolCalls: number;
    toolErrors: number;
    ease: number;
  }> = {},
) {
  return {
    batchId: "batch",
    tool,
    task,
    pass: extra.pass ?? true,
    timedOut: false,
    wallMs: extra.wallMs ?? 10_000,
    toolCalls: extra.toolCalls ?? 10,
    toolErrors: extra.toolErrors ?? 0,
    inputTokens: extra.inputTokens ?? 100,
    outputTokens: extra.outputTokens ?? 200,
    selfReport: {
      navigate: extra.ease ?? 5,
      click: extra.ease ?? 5,
      fill: extra.ease ?? 4,
      extract: extra.ease ?? 5,
    },
    provenance: { driver: "cursor", requestedModel: "grok-4.6" },
  };
}

test("phase ranks bobby against competitors on the same Grok batch", () => {
  const runs = [
    ...tasks.map((task) => run("bobby", task, { wallMs: 40_000, ease: 5 })),
    ...tasks.map((task) =>
      run("playwright-mcp", task, { wallMs: 20_000, inputTokens: 50, outputTokens: 50, ease: 3 }),
    ),
    ...tasks.map((task) => run("chrome-devtools-mcp", task, { pass: false, wallMs: 90_000 })),
    ...tasks.map((task) => run("raw-playwright", task, { wallMs: 60_000, ease: 2 })),
    {
      tool: "bobby-vision",
      skipped: true,
      skipReason: "mlx-unreachable",
    },
  ];

  const scorecard = buildPhaseScorecard(runs, {
    doctor: { failures: 0, warnings: 2, checks: 10 },
  });

  assert.equal(scorecard.tools.bobby.passRate, 1);
  assert.equal(scorecard.tools["playwright-mcp"].meanWallSecondsPassing, 20);
  assert.equal(scorecard.tools["bobby-vision"].skipped, true);
  const accuracy = scorecard.ranks.accuracy.find((row) => row.tool === "bobby");
  assert.equal(accuracy?.rank, 1);
  const perfBobby = scorecard.ranks.performance.find((row) => row.tool === "bobby");
  const perfPlaywright = scorecard.ranks.performance.find(
    (row) => row.tool === "playwright-mcp",
  );
  assert.equal(perfPlaywright?.rank, 1);
  assert.equal(perfBobby?.rank, 2);
  assert.equal(scorecard.grokBatch.status, "measured");
  assert.equal(scorecard.note.includes("not ranked"), true);
});

test("zero-run competitors are omitted from ranking", () => {
  const scorecard = buildPhaseScorecard(
    tasks.map((task) => run("bobby", task)),
  );
  const playwright = scorecard.ranks.accuracy.find(
    (row) => row.tool === "playwright-mcp",
  );
  assert.equal(playwright?.rank, 0);
  assert.equal(playwright?.value, 0);
  assert.equal(scorecard.grokBatch.status, "not-measured");
});

test("timeouts count as accuracy failures, not slow passes", () => {
  const stats = statsForTool("bobby", [
    { tool: "bobby", task: "onboarding", pass: true, timedOut: true, wallMs: 300_000, batchId: "a" },
    { tool: "bobby", task: "documents", pass: true, timedOut: false, wallMs: 10_000, batchId: "a" },
  ]);
  assert.equal(stats.passRate, 0.5);
  assert.equal(stats.meanWallSecondsPassing, 10);
});

test("parseDoctorOutput counts fail and warn tags", () => {
  const parsed = parseDoctorOutput("[ok] handshake: 12 tools\n[warn] vision: unset\n[fail] firefox: down\n");
  assert.deepEqual(parsed, { checks: 3, failures: 1, warnings: 1 });
});
