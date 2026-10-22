import assert from "node:assert/strict";
import test from "node:test";
import { createModelClient } from "../src/providers.js";

test("constructs each supported provider through the published Cavi factory", async () => {
  for (const [provider, kind] of [["claude", "claude-sdk"], ["codex", "codex-responses"],
    ["openclaw", "openclaw"], ["hermes", "hermes"], ["opencode", "opencode"], ["agy", "agy"]]) {
    const client = createModelClient({ CAVI_PROVIDER: provider, CAVI_API_KEY: "test-secret",
      CAVI_MODEL: "test-model", CAVI_BASE_URL: "http://127.0.0.1:1", CAVI_DIRECTORY: "/project" });
    assert.equal(client.providerKind, kind);
    await client.dispose();
  }
});

test("rejects unknown providers and missing configuration before any request", () => {
  for (const env of [{}, { CAVI_PROVIDER: "unknown" }, { CAVI_PROVIDER: "codex" },
    { CAVI_PROVIDER: "claude", CAVI_API_KEY: "key" }, { CAVI_PROVIDER: "hermes" },
    { CAVI_PROVIDER: "opencode", CAVI_BASE_URL: "http://127.0.0.1:1" }]) {
    assert.throws(() => createModelClient(env));
  }
});
