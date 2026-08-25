import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { fileURLToPath } from "node:url";

const sourceDir = path.dirname(fileURLToPath(import.meta.url));
const runPath = path.join(sourceDir, "run.ts");
const harnessDir = path.dirname(sourceDir);
const repoRoot = path.resolve(harnessDir, "../..");

test("transcript summary separates Bobby calls from host overhead", () => {
  const directory = mkdtempSync(path.join(tmpdir(), "bobby-run-metrics-"));
  const transcript = path.join(directory, "transcript.json");
  writeFileSync(
    transcript,
    JSON.stringify([
      {
        type: "assistant",
        message: {
          model: "claude-opus-5",
          content: [
            { type: "tool_use", name: "ToolSearch" },
            { type: "tool_use", name: "TaskCreate" },
            { type: "tool_use", name: "Bash" },
            { type: "tool_use", name: "mcp__bobby__workflow_start" },
            { type: "tool_use", name: "mcp__bobby__click" },
            { type: "tool_use", name: "Read" },
          ],
        },
      },
      {
        type: "user",
        message: {
          content: [{ type: "tool_result", is_error: true }],
        },
      },
      {
        type: "result",
        result: "finished",
        usage: { input_tokens: 12, output_tokens: 34 },
      },
    ]),
  );

  const result = spawnSync(
    process.execPath,
    ["--import", "tsx", runPath, "--summarize-transcript", transcript],
    { cwd: path.dirname(sourceDir), encoding: "utf8" },
  );

  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  assert.deepEqual(JSON.parse(result.stdout), {
    toolCalls: 6,
    bobbyToolCalls: 2,
    hostToolCalls: 4,
    discoveryToolCalls: 1,
    taskBookkeepingCalls: 1,
    shellToolCalls: 2,
    toolErrors: 1,
    inputTokens: 12,
    outputTokens: 34,
    resultText: "finished",
    model: "claude-opus-5",
    toolCallBreakdown: {
      Bash: 1,
      Read: 1,
      TaskCreate: 1,
      ToolSearch: 1,
      mcp__bobby__click: 1,
      mcp__bobby__workflow_start: 1,
    },
  });
});

test("provenance fingerprints the exact benchmark inputs", () => {
  const directory = mkdtempSync(path.join(tmpdir(), "bobby-run-provenance-"));
  const bobby = path.join(directory, "bobby");
  writeFileSync(bobby, "current-bobby-binary");

  const result = spawnSync(
    process.execPath,
    [
      "--import",
      "tsx",
      runPath,
      "--print-provenance",
      "true",
      "--model",
      "claude-opus-5",
      "--timebox-seconds",
      "300",
    ],
    {
      cwd: harnessDir,
      encoding: "utf8",
      env: { ...process.env, BOBBY_MCP_COMMAND: bobby },
    },
  );

  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  const provenance = JSON.parse(result.stdout);
  const sha256 = (value: Buffer | string) =>
    createHash("sha256").update(value).digest("hex");
  assert.deepEqual(provenance, {
    repoHead: provenance.repoHead,
    repoDirty: true,
    sourceStateSha256: provenance.sourceStateSha256,
    claudeCliVersion: provenance.claudeCliVersion,
    nodeVersion: process.version,
    platform: `${process.platform}-${process.arch}`,
    taskSetSha256: sha256(readFileSync(path.join(harnessDir, "tasks.json"))),
    runnerSetSha256: sha256(readFileSync(path.join(harnessDir, "runners.json"))),
    bobbyBinarySha256: sha256("current-bobby-binary"),
    requestedModel: "claude-opus-5",
    timeboxSeconds: 300,
    startupToolset: "explore",
    claudeIsolation: "strict-mcp,project-settings,no-skills,no-chrome,no-persistence",
  });
  assert.match(provenance.repoHead, /^[0-9a-f]{40,64}$/);
  assert.match(provenance.sourceStateSha256, /^[0-9a-f]{64}$/);
  assert.equal(typeof provenance.claudeCliVersion, "string");
  assert(provenance.claudeCliVersion.length > 0);
});

test("Claude runner isolates the benchmark from user tools and state", () => {
  const workDir = mkdtempSync(path.join(tmpdir(), "bobby-run-isolation-"));
  writeFileSync(path.join(workDir, ".mcp.json"), '{"mcpServers":{}}');

  const result = spawnSync(
    process.execPath,
    [
      "--import",
      "tsx",
      runPath,
      "--print-claude-args",
      workDir,
      "--model",
      "claude-opus-5",
    ],
    { cwd: harnessDir, encoding: "utf8" },
  );

  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  const args = JSON.parse(result.stdout);
  assert.deepEqual(args.slice(0, 2), ["-p", "benchmark prompt"]);
  assert(args.includes("--strict-mcp-config"));
  assert(args.includes("--disable-slash-commands"));
  assert(args.includes("--no-chrome"));
  assert(args.includes("--no-session-persistence"));
  assert.deepEqual(
    args.slice(args.indexOf("--setting-sources"), args.indexOf("--setting-sources") + 2),
    ["--setting-sources", "project"],
  );
  assert.deepEqual(
    args.slice(args.indexOf("--mcp-config"), args.indexOf("--mcp-config") + 2),
    ["--mcp-config", path.join(workDir, ".mcp.json")],
  );
});

test("the canonical Bobby benchmark pins its agent model", () => {
  const result = spawnSync("make", ["-n", "agent-eval"], {
    cwd: repoRoot,
    encoding: "utf8",
  });

  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  assert.match(result.stdout, /--tool bobby --timebox-seconds 300 --model claude-opus-5/);
});

test("npm-backed competitor runners use exact package versions", () => {
  const runners = JSON.parse(
    readFileSync(path.join(harnessDir, "runners.json"), "utf8"),
  );
  const packageSpecs = Object.values(runners).flatMap((runner: any) =>
    Object.values(runner.mcpServers ?? {})
      .filter((server: any) => server.command === "npx")
      .map((server: any) => String(server.args?.at(-1) ?? "")),
  );

  assert(packageSpecs.length > 0, "fixture must include npm-backed competitors");
  for (const packageSpec of packageSpecs) {
    assert.doesNotMatch(packageSpec, /@latest$/);
    assert.match(packageSpec, /@\d+\.\d+\.\d+$/);
  }
});
