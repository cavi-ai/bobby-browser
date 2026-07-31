import assert from "node:assert/strict";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import test from "node:test";
import { inspect } from "node:util";

import { BrowserRuntimeClient, INTERFACE_VERSION, RuntimeClientError, type Capability, type CommandEnvelope, type EventOptions, type InterfaceErrorCode } from "../src/index.js";

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
  response.writeHead(status, { "content-type": "application/json", "x-interface-version": INTERFACE_VERSION });
  response.end(JSON.stringify(body));
}

function envelope(): CommandEnvelope {
  return {
    schemaVersion: 2,
    commandId: COMMAND_ID,
    workflowId: WORKFLOW_ID,
    attemptId: ATTEMPT_ID,
    sessionId: SESSION_ID,
    pageId: null,
    deadline: new Date(Date.now() + 10_000).toISOString(),
    command: {
      kind: "primitive",
      input: { kind: "inspect", input: { selector: null, target: null, includeHtml: false } },
    },
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
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<never>((_resolve, reject) => { timer = setTimeout(() => reject(new Error("operation did not reject promptly")), 200); });
  try { await assert.rejects(Promise.race([promise, timeout]), (error: unknown) => validate(error)); }
  finally { if (timer !== undefined) clearTimeout(timer); }
  assert.ok(Date.now() - started < 200, "operation did not reject promptly");
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

test("listSessions returns the broker session array", async () => {
  const time = new Date().toISOString();
  const session = { id: SESSION_ID, profile: "default", proxy: null, page_ids: [], created_at: time, last_used_at: time, execution_policy: { javascriptEvaluation: false, visionAssist: false } };
  await withServer((request, response) => {
    assert.equal(request.method, "GET");
    assert.equal(request.url, "/v1/sessions");
    writeJson(response, 200, [session]);
  }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    const sessions = await client.listSessions();
    assert.equal(sessions.length, 1);
    assert.equal(sessions[0]?.id, SESSION_ID);
  });
});

test("deleteSession sends DELETE and accepts 204", async () => {
  await withServer((request, response) => {
    assert.equal(request.method, "DELETE");
    assert.equal(request.url, `/v1/sessions/${SESSION_ID}`);
    response.writeHead(204);
    response.end();
  }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    await client.deleteSession(SESSION_ID);
    await assert.rejects(client.deleteSession("not-a-uuid"), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol");
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
      error: { code: "invalidRequest", layer: "interface", message: "event history has a cursor gap", correlationId: CORRELATION_ID, commandId: null, retryable: false, retryAfterMs: null, reconciliationRequired: false, requiredCapability: null },
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
    await assert.rejects(badReader.read(), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol" && error.message === "Artifact verification failed");
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
    [200, { status: "restarted", commandId: COMMAND_ID, priorAttemptId: ATTEMPT_ID, attemptId: NEXT_ATTEMPT_ID, reason: "x", evidence: [] }],
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
    [200, { status: "restarted", checkpointId: CHECKPOINT_ID, lineage: { workflowId: WORKFLOW_ID, abandonedAttemptId: ATTEMPT_ID, attemptId: NEXT_ATTEMPT_ID, reason: "x" }, evidence: [] }],
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

test("caller abort and deadline reject while artifact hashing remains blocked", async () => {
  const bytes = new TextEncoder().encode("artifact");
  for (const kind of ["aborted", "deadline"] as const) {
    const tracked = trackedAbortController();
    let hashing!: () => void;
    const hashingStarted = new Promise<void>((resolve) => { hashing = resolve; });
    let settleDigest!: (value: ArrayBuffer) => void;
    let rejectDigest!: (error: Error) => void;
    const hashGate = new Promise<ArrayBuffer>((resolve, reject) => { settleDigest = resolve; rejectDigest = reject; });
    Object.defineProperty(crypto.subtle, "digest", { configurable: true, value: async () => { hashing(); return hashGate; } });
    let exposed = 0;
    let unhandled = 0;
    const onUnhandled = () => { unhandled += 1; };
    process.on("unhandledRejection", onUnhandled);
    try {
      const client = new BrowserRuntimeClient({
        baseUrl: "https://runtime.invalid",
        bearerToken: TOKEN,
        fetch: async () => new Response(bytes, { status: 200, headers: { "content-type": "application/octet-stream", "content-length": String(bytes.byteLength) } }),
      });
      const stream = await client.artifact(
        { referenceId: REFERENCE_ID, artifactId: COMMAND_ID, bytes: bytes.byteLength, sha256: "c7c5c1d70c5dec4416ab6158afd0b223ef40c29b1dc1f97ed9428b94d4cadb1c", mediaType: "application/octet-stream" },
        kind === "aborted" ? { signal: tracked.controller.signal, timeoutMs: 10_000 } : { timeoutMs: 20 },
      );
      const read = stream.getReader().read().then((chunk) => { exposed += chunk.value?.byteLength ?? 0; return chunk; });
      await hashingStarted;
      if (kind === "aborted") tracked.controller.abort();
      await rejectsPromptly(read, (error) => error instanceof RuntimeClientError && error.kind === kind);
      assert.equal(exposed, 0);
      if (kind === "aborted") rejectDigest(new Error(`late digest ${TOKEN}`));
      else settleDigest(new ArrayBuffer(32));
      await new Promise<void>((resolve) => setImmediate(resolve));
      assert.equal(unhandled, 0);
    } finally {
      settleDigest(new ArrayBuffer(32));
      process.removeListener("unhandledRejection", onUnhandled);
      Reflect.deleteProperty(crypto.subtle, "digest");
    }
    if (kind === "aborted") assert.deepEqual(tracked.counts(), { added: 1, removed: 1 });
  }
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

test("rejects out-of-context and oversized event batches without rewinding the iterator", async () => {
  const fixtures = [
    [{ events: [{ cursor: 1, kind: "old", payload: null }], latestAvailable: 10 }, { limit: 100 }],
    [{ events: [{ cursor: 11, kind: "one", payload: null }, { cursor: 12, kind: "two", payload: null }], latestAvailable: 12 }, { limit: 1 }],
  ] as const;
  for (const [payload, options] of fixtures) {
    let requests = 0;
    const client = new BrowserRuntimeClient({
      baseUrl: "https://runtime.invalid",
      bearerToken: TOKEN,
      fetch: async () => { requests += 1; return new Response(JSON.stringify(payload), { status: 200, headers: { "content-type": "application/json" } }); },
    });
    await assert.rejects(client.events(10, options)[Symbol.asyncIterator]().next(), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol");
    assert.equal(requests, 1);
  }
});

test("rejects invalid SDK resource and event options before network access", async () => {
  const invalidArtifactMaximums = [0, -1, 0.5, Number.NaN, Number.POSITIVE_INFINITY, 256 * 1024 * 1024 + 1];
  for (const maxArtifactBytes of invalidArtifactMaximums) {
    assert.throws(() => new BrowserRuntimeClient({ baseUrl: "https://runtime.invalid", bearerToken: TOKEN, maxArtifactBytes }), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol");
  }

  let requests = 0;
  const client = new BrowserRuntimeClient({ baseUrl: "https://runtime.invalid", bearerToken: TOKEN, fetch: async () => { requests += 1; throw new Error("must not fetch"); } });
  const invalid: Array<{ cursor: number; options: EventOptions }> = [
    { cursor: -1, options: {} },
    { cursor: 0.5, options: {} },
    { cursor: Number.NaN, options: {} },
    { cursor: Number.POSITIVE_INFINITY, options: {} },
    { cursor: 0, options: { limit: 0 } },
    { cursor: 0, options: { limit: 257 } },
    { cursor: 0, options: { limit: 1.5 } },
    { cursor: 0, options: { limit: Number.NaN } },
    { cursor: 0, options: { maxTransportRetries: -1 } },
    { cursor: 0, options: { maxTransportRetries: 11 } },
    { cursor: 0, options: { maxTransportRetries: 0.5 } },
    { cursor: 0, options: { maxTransportRetries: Number.POSITIVE_INFINITY } },
    { cursor: 0, options: { retryDelayMs: -1 } },
    { cursor: 0, options: { retryDelayMs: 60_001 } },
    { cursor: 0, options: { retryDelayMs: 0.5 } },
    { cursor: 0, options: { retryDelayMs: Number.NaN } },
  ];
  for (const fixture of invalid) {
    await assert.rejects(client.events(fixture.cursor, fixture.options)[Symbol.asyncIterator]().next(), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol");
  }
  assert.equal(requests, 0);
});

test("sanitizes artifact body and digest failures into typed protocol errors", async () => {
  const source = new ReadableStream<Uint8Array>({ pull(controller) { controller.error(new Error(`body source leaked ${TOKEN}`)); } });
  const client = new BrowserRuntimeClient({
    baseUrl: "https://runtime.invalid",
    bearerToken: TOKEN,
    fetch: async () => new Response(source, { status: 200, headers: { "content-type": "application/octet-stream", "content-length": "8" } }),
  });
  const stream = await client.artifact({ referenceId: REFERENCE_ID, artifactId: COMMAND_ID, bytes: 8, sha256: "00".repeat(32), mediaType: "application/octet-stream" });
  await assert.rejects(stream.getReader().read(), (error: unknown) => {
    assert.ok(error instanceof RuntimeClientError);
    assert.equal(error.kind, "protocol");
    const surfaces = [String(error), error.message, error.stack ?? "", inspect(error, { showHidden: true }), JSON.stringify(error), JSON.stringify(Object.fromEntries(Object.entries(error)))];
    for (const surface of surfaces) assert.doesNotMatch(surface, new RegExp(TOKEN));
    assert.equal("cause" in error, false);
    return true;
  });
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

test("enforces the complete InterfaceError HTTP status table and rejects error envelopes at 200", async () => {
  const cases: Array<[InterfaceErrorCode, number]> = [
    ["authenticationFailed", 401], ["tokenExpired", 401],
    ["missingCapability", 403], ["malformedScope", 403],
    ["artifactDenied", 404], ["notFound", 404],
    ["deadlineExceeded", 408], ["idempotencyConflict", 409],
    ["resourceExhausted", 429], ["internal", 500],
    ["invalidRequest", 422], ["unsupportedInterfaceVersion", 422],
    ["invalidIdempotencyKey", 422], ["unsupportedOperation", 422],
  ];
  const payload = (code: InterfaceErrorCode) => ({ error: { code, layer: "interface", message: "x", correlationId: CORRELATION_ID, commandId: null, retryable: false, retryAfterMs: null, reconciliationRequired: false, requiredCapability: null } });
  for (const [code, status] of cases) {
    const accepted = new BrowserRuntimeClient({ baseUrl: "https://runtime.invalid", bearerToken: TOKEN, fetch: async () => new Response(JSON.stringify(payload(code)), { status, headers: { "content-type": "application/json" } }) });
    await assert.rejects(accepted.runtimeInfo(), (error: unknown) => error instanceof RuntimeClientError && error.kind === "http" && error.code === code && error.status === status);
    const wrong = new BrowserRuntimeClient({ baseUrl: "https://runtime.invalid", bearerToken: TOKEN, fetch: async () => new Response(JSON.stringify(payload(code)), { status: 200, headers: { "content-type": "application/json" } }) });
    await assert.rejects(wrong.runtimeInfo(), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol" && error.code === undefined);
  }
  const extraEnvelope = new BrowserRuntimeClient({ baseUrl: "https://runtime.invalid", bearerToken: TOKEN, fetch: async () => new Response(JSON.stringify({ ...payload("internal"), unexpected: true }), { status: 500, headers: { "content-type": "application/json" } }) });
  await assert.rejects(extraEnvelope.runtimeInfo(), (error: unknown) => error instanceof RuntimeClientError && error.kind === "protocol");
});

test("matches broker status precedence for reconciliation and invalidRequest body limits", async () => {
  const error = (code: InterfaceErrorCode, reconciliationRequired = false) => ({
    error: {
      code,
      layer: "interface",
      message: "x",
      correlationId: CORRELATION_ID,
      commandId: null,
      retryable: false,
      retryAfterMs: null,
      reconciliationRequired,
      requiredCapability: null,
    },
  });
  const cases: Array<{ code: InterfaceErrorCode; reconciliationRequired: boolean; accepted: number[]; rejected: number[] }> = [
    { code: "authenticationFailed", reconciliationRequired: true, accepted: [409], rejected: [401] },
    { code: "internal", reconciliationRequired: true, accepted: [409], rejected: [500] },
    { code: "invalidRequest", reconciliationRequired: true, accepted: [409], rejected: [413, 422] },
    { code: "invalidRequest", reconciliationRequired: false, accepted: [413, 422], rejected: [409, 500] },
  ];
  for (const fixture of cases) {
    for (const status of fixture.accepted) {
      const client = new BrowserRuntimeClient({
        baseUrl: "https://runtime.invalid",
        bearerToken: TOKEN,
        fetch: async () => new Response(JSON.stringify(error(fixture.code, fixture.reconciliationRequired)), { status, headers: { "content-type": "application/json" } }),
      });
      await assert.rejects(client.runtimeInfo(), (caught: unknown) => caught instanceof RuntimeClientError && caught.kind === "http" && caught.code === fixture.code && caught.status === status && caught.reconciliationRequired === fixture.reconciliationRequired);
    }
    for (const status of fixture.rejected) {
      const client = new BrowserRuntimeClient({
        baseUrl: "https://runtime.invalid",
        bearerToken: TOKEN,
        fetch: async () => new Response(JSON.stringify(error(fixture.code, fixture.reconciliationRequired)), { status, headers: { "content-type": "application/json" } }),
      });
      await assert.rejects(client.runtimeInfo(), (caught: unknown) => caught instanceof RuntimeClientError && caught.kind === "protocol" && caught.code === undefined);
    }
  }
});

test("accepts only the broker EventGap InterfaceError envelope at 409", async () => {
  const brokerError = {
    code: "invalidRequest",
    layer: "interface",
    message: "event history has a cursor gap",
    correlationId: CORRELATION_ID,
    commandId: null,
    retryable: false,
    retryAfterMs: null,
    reconciliationRequired: false,
    requiredCapability: null,
  } as const;
  const gap = { reason: "historyLost", earliestAvailable: 9 } as const;
  const accepted = new BrowserRuntimeClient({
    baseUrl: "https://runtime.invalid",
    bearerToken: TOKEN,
    fetch: async () => new Response(JSON.stringify({ error: brokerError, gap }), { status: 409, headers: { "content-type": "application/json" } }),
  });
  await assert.rejects(accepted.events(0)[Symbol.asyncIterator]().next(), (caught: unknown) => caught instanceof RuntimeClientError && caught.kind === "http" && caught.code === "invalidRequest" && caught.eventGap?.earliestAvailable === 9);

  const rejected = [
    { ...brokerError, code: "idempotencyConflict" },
    { ...brokerError, layer: "browser" },
    { ...brokerError, message: "gap" },
    { ...brokerError, commandId: COMMAND_ID },
    { ...brokerError, retryable: true, retryAfterMs: 1 },
    { ...brokerError, reconciliationRequired: true },
    { ...brokerError, requiredCapability: "event:read" },
  ];
  for (const eventGapError of rejected) {
    const client = new BrowserRuntimeClient({
      baseUrl: "https://runtime.invalid",
      bearerToken: TOKEN,
      fetch: async () => new Response(JSON.stringify({ error: eventGapError, gap }), { status: 409, headers: { "content-type": "application/json" } }),
    });
    await assert.rejects(client.events(0)[Symbol.asyncIterator]().next(), (caught: unknown) => caught instanceof RuntimeClientError && caught.kind === "protocol" && caught.code === undefined);
  }
});

test("redacts the bearer token from all RuntimeClientError metadata and serialization surfaces", async () => {
  const scenarios = [
    { token: CORRELATION_ID, status: 409, error: { code: "idempotencyConflict", message: `secret ${CORRELATION_ID}`, correlationId: CORRELATION_ID, commandId: CORRELATION_ID, requiredCapability: null } },
    { token: "read", status: 403, error: { code: "missingCapability", message: "read denied", correlationId: CORRELATION_ID, commandId: null, requiredCapability: "artifact:read" } },
    { token: "internal", status: 500, error: { code: "internal", message: "internal failure", correlationId: CORRELATION_ID, commandId: null, requiredCapability: null } },
    { token: "http", status: 500, error: { code: "internal", message: "http failure", correlationId: CORRELATION_ID, commandId: null, requiredCapability: null } },
  ] as const;
  for (const scenario of scenarios) {
    const body = { error: { ...scenario.error, layer: "interface", retryable: false, retryAfterMs: null, reconciliationRequired: scenario.status === 409 } };
    const client = new BrowserRuntimeClient({ baseUrl: "https://runtime.invalid", bearerToken: scenario.token, fetch: async () => new Response(JSON.stringify(body), { status: scenario.status, headers: { "content-type": "application/json" } }) });
    await assert.rejects(client.runtimeInfo(), (error: unknown) => {
      assert.ok(error instanceof RuntimeClientError);
      const surfaces = [String(error), error.message, error.stack ?? "", inspect(error, { showHidden: true }), JSON.stringify(error), JSON.stringify(Object.fromEntries(Object.entries(error)))];
      for (const surface of surfaces) assert.equal(surface.includes(scenario.token), false, surface);
      return true;
    });
  }
});

test("RuntimeClientError metadata exposes exact contract types", () => {
  const error = new RuntimeClientError({ kind: "protocol", message: "x" });
  const code: InterfaceErrorCode | undefined = error.code;
  const requiredCapability: Capability | null | undefined = error.requiredCapability;
  assert.equal(code, undefined);
  assert.equal(requiredCapability, undefined);
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
    recoveryReceipts: [],
    createdAt: time,
  };
  const validError = { code: "invalidRequest", layer: "interface", message: "event history has a cursor gap", correlationId: CORRELATION_ID, commandId: null, retryable: false, retryAfterMs: null, reconciliationRequired: false, requiredCapability: null };
  const cases: Array<[string, number, unknown, (client: BrowserRuntimeClient) => Promise<unknown>]> = [
    ["RuntimeInfo", 200, { version: "1", capabilities: [], active_sessions: 0.5, queued_jobs: 0, uptime_ms: 0 }, (client) => client.runtimeInfo()],
    ["SessionState", 200, { id: SESSION_ID, profile: "default", proxy: null, page_ids: ["bad"], created_at: time, last_used_at: time, execution_policy: { javascriptEvaluation: false, visionAssist: false } }, (client) => client.createSession({ profile: "default", proxy: null })],
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
