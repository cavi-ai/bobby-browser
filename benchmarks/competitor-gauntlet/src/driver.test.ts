import assert from "node:assert/strict";
import test from "node:test";
import {
  bobbyGauntletToml,
  buildCursorAgentOptions,
  CURSOR_ISOLATION,
  isGrokModelId,
  mcpJsonToCursorServers,
  probeMlx,
  resolveGrokModel,
} from "./driver.js";

test("resolveGrokModel refuses a non-Grok requested id", () => {
  assert.throws(
    () => resolveGrokModel("claude-opus-5", [{ id: "grok-4.6" }]),
    /refusing claude-opus-5/,
  );
});

test("resolveGrokModel uses the requested Grok id when listed", () => {
  assert.equal(
    resolveGrokModel("grok-4.6", [{ id: "composer-2.5" }, { id: "grok-4.6" }]),
    "grok-4.6",
  );
});

test("resolveGrokModel refuses a Grok id the account cannot select", () => {
  assert.throws(
    () => resolveGrokModel("grok-4.6", [{ id: "grok-4.5" }]),
    /not available/,
  );
});

test("resolveGrokModel picks the latest listed Grok when none is requested", () => {
  assert.equal(
    resolveGrokModel(undefined, [
      { id: "composer-2.5" },
      { id: "grok-4.5" },
      { id: "grok-4.6" },
    ]),
    "grok-4.6",
  );
});

test("resolveGrokModel refuses to fall back when no Grok is listed", () => {
  assert.throws(
    () => resolveGrokModel(undefined, [{ id: "composer-2.5" }]),
    /no Grok model is available/,
  );
});

test("isGrokModelId matches Cursor Grok slugs", () => {
  assert.equal(isGrokModelId("grok-4.6"), true);
  assert.equal(isGrokModelId("cursor-grok-4.6-high"), true);
  assert.equal(isGrokModelId("composer-2.5"), false);
});

test("mcpJsonToCursorServers maps stdio servers and drops empty commands", () => {
  const servers = mcpJsonToCursorServers({
    mcpServers: {
      bobby: {
        command: "/tmp/bobby",
        args: ["mcp-stdio"],
        env: { BOBBY_BROWSER_CONFIG: "/tmp/cfg" },
      },
      skip: { command: "" },
    },
  });
  assert.deepEqual(servers, {
    bobby: {
      type: "stdio",
      command: "/tmp/bobby",
      args: ["mcp-stdio"],
      env: { BOBBY_BROWSER_CONFIG: "/tmp/cfg" },
    },
  });
});

test("buildCursorAgentOptions isolates the run from ambient Cursor settings", () => {
  const options = buildCursorAgentOptions({
    workDir: "/tmp/cg",
    model: "grok-4.6",
    apiKey: "cursor_test",
    mcpServers: {
      bobby: { type: "stdio", command: "bobby", args: ["mcp-stdio"] },
    },
  });
  assert.equal(options.model.id, "grok-4.6");
  assert.equal(options.local.cwd, "/tmp/cg");
  assert.deepEqual(options.local.settingSources, []);
  assert.equal(CURSOR_ISOLATION, "inline-mcp,no-setting-sources");
});

test("bobbyGauntletToml vision-on pins local MLX Qwen3.5 and the loopback proxy", () => {
  const toml = bobbyGauntletToml(true);
  assert.match(toml, /startup_toolset = "explore"/);
  assert.match(toml, /provider = "mlx"/);
  assert.match(toml, /Qwen3\.5-27B-4bit/);
  assert.match(toml, /127\.0\.0\.1:9101/);
  assert.match(toml, /127\.0\.0\.1:9100\/vision/);
  assert.equal(bobbyGauntletToml(false).includes("[vision]"), false);
});

test("probeMlx is true when the loopback server answers", async () => {
  const ok = await probeMlx(
    "http://127.0.0.1:9101",
    async () => new Response("ok", { status: 200 }),
  );
  assert.equal(ok, true);
  const down = await probeMlx("http://127.0.0.1:9101", async () => {
    throw new Error("connect");
  });
  assert.equal(down, false);
});
