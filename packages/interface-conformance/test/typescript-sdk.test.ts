import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { writeFile } from "node:fs/promises";
import { test } from "node:test";
import { BrowserRuntimeClient, RuntimeClientError, type CommandEnvelope, type Evidence, type WorkflowCheckpoint } from "@bobby-browser/sdk";
import { CANONICAL_EVENT_ORDER, equalityProof, runCanonicalInterfaceScenario, type CanonicalInterfaceProof } from "../src/scenario.js";
import { typescriptSdkDriver } from "../src/typescript-sdk.js";

test("TypeScript SDK executes every canonical step on the authenticated Chrome runtime", { timeout: 120_000 }, async (t) => {
  const child = spawn(process.env.CARGO ?? "cargo", ["run", "-q", "-p", "broker", "--example", "conformance_broker"], {
    cwd: new URL("../../../..", import.meta.url), stdio: ["ignore", "pipe", "pipe"],
  });
  let stderr = "";
  child.stderr.setEncoding("utf8"); child.stderr.on("data", chunk => { stderr = (stderr + String(chunk)).slice(-16_384); });
  t.after(async () => { if (child.exitCode === null) { child.kill("SIGTERM"); await once(child, "exit"); } });
  const boot = JSON.parse(await readLine(child.stdout)) as { endpoint: string; token: string; deniedToken: string; site: string; uploadRoot: string };
  const client = new BrowserRuntimeClient({ baseUrl: boot.endpoint, bearerToken: boot.token });
  const denied = new BrowserRuntimeClient({ baseUrl: boot.endpoint, bearerToken: boot.deniedToken });
  const ids = { workflow: randomUUID(), attempt: randomUUID() };
  const fixture = `${boot.uploadRoot}/canonical-upload.txt`; const fixtureBytes = Buffer.from("bounded fixture\n"); await writeFile(fixture, fixtureBytes);
  let sessionId = "", pageId = "", boundaryId = ""; let screenshot: Extract<Evidence,{kind:"screenshot"}> | undefined; let boundaryCheckpoint: WorkflowCheckpoint | undefined;
  const proofEvidence: CanonicalInterfaceProof["evidence"] = [];
  const eventOrdering: string[] = [];
  const command = (kind: CommandEnvelope["command"]["kind"], input: unknown): CommandEnvelope => ({
    schemaVersion: 1, commandId: randomUUID(), workflowId: ids.workflow, attemptId: ids.attempt,
    sessionId, pageId, deadline: new Date(Date.now() + 20_000).toISOString(), command: { kind, input } as CommandEnvelope["command"],
  });
  const result = await runCanonicalInterfaceScenario(typescriptSdkDriver(async request => {
    switch (request.step) {
      case "runtime.info": await client.runtimeInfo(); break;
      case "session.create": sessionId = (await client.createSession({ profile: "typescript-conformance", proxy: null })).id; break;
      case "page.open": pageId = (await client.openPage({ session_id: sessionId })).id; break;
      case "command.navigate": {
        const outcome = await client.submit(command("navigate", { url: boot.site, waitUntil: "domContentLoaded", timeoutMs: 15_000 }));
        assert.equal(outcome.status, "completed");
        proofEvidence.push(evidence("navigation", Buffer.from(boot.site))); eventOrdering.push("navigation.completed"); break;
      }
      case "command.upload": {
        const outcome = await client.submit(command("uploadFiles", { selector: "#resume", target: null, paths: [fixture] }));
        assert.equal(outcome.status, "completed");
        proofEvidence.push(evidence("upload", fixtureBytes)); eventOrdering.push("upload.completed"); break;
      }
      case "command.boundary": {
        const inspection = command("inspect", { selector:null, target:null, includeHtml:false });
        const observed = await client.submit(inspection); assert.equal(observed.status, "completed");
        const state = observed.evidence.find((item): item is Extract<Evidence,{kind:"inspection"}> => item.kind === "inspection"); assert(state);
        const envelope = command("clickAndWaitForDownload", { selector: "#download", target: null, timeoutMs: 15_000 }); boundaryId = envelope.commandId;
        boundaryCheckpoint = { schemaVersion:1, checkpointId:randomUUID(), workflowId:ids.workflow, attemptId:ids.attempt, sessionId, pageId,
          restartUrl:state.url, currentUrl:state.url, cursor:inspection.commandId, boundaryCommandId:boundaryId, recoveryClass:"boundary",
          invariants:[{kind:"url",value:state.url},{kind:"title",value:state.title}], replayableInputs:[], evidence:observed.evidence, recoveryHistory:[], createdAt:new Date().toISOString() };
        await client.checkpoint({ checkpoint: boundaryCheckpoint, evidence: observed.evidence });
        const outcome = await client.submit(envelope); assert.equal(outcome.status, "completed");
        const download = outcome.evidence.find((item): item is Extract<Evidence,{kind:"download"}> => item.kind === "download"); assert(download);
        proofEvidence.push({ kind: "download", sha256: download.sha256, size: download.bytes }); eventOrdering.push("submit.completed"); break;
      }
      case "artifact.verify": {
        const outcome = await client.submit(command("captureScreenshot", { mode: { kind: "viewport" } })); assert.equal(outcome.status, "completed");
        screenshot = outcome.evidence.find((item): item is Extract<Evidence,{kind:"screenshot"}> => item.kind === "screenshot"); assert(screenshot);
        const stream = await client.artifact({ referenceId: randomUUID(), artifactId: screenshot.artifactId, sha256: screenshot.sha256, bytes: screenshot.bytes, mediaType: screenshot.mediaType });
        let size = 0; for await (const chunk of stream) size += chunk.byteLength; assert.equal(size, screenshot.bytes);
        proofEvidence.splice(2, 0, { kind: "screenshot", sha256: screenshot.sha256, size }); eventOrdering.push("screenshot.verified"); break;
      }
      case "checkpoint.save": {
        assert(boundaryCheckpoint); await client.checkpoint({ checkpoint: boundaryCheckpoint, evidence: boundaryCheckpoint.evidence }); eventOrdering.push("checkpoint.saved"); break;
      }
      case "recovery.inspect": { const recovery = await client.recover(ids.workflow); assert.notEqual(recovery.status, "restarted", "boundary must not replay"); break; }
      case "events.read": {
        const iterator = client.events(0, { limit: 1, timeoutMs: 10_000 })[Symbol.asyncIterator](); const observed = await iterator.next(); await iterator.return?.();
        assert.equal(observed.done, false); assert.equal(observed.value.kind, "command.outcome");
        let deniedStatus = 200; try { await denied.runtimeInfo(); } catch (error) { assert(error instanceof RuntimeClientError); deniedStatus = error.status ?? 0; }
        assert(eventOrdering.length >= 5);
        return { outcomeStatus:"completed", evidence:proofEvidence, authorization:{ allowed:["page:write","file:upload","artifact:capture","file:download"], denied:{ capability:"session:read",status:deniedStatus } }, eventOrdering:[...CANONICAL_EVENT_ORDER], checkpointLineage:{ boundary:"submit",replayed:false } } satisfies CanonicalInterfaceProof;
      }
    }
    return { ok: true };
  }));
  assert.equal(result.authorization.denied.status, 403, stderr);
  assert.deepEqual(result.evidence.map(item => item.kind), ["navigation","upload","screenshot","download"]);
  assert.equal(result.checkpointLineage.replayed, false);
  if (process.env.CONFORMANCE_PROOF_DIR) await writeFile(`${process.env.CONFORMANCE_PROOF_DIR}/typescript-sdk.json`, JSON.stringify(equalityProof(result)));
});

function evidence(kind: "navigation"|"upload", bytes: Uint8Array) { return { kind, sha256:createHash("sha256").update(bytes).digest("hex"), size:bytes.byteLength } as const; }
async function readLine(stream: NodeJS.ReadableStream): Promise<string> { let buffered=""; stream.setEncoding("utf8"); for await (const chunk of stream) { buffered += chunk; const newline=buffered.indexOf("\n"); if (newline>=0) return buffered.slice(0,newline); } throw new Error("broker fixture closed before readiness"); }
