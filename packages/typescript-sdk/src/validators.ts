import type {
  AccessibilityTarget,
  CandidateEvidence,
  CommandError,
  CommandOutcome,
  EventBatch,
  EventGap,
  Evidence,
  ExecutionRecord,
  InterfaceEvent,
  JsonValue,
  NetworkResourceType,
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

export function hasExactKeys(value: unknown, required: readonly string[], optional: readonly string[] = []): value is Record<string, unknown> {
  if (!isRecord(value)) return false;
  const allowed = new Set([...required, ...optional]);
  return required.every((key) => Object.hasOwn(value, key)) && Object.keys(value).every((key) => allowed.has(key));
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
  return hasExactKeys(value, ["version", "capabilities", "active_sessions", "queued_jobs", "uptime_ms"])
    && isString(value.version)
    && isStringArray(value.capabilities)
    && isSafeUnsigned(value.active_sessions)
    && isSafeUnsigned(value.queued_jobs)
    && isSafeUnsigned(value.uptime_ms);
}

function isSessionExecutionPolicy(value: unknown): value is SessionState["execution_policy"] {
  return hasExactKeys(value, ["javascriptEvaluation", "visionAssist"])
    && typeof value.javascriptEvaluation === "boolean"
    && typeof value.visionAssist === "boolean";
}

export function isSessionState(value: unknown): value is SessionState {
  return hasExactKeys(value, ["id", "profile", "proxy", "page_ids", "created_at", "last_used_at", "execution_policy"])
    && isUuid(value.id)
    && isString(value.profile)
    && isNullableString(value.proxy)
    && Array.isArray(value.page_ids) && value.page_ids.every(isUuid)
    && isIsoTimestamp(value.created_at)
    && isIsoTimestamp(value.last_used_at)
    && isSessionExecutionPolicy(value.execution_policy);
}

export function isSessionStateList(value: unknown): value is SessionState[] {
  return Array.isArray(value) && value.every(isSessionState);
}

export function isPageState(value: unknown): value is PageState {
  return hasExactKeys(value, ["id", "session_id", "url", "mode", "ready_state", "pending_requests"])
    && isUuid(value.id)
    && isUuid(value.session_id)
    && isNullableString(value.url)
    && (value.mode === "Document" || value.mode === "Interactive" || value.mode === "Render")
    && isString(value.ready_state)
    && isSafeUnsigned(value.pending_requests);
}

function isTextMatch(value: unknown): value is TextMatch {
  return hasExactKeys(value, ["kind", "value"])
    && (value.kind === "exact" || value.kind === "contains" || value.kind === "regex")
    && isString(value.value);
}

function isTargetSpec(value: unknown, depth = 0): value is TargetSpec {
  if (!hasExactKeys(value, ["css", "testId", "role", "accessibleName", "label", "text", "attributes", "framePath", "shadowPath", "ordinal", "allowBestMatch"]) || depth >= 64) return false;
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
  return hasExactKeys(value, ["pageId", "frame", "role", "name", "stableAttributes"])
    && isUuid(value.pageId)
    && isNullableString(value.frame)
    && isNullableString(value.role)
    && isNullableString(value.name)
    && isStringMap(value.stableAttributes);
}

function isCandidateEvidence(value: unknown): value is CandidateEvidence {
  return hasExactKeys(value, ["role", "name", "score", "reasons"])
    && isNullableString(value.role)
    && isNullableString(value.name)
    && isSafeSigned(value.score, -2_147_483_648, 2_147_483_647)
    && isStringArray(value.reasons);
}

function isPageEvidence(value: unknown): value is PageEvidence {
  return hasExactKeys(value, ["pageId", "url", "title"]) && isUuid(value.pageId) && isString(value.url) && isString(value.title);
}

const NETWORK_RESOURCE_TYPES = new Set<NetworkResourceType>([
  "Document", "Stylesheet", "Image", "Media", "Font", "Script", "TextTrack", "XHR", "Fetch",
  "Prefetch", "EventSource", "WebSocket", "Manifest", "SignedExchange", "Ping",
  "CSPViolationReport", "Preflight", "FedCM", "Other",
]);

function isNetworkResourceType(value: unknown): value is NetworkResourceType {
  return typeof value === "string" && NETWORK_RESOURCE_TYPES.has(value as NetworkResourceType);
}

function isNetworkQuietCondition(value: Record<string, unknown>): boolean {
  return value.kind === "networkQuiet"
    && hasExactKeys(value, ["kind", "idleMs", "maxInFlight"], ["ignoreUrlSubstrings", "ignoreResourceTypes", "ignoreLongLived"])
    && isSafeUnsigned(value.idleMs)
    && isSafeUnsigned(value.maxInFlight)
    && optional(value, "ignoreUrlSubstrings", isStringArray)
    && optional(value, "ignoreResourceTypes", (candidate): candidate is NetworkResourceType[] => Array.isArray(candidate) && candidate.every(isNetworkResourceType))
    && optional(value, "ignoreLongLived", (candidate): candidate is boolean => typeof candidate === "boolean");
}

function isWaitCondition(value: unknown): value is WaitCondition {
  if (!isRecord(value)) return false;
  if (value.kind === "element") return hasExactKeys(value, ["kind", "target", "state"]) && isTargetSpec(value.target) && (value.state === "attached" || value.state === "detached" || value.state === "visible" || value.state === "hidden" || value.state === "enabled" || value.state === "disabled");
  if (value.kind === "text" || value.kind === "value") return hasExactKeys(value, ["kind", "target", "matcher"]) && isTargetSpec(value.target) && isTextMatch(value.matcher);
  if (value.kind === "url") return hasExactKeys(value, ["kind", "matcher"]) && isTextMatch(value.matcher);
  if (value.kind === "document") return hasExactKeys(value, ["kind", "ready"]) && (value.ready === "commit" || value.ready === "domContentLoaded" || value.ready === "interactive" || value.ready === "networkIdle");
  return isNetworkQuietCondition(value);
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
      return hasExactKeys(value, ["kind", "path", "reason", "stateVersion", "elapsedMs", "bytes", "sha256"], ["finalUrl", "contentType", "status", "redirectChain"])
        && (value.path === "directHttp" || value.path === "chromium" || value.path === "chromiumFallback")
        && (value.reason === "eligibleStaticDocument" || value.reason === "eligibleExplicitDownload" || value.reason === "ineligibleCommand" || value.reason === "semanticTargetRequired" || value.reason === "javascriptRequired" || value.reason === "unsupportedContentType" || value.reason === "stateConflict" || value.reason === "policyRequired")
        && isSafeUnsigned(value.stateVersion)
        && isSafeUnsigned(value.elapsedMs)
        && (value.bytes === null || isSafeUnsigned(value.bytes))
        && (value.sha256 === null || isLowerSha256(value.sha256))
        && ((value.bytes === null) === (value.sha256 === null))
        && validExecutionPathOptionalFields(value);
    case "navigation": return hasExactKeys(value, ["kind", "url", "title"]) && isString(value.url) && isString(value.title);
    case "inspection": return hasExactKeys(value, ["kind", "selector", "url", "title", "text", "html"]) && isNullableString(value.selector) && isString(value.url) && isString(value.title) && isString(value.text) && isNullableString(value.html);
    case "element": return hasExactKeys(value, ["kind", "selector", "text"]) && isString(value.selector) && isNullableString(value.text);
    case "upload": return hasExactKeys(value, ["kind", "selector", "paths"]) && isString(value.selector) && isStringArray(value.paths);
    case "page": return hasExactKeys(value, ["kind", "pageId", "url", "title"]) && isUuid(value.pageId) && isString(value.url) && isString(value.title);
    case "pages": return hasExactKeys(value, ["kind", "pages"]) && Array.isArray(value.pages) && value.pages.every(isPageEvidence);
    case "popup": return hasExactKeys(value, ["kind", "openerPageId", "pageId", "url", "title"]) && isUuid(value.openerPageId) && isUuid(value.pageId) && isString(value.url) && isString(value.title);
    case "download": return hasExactKeys(value, ["kind", "filename", "path", "bytes", "sha256"]) && isString(value.filename) && isString(value.path) && isSafeUnsigned(value.bytes) && isLowerSha256(value.sha256);
    case "resolution": return hasExactKeys(value, ["kind", "target", "fingerprint", "candidates", "bestMatchAuthorized"]) && isTargetSpec(value.target) && isTargetFingerprint(value.fingerprint) && Array.isArray(value.candidates) && value.candidates.every(isCandidateEvidence) && typeof value.bestMatchAuthorized === "boolean";
    case "wait": return hasExactKeys(value, ["kind", "condition", "elapsedMs", "observations"], ["excludedClasses"]) && isWaitCondition(value.condition) && isSafeUnsigned(value.elapsedMs) && isSafeUnsigned(value.observations) && optional(value, "excludedClasses", isStringArray);
    case "screenshot": return hasExactKeys(value, ["kind", "artifactId", "mediaType", "width", "height", "bytes", "sha256"]) && isString(value.artifactId) && isString(value.mediaType) && isSafeUnsigned(value.width, 4_294_967_295) && isSafeUnsigned(value.height, 4_294_967_295) && isSafeUnsigned(value.bytes) && isLowerSha256(value.sha256);
    case "configuration": return hasExactKeys(value, ["kind", "name", "value"]) && isString(value.name) && isString(value.value);
    case "browserExecution": return hasExactKeys(value, ["kind", "engine", "browserVersion", "profileId", "interactionPath"]) && isString(value.engine) && isString(value.browserVersion) && isString(value.profileId) && isString(value.interactionPath);
    case "javaScriptResult": return hasExactKeys(value, ["kind", "value", "truncated"]) && isJsonValue(value.value) && typeof value.truncated === "boolean";
    case "intentExecution": return hasExactKeys(value, ["kind", "record"]) && isExecutionRecord(value.record);
    case "accessibilitySnapshot": return hasExactKeys(value, ["kind", "pageId", "nodes", "truncated"], []) && isUuid(value.pageId) && Array.isArray(value.nodes) && value.nodes.every(isAccessibilityNode) && typeof value.truncated === "boolean";
    default: return false;
  }
}

function isAccessibilityNode(value: unknown, depth = 0): boolean {
  return isRecord(value)
    && depth <= 32
    && (value.role === undefined || isString(value.role))
    && (value.name === undefined || isString(value.name))
    && optional(value, "target", isAccessibilityTarget)
    && optional(value, "value", isString)
    && optional(value, "description", isString)
    && optional(value, "required", (item): item is boolean => typeof item === "boolean")
    && optional(value, "disabled", (item): item is boolean => typeof item === "boolean")
    && optional(value, "readOnly", (item): item is boolean => typeof item === "boolean")
    && optional(value, "invalid", (item): item is boolean => typeof item === "boolean")
    && optional(value, "checked", (item): item is boolean => typeof item === "boolean")
    && optional(value, "autocomplete", isString)
    && optional(value, "valueMin", isString)
    && optional(value, "valueMax", isString)
    && (value.children === undefined || (Array.isArray(value.children) && value.children.every((child) => isAccessibilityNode(child, depth + 1))))
    && Object.keys(value).every((key) => ["role", "name", "target", "value", "description", "required", "disabled", "readOnly", "invalid", "checked", "autocomplete", "valueMin", "valueMax", "children"].includes(key));
}

function isAccessibilityTarget(value: unknown): value is AccessibilityTarget {
  return hasExactKeys(value, ["role", "accessibleName"], ["ordinal"])
    && isString(value.role)
    && isString(value.accessibleName)
    && optional(value, "ordinal", (item): item is number => isSafeUnsigned(item, 2047));
}

function isExecutionRecord(value: unknown): value is ExecutionRecord {
  return hasExactKeys(value, ["intentKind", "purpose", "resolutionPath", "planSummary", "candidates", "waitElapsedMs", "verification", "artifactIds", "visionProposalSha256"])
    && isString(value.intentKind)
    && isNullableString(value.purpose)
    && (value.resolutionPath === "deterministic" || value.resolutionPath === "visionFallback")
    && isString(value.planSummary)
    && Array.isArray(value.candidates) && value.candidates.every(isCandidateEvidence)
    && (value.waitElapsedMs === null || isSafeUnsigned(value.waitElapsedMs))
    && isString(value.verification)
    && isStringArray(value.artifactIds)
    && (value.visionProposalSha256 === null || isLowerSha256(value.visionProposalSha256));
}

function isEvidenceArray(value: unknown): value is Evidence[] { return Array.isArray(value) && value.every(isEvidence); }

export function isCommandError(value: unknown): value is CommandError {
  return hasExactKeys(value, ["code", "message", "layer", "retryable"])
    && (value.code === "invalidRequest" || value.code === "notFound" || value.code === "deadlineExceeded" || value.code === "browserLaunchFailed" || value.code === "browserCommandFailed" || value.code === "verificationFailed" || value.code === "journalFailed" || value.code === "resourceExhausted" || value.code === "policyDenied" || value.code === "internal" || value.code === "targetNotFound" || value.code === "targetAmbiguous" || value.code === "frameNotFound" || value.code === "shadowRootUnavailable" || value.code === "targetDetached" || value.code === "waitConditionTimedOut" || value.code === "screenshotCaptureFailed" || value.code === "networkPolicyDenied" || value.code === "httpResponseTooLarge" || value.code === "httpTransferFailed" || value.code === "httpStateConflict" || value.code === "httpEquivalenceUnproven" || value.code === "intentCompileFailed" || value.code === "intentActionMismatch" || value.code === "obstructionSuspected" || value.code === "visionAssistDenied" || value.code === "visionAssistFailed")
    && isString(value.message)
    && (value.layer === "interface" || value.layer === "broker" || value.layer === "workflow" || value.layer === "page" || value.layer === "driver" || value.layer === "browser" || value.layer === "network" || value.layer === "site" || value.layer === "journal")
    && typeof value.retryable === "boolean";
}

export function isCommandOutcome(value: unknown): value is CommandOutcome {
  if (!isRecord(value) || !isUuid(value.commandId)) return false;
  switch (value.status) {
    case "completed": return hasExactKeys(value, ["status", "commandId", "evidence"]) && isEvidenceArray(value.evidence);
    case "retryableFailure":
    case "policyDenied":
      return hasExactKeys(value, ["status", "commandId", "error"]) && isCommandError(value.error);
    case "failed":
      return (
        (hasExactKeys(value, ["status", "commandId", "error"])
          || hasExactKeys(value, ["status", "commandId", "error", "evidence"]))
        && isCommandError(value.error)
        && (value.evidence === undefined || isEvidenceArray(value.evidence))
      );
    case "needsReconciliation": return hasExactKeys(value, ["status", "commandId", "error", "evidence"]) && isCommandError(value.error) && isEvidenceArray(value.evidence);
    case "resourceExhausted": return hasExactKeys(value, ["status", "commandId", "error", "retryAfterMs"]) && isCommandError(value.error) && isSafeUnsigned(value.retryAfterMs);
    case "restarted": return hasExactKeys(value, ["status", "commandId", "priorAttemptId", "attemptId", "reason", "evidence"]) && isUuid(value.priorAttemptId) && isUuid(value.attemptId) && isString(value.reason) && isEvidenceArray(value.evidence);
    default: return false;
  }
}

function isCheckpointInvariant(value: unknown): boolean {
  if (!isRecord(value)) return false;
  if (value.kind === "url" || value.kind === "title") return hasExactKeys(value, ["kind", "value"]) && isString(value.value);
  return value.kind === "text" && hasExactKeys(value, ["kind", "selector", "value"]) && isString(value.selector) && isString(value.value);
}

export function isRecoveryDecision(value: unknown): value is RecoveryDecision {
  if (!isRecord(value) || !isUuid(value.checkpointId)) return false;
  if (value.status === "resumed") return hasExactKeys(value, ["status", "checkpointId", "attemptId", "evidence"]) && isUuid(value.attemptId) && isEvidenceArray(value.evidence);
  if (value.status === "needsReconciliation") return hasExactKeys(value, ["status", "checkpointId", "attemptId", "reason", "evidence"]) && isUuid(value.attemptId) && isString(value.reason) && isEvidenceArray(value.evidence);
  return value.status === "restarted"
    && hasExactKeys(value, ["status", "checkpointId", "lineage", "evidence"])
    && hasExactKeys(value.lineage, ["workflowId", "abandonedAttemptId", "attemptId", "reason"])
    && isUuid(value.lineage.workflowId)
    && isUuid(value.lineage.abandonedAttemptId)
    && isUuid(value.lineage.attemptId)
    && isString(value.lineage.reason)
    && isEvidenceArray(value.evidence);
}

export function isWorkflowCheckpoint(value: unknown): value is WorkflowCheckpoint {
  return hasExactKeys(value, ["schemaVersion", "checkpointId", "workflowId", "attemptId", "sessionId", "pageId", "restartUrl", "currentUrl", "cursor", "boundaryCommandId", "recoveryClass", "invariants", "replayableInputs", "evidence", "recoveryHistory", "recoveryReceipts", "createdAt"])
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
    && Array.isArray(value.recoveryHistory) && value.recoveryHistory.every((record) => hasExactKeys(record, ["recordedAt", "decision"]) && isIsoTimestamp(record.recordedAt) && isRecoveryDecision(record.decision))
    && Array.isArray(value.recoveryReceipts)
    && isIsoTimestamp(value.createdAt);
}

function isInterfaceEvent(value: unknown): value is InterfaceEvent {
  return hasExactKeys(value, ["cursor", "kind", "payload"])
    && isSafeUnsigned(value.cursor)
    && isString(value.kind)
    && "payload" in value
    && isJsonValue(value.payload);
}

export function isEventBatch(value: unknown, after: number, limit: number): value is EventBatch {
  if (!isSafeUnsigned(after) || !isSafeUnsigned(limit) || !hasExactKeys(value, ["events", "latestAvailable"]) || !isSafeUnsigned(value.latestAvailable) || value.latestAvailable < after || !Array.isArray(value.events) || value.events.length === 0 || value.events.length > limit || !value.events.every(isInterfaceEvent)) return false;
  let previous = after;
  for (const event of value.events) {
    if (event.cursor <= previous || event.cursor > value.latestAvailable) return false;
    previous = event.cursor;
  }
  return true;
}

export function isEventGap(value: unknown): value is EventGap {
  return hasExactKeys(value, ["reason", "earliestAvailable"])
    && (value.reason === "historyLost" || value.reason === "invalidLimit" || value.reason === "invalidCursor")
    && isSafeUnsigned(value.earliestAvailable);
}
