import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { chromium } from "playwright-core";
import { playwrightDriver } from "../src/playwright.js";
import { runCanonicalScenario } from "../src/scenario.js";

test("Playwright completes the canonical conformance workflow", { timeout: 120_000 }, async (t) => {
  const child = spawn("cargo", ["run", "-q", "-p", "cdp-gateway", "--example", "conformance_gateway"], {
    cwd: new URL("../../../..", import.meta.url),
    stdio: ["ignore", "pipe", "pipe"],
  });
  t.after(() => child.kill("SIGTERM"));
  const line = await readLine(child.stdout);
  const boot = JSON.parse(line) as { endpoint: string; token: string; site: string };
  const browser = await chromium.connectOverCDP(boot.endpoint, {
    headers: { Authorization: `Bearer ${boot.token}` },
  });
  t.after(() => browser.close());
  const context = browser.contexts()[0];
  if (!context) throw new Error("Playwright did not expose the default browser context");
  const page = context.pages()[0] ?? await context.newPage();
  const dir = await mkdtemp(join(tmpdir(), "interface-conformance-"));
  const fixture = join(dir, "resume.txt");
  await writeFile(fixture, "bounded fixture\n");
  const proof = await runCanonicalScenario(playwrightDriver(page), boot.site, fixture);
  assert.deepEqual(proof, { submitted: true, popupObserved: true, downloadVerified: true });
});

async function readLine(stream: NodeJS.ReadableStream): Promise<string> {
  let buffered = "";
  stream.setEncoding("utf8");
  for await (const chunk of stream) {
    buffered += chunk;
    const newline = buffered.indexOf("\n");
    if (newline >= 0) return buffered.slice(0, newline);
  }
  const [code] = await once(stream, "close");
  throw new Error(`gateway closed before readiness: ${String(code)}`);
}
