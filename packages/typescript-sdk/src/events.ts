import type { InterfaceError } from "./contracts.js";
import { isRecord, isUuid } from "./validators.js";

export { isEventBatch, isEventGap, isRecord } from "./validators.js";

export function isInterfaceError(value: unknown): value is InterfaceError {
  return isRecord(value) && isInterfaceErrorCode(value.code) && isErrorLayer(value.layer) && typeof value.message === "string" && isUuid(value.correlationId) && (isUuid(value.commandId) || value.commandId === null) && typeof value.retryable === "boolean" && (value.retryAfterMs === null || (Number.isSafeInteger(value.retryAfterMs) && (value.retryAfterMs as number) >= 0)) && typeof value.reconciliationRequired === "boolean" && (value.requiredCapability === null || isCapability(value.requiredCapability));
}

export function isErrorLayer(value: unknown): boolean {
  return value === "interface" || value === "broker" || value === "workflow" || value === "page" || value === "driver" || value === "browser" || value === "network" || value === "site" || value === "journal";
}

export function isInterfaceErrorCode(value: unknown): boolean {
  return value === "invalidRequest" || value === "unsupportedInterfaceVersion" || value === "invalidIdempotencyKey" || value === "idempotencyConflict" || value === "deadlineExceeded" || value === "authenticationFailed" || value === "tokenExpired" || value === "missingCapability" || value === "malformedScope" || value === "artifactDenied" || value === "unsupportedOperation" || value === "notFound" || value === "resourceExhausted" || value === "internal";
}

function isCapability(value: unknown): boolean {
  return value === "session:read" || value === "session:write" || value === "page:read" || value === "page:write" || value === "browser:mutate" || value === "file:upload" || value === "file:download" || value === "javascript:evaluate" || value === "recovery:read" || value === "recovery:write" || value === "artifact:read" || value === "artifact:capture";
}
