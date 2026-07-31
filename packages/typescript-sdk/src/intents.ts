/**
 * Helpers that build CommandEnvelope wire shapes for unified intents.
 *
 * Agents submit these via `command_execute` / `BrowserRuntimeClient.submit` —
 * there are no dedicated intent_* MCP tools. Nested shape matches Rust serde:
 * `{ kind: "intent", input: { kind: "locate", input: { … } } }`.
 */
import {
  DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS,
  MAX_INTENT_PURPOSE_BYTES,
  type CommandEnvelope,
  type CompleteFormIntent,
  type DismissObstructionIntent,
  type ExtractField,
  type ExtractIntent,
  type FillIntent,
  type FillValue,
  type FollowIntent,
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

export function completeFormRuntimeCommand(input: CompleteFormIntent): RuntimeCommand {
  assertIntentPurpose(input.purpose);
  if (input.fields.length === 0) throw new Error("completeForm fields must not be empty");
  if (input.fields.length > 128) throw new Error("completeForm fields must not exceed 128 items");
  const names = new Set<string>();
  const fields = input.fields.map((field) => {
    if (field.name.trim().length === 0) throw new Error("completeForm field name must not be empty");
    if (names.has(field.name)) throw new Error(`duplicate completeForm field name: ${field.name}`);
    names.add(field.name);
    assertIntentPurpose(field.purpose);
    return { ...field, hints: withHints(field.hints), value: normalizeFillValue(field.value) };
  });
  return { kind: "intent", input: { kind: "completeForm", input: { purpose: input.purpose, fields } } };
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

export function followRuntimeCommand(input: FollowIntent): RuntimeCommand {
  assertIntentPurpose(input.purpose);
  return {
    kind: "intent",
    input: {
      kind: "follow",
      input: {
        purpose: input.purpose,
        hints: withHints(input.hints),
        expectedDestination: input.expectedDestination,
        boundary: input.boundary ?? false,
      },
    },
  };
}

export function dismissObstructionRuntimeCommand(input: DismissObstructionIntent): RuntimeCommand {
  assertIntentPurpose(input.purpose);
  return {
    kind: "intent",
    input: {
      kind: "dismissObstruction",
      input: {
        purpose: input.purpose,
        hints: withHints(input.hints),
        timeoutMs: input.timeoutMs ?? DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS,
      },
    },
  };
}

export function extractRuntimeCommand(input: ExtractIntent): RuntimeCommand {
  assertIntentPurpose(input.purpose);
  if (input.fields.length === 0) {
    throw new Error("extract intent must include at least one field");
  }
  const seenNames = new Set<string>();
  const fields: ExtractField[] = input.fields.map((field) => {
    assertIntentPurpose(field.purpose);
    const name = field.name.trim();
    if (name.length === 0) {
      throw new Error("extract field name must not be empty");
    }
    if (seenNames.has(name)) {
      throw new Error(`duplicate extract field name: ${name}`);
    }
    seenNames.add(name);
    return {
      name,
      purpose: field.purpose,
      hints: withHints(field.hints),
      value: field.value,
    };
  });
  return {
    kind: "intent",
    input: {
      kind: "extract",
      input: {
        purpose: input.purpose,
        fields,
      },
    },
  };
}

export function intentEnvelope(meta: IntentEnvelopeMeta, command: RuntimeCommand): CommandEnvelope {
  if (command.kind !== "intent") {
    throw new Error('intentEnvelope requires RuntimeCommand with kind "intent"');
  }
  return {
    schemaVersion: meta.schemaVersion ?? 2,
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

export function followEnvelope(
  meta: IntentEnvelopeMeta,
  purpose: string,
  expectedDestination: FollowIntent["expectedDestination"],
  options?: { hints?: IntentHints; boundary?: boolean },
): CommandEnvelope {
  return intentEnvelope(
    meta,
    followRuntimeCommand({
      purpose,
      expectedDestination,
      hints: options?.hints,
      boundary: options?.boundary,
    }),
  );
}

export function dismissObstructionEnvelope(
  meta: IntentEnvelopeMeta,
  purpose: string,
  options?: { hints?: IntentHints; timeoutMs?: number },
): CommandEnvelope {
  return intentEnvelope(
    meta,
    dismissObstructionRuntimeCommand({
      purpose,
      hints: options?.hints,
      timeoutMs: options?.timeoutMs,
    }),
  );
}

export function extractEnvelope(
  meta: IntentEnvelopeMeta,
  purpose: string,
  fields: ExtractField[],
): CommandEnvelope {
  return intentEnvelope(meta, extractRuntimeCommand({ purpose, fields }));
}
