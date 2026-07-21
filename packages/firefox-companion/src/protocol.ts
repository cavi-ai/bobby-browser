export const PROTOCOL_VERSION = 1 as const;
export const MAX_COMPANION_PAYLOAD_BYTES = 1024 * 1024;
const MAX_ID_BYTES = 256;
const MAX_METADATA_BYTES = 256;
const MAX_PAIRING_CODE_BYTES = 512;
const MAX_OPERATION_BYTES = 64;
const MAX_ERROR_CODE_BYTES = 128;
const MAX_ERROR_MESSAGE_BYTES = 4 * 1024;
const MAX_TARGET_COUNT = 256;
const MAX_TARGET_ID_BYTES = 256;
const textEncoder = new TextEncoder();

export type BrowserEngine = "firefox" | "chromium" | "webKit";

export type BrowserIdentity = {
  engine: BrowserEngine;
  browserName: string;
  browserVersion: string;
  os: string;
  profileLabel: string;
};

export type CompanionCapabilities = {
  observe: boolean;
  navigate: boolean;
  nativeInput: boolean;
  tabs: boolean;
  frames: boolean;
  nativeDialogs: boolean;
};

export type PairRequest = {
  protocolVersion: typeof PROTOCOL_VERSION;
  pairingCode: string;
  companionId: string;
  profileId: string;
  identity: BrowserIdentity;
  capabilities: CompanionCapabilities;
};

export type ActionRequest = {
  protocolVersion: typeof PROTOCOL_VERSION;
  attachmentId: string;
  commandId: string;
  pageId: string;
  operation: string;
  input: unknown;
  deadlineUnixMs: number;
};

export type TargetKind = "page" | "frame";

export type BrowserTarget = {
  targetId: string;
  kind: TargetKind;
};

export type TargetDiscovery = {
  protocolVersion: typeof PROTOCOL_VERSION;
  profileId: string;
  targets: BrowserTarget[];
};

export type GrantedPage = {
  targetId: string;
  pageId: string;
};

export type AttachmentGrant = {
  protocolVersion: typeof PROTOCOL_VERSION;
  attachmentId: string;
  profileId: string;
  expiresAtUnixMs: number;
  pages: GrantedPage[];
};

export type CompanionRequest =
  | { kind: "pair"; input: PairRequest }
  | { kind: "grant"; input: AttachmentGrant }
  | { kind: "action"; input: ActionRequest }
  | { kind: "ping" };

export type InteractionPath = "engineNative" | "extensionApi" | "hostNative";

export type CompanionEvent =
  | {
      kind: "paired";
      output: { companionId: string; profileId: string };
    }
  | {
      kind: "actionCompleted";
      output: {
        commandId: string;
        interactionPath: InteractionPath;
        output: unknown;
      };
    }
  | {
      kind: "actionFailed";
      output: {
        commandId: string;
        code: string;
        message: string;
        effectUncertain: boolean;
      };
    }
  | { kind: "targetsDiscovered"; output: TargetDiscovery }
  | { kind: "pong" };

type JsonObject = Record<string, unknown>;

export class CompanionProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "CompanionProtocolError";
  }
}

function parsePayload(payload: string): unknown {
  if (new TextEncoder().encode(payload).byteLength > MAX_COMPANION_PAYLOAD_BYTES) {
    throw new CompanionProtocolError("companion payload exceeds the 1 MiB limit");
  }
  try {
    return JSON.parse(payload) as unknown;
  } catch {
    throw new CompanionProtocolError("companion payload must be valid JSON");
  }
}

function object(value: unknown, name: string): JsonObject {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new CompanionProtocolError(`${name} must be an object`);
  }
  return value as JsonObject;
}

function exactKeys(value: JsonObject, keys: readonly string[], name: string): void {
  const actual = Object.keys(value).sort();
  const expected = [...keys].sort();
  if (
    actual.length !== expected.length ||
    actual.some((key, index) => key !== expected[index])
  ) {
    throw new CompanionProtocolError(`${name} has an invalid shape`);
  }
}

function string(value: unknown, name: string, maximum = MAX_METADATA_BYTES): string {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    textEncoder.encode(value).byteLength > maximum
  ) {
    throw new CompanionProtocolError(`${name} must be a non-empty bounded string of at most ${maximum} bytes`);
  }
  return value;
}

function boolean(value: unknown, name: string): boolean {
  if (typeof value !== "boolean") {
    throw new CompanionProtocolError(`${name} must be a boolean`);
  }
  return value;
}

function uuid(value: unknown, name: string): string {
  const parsed = string(value, name, MAX_ID_BYTES);
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(parsed)) {
    throw new CompanionProtocolError(`${name} must be a UUID`);
  }
  return parsed;
}

function protocolVersion(value: unknown): typeof PROTOCOL_VERSION {
  if (value !== PROTOCOL_VERSION) {
    throw new CompanionProtocolError(`unsupported protocol version: ${String(value)}`);
  }
  return PROTOCOL_VERSION;
}

function rejectUnknownProtocolVersion(value: JsonObject): void {
  if ("protocolVersion" in value && value.protocolVersion !== PROTOCOL_VERSION) {
    protocolVersion(value.protocolVersion);
  }
  const nested = value.input;
  if (typeof nested === "object" && nested !== null && !Array.isArray(nested)) {
    const nestedObject = nested as JsonObject;
    if (
      "protocolVersion" in nestedObject &&
      nestedObject.protocolVersion !== PROTOCOL_VERSION
    ) {
      protocolVersion(nestedObject.protocolVersion);
    }
  }
}

function pairRequest(value: unknown): PairRequest {
  const input = object(value, "pair input");
  exactKeys(
    input,
    [
      "protocolVersion",
      "pairingCode",
      "companionId",
      "profileId",
      "identity",
      "capabilities",
    ],
    "pair input",
  );
  const identity = object(input.identity, "browser identity");
  exactKeys(
    identity,
    ["engine", "browserName", "browserVersion", "os", "profileLabel"],
    "browser identity",
  );
  if (!(["firefox", "chromium", "webKit"] as unknown[]).includes(identity.engine)) {
    throw new CompanionProtocolError("browser identity engine is invalid");
  }
  const capabilities = object(input.capabilities, "companion capabilities");
  exactKeys(
    capabilities,
    ["observe", "navigate", "nativeInput", "tabs", "frames", "nativeDialogs"],
    "companion capabilities",
  );
  return {
    protocolVersion: protocolVersion(input.protocolVersion),
    pairingCode: string(input.pairingCode, "pairingCode", MAX_PAIRING_CODE_BYTES),
    companionId: string(input.companionId, "companionId", MAX_ID_BYTES),
    profileId: string(input.profileId, "profileId", MAX_ID_BYTES),
    identity: {
      engine: identity.engine as BrowserEngine,
      browserName: string(identity.browserName, "browserName"),
      browserVersion: string(identity.browserVersion, "browserVersion"),
      os: string(identity.os, "os"),
      profileLabel: string(identity.profileLabel, "profileLabel"),
    },
    capabilities: {
      observe: boolean(capabilities.observe, "observe"),
      navigate: boolean(capabilities.navigate, "navigate"),
      nativeInput: boolean(capabilities.nativeInput, "nativeInput"),
      tabs: boolean(capabilities.tabs, "tabs"),
      frames: boolean(capabilities.frames, "frames"),
      nativeDialogs: boolean(capabilities.nativeDialogs, "nativeDialogs"),
    },
  };
}

function actionRequest(value: unknown): ActionRequest {
  const input = object(value, "action input");
  exactKeys(
    input,
    [
      "protocolVersion",
      "attachmentId",
      "commandId",
      "pageId",
      "operation",
      "input",
      "deadlineUnixMs",
    ],
    "action input",
  );
  if (!Number.isSafeInteger(input.deadlineUnixMs) || (input.deadlineUnixMs as number) < 0) {
    throw new CompanionProtocolError("deadlineUnixMs must be a nonnegative safe integer");
  }
  return {
    protocolVersion: protocolVersion(input.protocolVersion),
    attachmentId: string(input.attachmentId, "attachmentId", MAX_ID_BYTES),
    commandId: string(input.commandId, "commandId", MAX_ID_BYTES),
    pageId: string(input.pageId, "pageId", MAX_ID_BYTES),
    operation: string(input.operation, "operation", MAX_OPERATION_BYTES),
    input: input.input,
    deadlineUnixMs: input.deadlineUnixMs as number,
  };
}

function browserTarget(value: unknown, name: string): BrowserTarget {
  const target = object(value, name);
  exactKeys(target, ["targetId", "kind"], name);
  if (!(target.kind === "page" || target.kind === "frame")) {
    throw new CompanionProtocolError(`${name} kind is invalid`);
  }
  return {
    targetId: string(target.targetId, `${name} targetId`, MAX_TARGET_ID_BYTES),
    kind: target.kind,
  };
}

function attachmentGrant(value: unknown): AttachmentGrant {
  const input = object(value, "grant input");
  exactKeys(
    input,
    ["protocolVersion", "attachmentId", "profileId", "expiresAtUnixMs", "pages"],
    "grant input",
  );
  if (!Number.isSafeInteger(input.expiresAtUnixMs) || (input.expiresAtUnixMs as number) < 0) {
    throw new CompanionProtocolError("expiresAtUnixMs must be a nonnegative safe integer");
  }
  if (!Array.isArray(input.pages) || input.pages.length > MAX_TARGET_COUNT) {
    throw new CompanionProtocolError(`grant pages must contain at most ${MAX_TARGET_COUNT} entries`);
  }
  const targetIds = new Set<string>();
  const pageIds = new Set<string>();
  const pages = input.pages.map((value, index) => {
    const page = object(value, `grant page ${index}`);
    exactKeys(page, ["targetId", "pageId"], `grant page ${index}`);
    const targetId = string(page.targetId, `grant page ${index} targetId`, MAX_TARGET_ID_BYTES);
    const pageId = uuid(page.pageId, `grant page ${index} pageId`);
    if (targetIds.has(targetId) || pageIds.has(pageId)) {
      throw new CompanionProtocolError("grant pages must be unique");
    }
    targetIds.add(targetId);
    pageIds.add(pageId);
    return { targetId, pageId };
  });
  return {
    protocolVersion: protocolVersion(input.protocolVersion),
    attachmentId: uuid(input.attachmentId, "attachmentId"),
    profileId: uuid(input.profileId, "profileId"),
    expiresAtUnixMs: input.expiresAtUnixMs as number,
    pages,
  };
}

export function parseCompanionRequest(payload: string): CompanionRequest {
  const message = object(parsePayload(payload), "companion request");
  rejectUnknownProtocolVersion(message);
  switch (message.kind) {
    case "pair":
      exactKeys(message, ["kind", "input"], "pair request");
      return { kind: "pair", input: pairRequest(message.input) };
    case "action":
      exactKeys(message, ["kind", "input"], "action request");
      return { kind: "action", input: actionRequest(message.input) };
    case "grant":
      exactKeys(message, ["kind", "input"], "grant request");
      return { kind: "grant", input: attachmentGrant(message.input) };
    case "ping":
      exactKeys(message, ["kind"], "ping request");
      return { kind: "ping" };
    default:
      throw new CompanionProtocolError(`unknown request kind: ${String(message.kind)}`);
  }
}

function parseEventOutput(value: unknown, kind: string): JsonObject {
  return object(value, `${kind} output`);
}

export function parseCompanionEvent(payload: string): CompanionEvent {
  const message = object(parsePayload(payload), "companion event");
  rejectUnknownProtocolVersion(message);
  switch (message.kind) {
    case "paired": {
      exactKeys(message, ["kind", "output"], "paired event");
      const output = parseEventOutput(message.output, "paired");
      exactKeys(output, ["companionId", "profileId"], "paired output");
      return {
        kind: "paired",
        output: {
          companionId: string(output.companionId, "companionId", MAX_ID_BYTES),
          profileId: string(output.profileId, "profileId", MAX_ID_BYTES),
        },
      };
    }
    case "actionCompleted": {
      exactKeys(message, ["kind", "output"], "actionCompleted event");
      const output = parseEventOutput(message.output, "actionCompleted");
      string(output.commandId, "commandId");
      exactKeys(
        output,
        ["commandId", "interactionPath", "output"],
        "actionCompleted output",
      );
      if (
        !(["engineNative", "extensionApi", "hostNative"] as unknown[]).includes(
          output.interactionPath,
        )
      ) {
        throw new CompanionProtocolError("interactionPath is invalid");
      }
      return {
        kind: "actionCompleted",
        output: {
          commandId: string(output.commandId, "commandId", MAX_ID_BYTES),
          interactionPath: output.interactionPath as InteractionPath,
          output: output.output,
        },
      };
    }
    case "actionFailed": {
      exactKeys(message, ["kind", "output"], "actionFailed event");
      const output = parseEventOutput(message.output, "actionFailed");
      string(output.commandId, "commandId");
      exactKeys(
        output,
        ["commandId", "code", "message", "effectUncertain"],
        "actionFailed output",
      );
      return {
        kind: "actionFailed",
        output: {
          commandId: string(output.commandId, "commandId", MAX_ID_BYTES),
          code: string(output.code, "code", MAX_ERROR_CODE_BYTES),
          message: string(output.message, "message", MAX_ERROR_MESSAGE_BYTES),
          effectUncertain: boolean(output.effectUncertain, "effectUncertain"),
        },
      };
    }
    case "targetsDiscovered": {
      exactKeys(message, ["kind", "output"], "targetsDiscovered event");
      const output = parseEventOutput(message.output, "targetsDiscovered");
      exactKeys(output, ["protocolVersion", "profileId", "targets"], "targetsDiscovered output");
      if (!Array.isArray(output.targets) || output.targets.length > MAX_TARGET_COUNT) {
        throw new CompanionProtocolError(
          `targetsDiscovered targets must contain at most ${MAX_TARGET_COUNT} entries`,
        );
      }
      const targets = output.targets.map((value, index) =>
        browserTarget(value, `discovered target ${index}`),
      );
      if (new Set(targets.map((target) => target.targetId)).size !== targets.length) {
        throw new CompanionProtocolError("discovered target IDs must be unique");
      }
      return {
        kind: "targetsDiscovered",
        output: {
          protocolVersion: protocolVersion(output.protocolVersion),
          profileId: uuid(output.profileId, "profileId"),
          targets,
        },
      };
    }
    case "pong":
      exactKeys(message, ["kind"], "pong event");
      return { kind: "pong" };
    default:
      throw new CompanionProtocolError(`unknown event kind: ${String(message.kind)}`);
  }
}

export function serializeCompanionRequest(request: CompanionRequest): string {
  const payload = JSON.stringify(request);
  parseCompanionRequest(payload);
  return payload;
}

export function serializeCompanionEvent(event: CompanionEvent): string {
  const payload = JSON.stringify(event);
  parseCompanionEvent(payload);
  return payload;
}
