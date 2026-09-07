// Token accounting for Claude Code stream-json and Cursor SDK transcripts.
//
// The final `result` event's usage reflects only the last request, so
// copying it undercounts by orders of magnitude: cache-heavy agent loops
// re-read tens of MB of prompt cache per run while `input_tokens` sits near
// zero. The true cost of a run is the sum over every assistant/usage turn,
// plus the cache fields that the final event omits.

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
  inputTokens?: number | null;
  outputTokens?: number | null;
  cacheReadTokens?: number | null;
  cacheWriteTokens?: number | null;
  cacheCreationTokens?: number | null;
}

const SHELL_TOOLS = new Set([
  "Bash",
  "Read",
  "Write",
  "Edit",
  "Glob",
  "Grep",
  "shell",
  "read",
  "edit",
  "grep",
  "glob",
  "ls",
  "delete",
]);
const BOOKKEEPING_TOOLS = new Set([
  "TaskCreate",
  "TaskUpdate",
  "TaskGet",
  "TaskList",
  "updateTodos",
  "readTodos",
]);

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
    inputTokens += count(turn?.input_tokens) + count(turn?.inputTokens);
    outputTokens += count(turn?.output_tokens) + count(turn?.outputTokens);
    cacheReadTokens +=
      count(turn?.cache_read_input_tokens) + count(turn?.cacheReadTokens);
    cacheCreationTokens +=
      count(turn?.cache_creation_input_tokens) +
      count(turn?.cacheCreationTokens) +
      count(turn?.cacheWriteTokens);
  }
  return { inputTokens, outputTokens, cacheReadTokens, cacheCreationTokens };
}

function isBobbyTool(name: string): boolean {
  return name.startsWith("mcp__bobby__") || /bobby/i.test(name);
}

export function summarize(events: any[]) {
  let toolCalls = 0;
  let bobbyToolCalls = 0;
  let discoveryToolCalls = 0;
  let taskBookkeepingCalls = 0;
  let shellToolCalls = 0;
  let toolErrors = 0;
  let resultText = "";
  let model: string | undefined;
  const toolCallBreakdown = new Map<string, number>();
  const turns: AssistantUsage[] = [];
  const countFromToolCallEvents = events.some((event) => event.type === "tool_call");

  const recordTool = (name: string) => {
    toolCalls += 1;
    const key = name || "unknown";
    toolCallBreakdown.set(key, (toolCallBreakdown.get(key) ?? 0) + 1);
    if (isBobbyTool(key)) bobbyToolCalls += 1;
    if (key === "ToolSearch") discoveryToolCalls += 1;
    if (BOOKKEEPING_TOOLS.has(key)) taskBookkeepingCalls += 1;
    if (SHELL_TOOLS.has(key)) shellToolCalls += 1;
  };

  for (const event of events) {
    if (event.type === "assistant") {
      model ??= event.message?.model ?? event.model;
      if (typeof model === "object" && model && "id" in model) {
        model = String((model as { id: string }).id);
      }
      const usage = event.message?.usage ?? event.usage;
      turns.push(usage ?? {});
      if (!countFromToolCallEvents) {
        for (const block of event.message?.content ?? []) {
          if (block.type === "tool_use") recordTool(String(block.name ?? "unknown"));
        }
      }
    } else if (event.type === "tool_call") {
      if (event.status === "completed" || event.status === "error") {
        recordTool(String(event.name ?? "unknown"));
      }
      if (event.status === "error") toolErrors += 1;
    } else if (event.type === "usage" && event.usage) {
      turns.push(event.usage);
    } else if (event.type === "user") {
      for (const block of event.message?.content ?? []) {
        if (block.type === "tool_result" && block.is_error) toolErrors += 1;
      }
    } else if (event.type === "result") {
      resultText = event.result ?? resultText;
      const resultModel = event.model;
      if (typeof resultModel === "string") model ??= resultModel;
      if (resultModel && typeof resultModel === "object" && "id" in resultModel) {
        model ??= String(resultModel.id);
      }
    }
  }
  const totals = usageTotals(turns);
  return {
    toolCalls,
    bobbyToolCalls,
    hostToolCalls: toolCalls - bobbyToolCalls,
    discoveryToolCalls,
    taskBookkeepingCalls,
    shellToolCalls,
    toolErrors,
    resultText,
    model,
    toolCallBreakdown: Object.fromEntries(
      [...toolCallBreakdown.entries()].sort(([left], [right]) =>
        left.localeCompare(right),
      ),
    ),
    ...totals,
  };
}
