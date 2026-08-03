/**
 * Strict structural validators for `/v1` JSON responses.
 *
 * Used by {@link BrowserRuntimeClient} to reject unexpected shapes before
 * they reach application code.
 */
import type {
  AccessibilityTarget,
  RecoveryStatus,
  CandidateEvidence,
  CommandError,
  CommandOutcome,
  EventBatch,
  EventGap,
  Evidence,
  ExecutionRecord,
  FormControl,
  FormControlTarget,
  FormDescriptor,
  FormSnapshot,
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

const utf8 = new TextEncoder();
const boundedText = (value: unknown, max: number, allowEmpty = false): value is string => typeof value === "string" && (allowEmpty || value.length > 0) && utf8.encode(value).length <= max && !/[\u0000-\u001f\u007f]/.test(value);
const nullableBoundedText = (value: unknown, max: number): value is string | null => value === null || boundedText(value, max);
const oneOf = <T extends string>(value: unknown, values: readonly T[]): value is T => typeof value === "string" && values.includes(value as T);
const unique = (values: readonly unknown[]): boolean => new Set(values).size === values.length;

const CONTROL_KINDS = ["text", "email", "password", "search", "number", "checkbox", "radio", "switch", "selectOne", "selectMultiple", "date", "time", "dateTimeLocal", "range", "file", "contentEditable", "combobox", "listbox", "submit", "reset", "other"] as const;
const OPERATIONS = ["setText", "setChecked", "selectOne", "selectMany", "setFiles", "clear", "activate"] as const;
const VALIDITY_FLAGS = ["valueMissing", "typeMismatch", "patternMismatch", "tooLong", "tooShort", "rangeUnderflow", "rangeOverflow", "stepMismatch", "badInput", "customError"] as const;

function isTargetSegment(value: unknown): boolean {
  return hasExactKeys(value, ["role", "accessibleName", "ordinal"]) && boundedText(value.role, 128) && boundedText(value.accessibleName, 2048) && (value.ordinal === null || isSafeUnsigned(value.ordinal, 2047));
}
function isFormControlTarget(value: unknown): value is FormControlTarget {
  return hasExactKeys(value, ["role", "accessibleName", "ordinal", "framePath", "shadowPath"]) && boundedText(value.role, 128) && boundedText(value.accessibleName, 2048) && (value.ordinal === null || isSafeUnsigned(value.ordinal, 2047)) && Array.isArray(value.framePath) && value.framePath.length <= 8 && value.framePath.every(isTargetSegment) && Array.isArray(value.shadowPath) && value.shadowPath.length <= 8 && value.shadowPath.every(isTargetSegment);
}
function isControlActionEvidence(value: unknown): boolean {
  if (!hasExactKeys(value, ["operation", "target", "state", "validity", "nodeReplaced"]) || !oneOf(value.operation, OPERATIONS) || !isFormControlTarget(value.target) || typeof value.nodeReplaced !== "boolean" || !isRecord(value.state) || !isRecord(value.validity)) return false;
  const state = value.state;
  const stateValid = state.kind === "empty" ? hasExactKeys(state, ["kind"]) : state.kind === "text" ? hasExactKeys(state, ["kind", "value"]) && boundedText(state.value, 4096, true) : state.kind === "redacted" ? hasExactKeys(state, ["kind", "present"]) && typeof state.present === "boolean" : state.kind === "checked" ? hasExactKeys(state, ["kind", "checked"]) && typeof state.checked === "boolean" : state.kind === "selection" ? hasExactKeys(state, ["kind", "values"]) && Array.isArray(state.values) && state.values.length <= 512 && state.values.every((v) => boundedText(v, 4096, true)) : state.kind === "files" && hasExactKeys(state, ["kind", "count"]) && isSafeUnsigned(state.count, 512);
  const validity = value.validity;
  return stateValid && hasExactKeys(validity, ["willValidate", "valid", "flags", "message", "describedBy"]) && typeof validity.willValidate === "boolean" && typeof validity.valid === "boolean" && Array.isArray(validity.flags) && validity.flags.every((v) => oneOf(v, VALIDITY_FLAGS)) && nullableBoundedText(validity.message, 1024) && Array.isArray(validity.describedBy) && validity.describedBy.every((v) => boundedText(v, 2048));
}
function isFormControl(value: unknown): value is FormControl {
  if (!hasExactKeys(value, ["id", "formId", "groupId", "target", "controlKind", "accessibleName", "label", "description", "placeholder", "autocomplete", "state", "constraints", "validity", "options", "supportedOperations"]) || !boundedText(value.id, 128) || !(value.formId === null || boundedText(value.formId, 128)) || !(value.groupId === null || boundedText(value.groupId, 128)) || !(value.target === null || isFormControlTarget(value.target)) || !oneOf(value.controlKind, CONTROL_KINDS) || !nullableBoundedText(value.accessibleName, 2048) || !nullableBoundedText(value.label, 2048) || !nullableBoundedText(value.description, 2048) || !nullableBoundedText(value.placeholder, 2048) || !nullableBoundedText(value.autocomplete, 2048)) return false;
  const state = value.state;
  if (!isRecord(state)) return false;
  const stateValid = state.kind === "empty" ? hasExactKeys(state, ["kind"]) : state.kind === "text" ? hasExactKeys(state, ["kind", "value"]) && value.controlKind !== "password" && boundedText(state.value, 4096, true) : state.kind === "redacted" ? hasExactKeys(state, ["kind", "present"]) && typeof state.present === "boolean" : state.kind === "checked" ? hasExactKeys(state, ["kind", "checked"]) && typeof state.checked === "boolean" : state.kind === "selection" ? hasExactKeys(state, ["kind", "values"]) && Array.isArray(state.values) && state.values.length <= 512 && state.values.every((v) => boundedText(v, 4096, true)) : state.kind === "files" && hasExactKeys(state, ["kind", "count"]) && isSafeUnsigned(state.count, 512);
  const c = value.constraints;
  const constraintsValid = hasExactKeys(c, ["required", "readOnly", "disabled", "pattern", "minLength", "maxLength", "min", "max", "step", "multiple", "accept"]) && typeof c.required === "boolean" && typeof c.readOnly === "boolean" && typeof c.disabled === "boolean" && nullableBoundedText(c.pattern, 2048) && (c.minLength === null || isSafeUnsigned(c.minLength, 4_294_967_295)) && (c.maxLength === null || isSafeUnsigned(c.maxLength, 4_294_967_295)) && !(typeof c.minLength === "number" && typeof c.maxLength === "number" && c.minLength > c.maxLength) && nullableBoundedText(c.min, 4096) && nullableBoundedText(c.max, 4096) && nullableBoundedText(c.step, 4096) && typeof c.multiple === "boolean" && Array.isArray(c.accept) && c.accept.length <= 128 && c.accept.every((v) => boundedText(v, 2048));
  const validity = value.validity;
  const validityValid = hasExactKeys(validity, ["willValidate", "valid", "flags", "message", "describedBy"]) && typeof validity.willValidate === "boolean" && typeof validity.valid === "boolean" && Array.isArray(validity.flags) && validity.flags.length <= 10 && validity.flags.every((v) => oneOf(v, VALIDITY_FLAGS)) && unique(validity.flags) && (!validity.valid || validity.flags.length === 0) && nullableBoundedText(validity.message, 1024) && Array.isArray(validity.describedBy) && validity.describedBy.length <= 512 && validity.describedBy.every((v) => boundedText(v, 2048));
  const optionsValid = Array.isArray(value.options) && value.options.length <= 512 && value.options.every((option) => hasExactKeys(option, ["value", "label", "disabled", "selected", "groupLabel"]) && boundedText(option.value, 4096, true) && boundedText(option.label, 2048, true) && typeof option.disabled === "boolean" && typeof option.selected === "boolean" && nullableBoundedText(option.groupLabel, 2048));
  return stateValid && constraintsValid && validityValid && optionsValid && Array.isArray(value.supportedOperations) && value.supportedOperations.every((v) => oneOf(v, OPERATIONS)) && unique(value.supportedOperations);
}
function isFormDescriptor(value: unknown, globalIds: Set<string>): value is FormDescriptor {
  if (!hasExactKeys(value, ["id", "target", "accessibleName", "description", "groups", "controls", "submitControlIds", "resetControlIds", "validity"]) || !boundedText(value.id, 128) || !(value.target === null || isFormControlTarget(value.target)) || !nullableBoundedText(value.accessibleName, 2048) || !nullableBoundedText(value.description, 2048) || !Array.isArray(value.groups) || value.groups.length > 128 || !Array.isArray(value.controls) || value.controls.length > 512 || !value.controls.every(isFormControl)) return false;
  const formGroups = value.groups;
  const controls = new Map<string, FormControl>();
  for (const control of value.controls) { if (control.formId !== value.id || controls.has(control.id) || globalIds.has(control.id)) return false; controls.set(control.id, control); globalIds.add(control.id); }
  const groups = new Set<string>();
  for (const group of formGroups) { if (!hasExactKeys(group, ["id", "label", "description", "controlIds"]) || !boundedText(group.id, 128) || groups.has(group.id) || !nullableBoundedText(group.label, 2048) || !nullableBoundedText(group.description, 2048) || !Array.isArray(group.controlIds) || group.controlIds.length > 512 || group.controlIds.some((id) => !boundedText(id, 128) || controls.get(id)?.groupId !== group.id)) return false; groups.add(group.id); }
  if ([...controls.values()].some((control) => control.groupId !== null && (!groups.has(control.groupId) || !formGroups.some((group) => isRecord(group) && group.id === control.groupId && Array.isArray(group.controlIds) && group.controlIds.includes(control.id))))) return false;
  if (!Array.isArray(value.submitControlIds) || value.submitControlIds.length > 512 || !unique(value.submitControlIds) || value.submitControlIds.some((id) => controls.get(id)?.controlKind !== "submit") || !Array.isArray(value.resetControlIds) || value.resetControlIds.length > 512 || !unique(value.resetControlIds) || value.resetControlIds.some((id) => controls.get(id)?.controlKind !== "reset")) return false;
  return hasExactKeys(value.validity, ["valid", "invalidControlIds"]) && typeof value.validity.valid === "boolean" && Array.isArray(value.validity.invalidControlIds) && value.validity.invalidControlIds.length <= 512 && unique(value.validity.invalidControlIds) && value.validity.invalidControlIds.every((id) => controls.has(id));
}
export function isFormSnapshot(value: unknown): value is FormSnapshot {
  if (!hasExactKeys(value, ["schemaVersion", "pageId", "forms", "unownedControls", "truncated"]) || value.schemaVersion !== 1 || !isUuid(value.pageId) || !Array.isArray(value.forms) || value.forms.length > 64 || !Array.isArray(value.unownedControls) || typeof value.truncated !== "boolean") return false;
  const ids = new Set<string>(); const formIds = new Set<string>();
  for (const form of value.forms) { if (!isRecord(form) || typeof form.id !== "string" || formIds.has(form.id) || !isFormDescriptor(form, ids)) return false; formIds.add(form.id); }
  if (value.unownedControls.length + [...value.forms].reduce((n, form) => n + (isRecord(form) && Array.isArray(form.controls) ? form.controls.length : 0), 0) > 512) return false;
  return value.unownedControls.every((control) => isFormControl(control) && control.formId === null && control.groupId === null && !ids.has(control.id) && (ids.add(control.id), true));
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
    case "formSnapshot": return hasExactKeys(value, ["kind", "snapshot"]) && isFormSnapshot(value.snapshot);
    case "controlAction": return hasExactKeys(value, ["kind", "action"]) && isControlActionEvidence(value.action);
    case "cookieState": return hasExactKeys(value, ["kind", "pageId", "cookies"], []) && (value.pageId === null || isUuid(value.pageId)) && Array.isArray(value.cookies) && value.cookies.every(isCookieRecord);
    case "pdfArtifact": return hasExactKeys(value, ["kind", "artifactId", "mediaType", "bytes", "sha256"]) && isString(value.artifactId) && isString(value.mediaType) && isSafeUnsigned(value.bytes) && isLowerSha256(value.sha256);
    case "dialog": return hasExactKeys(value, ["kind", "dialogType", "message", "action"]) && isString(value.dialogType) && isString(value.message) && (value.action === "accept" || value.action === "dismiss");
    default: return false;
  }
}

export function isRecoveryStatus(value: unknown): value is RecoveryStatus {
  return hasExactKeys(value, ["workflowId", "checkpoint", "receipts"])
    && isUuid(value.workflowId)
    && isWorkflowCheckpoint(value.checkpoint)
    && Array.isArray(value.receipts);
}

function isCookieRecord(value: unknown): boolean {
  return hasExactKeys(value, ["name", "value", "domain", "path", "secure", "httpOnly"], ["sameSite", "expiresUnix"])
    && isString(value.name)
    && isString(value.value)
    && isString(value.domain)
    && isString(value.path)
    && typeof value.secure === "boolean"
    && typeof value.httpOnly === "boolean"
    && (value.sameSite === undefined || isString(value.sameSite))
    && (value.expiresUnix === undefined || (typeof value.expiresUnix === "number" && Number.isFinite(value.expiresUnix)));
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
