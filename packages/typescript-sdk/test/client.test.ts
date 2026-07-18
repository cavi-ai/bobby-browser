import assert from "node:assert/strict";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import test from "node:test";

import { BrowserRuntimeClient, RuntimeClientError, type CommandEnvelope } from "../src/index.js";

const TOKEN = "test-bearer-token";
const COMMAND_ID = "00000000-0000-4000-8000-000000000001";
const WORKFLOW_ID = "00000000-0000-4000-8000-000000000002";
const ATTEMPT_ID = "00000000-0000-4000-8000-000000000003";
const NEXT_ATTEMPT_ID = "00000000-0000-4000-8000-000000000004";
const SESSION_ID = "00000000-0000-4000-8000-000000000005";
const REFERENCE_ID = "00000000-0000-4000-8000-000000000006";
const CHECKPOINT_ID = "00000000-0000-4000-8000-000000000007";
const CORRELATION_ID = "00000000-0000-4000-8000-000000000008";

async function withServer(
  handler: (request: IncomingMessage, response: ServerResponse) => void | Promise<void>,
  run: (baseUrl: string) => Promise<void>,
): Promise<void> {
  const server = createServer(handler);
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("fixture server did not bind");
  try {
    await run(`http://127.0.0.1:${address.port}`);
  } finally {
    await new Promise<void>((resolve, reject) => server.close((error?: Error) => error ? reject(error) : resolve()));
  }
}

function writeJson(response: ServerResponse, status: number, body: unknown): void {
  response.writeHead(status, { "content-type": "application/json", "x-interface-version": "2026-07-17" });
  response.end(JSON.stringify(body));
}

function envelope(): CommandEnvelope {
  return {
    schemaVersion: 1,
    commandId: COMMAND_ID,
    workflowId: WORKFLOW_ID,
    attemptId: ATTEMPT_ID,
    sessionId: SESSION_ID,
    pageId: null,
    deadline: new Date(Date.now() + 10_000).toISOString(),
    command: { kind: "inspect", input: { selector: null, target: null, includeHtml: false } },
  };
}

function trackedAbortController(): { controller: AbortController; counts: () => { added: number; removed: number } } {
  const controller = new AbortController();
  const signal = controller.signal;
  const add = signal.addEventListener.bind(signal);
  const remove = signal.removeEventListener.bind(signal);
  let added = 0;
  let removed = 0;
  Object.defineProperty(signal, "addEventListener", { configurable: true, value(type: "abort", listener: EventListenerOrEventListenerObject, options?: boolean | AddEventListenerOptions) {
    if (type === "abort") added += 1;
    return add(type, listener, options);
  } });
  Object.defineProperty(signal, "removeEventListener", { configurable: true, value(type: "abort", listener: EventListenerOrEventListenerObject, options?: boolean | EventListenerOptions) {
    if (type === "abort") removed += 1;
    return remove(type, listener, options);
  } });
  return { controller, counts: () => ({ added, removed }) };
}

async function rejectsPromptly(promise: Promise<unknown>, validate: (error: unknown) => boolean): Promise<void> {
  const started = Date.now();
  await assert.rejects(promise, (error: unknown) => validate(error));
  assert.ok(Date.now() - started < 500, "operation did not reject promptly");
}

test("preserves reconciliation metadata without exposing the bearer token", async () => {
  await withServer((request, response) => {
    assert.equal(request.headers.authorization, `Bearer ${TOKEN}`);
    writeJson(response, 409, { error: {
      code: "idempotencyConflict", layer: "interface", message: "reconcile", correlationId: CORRELATION_ID,
      commandId: COMMAND_ID, retryable: false, retryAfterMs: null, reconciliationRequired: true, requiredCapability: null,
    } });
  }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    await assert.rejects(client.submit(envelope()), (error: unknown) => {
      assert.ok(error instanceof RuntimeClientError);
      const runtimeError = error as RuntimeClientError;
      assert.equal(runtimeError.code, "idempotencyConflict");
      assert.equal(runtimeError.reconciliationRequired, true);
      assert.equal(runtimeError.commandId, COMMAND_ID);
      assert.doesNotMatch(runtimeError.message, new RegExp(TOKEN));
      assert.doesNotMatch(String(runtimeError), new RegExp(TOKEN));
      return true;
    });
  });
});

test("does not retry a POST command after a transport failure", async () => {
  let requests = 0;
  await withServer((request) => {
    requests += 1;
    request.socket.destroy();
  }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    await assert.rejects(client.submit(envelope()), RuntimeClientError);
  });
  assert.equal(requests, 1);
});

test("retries safe event polling transport failures and ends on an event gap", async () => {
  let requests = 0;
  await withServer((request, response) => {
    requests += 1;
    if (requests === 1) {
      request.socket.destroy();
      return;
    }
    writeJson(response, 409, {
      error: { code: "invalidRequest", layer: "interface", message: "gap", correlationId: CORRELATION_ID, commandId: null, retryable: false, retryAfterMs: null, reconciliationRequired: false, requiredCapability: null },
      gap: { reason: "historyLost", earliestAvailable: 9 },
    });
  }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    await assert.rejects(async () => {
      for await (const _event of client.events(0, { maxTransportRetries: 1, retryDelayMs: 0 })) {
        // An EventGap must terminate before yielding an event.
      }
    }, (error: unknown) => error instanceof RuntimeClientError && error.eventGap?.earliestAvailable === 9);
  });
  assert.equal(requests, 2);
});

test("rejects a malformed structured error instead of assigning it typed metadata", async () => {
  await withServer((_request, response) => {
    writeJson(response, 409, { error: {
      code: "notAContractCode", layer: "interface", message: "bad", correlationId: CORRELATION_ID,
      commandId: null, retryable: false, retryAfterMs: null, reconciliationRequired: false, requiredCapability: null,
    } });
  }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    await assert.rejects(client.submit(envelope()), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol" && error.code === undefined);
  });
});

test("streams artifact bytes and rejects a digest mismatch at stream completion", async () => {
  const bytes = new TextEncoder().encode("artifact");
  const digest = "c7c5c1d70c5dec4416ab6158afd0b223ef40c29b1dc1f97ed9428b94d4cadb1c";
  await withServer((_request, response) => {
    response.writeHead(200, { "content-type": "application/octet-stream", "content-length": String(bytes.byteLength) });
    response.end(bytes);
  }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    const stream = await client.artifact({ referenceId: REFERENCE_ID, artifactId: COMMAND_ID, bytes: bytes.byteLength, sha256: digest, mediaType: "application/octet-stream" });
    const reader = stream.getReader();
    let received = "";
    while (true) {
      const next = await reader.read();
      if (next.done) break;
      received += new TextDecoder().decode(next.value);
    }
    assert.equal(received, "artifact");

    const bad = await client.artifact({ referenceId: REFERENCE_ID, artifactId: COMMAND_ID, bytes: bytes.byteLength, sha256: "00".repeat(32), mediaType: "application/octet-stream" });
    const badReader = bad.getReader();
    await assert.rejects(badReader.read(), /artifact digest/);
  });
});

test("rejects a command outcome whose HTTP status does not match its variant", async () => {
  await withServer((_request, response) => writeJson(response, 200, { status: "policyDenied", commandId: COMMAND_ID, error: { code: "policyDenied", message: "denied", layer: "workflow", retryable: false } }), async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    await assert.rejects(client.submit(envelope()), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol");
  });
});

test("enforces the broker status table for every command outcome variant", async () => {
  const cases = [
    [200, { status: "completed", commandId: COMMAND_ID, evidence: [] }],
    [503, { status: "retryableFailure", commandId: COMMAND_ID, error: { code: "internal", message: "x", layer: "workflow", retryable: true } }],
    [409, { status: "needsReconciliation", commandId: COMMAND_ID, error: { code: "internal", message: "x", layer: "workflow", retryable: false }, evidence: [] }],
    [403, { status: "policyDenied", commandId: COMMAND_ID, error: { code: "policyDenied", message: "x", layer: "workflow", retryable: false } }],
    [429, { status: "resourceExhausted", commandId: COMMAND_ID, error: { code: "resourceExhausted", message: "x", layer: "workflow", retryable: true }, retryAfterMs: 1 }],
    [422, { status: "failed", commandId: COMMAND_ID, error: { code: "invalidRequest", message: "x", layer: "workflow", retryable: false } }],
    [500, { status: "failed", commandId: COMMAND_ID, error: { code: "internal", message: "x", layer: "workflow", retryable: false } }],
    [200, { status: "restarted", commandId: COMMAND_ID, priorAttemptId: ATTEMPT_ID, attemptId: NEXT_ATTEMPT_ID, reason: "x" }],
  ] as const;
  for (const [status, outcome] of cases) {
    await withServer((_request, response) => writeJson(response, status, outcome), async (baseUrl) => {
      const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
      assert.equal((await client.submit(envelope())).status, outcome.status);
    });
    await withServer((_request, response) => writeJson(response, 418, outcome), async (baseUrl) => {
      const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
      await assert.rejects(client.submit(envelope()), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol");
    });
  }
});

test("enforces the broker status table for every recovery decision variant", async () => {
  const cases = [
    [200, { status: "resumed", checkpointId: CHECKPOINT_ID, attemptId: ATTEMPT_ID, evidence: [] }],
    [409, { status: "needsReconciliation", checkpointId: CHECKPOINT_ID, attemptId: ATTEMPT_ID, reason: "x", evidence: [] }],
    [200, { status: "restarted", checkpointId: CHECKPOINT_ID, lineage: { workflowId: WORKFLOW_ID, abandonedAttemptId: ATTEMPT_ID, attemptId: NEXT_ATTEMPT_ID, reason: "x" } }],
  ] as const;
  for (const [status, decision] of cases) {
    await withServer((_request, response) => writeJson(response, status, decision), async (baseUrl) => {
      const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
      assert.equal((await client.recover(WORKFLOW_ID)).status, decision.status);
    });
    await withServer((_request, response) => writeJson(response, 418, decision), async (baseUrl) => {
      const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
      await assert.rejects(client.recover(WORKFLOW_ID), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol");
    });
  }
});

test("rejects an over-limit artifact reference before issuing a request", async () => {
  let requests = 0;
  await withServer((_request, response) => { requests += 1; response.end(); }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN, maxArtifactBytes: 1 });
    await assert.rejects(client.artifact({ referenceId: REFERENCE_ID, artifactId: COMMAND_ID, bytes: 2, sha256: "00".repeat(32), mediaType: "application/octet-stream" }), RuntimeClientError);
  });
  assert.equal(requests, 0);
});

test("rejects invalid artifact UUIDs, IDs, digests, and numeric bounds before fetch", async () => {
  let requests = 0;
  const client = new BrowserRuntimeClient({
    baseUrl: "https://runtime.invalid",
    bearerToken: TOKEN,
    fetch: async () => { requests += 1; return new Response(); },
  });
  for (const bytes of [Number.NaN, Number.POSITIVE_INFINITY, -1, 0.5, Number.MAX_SAFE_INTEGER + 1]) {
    await assert.rejects(client.artifact({ referenceId: REFERENCE_ID, artifactId: COMMAND_ID, bytes, sha256: "00".repeat(32), mediaType: "application/octet-stream" }), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol");
  }
  for (const reference of [
    { referenceId: "not-a-uuid", artifactId: COMMAND_ID, bytes: 0, sha256: "00".repeat(32), mediaType: "application/octet-stream" },
    { referenceId: REFERENCE_ID, artifactId: "not/an/artifact", bytes: 0, sha256: "00".repeat(32), mediaType: "application/octet-stream" },
    { referenceId: REFERENCE_ID, artifactId: COMMAND_ID, bytes: 0, sha256: "AA".repeat(32), mediaType: "application/octet-stream" },
  ]) await assert.rejects(client.artifact(reference), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol");
  assert.equal(requests, 0);
});

test("requires exact artifact content length and compares media-type essence on both sides", async () => {
  const bytes = new TextEncoder().encode("artifact");
  const reference = { referenceId: REFERENCE_ID, artifactId: COMMAND_ID, bytes: bytes.byteLength, sha256: "c7c5c1d70c5dec4416ab6158afd0b223ef40c29b1dc1f97ed9428b94d4cadb1c", mediaType: "application/octet-stream; profile=binary" };
  const headerCases: Array<Record<string, string>> = [
    { "content-type": "application/octet-stream" },
    { "content-type": "application/octet-stream", "content-length": String(bytes.byteLength + 1) },
    { "content-type": "text/plain", "content-length": String(bytes.byteLength) },
  ];
  for (const headers of headerCases) {
    const client = new BrowserRuntimeClient({ baseUrl: "https://runtime.invalid", bearerToken: TOKEN, fetch: async () => new Response(bytes, { status: 200, headers }) });
    await assert.rejects(client.artifact(reference), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol");
  }

  const client = new BrowserRuntimeClient({ baseUrl: "https://runtime.invalid", bearerToken: TOKEN, fetch: async () => new Response(bytes, { status: 200, headers: { "content-type": "Application/Octet-Stream; charset=binary", "content-length": String(bytes.byteLength) } }) });
  const result = await new Response(await client.artifact(reference)).bytes();
  assert.deepEqual(result, bytes);
});

test("keeps the deadline active while a JSON body stalls after headers", async () => {
  await withServer((_request, response) => {
    response.writeHead(200, { "content-type": "application/json" });
    response.write("{");
  }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    await assert.rejects(client.runtimeInfo({ timeoutMs: 15 }), (error: unknown) => error instanceof RuntimeClientError && error.kind === "deadline");
  });
});

test("caller abort remains active through a stalled JSON body and releases its listener", async () => {
  const tracked = trackedAbortController();
  await withServer((_request, response) => {
    response.writeHead(200, { "content-type": "application/json" });
    response.write("{");
  }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    const operation = client.runtimeInfo({ signal: tracked.controller.signal, timeoutMs: 10_000 });
    setTimeout(() => tracked.controller.abort(), 15);
    await rejectsPromptly(operation, (error) => error instanceof RuntimeClientError && error.kind === "aborted");
  });
  assert.deepEqual(tracked.counts(), { added: 1, removed: 1 });
});

test("deadline during a delayed artifact chunk rejects before exposing bytes", async () => {
  const bytes = new TextEncoder().encode("artifact");
  await withServer((_request, response) => {
    response.writeHead(200, { "content-type": "application/octet-stream", "content-length": String(bytes.byteLength) });
    response.flushHeaders();
    setTimeout(() => response.end(bytes), 250);
  }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    const stream = await client.artifact({ referenceId: REFERENCE_ID, artifactId: COMMAND_ID, bytes: bytes.byteLength, sha256: "c7c5c1d70c5dec4416ab6158afd0b223ef40c29b1dc1f97ed9428b94d4cadb1c", mediaType: "application/octet-stream" }, { timeoutMs: 20 });
    let exposed = 0;
    await rejectsPromptly(stream.getReader().read().then((chunk) => { exposed += chunk.value?.byteLength ?? 0; return chunk; }), (error) => error instanceof RuntimeClientError && error.kind === "deadline");
    assert.equal(exposed, 0);
  });
});

test("caller abort during artifact body read rejects before exposing bytes and releases listeners", async () => {
  const tracked = trackedAbortController();
  const bytes = new TextEncoder().encode("artifact");
  await withServer((_request, response) => {
    response.writeHead(200, { "content-type": "application/octet-stream", "content-length": String(bytes.byteLength) });
    response.flushHeaders();
    setTimeout(() => response.end(bytes), 250);
  }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    const stream = await client.artifact({ referenceId: REFERENCE_ID, artifactId: COMMAND_ID, bytes: bytes.byteLength, sha256: "c7c5c1d70c5dec4416ab6158afd0b223ef40c29b1dc1f97ed9428b94d4cadb1c", mediaType: "application/octet-stream" }, { signal: tracked.controller.signal, timeoutMs: 10_000 });
    let exposed = 0;
    const read = stream.getReader().read().then((chunk) => { exposed += chunk.value?.byteLength ?? 0; return chunk; });
    setTimeout(() => tracked.controller.abort(), 15);
    await rejectsPromptly(read, (error) => error instanceof RuntimeClientError && error.kind === "aborted");
    assert.equal(exposed, 0);
  });
  assert.deepEqual(tracked.counts(), { added: 1, removed: 1 });
});

test("caller abort during artifact hashing rejects before release", async () => {
  const tracked = trackedAbortController();
  const bytes = new TextEncoder().encode("artifact");
  const originalDigest = crypto.subtle.digest.bind(crypto.subtle);
  let hashing!: () => void;
  const hashingStarted = new Promise<void>((resolve) => { hashing = resolve; });
  let release!: () => void;
  const hashGate = new Promise<void>((resolve) => { release = resolve; });
  Object.defineProperty(crypto.subtle, "digest", { configurable: true, value: async (...args: Parameters<SubtleCrypto["digest"]>) => {
    hashing();
    await hashGate;
    return originalDigest(...args);
  } });
  try {
    const client = new BrowserRuntimeClient({
      baseUrl: "https://runtime.invalid",
      bearerToken: TOKEN,
      fetch: async () => new Response(bytes, { status: 200, headers: { "content-type": "application/octet-stream", "content-length": String(bytes.byteLength) } }),
    });
    const stream = await client.artifact({ referenceId: REFERENCE_ID, artifactId: COMMAND_ID, bytes: bytes.byteLength, sha256: "c7c5c1d70c5dec4416ab6158afd0b223ef40c29b1dc1f97ed9428b94d4cadb1c", mediaType: "application/octet-stream" }, { signal: tracked.controller.signal, timeoutMs: 10_000 });
    const read = stream.getReader().read();
    await hashingStarted;
    tracked.controller.abort();
    release();
    await rejectsPromptly(read, (error) => error instanceof RuntimeClientError && error.kind === "aborted");
  } finally {
    Reflect.deleteProperty(crypto.subtle, "digest");
  }
  assert.deepEqual(tracked.counts(), { added: 1, removed: 1 });
});

test("deadline and caller abort interrupt event retry backoff without leaked listeners", async () => {
  const transportFailure: typeof fetch = async () => { throw new Error("transport"); };
  const deadlineClient = new BrowserRuntimeClient({ baseUrl: "https://runtime.invalid", bearerToken: TOKEN, fetch: transportFailure });
  await rejectsPromptly(deadlineClient.events(0, { timeoutMs: 20, retryDelayMs: 10_000, maxTransportRetries: 1 })[Symbol.asyncIterator]().next(), (error) => error instanceof RuntimeClientError && error.kind === "deadline");

  const tracked = trackedAbortController();
  const callerClient = new BrowserRuntimeClient({ baseUrl: "https://runtime.invalid", bearerToken: TOKEN, fetch: transportFailure });
  const next = callerClient.events(0, { signal: tracked.controller.signal, timeoutMs: 10_000, retryDelayMs: 10_000, maxTransportRetries: 1 })[Symbol.asyncIterator]().next();
  setTimeout(() => tracked.controller.abort(), 15);
  await rejectsPromptly(next, (error) => error instanceof RuntimeClientError && error.kind === "aborted");
  assert.deepEqual(tracked.counts(), { added: 1, removed: 1 });
});

test("preserves the structured InterfaceError metadata matrix for recover and artifact", async () => {
  const cases = [
    { status: 401, code: "authenticationFailed", retryable: false, retryAfterMs: null, reconciliationRequired: false },
    { status: 403, code: "missingCapability", retryable: false, retryAfterMs: null, reconciliationRequired: false },
    { status: 409, code: "idempotencyConflict", retryable: false, retryAfterMs: null, reconciliationRequired: true },
    { status: 429, code: "resourceExhausted", retryable: true, retryAfterMs: 250, reconciliationRequired: false },
  ] as const;
  for (const fixture of cases) await withServer((request, response) => {
    const recover = request.url?.startsWith("/v1/recovery") ?? false;
    writeJson(response, fixture.status, { error: {
      code: fixture.code,
      layer: "interface",
      message: `failure ${TOKEN}`,
      correlationId: CORRELATION_ID,
      commandId: fixture.status === 409 ? COMMAND_ID : null,
      retryable: fixture.retryable,
      retryAfterMs: fixture.retryAfterMs,
      reconciliationRequired: fixture.reconciliationRequired,
      requiredCapability: fixture.status === 403 ? (recover ? "recovery:write" : "artifact:read") : null,
    } });
  }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    const operations = [
      ["recovery:write", () => client.recover(WORKFLOW_ID)],
      ["artifact:read", () => client.artifact({ referenceId: REFERENCE_ID, artifactId: COMMAND_ID, bytes: 0, sha256: "00".repeat(32), mediaType: "application/octet-stream" })],
    ] as const;
    for (const [capability, operation] of operations) await assert.rejects(operation(), (caught: unknown) => {
      assert.ok(caught instanceof RuntimeClientError);
      assert.equal(caught.status, fixture.status);
      assert.equal(caught.code, fixture.code);
      assert.equal(caught.correlationId, CORRELATION_ID);
      assert.equal(caught.commandId, fixture.status === 409 ? COMMAND_ID : null);
      assert.equal(caught.retryable, fixture.retryable);
      assert.equal(caught.retryAfterMs, fixture.retryAfterMs);
      assert.equal(caught.reconciliationRequired, fixture.reconciliationRequired);
      assert.equal(caught.requiredCapability, fixture.status === 403 ? capability : null);
      assert.doesNotMatch(caught.message, new RegExp(TOKEN));
      return true;
    });
  });
});

test("client rejects malformed nested fixtures from every JSON response family", async () => {
  const time = "2026-07-17T12:34:56Z";
  const malformedCheckpoint = {
    schemaVersion: 1,
    checkpointId: CHECKPOINT_ID,
    workflowId: WORKFLOW_ID,
    attemptId: ATTEMPT_ID,
    sessionId: SESSION_ID,
    pageId: COMMAND_ID,
    restartUrl: "https://example.test/",
    currentUrl: "https://example.test/",
    cursor: null,
    boundaryCommandId: null,
    recoveryClass: "replayable",
    invariants: [],
    replayableInputs: [],
    evidence: [],
    recoveryHistory: [{ recordedAt: "not-a-time", decision: { status: "resumed", checkpointId: CHECKPOINT_ID, attemptId: ATTEMPT_ID, evidence: [] } }],
    createdAt: time,
  };
  const validError = { code: "invalidRequest", layer: "interface", message: "gap", correlationId: CORRELATION_ID, commandId: null, retryable: false, retryAfterMs: null, reconciliationRequired: false, requiredCapability: null };
  const cases: Array<[string, number, unknown, (client: BrowserRuntimeClient) => Promise<unknown>]> = [
    ["RuntimeInfo", 200, { version: "1", capabilities: [], active_sessions: 0.5, queued_jobs: 0, uptime_ms: 0 }, (client) => client.runtimeInfo()],
    ["SessionState", 200, { id: SESSION_ID, profile: "default", proxy: null, page_ids: ["bad"], created_at: time, last_used_at: time }, (client) => client.createSession({ profile: "default", proxy: null })],
    ["PageState", 200, { id: COMMAND_ID, session_id: SESSION_ID, url: null, mode: "document", ready_state: "complete", pending_requests: 0 }, (client) => client.openPage({ session_id: SESSION_ID })],
    ["CommandOutcome", 200, { status: "completed", commandId: COMMAND_ID, evidence: [{ kind: "screenshot", artifactId: "a", mediaType: "image/png", width: 1, height: 1, bytes: 1, sha256: "BAD" }] }, (client) => client.submit(envelope())],
    ["WorkflowCheckpoint", 200, malformedCheckpoint, (client) => client.checkpoint({ checkpoint: malformedCheckpoint as never })],
    ["RecoveryDecision", 200, { status: "restarted", checkpointId: CHECKPOINT_ID, lineage: { workflowId: WORKFLOW_ID, abandonedAttemptId: "bad", attemptId: ATTEMPT_ID, reason: "x" } }, (client) => client.recover(WORKFLOW_ID)],
    ["EventBatch", 200, { events: [{ cursor: Number.MAX_SAFE_INTEGER + 1, kind: "x", payload: null }], latestAvailable: 0 }, async (client) => client.events(0)[Symbol.asyncIterator]().next()],
    ["EventGap", 409, { error: validError, gap: { reason: "historyLost", earliestAvailable: -1 } }, async (client) => client.events(0)[Symbol.asyncIterator]().next()],
  ];
  for (const [family, status, payload, operation] of cases) {
    const client = new BrowserRuntimeClient({
      baseUrl: "https://runtime.invalid",
      bearerToken: TOKEN,
      fetch: async () => new Response(JSON.stringify(payload), { status, headers: { "content-type": "application/json" } }),
    });
    await assert.rejects(operation(client), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol", family);
  }
});
