import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { test } from "node:test";
import { BrowserRuntimeClient, RuntimeClientError } from "@bobby-browser/sdk";
import { expectedCanonicalInterfaceProof, NEGATIVE_CAPABILITY_MATRIX, runCanonicalInterfaceScenario } from "../src/scenario.js";
import { typescriptSdkDriver } from "../src/typescript-sdk.js";

test("TypeScript SDK consumes the canonical scenario without boundary replay", async () => {
  const proof = await runCanonicalInterfaceScenario(typescriptSdkDriver(async request => {
    assert.equal(request.implicitBoundaryReplay, false);
    assert.equal(request.steps.length, 10);
    return expectedCanonicalInterfaceProof;
  }));
  assert.deepEqual(proof, expectedCanonicalInterfaceProof);
});

test("TypeScript SDK negative capability matrix is complete", () => {
  assert.equal(new Set(NEGATIVE_CAPABILITY_MATRIX.map(([step]) => step)).size, 10);
  for (const [, capability] of NEGATIVE_CAPABILITY_MATRIX) assert.match(capability, /^[a-z]+:[a-z]+$/);
});

test("TypeScript SDK observes live authenticated broker allow and deny decisions", { timeout: 60_000 }, async (t) => {
  const child = spawn(process.env.CARGO ?? "cargo", ["run", "-q", "-p", "broker", "--example", "conformance_broker"], {
    cwd: new URL("../../../..", import.meta.url), stdio: ["ignore", "pipe", "pipe"],
  });
  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", chunk => { stderr = (stderr + String(chunk)).slice(-8192); });
  t.after(async () => { if (child.exitCode === null) { child.kill("SIGTERM"); await once(child, "exit"); } });
  const boot = JSON.parse(await readLine(child.stdout)) as { endpoint: string; token: string; deniedToken: string };
  const allowed = new BrowserRuntimeClient({ baseUrl: boot.endpoint, bearerToken: boot.token });
  const denied = new BrowserRuntimeClient({ baseUrl: boot.endpoint, bearerToken: boot.deniedToken });
  const info = await allowed.runtimeInfo();
  assert.equal(typeof info.version, "string", stderr);
  const session = await allowed.createSession({ profile: "typescript-conformance", proxy: null });
  assert.match(session.id, /^[0-9a-f-]{36}$/);
  await assert.rejects(
    denied.createSession({ profile: "denied", proxy: null }),
    (error: unknown) => error instanceof RuntimeClientError && error.kind === "http" && error.status === 403,
  );
});

async function readLine(stream: NodeJS.ReadableStream): Promise<string> {
  let buffered = ""; stream.setEncoding("utf8");
  for await (const chunk of stream) { buffered += chunk; const newline = buffered.indexOf("\n"); if (newline >= 0) return buffered.slice(0, newline); }
  throw new Error("broker fixture closed before readiness");
}
