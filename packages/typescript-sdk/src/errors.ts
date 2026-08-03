import type { Capability, EventGap, InterfaceError, InterfaceErrorCode } from "./contracts.js";

/** Classification of {@link RuntimeClientError}. */
export type RuntimeClientErrorKind = "transport" | "protocol" | "http" | "aborted" | "deadline";

/** Strip secrets from error strings (typically removes the bearer token). */
export type RuntimeErrorRedactor = (value: string) => string;

/**
 * Client-side error with secrets redacted from `message`, `stack`, and inspect
 * output. Does not retain request headers, bodies, URL credentials, or tokens.
 */
export class RuntimeClientError extends Error {
  readonly #redact: RuntimeErrorRedactor;
  readonly kind: RuntimeClientErrorKind;
  readonly status: number | undefined;
  readonly code: InterfaceErrorCode | undefined;
  readonly correlationId: string | undefined;
  readonly commandId: string | null | undefined;
  readonly retryable: boolean | undefined;
  readonly retryAfterMs: number | null | undefined;
  readonly reconciliationRequired: boolean | undefined;
  readonly requiredCapability: Capability | null | undefined;
  readonly eventGap: EventGap | undefined;

  constructor(options: { kind: RuntimeClientErrorKind; status?: number; interfaceError?: InterfaceError; eventGap?: EventGap; message?: string; redactor?: RuntimeErrorRedactor }) {
    const remote = options.interfaceError;
    const redact = options.redactor ?? ((value: string) => value);
    const safeCode = remote && redact(remote.code) === remote.code ? remote.code : undefined;
    const safeCapability = remote?.requiredCapability === null
      ? null
      : remote?.requiredCapability !== undefined && redact(remote.requiredCapability) === remote.requiredCapability ? remote.requiredCapability : undefined;
    const rawMessage = options.message ?? (remote ? `Runtime request failed: ${options.status ?? "unknown"} ${safeCode ?? "[redacted]"}` : `Runtime client ${options.kind} failure`);
    super(redact(rawMessage));
    this.#redact = redact;
    this.name = redact("RuntimeClientError");
    this.kind = options.kind;
    this.status = options.status;
    this.code = safeCode;
    this.correlationId = remote === undefined ? undefined : redact(remote.correlationId);
    this.commandId = remote?.commandId === null ? null : remote?.commandId === undefined ? undefined : redact(remote.commandId);
    this.retryable = remote?.retryable;
    this.retryAfterMs = remote?.retryAfterMs;
    this.reconciliationRequired = remote?.reconciliationRequired;
    this.requiredCapability = safeCapability;
    this.eventGap = options.eventGap && redact(options.eventGap.reason) === options.eventGap.reason ? options.eventGap : undefined;
    for (const key of ["kind", "status", "code", "correlationId", "commandId", "retryable", "retryAfterMs", "reconciliationRequired", "requiredCapability", "eventGap"]) {
      Object.defineProperty(this, key, { enumerable: false });
    }
    if (this.stack !== undefined) this.stack = redact(this.stack);
  }

  [Symbol.for("nodejs.util.inspect.custom")](): string {
    return this.#redact(`RuntimeClientError { kind: ${this.kind}, status: ${this.status ?? "undefined"}, code: ${this.code ?? "undefined"} }`);
  }

  /** JSON-safe projection for logging (already redacted). */
  toJSON(): Record<string, unknown> {
    return {
      name: this.name,
      message: this.message,
      kind: this.#redact(this.kind),
      status: this.status,
      code: this.code,
      correlationId: this.correlationId,
      commandId: this.commandId,
      retryable: this.retryable,
      retryAfterMs: this.retryAfterMs,
      reconciliationRequired: this.reconciliationRequired,
      requiredCapability: this.requiredCapability,
      eventGap: this.eventGap,
    };
  }
}
