import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { test } from "node:test";

test("MCP over HTTP executes the shared real-Chrome canonical matrix on a production broker", { timeout: 180_000 }, async () => {
  const benchmarking = process.env.CONFORMANCE_PERFORMANCE_SAMPLES !== undefined;
  const args = ["test", "-q", "-p", "interface-conformance", "--test", "mcp_live", "mcp_http_performance_arm", "--", "--ignored"];
  if (benchmarking) args.push("--nocapture");
  const child = spawn(process.env.CARGO ?? "cargo", args, {
    cwd: new URL("../../../..", import.meta.url), stdio: [benchmarking ? "pipe" : "ignore", "pipe", "pipe"], env: process.env,
  });
  let output = "";
  child.stdout!.setEncoding("utf8"); child.stderr!.setEncoding("utf8");
  child.stdout!.on("data", chunk => { output = (output + String(chunk)).slice(-16_384); if (benchmarking) process.stdout.write(String(chunk)); });
  child.stderr!.on("data", chunk => { output = (output + String(chunk)).slice(-16_384); });
  const [code] = await once(child, "exit") as [number | null, NodeJS.Signals | null];
  assert.equal(code, 0, output);
  assert.match(output, /1 passed/);
});
