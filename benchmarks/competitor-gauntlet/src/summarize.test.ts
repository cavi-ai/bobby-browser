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
  assert.equal(summary.bobbyToolCalls, 0);
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

test("summarize counts Cursor tool_call events and camelCase usage once", () => {
  const summary = summarize([
    {
      type: "assistant",
      message: {
        model: { id: "grok-4.6" },
        content: [{ type: "tool_use", name: "mcp__bobby__click" }],
      },
    },
    {
      type: "tool_call",
      name: "mcp__bobby__click",
      status: "running",
    },
    {
      type: "tool_call",
      name: "mcp__bobby__click",
      status: "completed",
    },
    {
      type: "tool_call",
      name: "shell",
      status: "error",
    },
    {
      type: "usage",
      usage: { inputTokens: 11, outputTokens: 22, cacheReadTokens: 3, cacheWriteTokens: 4 },
    },
    { type: "result", result: "done", model: { id: "grok-4.6" } },
  ]);

  assert.equal(summary.toolCalls, 2);
  assert.equal(summary.bobbyToolCalls, 1);
  assert.equal(summary.hostToolCalls, 1);
  assert.equal(summary.shellToolCalls, 1);
  assert.equal(summary.toolErrors, 1);
  assert.equal(summary.inputTokens, 11);
  assert.equal(summary.outputTokens, 22);
  assert.equal(summary.cacheReadTokens, 3);
  assert.equal(summary.cacheCreationTokens, 4);
  assert.equal(summary.model, "grok-4.6");
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