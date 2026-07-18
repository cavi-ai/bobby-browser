import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

const performanceSource = new URL("../../test/performance.test.ts", import.meta.url);
const performanceSupportSource = new URL("../../test/performance-support.ts", import.meta.url);
const packageManifest = new URL("../../package.json", import.meta.url);

test("performance gate keeps one persistent driver process per adapter", async () => {
  const source = `${await readFile(performanceSource, "utf8")}\n${await readFile(performanceSupportSource, "utf8")}`;

  assert.match(source, /CONFORMANCE_PERFORMANCE_SAMPLES/);
  assert.match(source, /measurement-start/);
  assert.match(source, /client-disconnected/);
  assert.doesNotMatch(source, /for \(let index = 0; index < samples; index\+\+\) values\.push\(await run\(adapter\)\)/);
});

test("performance output reports paired operation and adapter deltas without a baseline or clamp", async () => {
  const source = await readFile(performanceSource, "utf8");

  assert.match(source, /operation_median_ms=/);
  assert.match(source, /adapter_wall_median_ms=/);
  assert.match(source, /adapter_overhead_median_ms=/);
  assert.match(source, /rss_before_kib=/);
  assert.match(source, /rss_peak_kib=/);
  assert.match(source, /rss_after_disconnect_kib=/);
  assert.match(source, /rss_growth_kib=/);
  assert.doesNotMatch(source, /browserBaselineMs|rust.*baseline/i);
  assert.doesNotMatch(source, /Math\.max\(0,\s*sample\.elapsedMs/);
});

test("package exposes the durable real performance release gate", async () => {
  const manifest = JSON.parse(await readFile(packageManifest, "utf8")) as {
    scripts?: Record<string, string>;
  };

  assert.equal(
    manifest.scripts?.["test:performance"],
    "pnpm run build && node --test dist/test/performance.test.js",
  );
  assert.match(manifest.scripts?.["test:release"] ?? "", /test:performance/);
});
