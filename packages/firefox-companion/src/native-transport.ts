import {
  MAX_COMPANION_PAYLOAD_BYTES,
  PROTOCOL_VERSION,
  parseCompanionEvent,
  parseCompanionRequest,
  type BrowserIdentity,
  type CompanionCapabilities,
} from "./protocol.js";

export const NATIVE_HOST_NAME = "com.bobby_browser.companion";
export const MAX_NATIVE_MESSAGE_BYTES = MAX_COMPANION_PAYLOAD_BYTES;
const INITIAL_RECONNECT_DELAY_MS = 100;
const MAX_RECONNECT_DELAY_MS = 5_000;

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
};

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

function isNativePairRequest(message: unknown): message is NativePairRequest {
  if (!isObject(message) || message.kind !== "pair" || !isObject(message.input)) {
    return false;
  }
  const input = message.input;
  return (
    Object.keys(message).sort().join(",") === "input,kind" &&
    Object.keys(input).sort().join(",") ===
      "capabilities,companionId,identity,profileId,protocolVersion" &&
    input.protocolVersion === PROTOCOL_VERSION &&
    typeof input.companionId === "string" &&
    input.companionId.length > 0 &&
    typeof input.profileId === "string" &&
    input.profileId.length > 0 &&
    isObject(input.identity) &&
    isObject(input.capabilities) &&
    !("pairingCode" in input)
  );
}

function validateOutbound(message: unknown): void {
  const encoded = encodeBounded(message);
  if (isNativePairRequest(message)) return;
  try {
    parseCompanionRequest(encoded);
    return;
  } catch {}
  try {
    parseCompanionEvent(encoded);
    return;
  } catch {}
  throw new Error("native message is not a valid protocol v1 message");
}

function validateInbound(message: unknown): unknown | undefined {
  let encoded: string;
  try {
    encoded = encodeBounded(message);
  } catch {
    return undefined;
  }
  try {
    return parseCompanionRequest(encoded);
  } catch {}
  try {
    return parseCompanionEvent(encoded);
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
  #pairRequest: NativePairRequest | undefined;

  constructor(dependencies: NativeTransportDependencies) {
    this.#dependencies = dependencies;
  }

  start(listener: (message: unknown) => void | Promise<void>): void {
    this.#listener = listener;
    if (this.#running) return;
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
    if (this.#reconnectHandle !== undefined) {
      (this.#dependencies.cancelReconnect ?? clearTimeout)(this.#reconnectHandle as never);
      this.#reconnectHandle = undefined;
    }
    const port = this.#port;
    this.#port = undefined;
    this.#pairRequest = undefined;
    port?.disconnect();
  }

  #connect(): void {
    if (!this.#running) return;
    try {
      const port = this.#dependencies.connectNative(NATIVE_HOST_NAME);
      this.#port = port;
      port.onMessage.addListener((message) => {
        if (port !== this.#port) return;
        const validated = validateInbound(message);
        if (validated !== undefined) void this.#listener?.(validated);
      });
      port.onDisconnect.addListener(() => {
        if (port !== this.#port) return;
        this.#port = undefined;
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
