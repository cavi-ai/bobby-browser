import { readFileSync } from "node:fs";

// Per-run attribution for the competitor gauntlet result schema (PRD phase
// 18): every run record carries where its actions resolved from, the model
// tier that drove it, and the interface failure taxonomy it hit. The bobby
// runner gets a metrics snapshot from the stdio gateway's
// BOBBY_METRICS_SNAPSHOT_PATH dump on session close; other tools leave the
// snapshot-derived fields null.

export type ResolutionCounts = {
  deterministic: number;
  context: number;
  visionPrefill: number;
  visionFallback: number;
};

export type RunAttribution = {
  // Snapshot-derived; null when the run produced no metrics snapshot
  // (non-bobby tools, or a bobby binary older than the dump hook).
  actionCount: number | null;
  resolution: ResolutionCounts | null;
  modelTier: "flagship" | "mid" | "small" | "unknown";
  // Interface error codes (e.g. targetNotFound) counted from tool_result
  // error bodies that parse as { "error": { "code": ... } }. Host errors
  // with unstructured text contribute nothing.
  failureTaxonomy: Record<string, number>;
};

export function readMetricsSnapshot(path: string): any | null {
  try {
    const parsed = JSON.parse(readFileSync(path, "utf8"));
    return parsed && typeof parsed === "object" ? parsed : null;
  } catch {
    return null;
  }
}

export function modelTier(
  model: string | null | undefined,
): RunAttribution["modelTier"] {
  if (!model) return "unknown";
  if (/opus|grok/i.test(model)) return "flagship";
  if (/sonnet/i.test(model)) return "mid";
  if (/haiku/i.test(model)) return "small";
  return "unknown";
}

export function buildAttribution(
  events: any[],
  snapshot: any | null,
  model: string | null | undefined,
): RunAttribution {
  const intent = snapshot?.intent;
  return {
    actionCount: intent ? count(intent.total) : null,
    resolution: intent
      ? {
          deterministic: count(intent.deterministic),
          context: count(intent.context),
          visionPrefill: count(intent.visionPrefill),
          visionFallback: count(intent.visionFallback),
        }
      : null,
    modelTier: modelTier(model),
    failureTaxonomy: failureTaxonomy(events),
  };
}

export function failureTaxonomy(events: any[]): Record<string, number> {
  const counts = new Map<string, number>();
  for (const event of events) {
    if (event?.type !== "user") continue;
    for (const block of event.message?.content ?? []) {
      if (block?.type !== "tool_result" || !block.is_error) continue;
      for (const text of resultTexts(block.content)) {
        const code = extractErrorCode(text);
        if (code) counts.set(code, (counts.get(code) ?? 0) + 1);
      }
    }
  }
  return Object.fromEntries(
    [...counts.entries()].sort(([left], [right]) => left.localeCompare(right)),
  );
}

function count(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0
    ? value
    : 0;
}

function resultTexts(content: unknown): string[] {
  if (typeof content === "string") return [content];
  if (Array.isArray(content)) {
    return content.flatMap((block) =>
      block && typeof block === "object" && typeof block.text === "string"
        ? [block.text]
        : [],
    );
  }
  return [];
}

// camelCase interface codes: notFound, waitConditionTimedOut, ...
const ERROR_CODE = /^[a-z][A-Za-z0-9]{1,64}$/;

function extractErrorCode(text: string): string | null {
  // Tool errors arrive either as a bare JSON body or prefixed by a host
  // line ("Exit code 1\n{...}") — parse from the first brace.
  const start = text.indexOf("{");
  if (start < 0) return null;
  try {
    const parsed = JSON.parse(text.slice(start));
    const code = parsed?.error?.code;
    if (typeof code === "string" && ERROR_CODE.test(code)) return code;
  } catch {
    // not a JSON failure body
  }
  return null;
}
