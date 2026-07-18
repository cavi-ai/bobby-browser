import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { type PerformanceEvent, type PerformanceSample } from "./performance-support.js";

const adapters = ["rust-sdk", "typescript-sdk", "mcp", "playwright", "puppeteer"] as const;
const selectedAdapters = process.env.CONFORMANCE_PERFORMANCE_ADAPTERS
  ? adapters.filter(adapter => process.env.CONFORMANCE_PERFORMANCE_ADAPTERS!.split(",").includes(adapter))
  : adapters;
const samples = 7;
const maxSettledGrowthKiB = 256 * 1024;
const maxPeakGrowthKiB = 512 * 1024;

type AdapterResult = {
  samples: PerformanceSample[];
  rssBeforeKiB: number;
  rssPeakKiB: number;
  rssAfterDisconnectKiB: number;
};

function statistics(values: number[]) {
  const ordered = [...values].sort((a, b) => a - b);
  return {
    median: ordered[Math.floor(ordered.length / 2)]!,
    iqr: ordered[Math.floor(ordered.length * 3 / 4)]! - ordered[Math.floor(ordered.length / 4)]!,
  };
}

async function runPersistentAdapter(adapter: typeof adapters[number]): Promise<AdapterResult> {
  const controlDirectory = await mkdtemp(join(tmpdir(), `interface-performance-${adapter}-`));
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    CONFORMANCE_PERFORMANCE_SAMPLES: String(samples),
    CONFORMANCE_PERFORMANCE_WAIT_FOR_RSS: "1",
    CONFORMANCE_PERFORMANCE_CONTROL_DIR: controlDirectory,
  };
  delete env.NODE_TEST_CONTEXT;
  // Run the adapter test module directly. Nesting it under another `node --test`
  // process buffers its stdout until completion, which deadlocks the explicit
  // RSS marker/ack handshake.
  const child = spawn(process.execPath, [`dist/test/${adapter}.test.js`], {
    cwd: new URL("../..", import.meta.url), stdio: ["pipe", "pipe", "pipe"], env,
  });
  const childExit = once(child, "exit") as Promise<[number | null, NodeJS.Signals | null]>;
  assert(child.pid, `${adapter} benchmark child did not start`);
  let output = "";
  let rssBeforeKiB = 0, rssPeakKiB = 0, rssAfterDisconnectKiB = 0;
  let measured: PerformanceSample[] | undefined;
  let sampler: NodeJS.Timeout | undefined;
  let sampleChain = Promise.resolve();
  const samplePeak = () => {
    sampleChain = sampleChain.then(async () => {
      rssPeakKiB = Math.max(rssPeakKiB, await processTreeRssKiB(child.pid!));
    });
  };
  child.stdout.setEncoding("utf8"); child.stderr.setEncoding("utf8");
  child.stdout.on("data", chunk => {
    output = (output + String(chunk)).slice(-65_536);
  });
  child.stderr.on("data", chunk => { output = (output + String(chunk)).slice(-65_536); });
  const ready = await waitForEvent(join(controlDirectory, "ready.json"), child, output) as Extract<PerformanceEvent, { event: "measurement-start" }>;
  assert.equal(ready.adapter, adapter); assert.equal(ready.samples, samples); assert(ready.rootPid > 0);
  rssBeforeKiB = await processTreeRssKiB(child.pid!);
  assert(rssBeforeKiB > 0, `${adapter} process tree was not alive before measurement`);
  rssPeakKiB = rssBeforeKiB; sampler = setInterval(samplePeak, 50);
  const disconnected = await waitForEvent(join(controlDirectory, "disconnected.json"), child, output) as Extract<PerformanceEvent, { event: "client-disconnected" }>;
  assert.equal(disconnected.adapter, adapter); assert.equal(disconnected.samples.length, samples);
  if (sampler) clearInterval(sampler);
  await sampleChain;
  rssPeakKiB = Math.max(rssPeakKiB, await processTreeRssKiB(child.pid!));
  measured = disconnected.samples;
  rssAfterDisconnectKiB = await processTreeRssKiB(child.pid!);
  assert(rssAfterDisconnectKiB > 0, `${adapter} daemon/browser tree exited before post-disconnect RSS`);
  const ack = join(controlDirectory, "ack.json"); const ackTmp = `${ack}.${process.pid}.tmp`;
  await writeFile(ackTmp, JSON.stringify({ event: "rss-sampled", rootPid: child.pid })); await rename(ackTmp, ack);
  const [code] = await childExit;
  await rm(controlDirectory, { recursive: true, force: true });
  assert.equal(code, 0, output);
  return { samples: measured, rssBeforeKiB, rssPeakKiB, rssAfterDisconnectKiB };
}

async function waitForEvent(path: string, child: ReturnType<typeof spawn>, output: string): Promise<PerformanceEvent> {
  const deadline = Date.now() + 180_000;
  while (Date.now() < deadline) {
    try { return JSON.parse(await readFile(path, "utf8")) as PerformanceEvent; }
    catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
    assert.equal(child.exitCode, null, `benchmark child exited before ${path}: ${output}`);
    await new Promise(resolve => setTimeout(resolve, 25));
  }
  child.kill("SIGTERM"); assert.fail(`timed out waiting for ${path}: ${output}`);
}

test("five actual adapters use one warmed persistent fixture for seven paired samples", { timeout: 1_800_000 }, async () => {
  assert(selectedAdapters.length > 0, "no recognized performance adapters selected");
  for (const adapter of selectedAdapters) {
    const result = await runPersistentAdapter(adapter);
    for (const sample of result.samples) {
      assert(sample.adapterOperationMs > 0, `${adapter} did not instrument adapter operation time`);
      assert(sample.adapterWallMs >= sample.adapterOperationMs, `${adapter} operation timing escaped its adapter wall boundary`);
      assert(Math.abs(sample.harnessEnvelopeOverheadMs - (sample.adapterWallMs - sample.adapterOperationMs)) < 0.001,
        `${adapter} harness envelope overhead was not the paired per-sample delta`);
    }
    const operation = statistics(result.samples.map(sample => sample.adapterOperationMs));
    const wall = statistics(result.samples.map(sample => sample.adapterWallMs));
    const overhead = statistics(result.samples.map(sample => sample.harnessEnvelopeOverheadMs));
    const rssGrowthKiB = result.rssAfterDisconnectKiB - result.rssBeforeKiB;
    const rssPeakGrowthKiB = result.rssPeakKiB - result.rssBeforeKiB;
    assert(rssGrowthKiB <= maxSettledGrowthKiB, `${adapter} retained ${rssGrowthKiB} KiB after client disconnect`);
    assert(rssPeakGrowthKiB <= maxPeakGrowthKiB, `${adapter} grew ${rssPeakGrowthKiB} KiB at peak`);
    console.log(
      `interface-performance adapter=${adapter} warmed_samples=${samples}` +
      ` adapter_operation_median_ms=${operation.median.toFixed(2)} adapter_operation_iqr_ms=${operation.iqr.toFixed(2)}` +
      ` adapter_wall_median_ms=${wall.median.toFixed(2)} adapter_wall_iqr_ms=${wall.iqr.toFixed(2)}` +
      ` harness_envelope_overhead_median_ms=${overhead.median.toFixed(2)} harness_envelope_overhead_iqr_ms=${overhead.iqr.toFixed(2)}` +
      ` rss_before_kib=${result.rssBeforeKiB} rss_peak_kib=${result.rssPeakKiB}` +
      ` process_tree_rss_after_transport_close_kib=${result.rssAfterDisconnectKiB} process_tree_rss_retained_kib=${rssGrowthKiB}`,
    );
  }
});

async function processTreeRssKiB(rootPid: number): Promise<number> {
  const ps = spawn("ps", ["-axo", "pid=,ppid=,rss="], { stdio: ["ignore", "pipe", "pipe"] });
  let stdout = "", stderr = "";
  ps.stdout.setEncoding("utf8"); ps.stderr.setEncoding("utf8");
  ps.stdout.on("data", chunk => { stdout += String(chunk); });
  ps.stderr.on("data", chunk => { stderr += String(chunk); });
  const [code] = await once(ps, "exit") as [number | null, NodeJS.Signals | null];
  assert.equal(code, 0, stderr);
  const rows = stdout.split("\n").map(line => line.trim().split(/\s+/).map(Number)).filter(row => row.length === 3 && row.every(Number.isFinite));
  const children = new Map<number, number[]>();
  const rss = new Map<number, number>();
  for (const [pid, ppid, resident] of rows as [number, number, number][]) {
    rss.set(pid, resident);
    const entries = children.get(ppid) ?? [];
    entries.push(pid); children.set(ppid, entries);
  }
  const pending = [rootPid];
  const seen = new Set<number>();
  let total = 0;
  while (pending.length) {
    const pid = pending.pop()!;
    if (seen.has(pid)) continue;
    seen.add(pid); total += rss.get(pid) ?? 0;
    pending.push(...(children.get(pid) ?? []));
  }
  return total;
}
