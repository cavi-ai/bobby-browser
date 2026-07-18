import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { chromium } from "playwright-core";
import { playwrightDriver } from "../src/playwright.js";
import { auditProtocolInventory, equalityProof, runCanonicalScenario } from "../src/scenario.js";
import { instrumentAsyncMethods, requestedPerformanceSamples, runPersistentPerformance } from "./performance-support.js";

test("Playwright completes the canonical conformance workflow", { timeout: 120_000 }, async (t) => {
  const child = spawn(process.env.CARGO ?? "cargo", ["run", "-q", "-p", "cdp-gateway", "--example", "conformance_gateway"], {
    cwd: new URL("../../../..", import.meta.url),
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", chunk => { stderr = (stderr + String(chunk)).slice(-16_384); });
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGTERM");
      await once(child, "exit");
    }
    const code = child.exitCode;
    const signal = child.signalCode;
    if (code !== null && code !== 0) throw new Error(`gateway exited ${code}: ${stderr}`);
    if (signal !== null && signal !== "SIGTERM") throw new Error(`gateway exited by ${signal}: ${stderr}`);
  });
  const line = await readLine(child.stdout).catch(error => {
    throw new Error(`gateway closed before readiness: ${stderr}`, { cause: error });
  });
  const boot = JSON.parse(line) as { endpoint: string; token: string; deniedToken: string; site: string };
  const browser = await chromium.connectOverCDP(boot.endpoint, {
    headers: { Authorization: `Bearer ${boot.token}` },
  });
  let disconnected = false;
  t.after(async () => {
    if (!disconnected) await browser.close({ reason: "interface conformance complete" });
  });
  const context = browser.contexts()[0];
  if (!context) throw new Error("Playwright did not expose the default browser context");
  const page = context.pages()[0] ?? await context.newPage();
  const dir = await mkdtemp(join(tmpdir(), "interface-conformance-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const fixture = join(dir, "resume.txt");
  await writeFile(fixture, "bounded fixture\n");
  const driver = playwrightDriver(page, boot.endpoint, boot.token, boot.deniedToken);
  const manifest = JSON.parse(await readFile(new URL("../../../../docs/cdp-support.json", import.meta.url), "utf8"));
  const execute = async (measured = driver) => {
    const proof = await runCanonicalScenario(measured, boot.site, fixture);
    assert.equal(proof.outcomeStatus, "completed");
    assert.equal(proof.authorization.denied.status, 403);
    assert.deepEqual(proof.evidence.map(item => item.kind), ["navigation", "upload", "screenshot", "download"]);
    return proof;
  };
  const samples = requestedPerformanceSamples();
  if (samples !== undefined) {
    await runPersistentPerformance({
      adapter: "playwright",
      samples,
      run: async timer => { await execute(instrumentAsyncMethods(driver, timer)); },
      disconnect: async () => {
        await browser.close({ reason: "performance client disconnected" });
        disconnected = true;
      },
    });
  } else {
    const proof = await execute();
    auditProtocolInventory(await driver.protocolInventory(), "playwright", manifest);
    if (process.env.CONFORMANCE_PROOF_DIR) await writeFile(join(process.env.CONFORMANCE_PROOF_DIR, "playwright.json"), JSON.stringify(equalityProof(proof)));
  }
});

async function readLine(stream: NodeJS.ReadableStream): Promise<string> {
  let buffered = "";
  stream.setEncoding("utf8");
  for await (const chunk of stream) {
    buffered += chunk;
    const newline = buffered.indexOf("\n");
    if (newline >= 0) return buffered.slice(0, newline);
  }
  throw new Error("gateway stdout closed before readiness");
}
