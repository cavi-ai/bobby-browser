import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { test } from "node:test";

test("MCP production Server executes the shared real-Chrome canonical matrix", { timeout: 120_000 }, async () => {
  const child = spawn(process.env.CARGO ?? "cargo", ["test", "-q", "-p", "interface-conformance", "--test", "mcp_live"], {
    cwd: new URL("../../../..", import.meta.url), stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, ...(process.env.CONFORMANCE_PROOF_DIR ? { CONFORMANCE_PROOF_PATH: `${process.env.CONFORMANCE_PROOF_DIR}/mcp.json` } : {}) },
  });
  let output = "";
  child.stdout.setEncoding("utf8"); child.stderr.setEncoding("utf8");
  child.stdout.on("data", chunk => { output = (output + String(chunk)).slice(-16_384); });
  child.stderr.on("data", chunk => { output = (output + String(chunk)).slice(-16_384); });
  const [code] = await once(child, "exit") as [number | null, NodeJS.Signals | null];
  assert.equal(code, 0, output);
  assert.match(output, /1 passed/);
});
