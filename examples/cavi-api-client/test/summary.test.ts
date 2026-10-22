import assert from "node:assert/strict";
import { once } from "node:events";
import { spawn } from "node:child_process";
import { createServer, type IncomingMessage } from "node:http";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { createApiClient, createRuntimeProviderRegistry, type CapabilityClient } from "@cavi-ai/api-client";
import { createClaudeProviderModule } from "@cavi-ai/api-client/providers/claude";
import { BrowserRuntimeClient, type AccessibilityNode } from "@cavi-ai/bobby-browser";
import { summarizePage } from "../src/summarize.js";

const sessionId = "00000000-0000-4000-8000-000000000001";
const pageId = "00000000-0000-4000-8000-000000000002";

async function json(request: IncomingMessage): Promise<Record<string, unknown>> {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  return JSON.parse(Buffer.concat(chunks).toString());
}

function browser(nodes: AccessibilityNode[], options: { status?: string; pageId?: string; truncated?: boolean } = {}) {
  return new BrowserRuntimeClient({
    baseUrl: "http://bobby.invalid",
    bearerToken: "browser-secret",
    fetch: async (_url, init) => {
      const envelope = JSON.parse(String(init?.body));
      assert.equal(envelope.command.input.kind, "accessibilitySnapshot");
      return Response.json({ status: options.status ?? "completed", commandId: envelope.commandId,
        evidence: [{ kind: "accessibilitySnapshot", pageId: options.pageId ?? pageId, nodes,
          truncated: options.truncated ?? false }] });
    },
  });
}

const completed: CapabilityClient["streamRun"] = async (_body, handlers) => {
  handlers.onEvent({ event: "message.delta", runId: "run-1", delta: "draft" });
  handlers.onEvent({ event: "run.completed", runId: "run-1", output: "A page about testing." });
  return { ok: true, data: { runId: "run-1", outcome: "completed" }, source: "live" };
};

test("published Cavi client streams a summary of Bobby evidence over HTTP", async () => {
  const requests: string[] = [];
  const server = createServer(async (request, response) => {
    requests.push(request.url!);
    const body = await json(request);
    if (request.url === "/v1/commands") {
      assert.equal(request.headers.authorization, "Bearer browser-secret");
      assert.ok(request.headers["x-deadline"]);
      assert.equal((body.command as { input: { kind: string } }).input.kind, "accessibilitySnapshot");
      response.setHeader("content-type", "application/json");
      response.end(JSON.stringify({ status: "completed", commandId: body.commandId, evidence: [{
        kind: "accessibilitySnapshot", pageId, truncated: false,
        nodes: [{ role: "heading", name: "Testing", value: "private-value", description: "private-description", url: "https://private.invalid" },
          { role: "textbox", name: "private-control", value: "private-form-value" }],
      }] }));
    } else if (request.url === "/v1/messages") {
      assert.equal(request.headers["x-api-key"], "model-secret");
      const prompt = JSON.stringify(body);
      assert.match(prompt, /Testing/);
      assert.doesNotMatch(prompt, /private-|browser-secret|model-secret|00000000/);
      response.setHeader("content-type", "text/event-stream");
      for (const event of [
        { type: "message_start", message: { id: "msg-1", role: "assistant", model: "test-model", content: [], usage: { input_tokens: 1, output_tokens: 0 } } },
        { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "A page about testing." } },
        { type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 5 } },
        { type: "message_stop" },
      ]) response.write(`event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`);
      response.end();
    } else { response.writeHead(404).end(); }
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  const baseUrl = `http://127.0.0.1:${address.port}`;
  const model = createApiClient("claude", { registry: createRuntimeProviderRegistry({ modules: [
    createClaudeProviderModule({ apiKey: "model-secret", defaultModel: "test-model", baseUrl }),
  ] }) });
  try {
    const result = await summarizePage({ browser: new BrowserRuntimeClient({ baseUrl, bearerToken: "browser-secret" }),
      model, sessionId, pageId });
    assert.deepEqual(result, { text: "A page about testing.", truncated: false });
    assert.deepEqual(requests, ["/v1/commands", "/v1/messages"]);
  } finally {
    await model.dispose();
    server.closeAllConnections();
    await new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
  }
});

test("CLI creates and deletes its session on success and provider failure", async () => {
  for (const fail of [false, true]) {
    const requests: string[] = [];
    const server = createServer(async (request, response) => {
      requests.push(`${request.method} ${request.url}`);
      response.setHeader("content-type", "application/json");
      const body = request.method === "POST" ? await json(request) : {};
      if (request.url === "/v1/sessions") {
        response.end(JSON.stringify({ id: sessionId, profile: "default", proxy: null, page_ids: [],
          created_at: new Date().toISOString(), last_used_at: new Date().toISOString(),
          execution_policy: { javascriptEvaluation: false, visionAssist: false, fingerprint: false, humanize: false } }));
      } else if (request.url === "/v1/pages") {
        response.end(JSON.stringify({ id: pageId, session_id: sessionId,
          url: null, mode: "Document", ready_state: "complete", pending_requests: 0 }));
      } else if (request.url === "/v1/commands") {
        const kind = (body.command as { input: { kind: string } }).input.kind;
        assert.ok(["navigate", "accessibilitySnapshot"].includes(kind));
        response.end(JSON.stringify({ status: "completed", commandId: body.commandId,
          evidence: kind === "navigate" ? [] : [{ kind: "accessibilitySnapshot", pageId, truncated: false,
            nodes: [{ role: "heading", name: "Testing" }] }] }));
      } else if (request.url === "/v1/messages") {
        if (fail) response.writeHead(401).end(JSON.stringify({ error: { type: "authentication_error", message: "private-diagnostic" } }));
        else {
          response.setHeader("content-type", "text/event-stream");
          response.end('event: message_start\ndata: {"type":"message_start","message":{"id":"msg-1"}}\n\nevent: content_block_delta\ndata: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"A summary."}}\n\nevent: message_stop\ndata: {"type":"message_stop"}\n\n');
        }
      } else if (request.method === "DELETE" && request.url === `/v1/sessions/${sessionId}`) {
        response.writeHead(204).end();
      } else response.writeHead(404).end();
    });
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    assert.ok(address && typeof address !== "string");
    const baseUrl = `http://127.0.0.1:${address.port}`;
    try {
      const child = spawn(process.execPath, [fileURLToPath(new URL("../src/cli.js", import.meta.url)), "https://example.com"], {
        env: { ...process.env, BOBBY_BASE_URL: baseUrl, AUTOMATION_RUNTIME_TOKEN: "browser-secret",
          CAVI_PROVIDER: "claude", CAVI_API_KEY: "model-secret", CAVI_MODEL: "test-model", CAVI_BASE_URL: baseUrl },
        stdio: ["ignore", "pipe", "pipe"], timeout: 10_000,
      });
      let stdout = "";
      let stderr = "";
      child.stdout.on("data", chunk => { stdout += chunk; });
      child.stderr.on("data", chunk => { stderr += chunk; });
      const [exitCode] = await once(child, "close");
      assert.equal(exitCode, fail ? 1 : 0, `${stderr}\n${JSON.stringify(requests)}`);
      if (!fail) assert.deepEqual(JSON.parse(stdout), { text: "A summary.", truncated: false });
      assert.doesNotMatch(stdout + stderr, /secret|private-diagnostic/);
      assert.deepEqual(requests, ["POST /v1/sessions", "POST /v1/pages", "POST /v1/commands",
        "POST /v1/commands", "POST /v1/messages", `DELETE /v1/sessions/${sessionId}`]);
    } finally {
      server.closeAllConnections();
      await new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
    }
  }
});

test("uses terminal text instead of duplicating streamed deltas", async () => {
  assert.deepEqual(await summarizePage({ browser: browser([{ role: "heading", name: "Testing" }]),
    model: { streamRun: completed }, sessionId, pageId }), { text: "A page about testing.", truncated: false });
});

test("rejects wrong-page evidence and empty page context before a model call", async () => {
  for (const runtime of [browser([{ role: "heading", name: "Test" }], { pageId: sessionId }), browser([])]) {
    await assert.rejects(summarizePage({ browser: runtime, model: { streamRun: async () => {
      assert.fail("model must not receive invalid page evidence");
    } }, sessionId, pageId }));
  }
});

test("bounds UTF-8 page context and reports truncation", async () => {
  const result = await summarizePage({ browser: browser([{ role: "heading", name: "😀".repeat(10_000) }]),
    model: { streamRun: async (body, handlers, options) => {
      assert.ok(Buffer.byteLength(String(body.input)) < 10_000);
      assert.ok(options?.signal);
      return completed(body, handlers, options);
    } }, sessionId, pageId });
  assert.equal(result.truncated, true);
});

test("fails on incomplete runs, empty text and oversized output", async () => {
  for (const outcome of [null, "failed", "cancelled", "empty", "oversized"] as const) {
    await assert.rejects(summarizePage({ browser: browser([{ role: "heading", name: "Test" }]), sessionId, pageId,
      model: { streamRun: async (_body, handlers) => {
        if (outcome === "oversized") handlers.onEvent({ event: "message.delta", runId: "run-1", delta: "x".repeat(20_000) });
        return { ok: true, data: { runId: "run-1", outcome: outcome === "empty" || outcome === "oversized" ? "completed" : outcome }, source: "live" };
      } } }));
  }
});

test("propagates caller abort and a bounded deadline to the model", async () => {
  for (const cancel of [true, false]) {
    const controller = new AbortController();
    await assert.rejects(summarizePage({ browser: browser([{ role: "heading", name: "Test" }]), sessionId, pageId,
      signal: controller.signal, timeoutMs: 100,
      model: { streamRun: async (_body, _handlers, options) => {
        assert.ok(options?.signal);
        if (cancel) controller.abort();
        if (!options.signal.aborted) await once(options.signal, "abort");
        options.signal.throwIfAborted();
        throw new Error("unreachable");
      } } }));
  }
});
