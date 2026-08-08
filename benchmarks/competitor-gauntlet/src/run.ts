import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import {
  appendFileSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const harnessDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const repoRoot = path.resolve(harnessDir, "../..");
const resultsDir =
  process.env.GAUNTLET_RESULTS_DIR ??
  path.join(repoRoot, "benchmarks/results");
const fixturePath = path.join(
  repoRoot,
  "crates/runtime-tests/tests/fixtures/approved-upload.txt",
);

const tasks = JSON.parse(readFileSync(path.join(harnessDir, "tasks.json"), "utf8"));
const runners = JSON.parse(
  readFileSync(path.join(harnessDir, "runners.json"), "utf8"),
);

function arg(name: string, fallback?: string): string | undefined {
  const index = process.argv.indexOf(`--${name}`);
  return index >= 0 ? process.argv[index + 1] : fallback;
}

const SELF_REPORT = `
When the task is complete (or you are giving up), your final message must end with a self-report as a single JSON code block, no other text after it:
\`\`\`json
{"selfReport":{"navigate":1,"click":1,"fill":1,"extract":1,"blockers":"...","bottlenecks":"..."}}
\`\`\`
Score each of navigate/click/fill/extract 1-5 for how easy the tooling made that action (5 = effortless, 1 = could not do it; score 0 for actions the task never needed). In "blockers" list anything that stopped or nearly stopped you; in "bottlenecks" what slowed you down. Be honest and specific — this report is the point of the exercise.`;

interface TaskAssert {
  path: string;
  eq?: unknown;
  eqFixtureSha256?: boolean;
}

function getPath(value: unknown, dotted: string): unknown {
  let current = value;
  for (const key of dotted.split(".")) {
    if (current === null || typeof current !== "object") return undefined;
    current = (current as Record<string, unknown>)[key];
  }
  return current;
}

async function startServer(seed: string): Promise<{
  url: string;
  base: string;
  stop: () => void;
}> {
  const proc = spawn(
    "cargo",
    ["run", "-q", "-p", "gauntlet-server", "--", "--seed", seed],
    { cwd: repoRoot, stdio: ["ignore", "pipe", "inherit"] },
  );
  const url = await new Promise<string>((resolve, reject) => {
    let buffer = "";
    const timer = setTimeout(() => reject(new Error("server start timeout")), 120_000);
    proc.stdout.on("data", (chunk) => {
      buffer += chunk;
      const line = buffer.split("\n")[0].trim();
      if (line.startsWith("http")) {
        clearTimeout(timer);
        resolve(line);
      }
    });
    proc.on("exit", (code) => reject(new Error(`server exited ${code}`)));
  });
  const parsed = new URL(url);
  return {
    url,
    base: `${parsed.protocol}//${parsed.host}`,
    stop: () => proc.kill(),
  };
}

async function runClaude(
  prompt: string,
  workDir: string,
  timeboxMs: number,
): Promise<{ events: any[]; timedOut: boolean }> {
  const args = [
    "-p",
    prompt,
    "--output-format",
    "stream-json",
    "--verbose",
    "--dangerously-skip-permissions",
  ];
  if (exists(path.join(workDir, ".mcp.json"))) {
    args.push("--mcp-config", path.join(workDir, ".mcp.json"));
  }
  const model = arg("model");
  if (model) args.push("--model", model);
  const proc = spawn("claude", args, {
    cwd: workDir,
    stdio: ["ignore", "pipe", "inherit"],
  });
  const events: any[] = [];
  let buffer = "";
  let timedOut = false;
  const killer = setTimeout(() => {
    timedOut = true;
    proc.kill("SIGKILL");
  }, timeboxMs);
  proc.stdout.on("data", (chunk) => {
    buffer += chunk;
    let newline;
    while ((newline = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      if (!line) continue;
      try {
        events.push(JSON.parse(line));
      } catch {
        // non-JSON line from the CLI; ignore
      }
    }
  });
  await new Promise((resolve) => proc.on("exit", resolve));
  clearTimeout(killer);
  return { events, timedOut };
}

function exists(p: string): boolean {
  try {
    readFileSync(p);
    return true;
  } catch {
    return false;
  }
}

function summarize(events: any[]) {
  let toolCalls = 0;
  let toolErrors = 0;
  let inputTokens = 0;
  let outputTokens = 0;
  let resultText = "";
  let model: string | undefined;
  for (const event of events) {
    if (event.type === "assistant") {
      model ??= event.message?.model;
      for (const block of event.message?.content ?? []) {
        if (block.type === "tool_use") toolCalls += 1;
      }
    } else if (event.type === "user") {
      for (const block of event.message?.content ?? []) {
        if (block.type === "tool_result" && block.is_error) toolErrors += 1;
      }
    } else if (event.type === "result") {
      resultText = event.result ?? "";
      inputTokens = event.usage?.input_tokens ?? 0;
      outputTokens = event.usage?.output_tokens ?? 0;
      model ??= event.model;
    }
  }
  return { toolCalls, toolErrors, inputTokens, outputTokens, resultText, model };
}

function parseSelfReport(text: string): unknown {
  const match = text.match(/```json\s*(\{[\s\S]*"selfReport"[\s\S]*\})\s*```/);
  if (!match) return null;
  try {
    return JSON.parse(match[1]).selfReport;
  } catch {
    return null;
  }
}

async function verify(
  base: string,
  task: (typeof tasks)[number],
  downloadsDir: string,
): Promise<{ pass: boolean; failures: string[] }> {
  const snapshot = await (await fetch(`${base}/__gauntlet/snapshot`)).json();
  const failures: string[] = [];
  const fixtureSha = createHash("sha256")
    .update(readFileSync(fixturePath))
    .digest("hex");
  for (const assertion of task.assert as TaskAssert[]) {
    const actual = getPath(snapshot, assertion.path);
    const expected = assertion.eqFixtureSha256 ? fixtureSha : assertion.eq;
    if (actual !== expected) {
      failures.push(`${assertion.path}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
    }
  }
  if (task.download) {
    const file = path.join(downloadsDir, task.download.file);
    try {
      const content = readFileSync(file, "utf8");
      if (content !== task.download.content) {
        failures.push(`download ${task.download.file}: content mismatch`);
      }
    } catch {
      failures.push(`download ${task.download.file}: missing`);
    }
  }
  return { pass: failures.length === 0, failures };
}

async function main() {
  const toolName = arg("tool");
  const taskId = arg("task");
  const runs = Number(arg("runs", "1"));
  const timeboxMs = Number(arg("timebox-seconds", "480")) * 1000;
  const batchId = randomUUID();
  if (!toolName) {
    console.error(
      `--tool required. Benchmark bobby with --tool bobby. The full competitor gamut runs ONLY when explicitly called: --tool all. One of: ${Object.keys(runners).join(", ")}, all`,
    );
    process.exit(2);
  }
  const toolNames =
    toolName === "all"
      ? Object.keys(runners)
      : toolName.split(",").filter((name) => runners[name]);
  if (toolNames.length === 0) {
    console.error(`unknown --tool ${toolName}`);
    process.exit(2);
  }
  const selected = taskId ? tasks.filter((t: any) => t.id === taskId) : tasks;
  if (selected.length === 0) {
    console.error(`unknown --task ${taskId}`);
    process.exit(2);
  }
  mkdirSync(resultsDir, { recursive: true });
  mkdirSync(path.join(resultsDir, "transcripts"), { recursive: true });

  for (const tool of toolNames) {
    const runner = runners[tool];

    for (const task of selected) {
      for (let run = 1; run <= runs; run += 1) {
        const seed = `cg-${tool}-${task.id}-${run}-${Date.now()}`;
        const server = await startServer(seed);
        const parsed = new URL(server.url);
        const entryUrl = `${server.base}${task.entry}${parsed.search}`;
        const workDir = await mkdtemp(path.join(tmpdir(), `cg-${tool}-`));
        const downloadsDir = path.join(workDir, "downloads");
        mkdirSync(downloadsDir, { recursive: true });
        // Relative upload_roots in config resolve against the gateway cwd
        // (the Claude workDir). Ensure the default root exists and stage the
        // fixture inside it so upload_files does not policyDeny.
        const uploadRoot = path.join(workDir, "data", "uploads");
        mkdirSync(uploadRoot, { recursive: true });
        const stagedFixture = path.join(uploadRoot, "approved-upload.txt");
        writeFileSync(stagedFixture, readFileSync(fixturePath));

        const mcpConfig = { mcpServers: structuredClone(runner.mcpServers) };
        // Prefer the repo's own release build for the bobby runner — the
        // benchmark should measure this checkout, not a stale installed binary.
        const repoBobby = path.join(repoRoot, "target/release/bobby");
        const bobbyCommand =
          process.env.BOBBY_MCP_COMMAND ??
          (exists(repoBobby) ? repoBobby : "bobby");
        // Per-run config: allow loopback (gauntlet-server is 127.0.0.1) and
        // keep upload_roots relative to workDir. HttpConfig is partial-override
        // safe (#[serde(default)]); still write a complete enough file that
        // BOBBY_BROWSER_CONFIG never fails parse and drops MCP.
        const gauntletConfigPath = path.join(workDir, "bobby-gauntlet.toml");
        if (tool === "bobby") {
          writeFileSync(
            gauntletConfigPath,
            [
              "[browser]",
              'upload_roots = ["./data/uploads"]',
              'downloads_dir = "./downloads"',
              'artifacts_dir = "./artifacts"',
              'profiles_dir = "./profiles"',
              "headless = true",
              "",
              "[http]",
              "allow_loopback = true",
              "allow_private_network = false",
              "max_redirects = 5",
              "max_header_bytes = 65536",
              "max_body_bytes = 8388608",
              "max_download_bytes = 67108864",
              "request_timeout_ms = 30000",
              "max_concurrent_requests = 8",
              "",
              "[mcp]",
              'startup_toolset = "full"',
              "",
            ].join("\n"),
          );
        }
        for (const serverConfig of Object.values(mcpConfig.mcpServers) as any[]) {
          if (typeof serverConfig.command === "string") {
            serverConfig.command = serverConfig.command.replace(
              "${BOBBY_MCP_COMMAND}",
              bobbyCommand,
            );
          }
          if (serverConfig.env) {
            for (const [key, value] of Object.entries(serverConfig.env)) {
              if (typeof value === "string") {
                serverConfig.env[key] = value
                  .replace("${BOBBY_MCP_COMMAND}", bobbyCommand)
                  .replace("${BOBBY_GAUNTLET_CONFIG}", gauntletConfigPath);
              }
            }
          }
        }
        writeFileSync(
          path.join(workDir, ".mcp.json"),
          JSON.stringify(mcpConfig, null, 2),
        );

        const prompt =
          task.prompt
            .replace("{{url}}", entryUrl)
            .replace(
              "{{fixture}}",
              tool === "bobby" ? stagedFixture : fixturePath,
            )
            .replace("{{downloads}}", downloadsDir) +
          (runner.promptSuffix
            ? "\n\n" +
              runner.promptSuffix.replaceAll("{{harnessDir}}", harnessDir)
            : "") +
          "\n" +
          SELF_REPORT;

        const started = Date.now();
        const { events, timedOut } = await runClaude(prompt, workDir, timeboxMs);
        const wallMs = Date.now() - started;
        const summary = summarize(events);
        const outcome = await verify(server.base, task, downloadsDir);
        server.stop();

        const transcriptFile = path.join(
          resultsDir,
          "transcripts",
          `${seed}.json`,
        );
        writeFileSync(transcriptFile, JSON.stringify(events, null, 2));
        const record = {
          batchId,
          seed,
          tool,
          task: task.id,
          run,
          at: new Date().toISOString(),
          model: summary.model ?? null,
          pass: outcome.pass && !timedOut,
          timedOut,
          failures: outcome.failures,
          wallMs,
          toolCalls: summary.toolCalls,
          toolErrors: summary.toolErrors,
          inputTokens: summary.inputTokens,
          outputTokens: summary.outputTokens,
          selfReport: parseSelfReport(summary.resultText),
          transcript: path.relative(repoRoot, transcriptFile),
        };
        appendFileSync(
          path.join(resultsDir, "runs.jsonl"),
          JSON.stringify(record) + "\n",
        );
        console.log(
          `${record.pass ? "PASS" : "FAIL"} ${tool}/${task.id}#${run} ` +
            `${(wallMs / 1000).toFixed(1)}s calls=${summary.toolCalls} errors=${summary.toolErrors} ` +
            `${outcome.failures.join("; ")}`,
        );
      }
    }
  }
}

await main();
