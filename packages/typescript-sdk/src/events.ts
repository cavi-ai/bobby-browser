import type { EventBatch, EventGap, InterfaceError, InterfaceEvent } from "./contracts.js";

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function isInterfaceError(value: unknown): value is InterfaceError {
  return isRecord(value) && isInterfaceErrorCode(value.code) && isErrorLayer(value.layer) && typeof value.message === "string" && typeof value.correlationId === "string" && (typeof value.commandId === "string" || value.commandId === null) && typeof value.retryable === "boolean" && (typeof value.retryAfterMs === "number" || value.retryAfterMs === null) && typeof value.reconciliationRequired === "boolean" && (value.requiredCapability === null || isCapability(value.requiredCapability));
}

export function isEventGap(value: unknown): value is EventGap {
  return isRecord(value) && (value.reason === "historyLost" || value.reason === "invalidLimit" || value.reason === "invalidCursor") && typeof value.earliestAvailable === "number" && Number.isSafeInteger(value.earliestAvailable) && value.earliestAvailable >= 0;
}

export function isEventBatch(value: unknown): value is EventBatch {
  return isRecord(value) && typeof value.latestAvailable === "number" && Number.isSafeInteger(value.latestAvailable) && value.latestAvailable >= 0 && Array.isArray(value.events) && value.events.every(isInterfaceEvent);
}

function isInterfaceEvent(value: unknown): value is InterfaceEvent {
  return isRecord(value) && typeof value.cursor === "number" && Number.isSafeInteger(value.cursor) && value.cursor >= 0 && typeof value.kind === "string" && "payload" in value;
}

export function isErrorLayer(value: unknown): boolean {
  return value === "interface" || value === "broker" || value === "workflow" || value === "page" || value === "driver" || value === "browser" || value === "network" || value === "site" || value === "journal";
}

export function isInterfaceErrorCode(value: unknown): boolean {
  return value === "invalidRequest" || value === "unsupportedInterfaceVersion" || value === "invalidIdempotencyKey" || value === "idempotencyConflict" || value === "deadlineExceeded" || value === "authenticationFailed" || value === "tokenExpired" || value === "missingCapability" || value === "malformedScope" || value === "artifactDenied" || value === "unsupportedOperation" || value === "notFound" || value === "resourceExhausted" || value === "internal";
}

function isCapability(value: unknown): boolean {
  return value === "sessionRead" || value === "sessionWrite" || value === "pageRead" || value === "pageWrite" || value === "browserMutate" || value === "recoveryRead" || value === "recoveryWrite" || value === "artifactRead" || value === "artifactCapture";
}
