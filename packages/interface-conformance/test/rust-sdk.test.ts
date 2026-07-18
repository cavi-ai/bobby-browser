import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { test } from "node:test";

test("Rust SDK executes the shared real-Chrome canonical matrix", { timeout: 120_000 }, async () => {
  const benchmarking = process.env.CONFORMANCE_PERFORMANCE_SAMPLES !== undefined;
  const args = ["test", "-q", "-p", "interface-conformance", "--test", "rust_sdk", "rust_sdk_executes_every_canonical_step_on_real_chrome"];
  if (benchmarking) args.push("--", "--nocapture");
  const child=spawn(process.env.CARGO??"cargo",args,{cwd:new URL("../../../..",import.meta.url),stdio:[benchmarking?"pipe":"ignore","pipe","pipe"],env:{...process.env,...(process.env.CONFORMANCE_PROOF_DIR?{CONFORMANCE_PROOF_PATH:`${process.env.CONFORMANCE_PROOF_DIR}/rust-sdk.json`}:{})}});
  let output="";child.stdout!.setEncoding("utf8");child.stderr!.setEncoding("utf8");child.stdout!.on("data",c=>{output=(output+String(c)).slice(-16384);if(benchmarking)process.stdout.write(String(c))});child.stderr!.on("data",c=>{output=(output+String(c)).slice(-16384)});
  const[code]=await once(child,"exit") as [number|null,NodeJS.Signals|null];
  assert.equal(code,0,output);assert.match(output,/1 passed/);
});
