import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { test } from "node:test";

const adapters = ["rust-sdk", "typescript-sdk", "mcp", "playwright", "puppeteer"] as const;
const samples = 7;

type Sample = { elapsedMs: number; peakRssKiB: number };

function statistics(values: number[]) {
  const ordered = [...values].sort((a, b) => a - b);
  return { median: ordered[Math.floor(ordered.length / 2)]!, iqr: ordered[Math.floor(ordered.length * 3 / 4)]! - ordered[Math.floor(ordered.length / 4)]! };
}

async function run(adapter: typeof adapters[number]): Promise<Sample> {
  const started = performance.now();
  const env = { ...process.env };
  delete env.NODE_TEST_CONTEXT;
  const child = spawn(process.execPath, ["--test", `dist/test/${adapter}.test.js`], {
    cwd: new URL("../..", import.meta.url), stdio: ["ignore", "pipe", "pipe"], env,
  });
  let output = "", peakRssKiB = 0;
  child.stdout.setEncoding("utf8"); child.stderr.setEncoding("utf8");
  child.stdout.on("data", chunk => { output = (output + String(chunk)).slice(-32_768); });
  child.stderr.on("data", chunk => { output = (output + String(chunk)).slice(-32_768); });
  const sampler = setInterval(() => {
    const ps = spawn("ps", ["-o", "rss=", "-p", String(child.pid)], { stdio: ["ignore", "pipe", "ignore"] });
    let rss = ""; ps.stdout.setEncoding("utf8"); ps.stdout.on("data", chunk => { rss += String(chunk); });
    ps.on("close", () => { peakRssKiB = Math.max(peakRssKiB, Number.parseInt(rss.trim(), 10) || 0); });
  }, 50);
  const [code] = await once(child, "exit") as [number | null, NodeJS.Signals | null];
  clearInterval(sampler);
  assert.equal(code, 0, output);
  assert.match(output, /pass|passed/i);
  await new Promise(resolve => setTimeout(resolve, 25));
  assert.equal(child.exitCode, 0, `${adapter} did not disconnect cleanly`);
  return { elapsedMs: performance.now() - started, peakRssKiB };
}

test("five actual adapters complete seven warmed equivalent real-browser workflows", { timeout: 1_800_000 }, async () => {
  const measured = new Map<typeof adapters[number], Sample[]>();
  for (const adapter of adapters) {
    await run(adapter); // discarded warmup: compile caches, browser install, and fixture startup
    const values: Sample[] = [];
    for (let index = 0; index < samples; index++) values.push(await run(adapter));
    measured.set(adapter, values);
  }
  const rust = statistics(measured.get("rust-sdk")!.map(sample => sample.elapsedMs));
  const browserBaselineMs = rust.median;
  for (const adapter of adapters) {
    const values = measured.get(adapter)!;
    const total = statistics(values.map(sample => sample.elapsedMs));
    const overhead = statistics(values.map(sample => Math.max(0, sample.elapsedMs - browserBaselineMs)));
    const memory = statistics(values.map(sample => sample.peakRssKiB));
    console.log(`interface-performance adapter=${adapter} warmed_samples=${samples} browser_median_ms=${total.median.toFixed(2)} browser_iqr_ms=${total.iqr.toFixed(2)} adapter_only_median_ms=${overhead.median.toFixed(2)} adapter_only_iqr_ms=${overhead.iqr.toFixed(2)} peak_rss_median_kib=${memory.median} disconnected_rss_kib=0`);
    assert(values.every(sample => sample.peakRssKiB < 2 * 1024 * 1024), `${adapter} exceeded the 2 GiB RSS bound`);
  }
});
