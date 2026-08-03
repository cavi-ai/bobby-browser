/**
 * Builders for intent {@link RuntimeCommand} and {@link CommandEnvelope} values.
 *
 * Pass envelopes to {@link BrowserRuntimeClient.submit}. Prefer these helpers
 * over hand-rolling nested `{ kind, input }` shapes.
 *
 * Nested wire shape:
 * `{ kind: "intent", input: { kind: "locate", input: { … } } }`.
 */
import {
  DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS,
  MAX_INTENT_PURPOSE_BYTES,
  type CommandEnvelope,
  type CompleteFormIntent,
  type AccessibilityTarget,
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

/** Identifiers and deadline shared by intent envelope helpers. */
export interface IntentEnvelopeMeta {
  commandId: Id;
  workflowId: Id;
  attemptId: Id;
  sessionId: Id;
  pageId?: Id | null;
  /** ISO-8601 deadline stamped on the envelope. */
  deadline: string;
  /** Envelope schema version (default `2`). */
  schemaVersion?: number;
}

function utf8ByteLength(value: string): number {
  return new TextEncoder().encode(value).length;
}

/** Throws if `purpose` is empty or exceeds {@link MAX_INTENT_PURPOSE_BYTES}. */
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
    ordinal: null,
    framePath: [],
    shadowPath: [],
    allowBestMatch: false,
  };
}

/** Map an accessibility target into intent hints (`role` + exact `nearText`). */
export function intentHintsFromAccessibilityTarget(target: AccessibilityTarget): IntentHints {
  return {
    role: target.role,
    nearText: { kind: "exact", value: target.accessibleName },
    ...(target.ordinal === undefined ? {} : { ordinal: target.ordinal }),
  };
}

function withHints(hints?: IntentHints): IntentHints {
  return { ...defaultHints(), ...hints };
}

/** Build a `locate` intent command. */
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

/** Build a `fill` intent command. */
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

/** Build a `completeForm` intent command (1–128 uniquely named fields). */
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

/** Build a `submitAndVerify` intent command. */
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

/** Build a `waitForState` intent command. */
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

/** Build a `follow` intent command. */
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

/** Build a `dismissObstruction` intent command. */
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

/** Build an `extract` intent command (at least one uniquely named field). */
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

/** Wrap an intent {@link RuntimeCommand} in a {@link CommandEnvelope}. */
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

/** Convenience: {@link locateRuntimeCommand} + {@link intentEnvelope}. */
export function locateEnvelope(meta: IntentEnvelopeMeta, purpose: string, hints?: IntentHints): CommandEnvelope {
  return intentEnvelope(meta, locateRuntimeCommand({ purpose, hints }));
}

/** Convenience: {@link fillRuntimeCommand} + {@link intentEnvelope}. */
export function fillEnvelope(meta: IntentEnvelopeMeta, purpose: string, value: FillValue, hints?: IntentHints): CommandEnvelope {
  return intentEnvelope(meta, fillRuntimeCommand({ purpose, value, hints }));
}

/** Convenience: {@link submitAndVerifyRuntimeCommand} + {@link intentEnvelope}. */
export function submitAndVerifyEnvelope(
  meta: IntentEnvelopeMeta,
  purpose: string,
  expectedState: SubmitAndVerifyIntent["expectedState"],
  hints?: IntentHints,
): CommandEnvelope {
  return intentEnvelope(meta, submitAndVerifyRuntimeCommand({ purpose, expectedState, hints }));
}

/** Convenience: {@link waitForStateRuntimeCommand} + {@link intentEnvelope}. */
export function waitForStateEnvelope(
  meta: IntentEnvelopeMeta,
  condition: WaitCondition,
  timeoutMs: number,
): CommandEnvelope {
  return intentEnvelope(meta, waitForStateRuntimeCommand({ condition, timeoutMs }));
}

/** Convenience: {@link followRuntimeCommand} + {@link intentEnvelope}. */
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

/** Convenience: {@link dismissObstructionRuntimeCommand} + {@link intentEnvelope}. */
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

/** Convenience: {@link extractRuntimeCommand} + {@link intentEnvelope}. */
export function extractEnvelope(
  meta: IntentEnvelopeMeta,
  purpose: string,
  fields: ExtractField[],
): CommandEnvelope {
  return intentEnvelope(meta, extractRuntimeCommand({ purpose, fields }));
}
