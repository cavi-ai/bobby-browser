import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import puppeteer from "puppeteer-core";
import { puppeteerDriver } from "../src/puppeteer.js";
import { auditProtocolInventory, equalityProof, runCanonicalScenario } from "../src/scenario.js";

test("Puppeteer completes the canonical conformance workflow", { timeout: 120_000 }, async (t) => {
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
    if (child.exitCode !== null && child.exitCode !== 0) throw new Error(`gateway exited ${child.exitCode}: ${stderr}`);
    if (child.signalCode !== null && child.signalCode !== "SIGTERM") throw new Error(`gateway exited by ${child.signalCode}: ${stderr}`);
  });
  const line = await readLine(child.stdout).catch(error => {
    throw new Error(`gateway closed before readiness: ${stderr}`, { cause: error });
  });
  const boot = JSON.parse(line) as { endpoint: string; token: string; deniedToken: string; site: string };
  const discovery = await fetch(`${boot.endpoint}/json/version`, {
    headers: { Authorization: `Bearer ${boot.token}` },
  });
  assert.equal(discovery.status, 200);
  const version = await discovery.json() as { webSocketDebuggerUrl: string };
  const browser = await puppeteer.connect({
    browserWSEndpoint: version.webSocketDebuggerUrl,
    headers: { Authorization: `Bearer ${boot.token}` },
    defaultViewport: null,
  });
  t.after(() => browser.disconnect());
  const pages = await browser.pages();
  const page = pages[0] ?? await browser.newPage();
  const dir = await mkdtemp(join(tmpdir(), "interface-conformance-puppeteer-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const fixture = join(dir, "resume.txt");
  await writeFile(fixture, "bounded fixture\n");
  const driver = puppeteerDriver(page, boot.endpoint, boot.token, boot.deniedToken);
  const proof = await runCanonicalScenario(driver, boot.site, fixture);
  const manifest = JSON.parse(await readFile(new URL("../../../../docs/cdp-support.json", import.meta.url), "utf8"));
  auditProtocolInventory(await driver.protocolInventory(), "puppeteer", manifest);
  assert.equal(proof.outcomeStatus, "completed");
  assert.equal(proof.authorization.denied.status, 403);
  assert.deepEqual(proof.evidence.map(item => item.kind), ["navigation", "upload", "screenshot", "download"]);
  if (process.env.CONFORMANCE_PROOF_DIR) await writeFile(join(process.env.CONFORMANCE_PROOF_DIR, "puppeteer.json"), JSON.stringify(equalityProof(proof)));
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
