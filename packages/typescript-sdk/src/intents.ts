/**
 * Helpers that build CommandEnvelope wire shapes for unified intents.
 *
 * Agents submit these via `command_execute` / `BrowserRuntimeClient.submit` —
 * there are no dedicated intent_* MCP tools. Nested shape matches Rust serde:
 * `{ kind: "intent", input: { kind: "locate", input: { … } } }`.
 */
import {
  MAX_INTENT_PURPOSE_BYTES,
  type CommandEnvelope,
  type FillIntent,
  type FillValue,
  type Id,
  type IntentHints,
  type LocateIntent,
  type RuntimeCommand,
  type SubmitAndVerifyIntent,
  type WaitCondition,
  type WaitForStateIntent,
} from "./contracts.js";

export interface IntentEnvelopeMeta {
  commandId: Id;
  workflowId: Id;
  attemptId: Id;
  sessionId: Id;
  pageId?: Id | null;
  deadline: string;
  schemaVersion?: number;
}

function utf8ByteLength(value: string): number {
  return new TextEncoder().encode(value).length;
}

export function assertIntentPurpose(purpose: string): void {
  if (purpose.length === 0) {
    throw new Error("intent purpose must be non-empty");
  }
  if (utf8ByteLength(purpose) > MAX_INTENT_PURPOSE_BYTES) {
    throw new Error(`intent purpose exceeds ${MAX_INTENT_PURPOSE_BYTES} bytes`);
  }
}

function defaultHints(): IntentHints {
  return {
    role: null,
    nearText: null,
    framePath: [],
    shadowPath: [],
    allowBestMatch: false,
  };
}

function withHints(hints?: IntentHints): IntentHints {
  return { ...defaultHints(), ...hints };
}

export function locateRuntimeCommand(input: LocateIntent): RuntimeCommand {
  assertIntentPurpose(input.purpose);
  return {
    kind: "intent",
    input: {
      kind: "locate",
      input: {
        purpose: input.purpose,
        hints: withHints(input.hints),
      },
    },
  };
}

export function fillRuntimeCommand(input: FillIntent): RuntimeCommand {
  assertIntentPurpose(input.purpose);
  return {
    kind: "intent",
    input: {
      kind: "fill",
      input: {
        purpose: input.purpose,
        hints: withHints(input.hints),
        value: normalizeFillValue(input.value),
      },
    },
  };
}

function normalizeFillValue(value: FillValue): FillValue {
  if (value.kind === "text") {
    return { kind: "text", text: value.text, clearFirst: value.clearFirst ?? false };
  }
  return value;
}

export function submitAndVerifyRuntimeCommand(input: SubmitAndVerifyIntent): RuntimeCommand {
  assertIntentPurpose(input.purpose);
  return {
    kind: "intent",
    input: {
      kind: "submitAndVerify",
      input: {
        purpose: input.purpose,
        hints: withHints(input.hints),
        expectedState: input.expectedState,
      },
    },
  };
}

export function waitForStateRuntimeCommand(input: WaitForStateIntent): RuntimeCommand {
  return {
    kind: "intent",
    input: {
      kind: "waitForState",
      input: {
        condition: input.condition,
        timeoutMs: input.timeoutMs,
      },
    },
  };
}

export function intentEnvelope(meta: IntentEnvelopeMeta, command: RuntimeCommand): CommandEnvelope {
  if (command.kind !== "intent") {
    throw new Error('intentEnvelope requires RuntimeCommand with kind "intent"');
  }
  return {
    schemaVersion: meta.schemaVersion ?? 1,
    commandId: meta.commandId,
    workflowId: meta.workflowId,
    attemptId: meta.attemptId,
    sessionId: meta.sessionId,
    pageId: meta.pageId ?? null,
    deadline: meta.deadline,
    command,
  };
}

export function locateEnvelope(meta: IntentEnvelopeMeta, purpose: string, hints?: IntentHints): CommandEnvelope {
  return intentEnvelope(meta, locateRuntimeCommand({ purpose, hints }));
}

export function fillEnvelope(meta: IntentEnvelopeMeta, purpose: string, value: FillValue, hints?: IntentHints): CommandEnvelope {
  return intentEnvelope(meta, fillRuntimeCommand({ purpose, value, hints }));
}

export function submitAndVerifyEnvelope(
  meta: IntentEnvelopeMeta,
  purpose: string,
  expectedState: SubmitAndVerifyIntent["expectedState"],
  hints?: IntentHints,
): CommandEnvelope {
  return intentEnvelope(meta, submitAndVerifyRuntimeCommand({ purpose, expectedState, hints }));
}

export function waitForStateEnvelope(
  meta: IntentEnvelopeMeta,
  condition: WaitCondition,
  timeoutMs: number,
): CommandEnvelope {
  return intentEnvelope(meta, waitForStateRuntimeCommand({ condition, timeoutMs }));
}
