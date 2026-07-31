import {
  MAX_COMPANION_PAYLOAD_BYTES,
  PROTOCOL_VERSION,
  parseCompanionEvent,
  parseCompanionRequest,
  type BrowserIdentity,
  type CompanionCapabilities,
  type CompanionEvent,
  type CompanionRequest,
} from "./protocol.js";

export const NATIVE_HOST_NAME = "com.bobby_browser.companion";
export const MAX_NATIVE_MESSAGE_BYTES = MAX_COMPANION_PAYLOAD_BYTES;
const INITIAL_RECONNECT_DELAY_MS = 100;
const MAX_RECONNECT_DELAY_MS = 5_000;
export const TERMINAL_AUTH_COOLDOWN_MS = 15_000;

export type NativePairRequest = {
  kind: "pair";
  input: {
    protocolVersion: typeof PROTOCOL_VERSION;
    companionId: string;
    profileId: string;
    identity: BrowserIdentity;
    capabilities: CompanionCapabilities;
  };
};

type Listener<T> = { addListener(listener: T): void };

export type NativePort = {
  postMessage(message: unknown): void;
  disconnect(): void;
  onMessage: Listener<(message: unknown) => void>;
  onDisconnect: Listener<() => void>;
};

export type NativeTransportDependencies = {
  connectNative(hostName: string): NativePort;
  scheduleReconnect?: (callback: () => void, delayMs: number) => unknown;
  cancelReconnect?: (handle: unknown) => void;
  onListenerError?: (error: unknown) => void;
};

export type NativeInboundMessage =
  | Exclude<CompanionRequest, { kind: "pair" }>
  | Extract<CompanionEvent, { kind: "paired" }>
  | NativeTerminalStatus;

export type NativeTerminalStatus = {
  kind: "nativeStatus";
  output: { state: "invalidAuth" | "revoked" };
};

const MAX_NATIVE_METADATA_BYTES = 256;
const FORBIDDEN_SECRET_FIELD =
  /(?:pairing[_-]?code|bearer|authorization|endpoint|credential|password|passwd|api[-_]?key|token|secret)/i;
const SECRET_VALUE = /(?:^|\s)(?:bearer|basic)\s+/i;
const PRIVATE_SECRET_VALUE = /private[-_ ]?(?:token|secret|key)/i;
const SELECTOR_SYNTAX = /[\s>+~()[\]]/;
const SENSITIVE_URL_QUERY_KEYS = new Set([
  "authorization",
  "bearer",
  "token",
  "accesstoken",
  "refreshtoken",
  "sessiontoken",
  "secret",
  "clientsecret",
  "password",
  "passwd",
  "credential",
  "apikey",
  "accesskey",
  "privatekey",
  "key",
]);

function encodeBounded(message: unknown): string {
  let encoded: string;
  try {
    encoded = JSON.stringify(message);
  } catch {
    throw new Error("native message must be JSON serializable");
  }
  if (encoded === undefined) {
    throw new Error("native message must be JSON serializable");
  }
  if (new TextEncoder().encode(encoded).byteLength > MAX_NATIVE_MESSAGE_BYTES) {
    throw new Error("native message exceeds the 1 MiB limit");
  }
  return encoded;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function exactKeys(value: Record<string, unknown>, expected: readonly string[]): boolean {
  const actual = Object.keys(value).sort();
  const sortedExpected = [...expected].sort();
  return (
    actual.length === sortedExpected.length &&
    actual.every((key, index) => key === sortedExpected[index])
  );
}

function boundedString(value: unknown): value is string {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    new TextEncoder().encode(value).byteLength <= MAX_NATIVE_METADATA_BYTES
  );
}

function isBrowserIdentity(value: unknown): value is BrowserIdentity {
  if (!isObject(value) || !exactKeys(value, ["engine", "browserName", "browserVersion", "os", "profileLabel"])) {
    return false;
  }
  return (
    (["firefox", "chromium", "webKit"] as unknown[]).includes(value.engine) &&
    boundedString(value.browserName) &&
    boundedString(value.browserVersion) &&
    boundedString(value.os) &&
    boundedString(value.profileLabel)
  );
}

function isCapabilities(value: unknown): value is CompanionCapabilities {
  if (
    !isObject(value) ||
    !exactKeys(value, ["observe", "navigate", "nativeInput", "tabs", "frames", "nativeDialogs"])
  ) {
    return false;
  }
  return Object.values(value).every((item) => typeof item === "boolean");
}

function assertSafeUrl(value: string): void {
  if (!/^[a-z][a-z\d+.-]*:/i.test(value)) return;
  // A CSS path such as `div:nth-of-type(2) > a` parses as a URL with the scheme
  // `div:`, so selector syntax without an explicit `scheme://` is not a URL.
  if (SELECTOR_SYNTAX.test(value) && !/^[a-z][a-z\d+.-]*:\/\//i.test(value)) return;
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new Error("extension channel contains an invalid URL");
  }
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password) {
    throw new Error("extension channel contains endpoint or URL secret material");
  }
  for (const [name, item] of url.searchParams) {
    const normalizedName = name.toLowerCase().replaceAll(/[-_]/g, "");
    if (
      SENSITIVE_URL_QUERY_KEYS.has(normalizedName) ||
      SECRET_VALUE.test(item) ||
      PRIVATE_SECRET_VALUE.test(item)
    ) {
      throw new Error("extension channel contains endpoint or URL secret material");
    }
  }
  if (SECRET_VALUE.test(url.pathname) || PRIVATE_SECRET_VALUE.test(url.pathname + url.hash)) {
    throw new Error("extension channel contains endpoint or URL secret material");
  }
}

function assertExtensionSafe(
  value: unknown,
  depth = 0,
  budget: { nodes: number } = { nodes: 20_000 },
): void {
  budget.nodes -= 1;
  if (budget.nodes < 0 || depth > 32) {
    throw new Error("extension channel message nesting exceeds the safety limit");
  }
  if (typeof value === "string") {
    if (SECRET_VALUE.test(value) || PRIVATE_SECRET_VALUE.test(value)) {
      throw new Error("extension channel contains secret material");
    }
    assertSafeUrl(value);
    return;
  }
  if (Array.isArray(value)) {
    for (const item of value) assertExtensionSafe(item, depth + 1, budget);
    return;
  }
  if (!isObject(value)) return;
  for (const [name, item] of Object.entries(value)) {
    if (FORBIDDEN_SECRET_FIELD.test(name)) {
      throw new Error("extension channel contains a forbidden secret field");
    }
    assertExtensionSafe(item, depth + 1, budget);
  }
}

function isNativePairRequest(message: unknown): message is NativePairRequest {
  if (!isObject(message) || message.kind !== "pair" || !isObject(message.input)) {
    return false;
  }
  const input = message.input;
  return (
    exactKeys(message, ["kind", "input"]) &&
    exactKeys(input, ["protocolVersion", "companionId", "profileId", "identity", "capabilities"]) &&
    input.protocolVersion === PROTOCOL_VERSION &&
    boundedString(input.companionId) &&
    boundedString(input.profileId) &&
    isBrowserIdentity(input.identity) &&
    isCapabilities(input.capabilities)
  );
}

function validateOutbound(message: unknown): void {
  const encoded = encodeBounded(message);
  assertExtensionSafe(message);
  if (isNativePairRequest(message)) return;
  try {
    const event = parseCompanionEvent(encoded);
    if (event.kind !== "paired") return;
  } catch {}
  throw new Error("native message is invalid for the outbound extension direction");
}

export function parseNativeInboundMessage(message: unknown): NativeInboundMessage {
  const encoded = typeof message === "string" ? message : encodeBounded(message);
  if (new TextEncoder().encode(encoded).byteLength > MAX_NATIVE_MESSAGE_BYTES) {
    throw new Error("native message exceeds the 1 MiB limit");
  }
  let decoded: unknown;
  try {
    decoded = typeof message === "string" ? (JSON.parse(message) as unknown) : message;
  } catch {
    throw new Error("native message is not valid JSON");
  }
  assertExtensionSafe(decoded);
  if (
    isObject(decoded) &&
    exactKeys(decoded, ["kind", "output"]) &&
    decoded.kind === "nativeStatus" &&
    isObject(decoded.output) &&
    exactKeys(decoded.output, ["state"]) &&
    (decoded.output.state === "invalidAuth" || decoded.output.state === "revoked")
  ) {
    return decoded as NativeTerminalStatus;
  }
  try {
    const request = parseCompanionRequest(encoded);
    if (request.kind !== "pair") return request;
  } catch {}
  try {
    const event = parseCompanionEvent(encoded);
    if (event.kind === "paired") return event;
  } catch {}
  throw new Error("native message is invalid for the inbound native direction");
}

function validateInbound(message: unknown): NativeInboundMessage | undefined {
  try {
    return parseNativeInboundMessage(message);
  } catch {
    return undefined;
  }
}

export class NativeCompanionTransport {
  readonly #dependencies: NativeTransportDependencies;
  #port: NativePort | undefined;
  #listener: ((message: unknown) => void | Promise<void>) | undefined;
  #reconnectHandle: unknown;
  #reconnectDelayMs = INITIAL_RECONNECT_DELAY_MS;
  #running = false;
  #terminalAuth = false;
  #portValidated = false;
  #pairRequest: NativePairRequest | undefined;

  constructor(dependencies: NativeTransportDependencies) {
    this.#dependencies = dependencies;
  }

  start(listener: (message: unknown) => void | Promise<void>): void {
    this.#listener = listener;
    if (this.#running || this.#terminalAuth) return;
    this.#running = true;
    this.#connect();
  }

  send(message: unknown): void {
    validateOutbound(message);
    if (isNativePairRequest(message)) this.#pairRequest = message;
    if (!this.#port) throw new Error("native companion is not connected");
    this.#port.postMessage(message);
  }

  stop(): void {
    this.#running = false;
    this.#terminalAuth = false;
    if (this.#reconnectHandle !== undefined) {
      (this.#dependencies.cancelReconnect ?? clearTimeout)(this.#reconnectHandle as never);
      this.#reconnectHandle = undefined;
    }
    const port = this.#port;
    this.#port = undefined;
    this.#portValidated = false;
    this.#pairRequest = undefined;
    port?.disconnect();
  }

  #connect(): void {
    if (!this.#running) return;
    try {
      const port = this.#dependencies.connectNative(NATIVE_HOST_NAME);
      this.#port = port;
      this.#portValidated = false;
      port.onMessage.addListener((message) => {
        if (port !== this.#port) return;
        const validated = validateInbound(message);
        if (validated === undefined || !this.#listener) return;
        if (validated.kind === "nativeStatus") {
          // A terminal auth status (invalidAuth/revoked) means the host read a
          // stale or rotated descriptor, not that pairing is impossible
          // forever: the next respawn reads the current descriptor. Cool down
          // instead of stopping permanently so a rotated pairing code
          // self-heals without a browser restart.
          this.#terminalAuth = true;
          this.#port = undefined;
          if (this.#reconnectHandle !== undefined) {
            (this.#dependencies.cancelReconnect ?? clearTimeout)(this.#reconnectHandle as never);
            this.#reconnectHandle = undefined;
          }
          if (this.#running) {
            const schedule =
              this.#dependencies.scheduleReconnect ??
              ((callback: () => void, delayMs: number) => setTimeout(callback, delayMs));
            this.#reconnectHandle = schedule(() => {
              this.#reconnectHandle = undefined;
              this.#terminalAuth = false;
              this.#reconnectDelayMs = INITIAL_RECONNECT_DELAY_MS;
              this.#connect();
            }, TERMINAL_AUTH_COOLDOWN_MS);
          }
          return;
        }
        if (!this.#portValidated) {
          this.#portValidated = true;
          this.#reconnectDelayMs = INITIAL_RECONNECT_DELAY_MS;
        }
        try {
          void Promise.resolve(this.#listener(validated)).catch((error: unknown) => {
            this.#dependencies.onListenerError?.(error);
          });
        } catch (error) {
          this.#dependencies.onListenerError?.(error);
        }
      });
      port.onDisconnect.addListener(() => {
        if (port !== this.#port) return;
        this.#port = undefined;
        this.#portValidated = false;
        this.#scheduleReconnect();
      });
      if (this.#pairRequest) port.postMessage(this.#pairRequest);
    } catch {
      this.#port = undefined;
      this.#scheduleReconnect();
    }
  }

  #scheduleReconnect(): void {
    if (!this.#running || this.#reconnectHandle !== undefined) return;
    const schedule =
      this.#dependencies.scheduleReconnect ??
      ((callback: () => void, delayMs: number) => setTimeout(callback, delayMs));
    const delay = this.#reconnectDelayMs;
    this.#reconnectDelayMs = Math.min(delay * 2, MAX_RECONNECT_DELAY_MS);
    this.#reconnectHandle = schedule(() => {
      this.#reconnectHandle = undefined;
      this.#connect();
    }, delay);
  }
}
