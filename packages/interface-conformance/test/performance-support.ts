import assert from "node:assert/strict";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { join } from "node:path";

export type PerformanceSample = {
  adapterWallMs: number;
  adapterOperationMs: number;
  harnessEnvelopeOverheadMs: number;
};

export type PerformanceEvent =
  | { event: "measurement-start"; adapter: string; samples: number; rootPid: number }
  | { event: "sample"; adapter: string; index: number; sample: PerformanceSample; rootPid: number }
  | { event: "client-disconnected"; adapter: string; samples: PerformanceSample[]; rootPid: number };

export class OperationTimer {
  elapsedMs = 0;

  async measure<T>(operation: () => Promise<T>): Promise<T> {
    const started = performance.now();
    try {
      return await operation();
    } finally {
      this.elapsedMs += performance.now() - started;
    }
  }
}

export function requestedPerformanceSamples(): number | undefined {
  const raw = process.env.CONFORMANCE_PERFORMANCE_SAMPLES;
  if (raw === undefined) return undefined;
  const samples = Number.parseInt(raw, 10);
  assert(Number.isSafeInteger(samples) && samples >= 7, "performance gate requires at least seven samples");
  return samples;
}

export async function runPersistentPerformance(options: {
  adapter: string;
  samples: number;
  run: (timer: OperationTimer) => Promise<unknown>;
  disconnect: () => Promise<void>;
}): Promise<void> {
  await options.run(new OperationTimer()); // exactly one discarded warmup
  await emit({ event: "measurement-start", adapter: options.adapter, samples: options.samples, rootPid: process.pid }, "ready.json");
  const samples: PerformanceSample[] = [];
  for (let index = 0; index < options.samples; index += 1) {
    const timer = new OperationTimer();
    const started = performance.now();
    await options.run(timer);
    const adapterWallMs = performance.now() - started;
    const sample = {
      adapterWallMs,
      adapterOperationMs: timer.elapsedMs,
      harnessEnvelopeOverheadMs: adapterWallMs - timer.elapsedMs,
    };
    samples.push(sample);
    await emit({ event: "sample", adapter: options.adapter, index, sample, rootPid: process.pid }, `sample-${index}.json`);
  }
  await options.disconnect();
  await emit({ event: "client-disconnected", adapter: options.adapter, samples, rootPid: process.pid }, "disconnected.json");
  await waitForRssAcknowledgement();
}

export function instrumentAsyncMethods<T extends object>(target: T, timer: OperationTimer): T {
  return new Proxy(target, {
    get(object, property, receiver) {
      const value = Reflect.get(object, property, receiver) as unknown;
      if (typeof value !== "function") return value;
      return (...args: unknown[]) => timer.measure(async () => Reflect.apply(value, object, args));
    },
  });
}

async function emit(event: PerformanceEvent, filename: string): Promise<void> {
  const directory = process.env.CONFORMANCE_PERFORMANCE_CONTROL_DIR;
  if (!directory) return;
  await mkdir(directory, { recursive: true });
  const destination = join(directory, filename);
  const temporary = `${destination}.${process.pid}.tmp`;
  await writeFile(temporary, JSON.stringify(event));
  await rename(temporary, destination);
}

async function waitForRssAcknowledgement(): Promise<void> {
  if (process.env.CONFORMANCE_PERFORMANCE_WAIT_FOR_RSS !== "1") return;
  const directory = process.env.CONFORMANCE_PERFORMANCE_CONTROL_DIR;
  assert(directory, "missing performance control directory");
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    try {
      const acknowledgement = JSON.parse(await readFile(join(directory, "ack.json"), "utf8")) as { event?: string };
      if (acknowledgement.event === "rss-sampled") return;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    }
    await new Promise(resolve => setTimeout(resolve, 25));
  }
  assert.fail("RSS acknowledgement timed out");
}
