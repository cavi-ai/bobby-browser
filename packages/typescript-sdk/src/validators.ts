import type {
  CandidateEvidence,
  CommandError,
  CommandOutcome,
  EventBatch,
  EventGap,
  Evidence,
  InterfaceEvent,
  JsonValue,
  PageEvidence,
  PageState,
  RecoveryDecision,
  SessionState,
  RuntimeInfo,
  TargetFingerprint,
  TargetSpec,
  TextMatch,
  WaitCondition,
  WorkflowCheckpoint,
} from "./contracts.js";

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isString(value: unknown): value is string { return typeof value === "string"; }
function isNullableString(value: unknown): value is string | null { return value === null || isString(value); }
function isStringArray(value: unknown): value is string[] { return Array.isArray(value) && value.every(isString); }
function isSafeUnsigned(value: unknown, maximum = Number.MAX_SAFE_INTEGER): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 && value <= maximum;
}
function isSafeSigned(value: unknown, minimum: number, maximum: number): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= minimum && value <= maximum;
}
function optional<T>(record: Record<string, unknown>, key: string, valid: (value: unknown) => value is T): boolean {
  return !(key in record) || valid(record[key]);
}

export function isUuid(value: unknown): value is string {
  return typeof value === "string" && /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(value);
}

export function isIsoTimestamp(value: unknown): value is string {
  if (typeof value !== "string") return false;
  const match = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.\d{1,9})?(?:Z|([+-])(\d{2}):(\d{2}))$/.exec(value);
  if (!match) return false;
  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  const hour = Number(match[4]);
  const minute = Number(match[5]);
  const second = Number(match[6]);
  const offsetHour = match[8] === undefined ? 0 : Number(match[8]);
  const offsetMinute = match[9] === undefined ? 0 : Number(match[9]);
  const leap = year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0);
  const days = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
  return month >= 1 && month <= 12
    && day >= 1 && day <= days[month - 1]!
    && hour <= 23 && minute <= 59 && second <= 59
    && offsetHour <= 23 && offsetMinute <= 59
    && Number.isFinite(Date.parse(value));
}

export function isLowerSha256(value: unknown): value is string {
  return typeof value === "string" && /^[0-9a-f]{64}$/.test(value);
}

function isStringMap(value: unknown): value is Record<string, string> {
  return isRecord(value) && Object.values(value).every(isString);
}

function isJsonValue(value: unknown, depth = 0): value is JsonValue {
  if (value === null || typeof value === "string" || typeof value === "boolean") return true;
  if (typeof value === "number") return Number.isFinite(value);
  if (depth >= 64) return false;
  if (Array.isArray(value)) return value.every((item) => isJsonValue(item, depth + 1));
  return isRecord(value) && Object.values(value).every((item) => isJsonValue(item, depth + 1));
}

export function isRuntimeInfo(value: unknown): value is RuntimeInfo {
  return isRecord(value)
    && isString(value.version)
    && isStringArray(value.capabilities)
    && isSafeUnsigned(value.active_sessions)
    && isSafeUnsigned(value.queued_jobs)
    && isSafeUnsigned(value.uptime_ms);
}

export function isSessionState(value: unknown): value is SessionState {
  return isRecord(value)
    && isUuid(value.id)
    && isString(value.profile)
    && isNullableString(value.proxy)
    && Array.isArray(value.page_ids) && value.page_ids.every(isUuid)
    && isIsoTimestamp(value.created_at)
    && isIsoTimestamp(value.last_used_at);
}

export function isPageState(value: unknown): value is PageState {
  return isRecord(value)
    && isUuid(value.id)
    && isUuid(value.session_id)
    && isNullableString(value.url)
    && (value.mode === "Document" || value.mode === "Interactive" || value.mode === "Render")
    && isString(value.ready_state)
    && isSafeUnsigned(value.pending_requests);
}

function isTextMatch(value: unknown): value is TextMatch {
  return isRecord(value)
    && (value.kind === "exact" || value.kind === "contains" || value.kind === "regex")
    && isString(value.value);
}

function isTargetSpec(value: unknown, depth = 0): value is TargetSpec {
  if (!isRecord(value) || depth >= 64) return false;
  return isNullableString(value.css)
    && isNullableString(value.testId)
    && isNullableString(value.role)
    && isNullableString(value.accessibleName)
    && isNullableString(value.label)
    && (value.text === null || isTextMatch(value.text))
    && isStringMap(value.attributes)
    && Array.isArray(value.framePath) && value.framePath.every((item) => isTargetSpec(item, depth + 1))
    && Array.isArray(value.shadowPath) && value.shadowPath.every((item) => isTargetSpec(item, depth + 1))
    && (value.ordinal === null || isSafeUnsigned(value.ordinal))
    && typeof value.allowBestMatch === "boolean";
}

function isTargetFingerprint(value: unknown): value is TargetFingerprint {
  return isRecord(value)
    && isUuid(value.pageId)
    && isNullableString(value.frame)
    && isNullableString(value.role)
    && isNullableString(value.name)
    && isStringMap(value.stableAttributes);
}

function isCandidateEvidence(value: unknown): value is CandidateEvidence {
  return isRecord(value)
    && isNullableString(value.role)
    && isNullableString(value.name)
    && isSafeSigned(value.score, -2_147_483_648, 2_147_483_647)
    && isStringArray(value.reasons);
}

function isPageEvidence(value: unknown): value is PageEvidence {
  return isRecord(value) && isUuid(value.pageId) && isString(value.url) && isString(value.title);
}

function isWaitCondition(value: unknown): value is WaitCondition {
  if (!isRecord(value)) return false;
  if (value.kind === "element") return isTargetSpec(value.target) && (value.state === "attached" || value.state === "detached" || value.state === "visible" || value.state === "hidden" || value.state === "enabled" || value.state === "disabled");
  if (value.kind === "text" || value.kind === "value") return isTargetSpec(value.target) && isTextMatch(value.matcher);
  if (value.kind === "url") return isTextMatch(value.matcher);
  if (value.kind === "document") return value.ready === "commit" || value.ready === "domContentLoaded" || value.ready === "interactive" || value.ready === "networkIdle";
  return value.kind === "networkQuiet" && isSafeUnsigned(value.idleMs) && isSafeUnsigned(value.maxInFlight);
}

function validExecutionPathOptionalFields(value: Record<string, unknown>): boolean {
  return optional(value, "finalUrl", isString)
    && optional(value, "contentType", isString)
    && optional(value, "status", (candidate): candidate is number => isSafeUnsigned(candidate, 65_535))
    && optional(value, "redirectChain", isStringArray);
}

export function isEvidence(value: unknown): value is Evidence {
  if (!isRecord(value)) return false;
  switch (value.kind) {
    case "executionPath":
      return (value.path === "directHttp" || value.path === "chromium" || value.path === "chromiumFallback")
        && (value.reason === "eligibleStaticDocument" || value.reason === "eligibleExplicitDownload" || value.reason === "ineligibleCommand" || value.reason === "semanticTargetRequired" || value.reason === "javascriptRequired" || value.reason === "unsupportedContentType" || value.reason === "stateConflict" || value.reason === "policyRequired")
        && isSafeUnsigned(value.stateVersion)
        && isSafeUnsigned(value.elapsedMs)
        && (value.bytes === null || isSafeUnsigned(value.bytes))
        && (value.sha256 === null || isLowerSha256(value.sha256))
        && ((value.bytes === null) === (value.sha256 === null))
        && validExecutionPathOptionalFields(value);
    case "navigation": return isString(value.url) && isString(value.title);
    case "inspection": return isNullableString(value.selector) && isString(value.url) && isString(value.title) && isString(value.text) && isNullableString(value.html);
    case "element": return isString(value.selector) && isNullableString(value.text);
    case "upload": return isString(value.selector) && isStringArray(value.paths);
    case "page": return isUuid(value.pageId) && isString(value.url) && isString(value.title);
    case "pages": return Array.isArray(value.pages) && value.pages.every(isPageEvidence);
    case "popup": return isUuid(value.openerPageId) && isUuid(value.pageId) && isString(value.url) && isString(value.title);
    case "download": return isString(value.filename) && isString(value.path) && isSafeUnsigned(value.bytes) && isLowerSha256(value.sha256);
    case "resolution": return isTargetSpec(value.target) && isTargetFingerprint(value.fingerprint) && Array.isArray(value.candidates) && value.candidates.every(isCandidateEvidence) && typeof value.bestMatchAuthorized === "boolean";
    case "wait": return isWaitCondition(value.condition) && isSafeUnsigned(value.elapsedMs) && isSafeUnsigned(value.observations);
    case "screenshot": return isString(value.artifactId) && isString(value.mediaType) && isSafeUnsigned(value.width, 4_294_967_295) && isSafeUnsigned(value.height, 4_294_967_295) && isSafeUnsigned(value.bytes) && isLowerSha256(value.sha256);
    default: return false;
  }
}

function isEvidenceArray(value: unknown): value is Evidence[] { return Array.isArray(value) && value.every(isEvidence); }

export function isCommandError(value: unknown): value is CommandError {
  return isRecord(value)
    && (value.code === "invalidRequest" || value.code === "notFound" || value.code === "deadlineExceeded" || value.code === "browserLaunchFailed" || value.code === "browserCommandFailed" || value.code === "verificationFailed" || value.code === "journalFailed" || value.code === "resourceExhausted" || value.code === "policyDenied" || value.code === "internal" || value.code === "targetNotFound" || value.code === "targetAmbiguous" || value.code === "frameNotFound" || value.code === "shadowRootUnavailable" || value.code === "targetDetached" || value.code === "waitConditionTimedOut" || value.code === "screenshotCaptureFailed" || value.code === "networkPolicyDenied" || value.code === "httpResponseTooLarge" || value.code === "httpTransferFailed" || value.code === "httpStateConflict" || value.code === "httpEquivalenceUnproven")
    && isString(value.message)
    && (value.layer === "interface" || value.layer === "broker" || value.layer === "workflow" || value.layer === "page" || value.layer === "driver" || value.layer === "browser" || value.layer === "network" || value.layer === "site" || value.layer === "journal")
    && typeof value.retryable === "boolean";
}

export function isCommandOutcome(value: unknown): value is CommandOutcome {
  if (!isRecord(value) || !isUuid(value.commandId)) return false;
  switch (value.status) {
    case "completed": return isEvidenceArray(value.evidence);
    case "retryableFailure":
    case "policyDenied":
    case "failed": return isCommandError(value.error);
    case "needsReconciliation": return isCommandError(value.error) && isEvidenceArray(value.evidence);
    case "resourceExhausted": return isCommandError(value.error) && isSafeUnsigned(value.retryAfterMs);
    case "restarted": return isUuid(value.priorAttemptId) && isUuid(value.attemptId) && isString(value.reason);
    default: return false;
  }
}

function isCheckpointInvariant(value: unknown): boolean {
  return isRecord(value) && ((value.kind === "url" || value.kind === "title")
    ? isString(value.value)
    : value.kind === "text" && isString(value.selector) && isString(value.value));
}

export function isRecoveryDecision(value: unknown): value is RecoveryDecision {
  if (!isRecord(value) || !isUuid(value.checkpointId)) return false;
  if (value.status === "resumed") return isUuid(value.attemptId) && isEvidenceArray(value.evidence);
  if (value.status === "needsReconciliation") return isUuid(value.attemptId) && isString(value.reason) && isEvidenceArray(value.evidence);
  return value.status === "restarted"
    && isRecord(value.lineage)
    && isUuid(value.lineage.workflowId)
    && isUuid(value.lineage.abandonedAttemptId)
    && isUuid(value.lineage.attemptId)
    && isString(value.lineage.reason);
}

export function isWorkflowCheckpoint(value: unknown): value is WorkflowCheckpoint {
  return isRecord(value)
    && value.schemaVersion === 1
    && isUuid(value.checkpointId)
    && isUuid(value.workflowId)
    && isUuid(value.attemptId)
    && isUuid(value.sessionId)
    && isUuid(value.pageId)
    && isString(value.restartUrl)
    && isString(value.currentUrl)
    && (value.cursor === null || isUuid(value.cursor))
    && (value.boundaryCommandId === null || isUuid(value.boundaryCommandId))
    && (value.recoveryClass === "replayable" || value.recoveryClass === "reconciliable" || value.recoveryClass === "boundary")
    && Array.isArray(value.invariants) && value.invariants.every(isCheckpointInvariant)
    && isStringArray(value.replayableInputs)
    && isEvidenceArray(value.evidence)
    && Array.isArray(value.recoveryHistory) && value.recoveryHistory.every((record) => isRecord(record) && isIsoTimestamp(record.recordedAt) && isRecoveryDecision(record.decision))
    && isIsoTimestamp(value.createdAt);
}

function isInterfaceEvent(value: unknown): value is InterfaceEvent {
  return isRecord(value)
    && isSafeUnsigned(value.cursor)
    && isString(value.kind)
    && "payload" in value
    && isJsonValue(value.payload);
}

export function isEventBatch(value: unknown): value is EventBatch {
  if (!isRecord(value) || !isSafeUnsigned(value.latestAvailable) || !Array.isArray(value.events) || value.events.length === 0 || !value.events.every(isInterfaceEvent)) return false;
  let previous = -1;
  for (const event of value.events) {
    if (event.cursor <= previous || event.cursor > value.latestAvailable) return false;
    previous = event.cursor;
  }
  return true;
}

export function isEventGap(value: unknown): value is EventGap {
  return isRecord(value)
    && (value.reason === "historyLost" || value.reason === "invalidLimit" || value.reason === "invalidCursor")
    && isSafeUnsigned(value.earliestAvailable);
}
