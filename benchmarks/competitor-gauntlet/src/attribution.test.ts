import assert from "node:assert";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";

import {
  buildAttribution,
  failureTaxonomy,
  modelTier,
  readMetricsSnapshot,
} from "./attribution.js";

const SNAPSHOT = {
  observationWindowMs: 42000,
  intent: {
    total: 9,
    locate: 3,
    fill: 4,
    completeForm: 0,
    extract: 1,
    submit: 1,
    waitForState: 0,
    follow: 0,
    dismiss: 0,
    solveChallenge: 0,
    detectChallenge: 0,
    deterministic: 7,
    context: 1,
    visionPrefill: 0,
    visionFallback: 1,
  },
  context: { hit: 1, miss: 2, ambiguousRefusal: 0, staleRejection: 0, error: 0 },
};

function assistantToolUse(id: string, name: string) {
  return {
    type: "assistant",
    message: { content: [{ type: "tool_use", id, name }] },
  };
}

function userToolResult(toolUseId: string, content: unknown, isError = true) {
  return {
    type: "user",
    message: {
      content: [
        { type: "tool_result", tool_use_id: toolUseId, is_error: isError, content },
      ],
    },
  };
}

const BOBBY_ERROR = JSON.stringify({
  status: "failed",
  commandId: "cmd-1",
  error: {
    code: "targetNotFound",
    message: "no candidate matched",
    layer: "interface",
    retryable: true,
  },
});

test("buildAttribution reads action count and resolution sources from the snapshot", () => {
  const attribution = buildAttribution([], SNAPSHOT, "claude-opus-5");
  assert.equal(attribution.actionCount, 9);
  assert.deepEqual(attribution.resolution, {
    deterministic: 7,
    context: 1,
    visionPrefill: 0,
    visionFallback: 1,
  });
  assert.equal(attribution.modelTier, "flagship");
  assert.deepEqual(attribution.failureTaxonomy, {});
});

test("buildAttribution tolerates a missing snapshot", () => {
  const attribution = buildAttribution([], null, "claude-sonnet-4-6");
  assert.equal(attribution.actionCount, null);
  assert.equal(attribution.resolution, null);
  assert.equal(attribution.modelTier, "mid");
});

test("modelTier maps the model families and refuses to guess", () => {
  assert.equal(modelTier("claude-opus-5"), "flagship");
  assert.equal(modelTier("grok-4.6"), "flagship");
  assert.equal(modelTier("claude-sonnet-4-6"), "mid");
  assert.equal(modelTier("claude-haiku-4-5"), "small");
  assert.equal(modelTier("default"), "unknown");
  assert.equal(modelTier(null), "unknown");
});

test("failureTaxonomy counts interface error codes from JSON tool errors", () => {
  const events = [
    assistantToolUse("t1", "mcp__bobby__click"),
    userToolResult("t1", [{ type: "text", text: BOBBY_ERROR }]),
    assistantToolUse("t2", "mcp__bobby__wait_for"),
    userToolResult("t2", BOBBY_ERROR.replace("targetNotFound", "waitConditionTimedOut")),
    userToolResult("t3", BOBBY_ERROR), // same code again, from a plain string body
  ];
  assert.deepEqual(failureTaxonomy(events), {
    targetNotFound: 2,
    waitConditionTimedOut: 1,
  });
});

test("failureTaxonomy parses a host-prefixed JSON body but ignores plain text", () => {
  const events = [
    userToolResult("t1", `Exit code 1\n${BOBBY_ERROR}`),
    userToolResult("t2", "Exit code 1\nls: in.fifo: No such file or directory"),
    userToolResult("t3", "{ not json"),
    userToolResult("t4", BOBBY_ERROR, false), // not an error result
  ];
  assert.deepEqual(failureTaxonomy(events), { targetNotFound: 1 });
});

test("readMetricsSnapshot returns the parsed object or null", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "bobby-attribution-"));
  try {
    const good = path.join(dir, "snapshot.json");
    writeFileSync(good, JSON.stringify(SNAPSHOT));
    assert.equal(readMetricsSnapshot(good).intent.total, 9);

    const bad = path.join(dir, "garbage.json");
    writeFileSync(bad, "not json {");
    assert.equal(readMetricsSnapshot(bad), null);

    assert.equal(readMetricsSnapshot(path.join(dir, "missing.json")), null);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
