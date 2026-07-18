import type { EventGap, InterfaceError } from "./contracts.js";

export type RuntimeClientErrorKind = "transport" | "protocol" | "http" | "aborted" | "deadline";

/** A redacted error surface: no request headers, request body, URL credentials, or bearer token are retained. */
export class RuntimeClientError extends Error {
  readonly kind: RuntimeClientErrorKind;
  readonly status: number | undefined;
  readonly code: string | undefined;
  readonly correlationId: string | undefined;
  readonly commandId: string | null | undefined;
  readonly retryable: boolean | undefined;
  readonly retryAfterMs: number | null | undefined;
  readonly reconciliationRequired: boolean | undefined;
  readonly requiredCapability: string | null | undefined;
  readonly eventGap: EventGap | undefined;

  constructor(options: { kind: RuntimeClientErrorKind; status?: number; interfaceError?: InterfaceError; eventGap?: EventGap; message?: string }) {
    const remote = options.interfaceError;
    super(options.message ?? (remote ? `Runtime request failed: ${options.status ?? "unknown"} ${remote.code}` : `Runtime client ${options.kind} failure`));
    this.name = "RuntimeClientError";
    this.kind = options.kind;
    this.status = options.status;
    this.code = remote?.code;
    this.correlationId = remote?.correlationId;
    this.commandId = remote?.commandId;
    this.retryable = remote?.retryable;
    this.retryAfterMs = remote?.retryAfterMs;
    this.reconciliationRequired = remote?.reconciliationRequired;
    this.requiredCapability = remote?.requiredCapability;
    this.eventGap = options.eventGap;
  }

  [Symbol.for("nodejs.util.inspect.custom")](): string {
    return `RuntimeClientError { kind: ${this.kind}, status: ${this.status ?? "undefined"}, code: ${this.code ?? "undefined"} }`;
  }
}
