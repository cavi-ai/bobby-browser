import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

const performanceSource = new URL("../../test/performance.test.ts", import.meta.url);
const performanceSupportSource = new URL("../../test/performance-support.ts", import.meta.url);
const packageManifest = new URL("../../package.json", import.meta.url);
const mcpSource = new URL("../../../../crates/interface-conformance/tests/mcp_live.rs", import.meta.url);
const rustSource = new URL("../../../../crates/interface-conformance/tests/rust_sdk.rs", import.meta.url);
const typescriptSource = new URL("../../test/typescript-sdk.test.ts", import.meta.url);

test("performance gate keeps one persistent driver process per adapter", async () => {
  const source = `${await readFile(performanceSource, "utf8")}\n${await readFile(performanceSupportSource, "utf8")}`;

  assert.match(source, /CONFORMANCE_PERFORMANCE_SAMPLES/);
  assert.match(source, /measurement-start/);
  assert.match(source, /client-disconnected/);
  assert.doesNotMatch(source, /for \(let index = 0; index < samples; index\+\+\) values\.push\(await run\(adapter\)\)/);
});

test("performance output reports paired operation and adapter deltas without a baseline or clamp", async () => {
  const source = await readFile(performanceSource, "utf8");

  assert.match(source, /adapter_operation_median_ms=/);
  assert.match(source, /adapter_wall_median_ms=/);
  assert.match(source, /harness_envelope_overhead_median_ms=/);
  assert.match(source, /rss_before_kib=/);
  assert.match(source, /rss_peak_kib=/);
  assert.match(source, /process_tree_rss_after_transport_close_kib=/);
  assert.match(source, /process_tree_rss_retained_kib=/);
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
  const release = manifest.scripts?.["test:release"] ?? "";
  assert.match(release, /--test interface_security/);
  assert.match(release, /--test interface_capacity/);
  assert.match(release, /--test interface_recovery/);
  assert.match(release, /--test interface_performance/);
  assert.match(release, /real_security_release_matrix_executes_every_production_boundary/);
  assert.match(release, /installed_chromium_daemon_abort_rebuilds_from_the_same_durable_journal/);
  assert.match(release, /pnpm run test && pnpm run test:performance$/);
});

test("performance adapters close real transports before post-disconnect RSS", async () => {
  const [mcp, rust, typescript] = await Promise.all([
    readFile(mcpSource, "utf8"),
    readFile(rustSource, "utf8"),
    readFile(typescriptSource, "utf8"),
  ]);

  assert.match(mcp, /tokio::process::Command/);
  assert.match(mcp, /Stdio::piped/);
  assert.match(mcp, /child\.stdin\.take/);
  assert.match(mcp, /drop\(.*stdin/);
  assert.match(mcp, /run_mcp_sample\(&metadata, &mut transport, &mut denied_transport/);
  assert.match(rust, /PrimitiveCommand::ClosePage/);
  assert.match(rust, /drop\(runtime\)/);
  assert.match(typescript, /new Agent\(\{ keepAlive: true \}\)/);
  assert.match(typescript, /agent\.destroy\(\)/);
  assert.match(typescript, /assert\.equal\(activeHttpSockets\(agent\), 0/);
});

test("performance labels describe measured operation time and process-tree retention", async () => {
  const source = await readFile(performanceSource, "utf8");
  assert.match(source, /adapter_operation_median_ms=/);
  assert.match(source, /process_tree_rss_after_transport_close_kib=/);
  assert.doesNotMatch(source, /browser time/i);
});
