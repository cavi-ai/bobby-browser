// Token accounting for Claude Code stream-json transcripts.
//
// The final `result` event's usage reflects only the last request, so
// copying it undercounts by orders of magnitude: cache-heavy agent loops
// re-read tens of MB of prompt cache per run while `input_tokens` sits near
// zero. The true cost of a run is the sum over every assistant turn, plus
// the cache fields (`cache_read_input_tokens` bills at ~0.1x,
// `cache_creation_input_tokens` at 1.25x) that the final event omits.

export interface TokenUsage {
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheCreationTokens: number;
}

interface AssistantUsage {
  input_tokens?: number | null;
  output_tokens?: number | null;
  cache_read_input_tokens?: number | null;
  cache_creation_input_tokens?: number | null;
}

function count(value: number | null | undefined): number {
  return typeof value === "number" && Number.isFinite(value) && value > 0
    ? value
    : 0;
}

export function usageTotals(turns: AssistantUsage[]): Required<TokenUsage> {
  let inputTokens = 0;
  let outputTokens = 0;
  let cacheReadTokens = 0;
  let cacheCreationTokens = 0;
  for (const turn of turns) {
    inputTokens += count(turn?.input_tokens);
    outputTokens += count(turn?.output_tokens);
    cacheReadTokens += count(turn?.cache_read_input_tokens);
    cacheCreationTokens += count(turn?.cache_creation_input_tokens);
  }
  return { inputTokens, outputTokens, cacheReadTokens, cacheCreationTokens };
}

export function summarize(events: any[]) {
  let toolCalls = 0;
  let toolErrors = 0;
  let resultText = "";
  let model: string | undefined;
  const turns: AssistantUsage[] = [];
  for (const event of events) {
    if (event.type === "assistant") {
      model ??= event.message?.model;
      const usage = event.message?.usage;
      if (usage) {
        turns.push(usage);
      } else {
        turns.push({});
      }
      for (const block of event.message?.content ?? []) {
        if (block.type === "tool_use") toolCalls += 1;
      }
    } else if (event.type === "user") {
      for (const block of event.message?.content ?? []) {
        if (block.type === "tool_result" && block.is_error) toolErrors += 1;
      }
    } else if (event.type === "result") {
      resultText = event.result ?? "";
      model ??= event.model;
    }
  }
  const totals = usageTotals(turns);
  return { toolCalls, toolErrors, resultText, model, ...totals };
}