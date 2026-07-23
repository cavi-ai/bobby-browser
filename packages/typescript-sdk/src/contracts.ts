/** Exact JSON contracts for broker interface version 2026-07-17. */
export const INTERFACE_VERSION = "2026-07-17" as const;

export type Id = string;
export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

export interface RuntimeInfo { version: string; capabilities: string[]; active_sessions: number; queued_jobs: number; uptime_ms: number; }
export interface SessionState { id: Id; profile: string; proxy: string | null; page_ids: Id[]; created_at: string; last_used_at: string; }
export type PageMode = "Document" | "Interactive" | "Render";
export interface PageState { id: Id; session_id: Id; url: string | null; mode: PageMode; ready_state: string; pending_requests: number; }
export interface CreateSessionRequest { profile: string; proxy: string | null; }
export interface OpenPageRequest { session_id: Id; }

export type ErrorLayer = "interface" | "broker" | "workflow" | "page" | "driver" | "browser" | "network" | "site" | "journal";
export type Capability = "session:read" | "session:write" | "page:read" | "page:write" | "browser:mutate" | "file:upload" | "file:download" | "javascript:evaluate" | "recovery:read" | "recovery:write" | "artifact:read" | "artifact:capture";
export type InterfaceErrorCode = "invalidRequest" | "unsupportedInterfaceVersion" | "invalidIdempotencyKey" | "idempotencyConflict" | "deadlineExceeded" | "authenticationFailed" | "tokenExpired" | "missingCapability" | "malformedScope" | "artifactDenied" | "unsupportedOperation" | "notFound" | "resourceExhausted" | "internal";
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

export type CommandErrorCode = "invalidRequest" | "notFound" | "deadlineExceeded" | "browserLaunchFailed" | "browserCommandFailed" | "verificationFailed" | "journalFailed" | "resourceExhausted" | "policyDenied" | "internal" | "targetNotFound" | "targetAmbiguous" | "frameNotFound" | "shadowRootUnavailable" | "targetDetached" | "waitConditionTimedOut" | "screenshotCaptureFailed" | "networkPolicyDenied" | "httpResponseTooLarge" | "httpTransferFailed" | "httpStateConflict" | "httpEquivalenceUnproven";
export interface CommandError { code: CommandErrorCode; message: string; layer: ErrorLayer; retryable: boolean; }

export type ExecutionPath = "directHttp" | "chromium" | "chromiumFallback";
export type ExecutionReason = "eligibleStaticDocument" | "eligibleExplicitDownload" | "ineligibleCommand" | "semanticTargetRequired" | "javascriptRequired" | "unsupportedContentType" | "stateConflict" | "policyRequired";
export interface TargetSpec { css: string | null; testId: string | null; role: string | null; accessibleName: string | null; label: string | null; text: TextMatch | null; attributes: Record<string, string>; framePath: TargetSpec[]; shadowPath: TargetSpec[]; ordinal: number | null; allowBestMatch: boolean; }
export type TextMatch = { kind: "exact" | "contains" | "regex"; value: string };
export interface TargetFingerprint { pageId: Id; frame: string | null; role: string | null; name: string | null; stableAttributes: Record<string, string>; }
export interface CandidateEvidence { role: string | null; name: string | null; score: number; reasons: string[]; }
export interface PageEvidence { pageId: Id; url: string; title: string; }
export type WaitCondition =
  | { kind: "element"; target: TargetSpec; state: "attached" | "detached" | "visible" | "hidden" | "enabled" | "disabled" }
  | { kind: "text" | "value"; target: TargetSpec; matcher: TextMatch }
  | { kind: "url"; matcher: TextMatch }
  | { kind: "document"; ready: "commit" | "domContentLoaded" | "interactive" | "networkIdle" }
  | { kind: "networkQuiet"; idleMs: number; maxInFlight: number };

/** Every serde(tag = "kind") Evidence variant in the committed Rust contract. */
export type Evidence =
  | { kind: "executionPath"; path: ExecutionPath; reason: ExecutionReason; stateVersion: number; elapsedMs: number; bytes: number | null; sha256: string | null; finalUrl?: string; contentType?: string; status?: number; redirectChain?: string[] }
  | { kind: "navigation"; url: string; title: string }
  | { kind: "inspection"; selector: string | null; url: string; title: string; text: string; html: string | null }
  | { kind: "element"; selector: string; text: string | null }
  | { kind: "upload"; selector: string; paths: string[] }
  | { kind: "page"; pageId: Id; url: string; title: string }
  | { kind: "pages"; pages: PageEvidence[] }
  | { kind: "popup"; openerPageId: Id; pageId: Id; url: string; title: string }
  | { kind: "download"; filename: string; path: string; bytes: number; sha256: string }
  | { kind: "resolution"; target: TargetSpec; fingerprint: TargetFingerprint; candidates: CandidateEvidence[]; bestMatchAuthorized: boolean }
  | { kind: "wait"; condition: WaitCondition; elapsedMs: number; observations: number }
  | { kind: "screenshot"; artifactId: Id; mediaType: string; width: number; height: number; bytes: number; sha256: string };

/** Every serde(tag = "status") CommandOutcome variant in the committed Rust contract. */
export type CommandOutcome =
  | { status: "completed"; commandId: Id; evidence: Evidence[] }
  | { status: "retryableFailure"; commandId: Id; error: CommandError }
  | { status: "needsReconciliation"; commandId: Id; error: CommandError; evidence: Evidence[] }
  | { status: "policyDenied"; commandId: Id; error: CommandError }
  | { status: "resourceExhausted"; commandId: Id; error: CommandError; retryAfterMs: number }
  | { status: "restarted"; commandId: Id; priorAttemptId: Id; attemptId: Id; reason: string }
  | { status: "failed"; commandId: Id; error: CommandError; evidence?: Evidence[] };

export type WaitUntil = "commit" | "domContentLoaded" | "interactive" | "networkIdle";
export interface NavigateCommand { url: string; waitUntil: WaitUntil; timeoutMs: number; }
export interface DownloadUrlCommand { url: string; expectedContentType: string | null; maxBytes: number; }
export interface InspectCommand { selector: string | null; target: TargetSpec | null; includeHtml: boolean; }
export interface ClickCommand { selector: string; target: TargetSpec | null; boundary: boolean; expectedUrl: string | null; }
export interface TypeTextCommand { selector: string; target: TargetSpec | null; value: string; clearFirst: boolean; }
export interface UploadFilesCommand { selector: string; target: TargetSpec | null; paths: string[]; }
export interface OpenPageCommand { url: string | null; }
export interface ClosePageCommand { pageId: Id; }
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
  | { kind: "clickAndWaitForPopup"; input: ClickAndWaitForPopupCommand }
  | { kind: "clickAndWaitForDownload"; input: ClickAndWaitForDownloadCommand }
  | { kind: "waitFor"; input: WaitForCommand }
  | { kind: "captureScreenshot"; input: CaptureScreenshotCommand };
export interface CommandEnvelope { schemaVersion: number; commandId: Id; workflowId: Id; attemptId: Id; sessionId: Id; pageId: Id | null; deadline: string; command: PrimitiveCommand; }

export type CommandClass = "replayable" | "reconciliable" | "boundary";
export type CheckpointInvariant = { kind: "url"; value: string } | { kind: "title"; value: string } | { kind: "text"; selector: string; value: string };
export interface WorkflowCheckpoint { schemaVersion: number; checkpointId: Id; workflowId: Id; attemptId: Id; sessionId: Id; pageId: Id; restartUrl: string; currentUrl: string; cursor: Id | null; boundaryCommandId: Id | null; recoveryClass: CommandClass; invariants: CheckpointInvariant[]; replayableInputs: string[]; evidence: Evidence[]; recoveryHistory: RecoveryRecord[]; createdAt: string; }
export interface RecoveryRecord { recordedAt: string; decision: RecoveryDecision; }
export type RecoveryDecision =
  | { status: "resumed"; checkpointId: Id; attemptId: Id; evidence: Evidence[] }
  | { status: "needsReconciliation"; checkpointId: Id; attemptId: Id; reason: string; evidence: Evidence[] }
  | { status: "restarted"; checkpointId: Id; lineage: { workflowId: Id; abandonedAttemptId: Id; attemptId: Id; reason: string } };
export interface CheckpointRequest { checkpoint: WorkflowCheckpoint; evidence?: Evidence[]; }

/** The /v1/events batch envelope, matching interface_core::Event rather than an invented schema. */
export interface InterfaceEvent { cursor: number; kind: string; payload: unknown; }
export interface EventBatch { events: InterfaceEvent[]; latestAvailable: number; }
export type EventGapReason = "historyLost" | "invalidLimit" | "invalidCursor";
export interface EventGap { reason: EventGapReason; earliestAvailable: number; }
export interface EventGapEnvelope { error: InterfaceError; gap: EventGap; }

export interface ArtifactReference { referenceId: Id; artifactId: string; sha256: string; bytes: number; mediaType: string; }
export interface RequestOptions { signal?: AbortSignal; deadline?: Date | string; timeoutMs?: number; correlationId?: Id; idempotencyKey?: string; }
export interface EventOptions extends RequestOptions {
  /** Broker batch bound: a safe integer from 1 through 256. */
  limit?: number;
  /** Safe GET-only transport retries: a safe integer from 0 through 10. */
  maxTransportRetries?: number;
  /** Delay between safe retries: a safe integer from 0 through 60,000 milliseconds. */
  retryDelayMs?: number;
}
