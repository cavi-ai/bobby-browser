import { randomUUID } from "node:crypto";
import { BrowserRuntimeClient } from "@cavi-ai/bobby-browser";
import { createModelClient, required } from "./providers.js";
import { summarizePage } from "./summarize.js";

async function main(): Promise<void> {
  const url = new URL(process.argv[2] ?? "");
  if (!["https:", "http:"].includes(url.protocol) || url.username || url.password) {
    throw new Error("Provide an HTTP(S) page URL without credentials");
  }
  const env = process.env;
  const modelSessionKey = env.CAVI_PROVIDER === "hermes" || env.CAVI_PROVIDER === "openclaw"
    ? required(env, "CAVI_SESSION_KEY") : undefined;
  const browser = new BrowserRuntimeClient({ baseUrl: required(env, "BOBBY_BASE_URL"),
    bearerToken: required(env, "AUTOMATION_RUNTIME_TOKEN"), maxJsonResponseBytes: 1_048_576 });
  const model = createModelClient(env);
  const controller = new AbortController();
  const stop = () => controller.abort(new Error("Interrupted"));
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);
  const timer = setTimeout(() => controller.abort(new Error("Example deadline exceeded")), 60_000);
  let sessionId: string | undefined;
  try {
    const session = await browser.createSession({ profile: "default", proxy: null },
      { signal: controller.signal, idempotencyKey: randomUUID() });
    sessionId = session.id;
    const page = await browser.openPage({ session_id: sessionId },
      { signal: controller.signal, idempotencyKey: randomUUID() });
    const commandId = randomUUID();
    const navigation = await browser.submit({ schemaVersion: 2, commandId,
      workflowId: randomUUID(), attemptId: randomUUID(), sessionId, pageId: page.id,
      deadline: new Date(Date.now() + 30_000).toISOString(), command: { kind: "primitive", input: {
        kind: "navigate", input: { url: url.href, waitUntil: "domContentLoaded", timeoutMs: 30_000 },
      } } }, { signal: controller.signal, idempotencyKey: randomUUID() });
    if (navigation.commandId !== commandId || navigation.status !== "completed") {
      throw new Error("Page navigation did not complete");
    }
    const summary = await summarizePage({ browser, model, sessionId, pageId: page.id,
      modelSessionKey, signal: controller.signal });
    // Model output is untrusted plain text. JSON encoding prevents terminal control sequences.
    process.stdout.write(`${JSON.stringify(summary)}\n`);
  } finally {
    clearTimeout(timer);
    process.removeListener("SIGINT", stop);
    process.removeListener("SIGTERM", stop);
    try {
      // Cleanup has its own short deadline, including after caller cancellation.
      if (sessionId) await browser.deleteSession(sessionId, { timeoutMs: 5_000, idempotencyKey: randomUUID() });
    } finally {
      await model.dispose();
    }
  }
}

main().catch(() => {
  // Provider errors can contain submitted text. Keep CLI diagnostics payload-free.
  process.stderr.write("Page summary failed; check runtime access and provider configuration.\n");
  process.exitCode = 1;
});
