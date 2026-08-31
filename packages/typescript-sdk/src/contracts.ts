/**
 * Wire contracts for the Bobby Browser `/v1` interface.
 *
 * Types here are the JSON shapes exchanged with a running runtime. Keep
 * {@link INTERFACE_VERSION} aligned with the server you call.
 */

/** Interface version negotiated via the `x-interface-version` request header. */
export const INTERFACE_VERSION = "2026-08-19" as const;

export type Id = string;
export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

export interface RuntimeInfo { version: string; capabilities: string[]; active_sessions: number; queued_jobs: number; uptime_ms: number; }
export interface SessionState { id: Id; profile: string; proxy: string | null; page_ids: Id[]; created_at: string; last_used_at: string; execution_policy: { javascriptEvaluation: boolean; visionAssist: boolean; fingerprint: boolean; humanize: boolean; visionNode?: string }; /** Present iff true: a godmode session running the ZigZagZig recovery ladder. */ zigzagzig?: boolean; }
export type PageMode = "Document" | "Interactive" | "Render";
export interface PageState { id: Id; session_id: Id; url: string | null; mode: PageMode; ready_state: string; pending_requests: number; }
/** Session execution policy. Omitted fields default to denied. */
export interface ExecutionPolicy { javascriptEvaluation?: boolean; visionAssist?: boolean; fingerprint?: boolean; humanize?: boolean; visionNode?: string; }
export interface CreateSessionRequest { profile: string; proxy: string | null; executionPolicy?: ExecutionPolicy; /** Godmode session: every capability on + the ZigZagZig recovery ladder on every page-bound command. */ zigzagzig?: boolean; }
export interface OpenPageRequest { session_id: Id; }

export type ErrorLayer = "interface" | "broker" | "workflow" | "page" | "driver" | "browser" | "network" | "site" | "journal";
export type Capability = "session:read" | "session:write" | "page:read" | "page:write" | "browser:mutate" | "file:upload" | "file:download" | "javascript:evaluate" | "intent:execute" | "vision:assist" | "recovery:read" | "recovery:write" | "artifact:read" | "artifact:capture" | "job:submit" | "job:read" | "job:cancel" | "authority:admin" | "browser:fingerprint" | "browser:humanize";
export type InterfaceErrorCode = "invalidRequest" | "unsupportedInterfaceVersion" | "invalidIdempotencyKey" | "idempotencyConflict" | "deadlineExceeded" | "authenticationFailed" | "tokenExpired" | "missingCapability" | "malformedScope" | "artifactDenied" | "unsupportedOperation" | "notFound" | "resourceExhausted" | "engineUnreachable" | "internal";
export interface InterfaceError {
  code: InterfaceErrorCode;
  layer: ErrorLayer;
  message: string;
  correlationId: Id;
  commandId: Id | null;
  retryable: boolean;
  retryAfterMs: number | null;
  reconciliationRequired: boolean;
  requiredCapability: Capability | null;
}

export type CommandErrorCode = "invalidRequest" | "notFound" | "deadlineExceeded" | "browserLaunchFailed" | "browserCommandFailed" | "verificationFailed" | "journalFailed" | "resourceExhausted" | "policyDenied" | "internal" | "targetNotFound" | "targetAmbiguous" | "frameNotFound" | "shadowRootUnavailable" | "targetDetached" | "waitConditionTimedOut" | "screenshotCaptureFailed" | "networkPolicyDenied" | "httpResponseTooLarge" | "httpTransferFailed" | "httpStateConflict" | "httpEquivalenceUnproven" | "intentCompileFailed" | "intentActionMismatch" | "obstructionSuspected" | "visionAssistDenied" | "visionAssistFailed";
export interface CommandError { code: CommandErrorCode; message: string; layer: ErrorLayer; retryable: boolean; }

/** Maximum UTF-8 byte length for intent `purpose` strings. */
export const MAX_INTENT_PURPOSE_BYTES = 256 as const;
/** Default timeout (ms) when `DismissObstructionIntent.timeoutMs` is omitted. */
export const DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS = 5_000 as const;

export type ExecutionPath = "directHttp" | "browser" | "browserFallback";
export type ExecutionReason = "eligibleStaticDocument" | "eligibleExplicitDownload" | "ineligibleCommand" | "semanticTargetRequired" | "javascriptRequired" | "unsupportedContentType" | "stateConflict" | "policyRequired";
export interface TargetSpec { css?: string | null; testId?: string | null; role?: string | null; accessibleName?: string | null; label?: string | null; text?: TextMatch | null; attributes?: Record<string, string>; framePath?: TargetSpec[]; shadowPath?: TargetSpec[]; ordinal?: number | null; allowBestMatch?: boolean; }
export type TextMatch = { kind: "exact" | "contains" | "regex"; value: string };
export interface TargetFingerprint { pageId: Id; frame: string | null; role: string | null; name: string | null; stableAttributes: Record<string, string>; }
export interface CandidateEvidence { role: string | null; name: string | null; score: number; reasons: string[]; }
export interface PageEvidence { pageId: Id; url: string; title: string; }
export type WaitCondition =
  | { kind: "element"; target: TargetSpec; state: "attached" | "detached" | "visible" | "hidden" | "enabled" | "disabled" }
  | { kind: "text" | "value"; target: TargetSpec; matcher: TextMatch }
  | { kind: "url"; matcher: TextMatch }
  | { kind: "document"; ready: "commit" | "domContentLoaded" | "interactive" | "networkIdle" }
  | {
      kind: "networkQuiet";
      idleMs: number;
      maxInFlight: number;
      ignoreUrlSubstrings?: string[];
      ignoreResourceTypes?: NetworkResourceType[];
      ignoreLongLived?: boolean;
    };

export type NetworkResourceType =
  | "Document"
  | "Stylesheet"
  | "Image"
  | "Media"
  | "Font"
  | "Script"
  | "TextTrack"
  | "XHR"
  | "Fetch"
  | "Prefetch"
  | "EventSource"
  | "WebSocket"
  | "Manifest"
  | "SignedExchange"
  | "Ping"
  | "CSPViolationReport"
  | "Preflight"
  | "FedCM"
  | "Other";

export type IntentResolutionPath = "deterministic" | "visionFallback";
export interface ExecutionRecord {
  intentKind: string;
  purpose: string | null;
  resolutionPath: IntentResolutionPath;
  planSummary: string;
  candidates: CandidateEvidence[];
  waitElapsedMs: number | null;
  verification: string;
  artifactIds: string[];
  visionProposalSha256: string | null;
}

/** Discriminated evidence payloads returned on command outcomes (`kind` tag). */
export type Evidence =
  | { kind: "executionPath"; path: ExecutionPath; reason: ExecutionReason; stateVersion: number; elapsedMs: number; bytes: number | null; sha256: string | null; finalUrl?: string; contentType?: string; status?: number; redirectChain?: string[] }
  | { kind: "navigation"; url: string; title: string }
  | { kind: "inspection"; selector: string | null; url: string; title: string; text: string; html: string | null }
  | { kind: "submitSettlement"; outcome: "settled" | "validationRejected" }
  | { kind: "element"; selector: string; text: string | null }
  | { kind: "upload"; selector: string; paths: string[] }
  | { kind: "page"; pageId: Id; url: string; title: string }
  | { kind: "pages"; pages: PageEvidence[] }
  | { kind: "popup"; openerPageId: Id; pageId: Id; url: string; title: string }
  | { kind: "download"; filename: string; path: string; bytes: number; sha256: string; savedTo?: string }
  | { kind: "configuration"; name: string; value: string }
  | { kind: "resolution"; target: TargetSpec; fingerprint: TargetFingerprint; candidates: CandidateEvidence[]; bestMatchAuthorized: boolean }
  | { kind: "wait"; condition: WaitCondition; elapsedMs: number; observations: number; excludedClasses?: string[] }
  | { kind: "screenshot"; artifactId: Id; mediaType: string; width: number; height: number; bytes: number; sha256: string }
  | { kind: "browserExecution"; engine: string; browserVersion: string; profileId: string; interactionPath: string }
  | { kind: "javaScriptResult"; value: JsonValue; truncated: boolean }
  | { kind: "accessibilitySnapshot"; pageId: Id; nodes: AccessibilityNode[]; truncated: boolean }
  | { kind: "formSnapshot"; snapshot: FormSnapshot }
  | { kind: "formValidation"; issues: FormValidationIssue[] }
  | { kind: "controlAction"; action: ControlActionEvidence }
  | { kind: "intentExecution"; record: ExecutionRecord }
  | { kind: "humanization"; engine: string; actions: number; synthesizedMs: number }
  | { kind: "extraction"; field: string; value: string | null; resolutionPath: IntentResolutionPath; errorCode: CommandErrorCode | null };

/** Result of `POST /v1/commands`, discriminated by `status`. */
export type CommandOutcome =
  | { status: "completed"; commandId: Id; evidence: Evidence[] }
  | { status: "retryableFailure"; commandId: Id; error: CommandError }
  | { status: "needsReconciliation"; commandId: Id; error: CommandError; evidence: Evidence[] }
  | { status: "policyDenied"; commandId: Id; error: CommandError }
  | { status: "resourceExhausted"; commandId: Id; error: CommandError; retryAfterMs: number }
  | { status: "restarted"; commandId: Id; priorAttemptId: Id; attemptId: Id; reason: string; evidence: Evidence[] }
  | { status: "failed"; commandId: Id; error: CommandError; evidence?: Evidence[] };

export type WaitUntil = "commit" | "domContentLoaded" | "interactive" | "networkIdle";
export interface NavigateCommand { url: string; waitUntil: WaitUntil; timeoutMs: number; }
export interface DownloadUrlCommand { url: string; expectedContentType: string | null; maxBytes: number; }
export interface InspectCommand { selector: string | null; target: TargetSpec | null; includeHtml: boolean; }
export type ClickModifier = "shift" | "ctrl" | "alt" | "meta";
export interface ClickCommand { selector: string; target: TargetSpec | null; boundary: boolean; expectedUrl: string | null; modifiers?: ClickModifier[]; }
export interface TypeTextCommand { selector: string; target: TargetSpec | null; value: string; clearFirst: boolean; expectedUrl?: string | null; }
export interface UploadFilesCommand { selector: string; target: TargetSpec | null; paths: string[]; }
export interface OpenPageCommand { url: string | null; }
export interface ClosePageCommand { pageId: Id; }
export interface ActivatePageCommand { pageId: Id; }
export interface AccessibilitySnapshotCommand { maxNodes?: number | null }
export interface ExtractStructuredCommand { schema: unknown; purpose?: string | null }
export interface CookieRecord { name: string; value: string; domain: string; path: string; secure: boolean; httpOnly: boolean; sameSite?: string; expiresUnix?: number }
export interface SetCookieParam { name: string; value: string; url: string; path?: string | null; secure?: boolean; httpOnly?: boolean; sameSite?: string | null; expiresUnix?: number | null }
export interface GetCookiesCommand { urls?: string[] }
export interface SetCookiesCommand { cookies: SetCookieParam[] }
export interface DeleteCookiesCommand { urls?: string[]; names?: string[] }
export interface PrintToPdfCommand { landscape?: boolean; printBackground?: boolean; scale?: number | null; pageRanges?: string | null }
export interface AccessibilityTarget { role: string; accessibleName: string; ordinal?: number }
export interface AccessibilityNode {
  role?: string;
  name?: string;
  target?: AccessibilityTarget;
  value?: string;
  description?: string;
  required?: boolean;
  disabled?: boolean;
  readOnly?: boolean;
  invalid?: boolean;
  checked?: boolean;
  autocomplete?: string;
  valueMin?: string;
  valueMax?: string;
  children?: AccessibilityNode[];
}

/** Schema version for {@link FormSnapshot} payloads. */
export const FORM_SNAPSHOT_SCHEMA_VERSION = 1 as const;
export type FormControlKind = "text" | "email" | "password" | "search" | "number" | "checkbox" | "radio" | "switch" | "selectOne" | "selectMultiple" | "date" | "time" | "dateTimeLocal" | "range" | "file" | "contentEditable" | "combobox" | "listbox" | "submit" | "reset" | "other";
export type FormControlOperation = "setText" | "setChecked" | "selectOne" | "selectMany" | "setFiles" | "clear" | "activate";
export type FormValidityFlag = "valueMissing" | "typeMismatch" | "patternMismatch" | "tooLong" | "tooShort" | "rangeUnderflow" | "rangeOverflow" | "stepMismatch" | "badInput" | "customError";
export interface SemanticTargetSegment { role: string; accessibleName: string; ordinal: number | null; }
export interface FormControlTarget { role: string; accessibleName: string; ordinal?: number | null; framePath?: SemanticTargetSegment[]; shadowPath?: SemanticTargetSegment[]; }
export type FormControlState = { kind: "empty" } | { kind: "text"; value: string } | { kind: "redacted"; present: boolean } | { kind: "checked"; checked: boolean } | { kind: "selection"; values: string[] } | { kind: "files"; count: number };
export interface FormControlConstraints { required?: boolean; readOnly?: boolean; disabled?: boolean; pattern?: string | null; minLength?: number | null; maxLength?: number | null; min?: string | null; max?: string | null; step?: string | null; multiple?: boolean; accept?: string[]; }
export interface FormControlValidity { willValidate?: boolean; valid: boolean; flags?: FormValidityFlag[]; message?: string | null; describedBy?: string[]; }
export interface FormValidationIssue { controlId: string; controlKind: FormControlKind; accessibleName: string | null; target: FormControlTarget | null; validity: FormControlValidity; }
export interface FormOption { value: string; label: string; disabled?: boolean; selected?: boolean; groupLabel?: string | null; }
export interface FormControl { id: string; formId?: string | null; groupId?: string | null; target?: FormControlTarget | null; controlKind: FormControlKind; accessibleName?: string | null; label?: string | null; description?: string | null; placeholder?: string | null; autocomplete?: string | null; state: FormControlState; constraints?: FormControlConstraints; validity: FormControlValidity; options?: FormOption[]; supportedOperations: FormControlOperation[]; }
export interface FormGroup { id: string; label: string | null; description: string | null; controlIds: string[]; }
export interface FormValidity { valid: boolean; invalidControlIds: string[]; }
export interface FormDescriptor { id: string; target: FormControlTarget | null; accessibleName: string | null; description: string | null; groups: FormGroup[]; controls: FormControl[]; submitControlIds: string[]; resetControlIds: string[]; validity: FormValidity; }
export interface FormSnapshot { schemaVersion: typeof FORM_SNAPSHOT_SCHEMA_VERSION; pageId: Id; forms: FormDescriptor[]; unownedControls: FormControl[]; truncated: boolean; }
/**
 * The unified mutation vocabulary for form controls. Used both as the
 * `controlAction` primitive's payload and as {@link FillIntent} /
 * {@link CompleteFormField}'s `value` — `activate` is accepted by the type
 * but rejected at runtime for fill and completeForm (control_action only).
 */
export type ControlAction =
  | { kind: "setText"; value: string; /** Defaults to `true` (replace) when omitted. */ clearFirst?: boolean }
  | { kind: "setChecked"; checked: boolean }
  | { kind: "selectOne"; value: string }
  | { kind: "selectMany"; values: string[] }
  | { kind: "setFiles"; paths: string[] }
  | { kind: "clear" }
  | { kind: "activate" };
export interface ControlActionCommand { target: FormControlTarget; action: ControlAction; }
export interface RevealedControl { controlKind: FormControlKind; accessibleName?: string; target?: FormControlTarget; }
export interface ControlActionEvidence { operation: FormControlOperation; target: FormControlTarget; state: FormControlState; validity: FormControlValidity; nodeReplaced: boolean; revealedControls?: RevealedControl[]; }
export interface ClickAndWaitForPopupCommand { selector: string; target: TargetSpec | null; timeoutMs: number; }
export interface ClickAndWaitForDownloadCommand { selector: string; target: TargetSpec | null; timeoutMs: number; }
export interface WaitForCommand { condition: WaitCondition; timeoutMs: number; }
export type ScreenshotMode = { kind: "viewport" | "fullPage" } | { kind: "element"; target: TargetSpec } | { kind: "clip"; x: number; y: number; width: number; height: number };
export interface CaptureScreenshotCommand { mode: ScreenshotMode; }
export type PrimitiveCommand =
  | { kind: "navigate"; input: NavigateCommand }
  | { kind: "downloadUrl"; input: DownloadUrlCommand }
  | { kind: "inspect"; input: InspectCommand }
  | { kind: "click"; input: ClickCommand }
  | { kind: "typeText"; input: TypeTextCommand }
  | { kind: "uploadFiles"; input: UploadFilesCommand }
  | { kind: "openPage"; input: OpenPageCommand }
  | { kind: "listPages"; input: null }
  | { kind: "closePage"; input: ClosePageCommand }
  | { kind: "activatePage"; input: ActivatePageCommand }
  | { kind: "accessibilitySnapshot"; input: AccessibilitySnapshotCommand }
  | { kind: "extractStructured"; input: ExtractStructuredCommand }
  | { kind: "getCookies"; input: GetCookiesCommand }
  | { kind: "setCookies"; input: SetCookiesCommand }
  | { kind: "deleteCookies"; input: DeleteCookiesCommand }
  | { kind: "printToPdf"; input: PrintToPdfCommand }
  | { kind: "clickAndWaitForPopup"; input: ClickAndWaitForPopupCommand }
  | { kind: "clickAndWaitForDownload"; input: ClickAndWaitForDownloadCommand }
  | { kind: "waitFor"; input: WaitForCommand }
  | { kind: "captureScreenshot"; input: CaptureScreenshotCommand }
  | { kind: "controlAction"; input: ControlActionCommand };

export interface IntentHints {
  role?: string | null;
  /**
   * Accessible name of the control, matched exactly. Accepts an
   * `a11y_snapshot` node's `target` verbatim. Equivalent to an exact
   * `nearText`; setting both to different values is refused.
   */
  accessibleName?: string | null;
  nearText?: TextMatch | null;
  ordinal?: number | null;
  framePath?: TargetSpec[];
  shadowPath?: TargetSpec[];
  allowBestMatch?: boolean;
}
export interface LocateIntent { purpose: string; hints?: IntentHints; }
export interface FillIntent { purpose: string; hints?: IntentHints; value: ControlAction; }
export interface CompleteFormField { name: string; purpose: string; hints?: IntentHints; value: ControlAction; }
export interface CompleteFormIntent { purpose: string; fields: CompleteFormField[]; }
export interface SubmitAndVerifyIntent { purpose: string; hints?: IntentHints; expectedState: WaitForCommand; }
export interface WaitForStateIntent { condition: WaitCondition; timeoutMs: number; }
/**
 * Follow a control and wait for the expected destination.
 * Set `boundary` when activation may mutate state or trigger a side effect
 * (same meaning as {@link ClickCommand.boundary}).
 */
export interface FollowIntent { purpose: string; hints?: IntentHints; expectedDestination: WaitForCommand; boundary?: boolean; }
/**
 * Dismiss an obstruction (overlay, cookie banner, etc.).
 * Always treated as reconciliable; verification (target detached or hidden)
 * is built in — callers do not supply a wait condition.
 */
export interface DismissObstructionIntent { purpose: string; hints?: IntentHints; timeoutMs?: number; }
/** Value to extract: plain text, a named attribute, or `href` shorthand. */
export type ExtractValueKind =
  | { kind: "text" }
  | { kind: "attribute"; attribute: string }
  | { kind: "href" };
/** One named field within an {@link ExtractIntent}; resolved independently of siblings. */
export interface ExtractField { name: string; purpose: string; hints?: IntentHints; value: ExtractValueKind; }
/**
 * Structured extraction intent. Replayable (does not mutate the page).
 * Fields resolve independently: a missing field is reported in that field's
 * extraction evidence rather than failing the whole command.
 */
export interface ExtractIntent { purpose: string; fields: ExtractField[]; }
/** Read-only challenge classification (captchas, verification widgets). Never acts on the page. */
export interface ChallengeRegion { x: number; y: number; width: number; height: number }
export interface DetectChallengeHints { region?: ChallengeRegion; timeoutMs?: number }
export interface DetectChallengeIntent { purpose: string; hints?: DetectChallengeHints }
/**
 * Vision solve loop against a captcha or verification widget. Reconciliable:
 * an interrupted attempt is inspectable, and the runtime never bypasses a
 * challenge — when the loop cannot clear it, surface the page to the operator.
 * Requires `vision:assist` plus the session's `visionAssist` policy.
 */
export interface SolveChallengeHints { region?: ChallengeRegion; timeoutMs?: number }
export interface SolveChallengeIntent { purpose: string; hints?: SolveChallengeHints }
/** Default challenge-solve budget (mirrors the runtime's 30s solve default). */
export const DEFAULT_SOLVE_CHALLENGE_TIMEOUT_MS = 30_000 as const;
/** Default challenge-detection budget (mirrors the runtime's 15s detect default). */
export const DEFAULT_DETECT_CHALLENGE_TIMEOUT_MS = 15_000 as const;
export type IntentCommand =
  | { kind: "locate"; input: LocateIntent }
  | { kind: "fill"; input: FillIntent }
  | { kind: "completeForm"; input: CompleteFormIntent }
  | { kind: "submitAndVerify"; input: SubmitAndVerifyIntent }
  | { kind: "waitForState"; input: WaitForStateIntent }
  | { kind: "follow"; input: FollowIntent }
  | { kind: "dismissObstruction"; input: DismissObstructionIntent }
  | { kind: "extract"; input: ExtractIntent }
  | { kind: "detectChallenge"; input: DetectChallengeIntent }
  | { kind: "solveChallenge"; input: SolveChallengeIntent };

/**
 * Nested command wire shape:
 * `{ kind: "intent" | "primitive", input: … }`.
 */
export type RuntimeCommand =
  | { kind: "primitive"; input: PrimitiveCommand }
  | { kind: "intent"; input: IntentCommand };

/** Envelope submitted to `POST /v1/commands`. */
export interface CommandEnvelope { schemaVersion: number; commandId: Id; workflowId: Id; attemptId: Id; sessionId: Id; pageId: Id | null; deadline: string; command: RuntimeCommand; }

export type CommandClass = "replayable" | "reconciliable" | "boundary";
export type CheckpointInvariant = { kind: "url"; value: string } | { kind: "title"; value: string } | { kind: "text"; selector: string; value: string };
export interface ContextAnswer { target: AccessibilityTarget; confidence: number; }
export interface WorkflowCheckpoint { schemaVersion: number; checkpointId: Id; workflowId: Id; attemptId: Id; sessionId: Id; pageId: Id; restartUrl: string; currentUrl: string; cursor: Id | null; boundaryCommandId: Id | null; recoveryClass: CommandClass; invariants: CheckpointInvariant[]; replayableInputs: string[]; evidence: Evidence[]; recoveryHistory: RecoveryRecord[]; recoveryReceipts: unknown[]; createdAt: string; }
export interface RecoveryRecord { recordedAt: string; decision: RecoveryDecision; }
export interface RecoveryStatus { workflowId: Id; checkpoint: WorkflowCheckpoint; receipts: unknown[]; }
export type RecoveryDecision =
  | { status: "resumed"; checkpointId: Id; attemptId: Id; evidence: Evidence[] }
  | { status: "needsReconciliation"; checkpointId: Id; attemptId: Id; reason: string; evidence: Evidence[] }
  | { status: "restarted"; checkpointId: Id; lineage: { workflowId: Id; abandonedAttemptId: Id; attemptId: Id; reason: string }; evidence: Evidence[] };
/**
 * A checkpoint plus the command ids whose evidence the runtime already
 * recorded. The runtime resolves each id against its own journal; a caller
 * cannot hand in evidence for work it did not perform, and an id with no
 * terminal journal record fails the checkpoint. Same contract as the MCP
 * `checkpoint_save` tool.
 */
export interface CheckpointRequest { checkpoint: WorkflowCheckpoint; evidenceRefs?: Id[]; }

/** Single event from `GET /v1/events`. */
export interface InterfaceEvent { cursor: number; kind: string; payload: unknown; }
/** Successful events batch body. */
export interface EventBatch { events: InterfaceEvent[]; latestAvailable: number; }
export type EventGapReason = "historyLost" | "invalidLimit" | "invalidCursor";
/** Cursor gap details when event history cannot resume from the requested cursor. */
export interface EventGap { reason: EventGapReason; earliestAvailable: number; }
/** HTTP `409` body for an event cursor gap. */
export interface EventGapEnvelope { error: InterfaceError; gap: EventGap; }

/** Reference returned by the runtime for a stored artifact. */
export interface ArtifactReference { referenceId: Id; artifactId: string; sha256: string; bytes: number; mediaType: string; }
/** Per-request options shared by client methods. */
export interface RequestOptions {
  /** AbortSignal cancelled by the caller. */
  signal?: AbortSignal;
  /** Absolute deadline; combined with `timeoutMs` as the earlier of the two. */
  deadline?: Date | string;
  /** Relative timeout in milliseconds (default 30_000 when neither deadline nor timeout is set). */
  timeoutMs?: number;
  /** Value for `x-correlation-id` (generated UUID when omitted). */
  correlationId?: Id;
  /** Value for `idempotency-key` when the operation supports it. */
  idempotencyKey?: string;
}
/** Options for {@link BrowserRuntimeClient.formSnapshot}. */
export interface FormSnapshotOptions extends RequestOptions {
  /** Cap on returned controls (1–512). */
  maxControls?: number;
}
/** Options for {@link BrowserRuntimeClient.events}. */
export interface EventOptions extends RequestOptions {
  /** Batch size: safe integer from 1 through 256 (default 100). */
  limit?: number;
  /** Transport-only retries for GET: safe integer from 0 through 10 (default 2). */
  maxTransportRetries?: number;
  /** Delay between transport retries: safe integer from 0 through 60_000 ms (default 50). */
  retryDelayMs?: number;
}
