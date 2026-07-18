import assert from "node:assert/strict";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import test from "node:test";

import { BrowserRuntimeClient, RuntimeClientError, type CommandEnvelope } from "../src/index.js";

const TOKEN = "test-bearer-token";

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
    commandId: "command-id",
    workflowId: "workflow-id",
    attemptId: "attempt-id",
    sessionId: "session-id",
    pageId: null,
    deadline: new Date(Date.now() + 10_000).toISOString(),
    command: { kind: "inspect", input: { selector: null, target: null, includeHtml: false } },
  };
}

test("preserves reconciliation metadata without exposing the bearer token", async () => {
  await withServer((request, response) => {
    assert.equal(request.headers.authorization, `Bearer ${TOKEN}`);
    writeJson(response, 409, { error: {
      code: "idempotencyConflict", layer: "interface", message: "reconcile", correlationId: "correlation-id",
      commandId: "command-id", retryable: false, retryAfterMs: null, reconciliationRequired: true, requiredCapability: null,
    } });
  }, async (baseUrl) => {
    const client = new BrowserRuntimeClient({ baseUrl, bearerToken: TOKEN });
    await assert.rejects(client.submit(envelope()), (error: unknown) => {
      assert.ok(error instanceof RuntimeClientError);
      const runtimeError = error as RuntimeClientError;
      assert.equal(runtimeError.code, "idempotencyConflict");
      assert.equal(runtimeError.reconciliationRequired, true);
      assert.equal(runtimeError.commandId, "command-id");
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
      error: { code: "invalidRequest", layer: "interface", message: "gap", correlationId: "correlation-id", commandId: null, retryable: false, retryAfterMs: null, reconciliationRequired: false, requiredCapability: null },
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
      code: "notAContractCode", layer: "interface", message: "bad", correlationId: "correlation-id",
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
    const stream = await client.artifact({ referenceId: "reference-id", artifactId: "artifact-id", bytes: bytes.byteLength, sha256: digest, mediaType: "application/octet-stream" });
    const reader = stream.getReader();
    let received = "";
    while (true) {
      const next = await reader.read();
      if (next.done) break;
      received += new TextDecoder().decode(next.value);
    }
    assert.equal(received, "artifact");

    const bad = await client.artifact({ referenceId: "reference-id", artifactId: "artifact-id", bytes: bytes.byteLength, sha256: "00", mediaType: "application/octet-stream" });
    const badReader = bad.getReader();
    await assert.rejects(badReader.read(), /artifact digest/);
  });
});
