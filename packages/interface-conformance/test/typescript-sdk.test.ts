import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { Agent, request as httpRequest } from "node:http";
import { writeFile } from "node:fs/promises";
import { test } from "node:test";
import { BrowserRuntimeClient, RuntimeClientError, type CommandEnvelope, type Evidence, type PrimitiveCommand, type WorkflowCheckpoint } from "@cavi-ai/bobby-browser";
import { equalityProof, runCanonicalInterfaceScenario, type CanonicalInterfaceProof } from "../src/scenario.js";
import { typescriptSdkDriver } from "../src/typescript-sdk.js";
import { type OperationTimer, requestedPerformanceSamples, runPersistentPerformance } from "./performance-support.js";

test("TypeScript SDK executes every canonical step on the authenticated Chrome runtime", { timeout: 1_800_000 }, async (t) => {
  const child = spawn(process.env.CARGO ?? "cargo", ["run", "-q", "-p", "runtime-tests", "--example", "conformance_broker"], {
    cwd: new URL("../../../..", import.meta.url), stdio: ["ignore", "pipe", "pipe"],
  });
  let stderr = "";
  child.stderr.setEncoding("utf8"); child.stderr.on("data", chunk => { stderr = (stderr + String(chunk)).slice(-16_384); });
  t.after(async () => { if (child.exitCode === null) { child.kill("SIGTERM"); await once(child, "exit"); } });
  const boot = JSON.parse(await readLine(child.stdout)) as { endpoint: string; token: string; deniedToken: string; site: string; uploadRoot: string };
  const agent = new Agent({ keepAlive: true });
  const dedicatedFetch = fetchWithAgent(agent);
  let client: BrowserRuntimeClient | undefined = new BrowserRuntimeClient({ baseUrl: boot.endpoint, bearerToken: boot.token, fetch: dedicatedFetch });
  let denied: BrowserRuntimeClient | undefined = new BrowserRuntimeClient({ baseUrl: boot.endpoint, bearerToken: boot.deniedToken, fetch: dedicatedFetch });
  const fixture = `${boot.uploadRoot}/canonical-upload.txt`;
  const fixtureBytes = Buffer.from("bounded fixture\n");
  await writeFile(fixture, fixtureBytes);

  // The daemon, authenticated clients, browser session, and page remain alive across
  // the discarded warmup and every measured sample.
  await client.runtimeInfo();
  const sessionId = (await client.createSession({ profile: "typescript-conformance", proxy: null })).id;
  const pageId = (await client.openPage({ session_id: sessionId })).id;

  const execute = async (timer?: OperationTimer): Promise<CanonicalInterfaceProof> => {
    assert(client && denied);
    const activeClient = client;
    const deniedClient = denied;
    const ids = { workflow: randomUUID(), attempt: randomUUID() };
    let boundaryId = "", savedCheckpointId = "", recoveryStatus = "", recoveryCheckpointId = "";
    let replayed = true;
    let screenshot: Extract<Evidence, { kind: "screenshot" }> | undefined;
    let boundaryCheckpoint: WorkflowCheckpoint | undefined;
    const proofEvidence: CanonicalInterfaceProof["evidence"] = [];
    const eventOrdering: string[] = [];
    const command = (kind: PrimitiveCommand["kind"], input: unknown): CommandEnvelope => ({
      schemaVersion: 2, commandId: randomUUID(), workflowId: ids.workflow, attemptId: ids.attempt,
      sessionId, pageId, deadline: new Date(Date.now() + 20_000).toISOString(),
      command: { kind: "primitive", input: { kind, input } as PrimitiveCommand },
    });
    const handle = async (request: { step: string }): Promise<unknown> => {
      switch (request.step) {
        case "runtime.info": await activeClient.runtimeInfo(); break;
        case "session.create": break; // persistent warmed browser session
        case "page.open": break; // persistent warmed page
        case "command.navigate": {
          const outcome = await activeClient.submit(command("navigate", { url: boot.site, waitUntil: "domContentLoaded", timeoutMs: 15_000 }));
          assert.equal(outcome.status, "completed");
          proofEvidence.push(evidence("navigation", Buffer.from(boot.site))); eventOrdering.push("navigation.completed"); break;
        }
        case "command.upload": {
          const outcome = await activeClient.submit(command("uploadFiles", { selector: "#resume", target: null, paths: [fixture] }));
          assert.equal(outcome.status, "completed");
          proofEvidence.push(evidence("upload", fixtureBytes)); eventOrdering.push("upload.completed"); break;
        }
        case "command.boundary": {
          const popupInspection = command("inspect", { selector: null, target: null, includeHtml: false });
          const popupObserved = await activeClient.submit(popupInspection); assert.equal(popupObserved.status, "completed");
          const popupState = popupObserved.evidence.find((item): item is Extract<Evidence, { kind: "inspection" }> => item.kind === "inspection"); assert(popupState);
          const popupEnvelope = command("clickAndWaitForPopup", { selector: "#root-popup", target: null, timeoutMs: 15_000 });
          const popupCheckpoint: WorkflowCheckpoint = { schemaVersion: 1, checkpointId: randomUUID(), workflowId: ids.workflow, attemptId: ids.attempt, sessionId, pageId, restartUrl: popupState.url, currentUrl: popupState.url, cursor: popupInspection.commandId, boundaryCommandId: popupEnvelope.commandId, recoveryClass: "boundary", invariants: [{ kind: "url", value: popupState.url }, { kind: "title", value: popupState.title }], replayableInputs: [], evidence: popupObserved.evidence, recoveryHistory: [], recoveryReceipts: [], createdAt: new Date().toISOString() };
          await activeClient.checkpoint({ checkpoint: popupCheckpoint, evidence: popupObserved.evidence }); eventOrdering.push("checkpoint.saved");
          const popupOutcome = await activeClient.submit(popupEnvelope); assert.equal(popupOutcome.status, "completed"); eventOrdering.push("boundary.completed");
          const inspection = command("inspect", { selector: null, target: null, includeHtml: false });
          const observed = await activeClient.submit(inspection); assert.equal(observed.status, "completed");
          const state = observed.evidence.find((item): item is Extract<Evidence, { kind: "inspection" }> => item.kind === "inspection"); assert(state);
          const envelope = command("clickAndWaitForDownload", { selector: "#download", target: null, timeoutMs: 15_000 }); boundaryId = envelope.commandId;
          boundaryCheckpoint = { schemaVersion: 1, checkpointId: randomUUID(), workflowId: ids.workflow, attemptId: ids.attempt, sessionId, pageId, restartUrl: state.url, currentUrl: state.url, cursor: inspection.commandId, boundaryCommandId: boundaryId, recoveryClass: "boundary", invariants: [{ kind: "url", value: state.url }, { kind: "title", value: state.title }], replayableInputs: [], evidence: observed.evidence, recoveryHistory: [], recoveryReceipts: [], createdAt: new Date().toISOString() };
          const saved = await activeClient.checkpoint({ checkpoint: boundaryCheckpoint, evidence: observed.evidence }); savedCheckpointId = saved.checkpointId; eventOrdering.push("checkpoint.saved");
          const outcome = await activeClient.submit(envelope); assert.equal(outcome.status, "completed");
          const download = outcome.evidence.find((item): item is Extract<Evidence, { kind: "download" }> => item.kind === "download"); assert(download);
          proofEvidence.push({ kind: "download", sha256: download.sha256, size: download.bytes }); eventOrdering.push("boundary.completed"); break;
        }
        case "artifact.verify": {
          const outcome = await activeClient.submit(command("captureScreenshot", { mode: { kind: "viewport" } })); assert.equal(outcome.status, "completed");
          screenshot = outcome.evidence.find((item): item is Extract<Evidence, { kind: "screenshot" }> => item.kind === "screenshot"); assert(screenshot);
          const stream = await activeClient.artifact({ referenceId: randomUUID(), artifactId: screenshot.artifactId, sha256: screenshot.sha256, bytes: screenshot.bytes, mediaType: screenshot.mediaType });
          let size = 0; for await (const chunk of stream) size += chunk.byteLength; assert.equal(size, screenshot.bytes);
          proofEvidence.splice(2, 0, { kind: "screenshot", sha256: screenshot.sha256, size }); eventOrdering.push("screenshot.verified"); break;
        }
        case "checkpoint.save": {
          assert(boundaryCheckpoint); const saved = await activeClient.checkpoint({ checkpoint: boundaryCheckpoint, evidence: boundaryCheckpoint.evidence }); assert.equal(saved.checkpointId, savedCheckpointId); break;
        }
        case "recovery.inspect": { const recovery = await activeClient.recover(ids.workflow); recoveryStatus = recovery.status; recoveryCheckpointId = recovery.checkpointId; replayed = recovery.status === "restarted"; assert.equal(recoveryCheckpointId, savedCheckpointId); eventOrdering.push("recovery.inspected"); break; }
        case "events.read": {
          const iterator = activeClient.events(0, { limit: 1, timeoutMs: 10_000 })[Symbol.asyncIterator](); const observed = await iterator.next(); await iterator.return?.();
          assert.equal(observed.done, false); assert.equal(observed.value.kind, "command.outcome");
          eventOrdering.push("events.read");
          let deniedStatus = 200; try { await deniedClient.runtimeInfo(); } catch (error) { assert(error instanceof RuntimeClientError); deniedStatus = error.status ?? 0; }
          assert(boundaryCheckpoint?.boundaryCommandId);
          return { outcomeStatus: "completed", evidence: proofEvidence, authorization: { allowed: ["page:write", "file:upload", "artifact:capture", "file:download"], denied: { capability: "session:read", status: deniedStatus } }, eventOrdering, checkpointLineage: { boundary: "boundary", replayed, checkpointId: recoveryCheckpointId, workflowId: ids.workflow, boundaryCommandId: boundaryCheckpoint.boundaryCommandId, recoveryStatus } } satisfies CanonicalInterfaceProof;
        }
      }
      return { ok: true };
    };
    const result = await runCanonicalInterfaceScenario(typescriptSdkDriver(request => timer ? timer.measure(() => handle(request)) : handle(request)));
    assert.equal(result.authorization.denied.status, 403, stderr);
    assert.deepEqual(result.evidence.map(item => item.kind), ["navigation", "upload", "screenshot", "download"]);
    assert.equal(result.checkpointLineage.replayed, false);
    return result;
  };

  const samples = requestedPerformanceSamples();
  if (samples !== undefined) {
    await runPersistentPerformance({
      adapter: "typescript-sdk",
      samples,
      run: execute,
      disconnect: async () => {
        await client!.submit(commandEnvelope(sessionId, pageId, "closePage", { pageId }));
        client = undefined; denied = undefined;
        agent.destroy();
        await waitForSocketDrain(agent);
        assert.equal(activeHttpSockets(agent), 0, "dedicated TypeScript HTTP transport retained sockets");
      },
    });
  } else {
    const result = await execute();
    if (process.env.CONFORMANCE_PROOF_DIR) await writeFile(`${process.env.CONFORMANCE_PROOF_DIR}/typescript-sdk.json`, JSON.stringify(equalityProof(result)));
  }
});

function evidence(kind: "navigation" | "upload", bytes: Uint8Array) { return { kind, sha256: createHash("sha256").update(bytes).digest("hex"), size: bytes.byteLength } as const; }
async function readLine(stream: NodeJS.ReadableStream): Promise<string> { let buffered = ""; stream.setEncoding("utf8"); for await (const chunk of stream) { buffered += chunk; const newline = buffered.indexOf("\n"); if (newline >= 0) return buffered.slice(0, newline); } throw new Error("broker fixture closed before readiness"); }

function commandEnvelope(sessionId: string, pageId: string, kind: PrimitiveCommand["kind"], input: unknown): CommandEnvelope {
  return { schemaVersion: 2, commandId: randomUUID(), workflowId: randomUUID(), attemptId: randomUUID(), sessionId, pageId,
    deadline: new Date(Date.now() + 20_000).toISOString(), command: { kind: "primitive", input: { kind, input } as PrimitiveCommand } };
}

function activeHttpSockets(agent: Agent): number {
  return [...Object.values(agent.sockets), ...Object.values(agent.freeSockets)].reduce((total, sockets) => total + (sockets?.length ?? 0), 0);
}

async function waitForSocketDrain(agent: Agent): Promise<void> {
  const deadline = Date.now() + 5_000;
  while (activeHttpSockets(agent) !== 0 && Date.now() < deadline) {
    await new Promise(resolve => setTimeout(resolve, 10));
  }
}

function fetchWithAgent(agent: Agent): typeof fetch {
  return (async (input: string | URL | Request, init?: RequestInit): Promise<Response> => {
    const url = new URL(typeof input === "string" || input instanceof URL ? input : input.url);
    return await new Promise<Response>((resolve, reject) => {
      const request = httpRequest(url, {
        agent,
        method: init?.method,
        headers: init?.headers as Record<string, string> | undefined,
        signal: init?.signal ?? undefined,
      }, response => {
        const chunks: Buffer[] = [];
        response.on("data", chunk => chunks.push(Buffer.from(chunk)));
        response.on("end", () => resolve(new Response(Buffer.concat(chunks), {
          status: response.statusCode ?? 500,
          headers: Object.entries(response.headers).flatMap(([name, value]) => value === undefined ? [] : [[name, Array.isArray(value) ? value.join(", ") : value]]),
        })));
      });
      request.on("error", reject);
      if (init?.body !== undefined && init.body !== null) request.write(String(init.body));
      request.end();
    });
  }) as typeof fetch;
}
