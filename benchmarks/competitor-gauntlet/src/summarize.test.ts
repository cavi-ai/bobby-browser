import assert from "node:assert/strict";
import test from "node:test";
import { summarize } from "./summarize.js";

const assistant = (model: string, content: unknown[], usage: Record<string, number>) => ({
  type: "assistant",
  message: { model, content, usage },
});

const toolUse = (name: string) => ({ type: "tool_use", name });

const userWithResults = (...results: { isError?: boolean }[]) => ({
  type: "user",
  message: {
    content: results.map((r) => ({ type: "tool_result", is_error: Boolean(r.isError) })),
  },
});

test("summarize aggregates per-turn usage instead of trusting the final event", () => {
  const events = [
    assistant("claude-opus-5", [toolUse("click")], {
      input_tokens: 5,
      output_tokens: 10,
      cache_read_input_tokens: 100,
      cache_creation_input_tokens: 20,
    }),
    userWithResults({}),
    assistant("claude-opus-5", [toolUse("type_text")], {
      input_tokens: 7,
      output_tokens: 30,
      cache_read_input_tokens: 200,
      cache_creation_input_tokens: 40,
    }),
    userWithResults({ isError: true }),
    {
      type: "result",
      result: "done",
      model: "claude-opus-5",
      usage: { input_tokens: 7, output_tokens: 30 },
    },
  ];

  const summary = summarize(events);

  assert.equal(summary.toolCalls, 2);
  assert.equal(summary.toolErrors, 1);
  // The old implementation copied the final `result` usage (7 in / 30 out),
  // which reported a small fraction of the true cost.
  assert.equal(summary.inputTokens, 12);
  assert.equal(summary.outputTokens, 40);
  assert.equal(summary.cacheReadTokens, 300);
  assert.equal(summary.cacheCreationTokens, 60);
  assert.equal(summary.model, "claude-opus-5");
  assert.equal(summary.resultText, "done");
});

test("summarize tolerates missing usage blocks and null token fields", () => {
  const events = [
    assistant("claude-opus-5", [toolUse("click")], {}),
    userWithResults({}),
    { type: "result", result: "", usage: null },
  ];

  const summary = summarize(events);

  assert.equal(summary.inputTokens, 0);
  assert.equal(summary.outputTokens, 0);
  assert.equal(summary.cacheReadTokens, 0);
  assert.equal(summary.cacheCreationTokens, 0);
  assert.equal(summary.toolCalls, 1);
});

test("summarize keeps the first model name and the last result text", () => {
  const events = [
    assistant("first", [], { input_tokens: 1, output_tokens: 1 }),
    { type: "result", result: "final", model: "second", usage: {} },
  ];

  const summary = summarize(events);

  assert.equal(summary.model, "first");
  assert.equal(summary.resultText, "final");
});