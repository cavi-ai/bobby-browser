import { INTERFACE_VERSION, type ArtifactReference, type CheckpointRequest, type CommandEnvelope, type CommandOutcome, type CreateSessionRequest, type EventOptions, type EventGap, type InterfaceError, type InterfaceEvent, type OpenPageRequest, type RecoveryDecision, type RequestOptions, type RuntimeInfo, type SessionState, type PageState, type WorkflowCheckpoint } from "./contracts.js";
import { RuntimeClientError } from "./errors.js";
import { isErrorLayer, isEventBatch, isEventGap, isInterfaceError, isRecord } from "./events.js";

const JSON_CONTENT_TYPE = /^application\/json(?:\s*;|$)/i;
const DEFAULT_TIMEOUT_MS = 30_000;
const DEFAULT_MAX_ARTIFACT_BYTES = 64 * 1024 * 1024;

interface RequestScope { signal: AbortSignal; deadline: Date; dispose(): void; }
interface ScopedResponse { response: Response; scope: RequestScope; }

function deadlineFor(options: RequestOptions | undefined): Date {
  const absolute = options?.deadline === undefined ? undefined : new Date(options.deadline);
  const relative = options?.timeoutMs === undefined ? undefined : new Date(Date.now() + options.timeoutMs);
  const deadline = absolute && relative ? new Date(Math.min(absolute.getTime(), relative.getTime())) : absolute ?? relative ?? new Date(Date.now() + DEFAULT_TIMEOUT_MS);
  if (!Number.isFinite(deadline.getTime()) || deadline.getTime() <= Date.now()) throw new RuntimeClientError({ kind: "deadline", message: "Request deadline has already elapsed" });
  return deadline;
}

function composeSignal(caller: AbortSignal | undefined, deadline: Date): RequestScope {
  const controller = new AbortController();
  let removeCaller = () => {};
  if (caller) {
    const abort = () => controller.abort(caller.reason);
    if (caller.aborted) abort();
    else {
      caller.addEventListener("abort", abort, { once: true });
      removeCaller = () => caller.removeEventListener("abort", abort);
    }
  }
  const timeout = setTimeout(() => controller.abort(new Error("deadline")), Math.max(0, deadline.getTime() - Date.now()));
  return { signal: controller.signal, deadline, dispose: () => { clearTimeout(timeout); removeCaller(); } };
}

function redactedInterfaceError(value: InterfaceError, bearerToken: string): InterfaceError {
  return { ...value, message: value.message.split(bearerToken).join("[redacted]") };
}

function contentType(response: Response): string | undefined {
  return response.headers.get("content-type")?.split(";", 1)[0]?.trim().toLowerCase();
}

export class BrowserRuntimeClient {
  readonly #baseUrl: string;
  readonly #bearerToken: string;
  readonly #fetch: typeof fetch;
  readonly #maxArtifactBytes: number;

  constructor(options: { baseUrl: string; bearerToken: string; fetch?: typeof fetch; maxArtifactBytes?: number }) {
    if (!options.baseUrl || !options.bearerToken) throw new Error("baseUrl and bearerToken are required");
    this.#baseUrl = options.baseUrl.replace(/\/+$/, "").replace(/\/v1$/, "");
    this.#bearerToken = options.bearerToken;
    this.#fetch = options.fetch ?? globalThis.fetch;
    this.#maxArtifactBytes = options.maxArtifactBytes ?? DEFAULT_MAX_ARTIFACT_BYTES;
    if (!Number.isSafeInteger(this.#maxArtifactBytes) || this.#maxArtifactBytes < 0) throw new Error("maxArtifactBytes must be a nonnegative safe integer");
  }

  [Symbol.for("nodejs.util.inspect.custom")](): string { return "BrowserRuntimeClient { bearerToken: [redacted] }"; }

  async runtimeInfo(options?: RequestOptions): Promise<RuntimeInfo> { return this.#json("GET", "/v1/runtime", undefined, options, isRuntimeInfo); }
  async createSession(input: CreateSessionRequest, options?: RequestOptions): Promise<SessionState> { return this.#json("POST", "/v1/sessions", input, options, isSessionState); }
  async openPage(input: OpenPageRequest, options?: RequestOptions): Promise<PageState> { return this.#json("POST", "/v1/pages", input, options, isPageState); }

  async submit(input: CommandEnvelope, options?: RequestOptions): Promise<CommandOutcome> {
    return this.#consumeJson("POST", "/v1/commands", input, options, (response, payload) => {
      if (!isCommandOutcome(payload)) throw this.#responseError(response.status, payload);
      if (commandStatus(payload) !== response.status) throw this.#protocol("command outcome status does not match broker mapping", response.status);
      return payload;
    });
  }

  async checkpoint(input: CheckpointRequest, options?: RequestOptions): Promise<WorkflowCheckpoint> { return this.#json("POST", "/v1/checkpoints", input, options, isWorkflowCheckpoint); }
  async recover(workflowId: string, options?: RequestOptions): Promise<RecoveryDecision> {
    return this.#consumeJson("POST", `/v1/recovery/${encodeURIComponent(workflowId)}`, undefined, options, (response, payload) => {
      if (!isRecoveryDecision(payload)) throw this.#responseError(response.status, payload);
      if ((payload.status === "needsReconciliation" ? 409 : 200) !== response.status) throw this.#protocol("recovery decision status does not match broker mapping", response.status);
      return payload;
    });
  }

  async *events(cursor: number, options: EventOptions = {}): AsyncIterable<InterfaceEvent> {
    const scope = composeSignal(options.signal, deadlineFor(options));
    let after = cursor;
    const limit = options.limit ?? 100;
    const retries = options.maxTransportRetries ?? 2;
    let failures = 0;
    try { while (true) {
      let response: Response;
      try {
        response = (await this.#request("GET", `/v1/events?after=${encodeURIComponent(after)}&limit=${encodeURIComponent(limit)}`, undefined, options, scope)).response;
      } catch (error) {
        if (!(error instanceof RuntimeClientError) || error.kind !== "transport" || failures >= retries) throw error;
        failures += 1;
        await delay(options.retryDelayMs ?? 50, scope.signal);
        continue;
      }
      failures = 0;
      const payload = await this.#readJson(response, scope);
      if (response.status === 409 && isRecord(payload) && isInterfaceError(payload.error) && isEventGap(payload.gap)) {
        throw new RuntimeClientError({ kind: "http", status: response.status, interfaceError: redactedInterfaceError(payload.error, this.#bearerToken), eventGap: payload.gap });
      }
      if (response.status !== 200 || !isEventBatch(payload)) throw this.#responseError(response.status, payload);
      for (const event of payload.events) { after = event.cursor; yield event; }
    }} finally { scope.dispose(); }
  }

  async artifact(reference: ArtifactReference, options?: RequestOptions): Promise<ReadableStream<Uint8Array>> {
    if (!validArtifactReference(reference, this.#maxArtifactBytes)) throw this.#protocol("artifact reference is outside the client hard bound");
    const scoped = await this.#request("GET", `/v1/artifacts/${encodeURIComponent(reference.artifactId)}`, undefined, options);
    const response = scoped.response;
    if (response.status !== 200 || !response.body) {
      try { throw this.#responseError(response.status, await this.#readJson(response, scoped.scope)); } finally { scoped.scope.dispose(); }
    }
    if (contentType(response) !== mediaTypeEssence(reference.mediaType)) { scoped.scope.dispose(); throw this.#protocol("artifact media type does not match its reference", response.status); }
    const length = response.headers.get("content-length");
    if (length !== null && (!/^\d+$/.test(length) || !Number.isSafeInteger(Number(length)) || Number(length) !== reference.bytes)) { scoped.scope.dispose(); throw this.#protocol("artifact content length does not match its reference", response.status); }
    return verifiedArtifactStream(response.body, reference, scoped.scope);
  }

  async #json<T>(method: string, path: string, body: unknown, options: RequestOptions | undefined, valid: (value: unknown) => value is T): Promise<T> {
    return this.#consumeJson(method, path, body, options, (response, payload) => {
      if (response.status !== 200 || !valid(payload)) throw this.#responseError(response.status, payload);
      return payload;
    });
  }

  async #consumeJson<T>(method: string, path: string, body: unknown, options: RequestOptions | undefined, consume: (response: Response, payload: unknown) => T): Promise<T> {
    const scoped = await this.#request(method, path, body, options);
    try { return consume(scoped.response, await this.#readJson(scoped.response, scoped.scope)); } finally { scoped.scope.dispose(); }
  }

  async #request(method: string, path: string, body: unknown, options: RequestOptions | undefined, existing?: RequestScope): Promise<ScopedResponse> {
    const owned = existing === undefined;
    const scope = existing ?? composeSignal(options?.signal, deadlineFor(options));
    try {
      const response = await this.#fetch(`${this.#baseUrl}${path}`, {
        method,
        signal: scope.signal,
        headers: {
          authorization: `Bearer ${this.#bearerToken}`,
          "x-interface-version": INTERFACE_VERSION,
          "x-correlation-id": options?.correlationId ?? crypto.randomUUID(),
          "x-deadline": scope.deadline.toISOString(),
          ...(options?.idempotencyKey ? { "idempotency-key": options.idempotencyKey } : {}),
          ...(body === undefined ? {} : { "content-type": "application/json" }),
        },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
      return { response, scope };
    } catch (error) {
      if (owned) scope.dispose();
      if (scope.signal.aborted) {
        const kind = scope.deadline.getTime() <= Date.now() ? "deadline" : "aborted";
        throw new RuntimeClientError({ kind, message: kind === "deadline" ? "Request deadline exceeded" : "Request was aborted" });
      }
      throw new RuntimeClientError({ kind: "transport", message: "Runtime transport request failed" });
    }
  }

  async #readJson(response: Response, scope: RequestScope): Promise<unknown> {
    if (!JSON_CONTENT_TYPE.test(response.headers.get("content-type") ?? "")) throw this.#protocol("response content type must be application/json", response.status);
    try { const value = await response.json(); if (scope.signal.aborted) throw scopeError(scope); return value; } catch (error) { if (scope.signal.aborted) throw scopeError(scope); throw this.#protocol("response body is not valid JSON", response.status); }
  }

  #responseError(status: number, payload: unknown): RuntimeClientError {
    if (isRecord(payload) && isInterfaceError(payload.error)) return new RuntimeClientError({ kind: "http", status, interfaceError: redactedInterfaceError(payload.error, this.#bearerToken) });
    return this.#protocol("response has an unexpected status or shape", status);
  }
  #protocol(message: string, status?: number): RuntimeClientError { return new RuntimeClientError({ kind: "protocol", status, message }); }
}

function commandStatus(outcome: CommandOutcome): number {
  if (outcome.status === "completed" || outcome.status === "restarted") return 200;
  if (outcome.status === "retryableFailure") return 503;
  if (outcome.status === "needsReconciliation") return 409;
  if (outcome.status === "policyDenied") return 403;
  if (outcome.status === "resourceExhausted") return 429;
  return outcome.error.code === "invalidRequest" ? 422 : 500;
}

function validArtifactReference(reference: ArtifactReference, maximum: number): boolean {
  return Number.isSafeInteger(reference.bytes) && reference.bytes >= 0 && reference.bytes <= maximum && /^[0-9a-f]{64}$/.test(reference.sha256) && reference.artifactId.length > 0 && reference.artifactId.length <= 128 && mediaTypeEssence(reference.mediaType) !== undefined;
}

function mediaTypeEssence(value: string): string | undefined {
  const essence = value.split(";", 1)[0]?.trim().toLowerCase();
  return essence && /^[!#$%&'*+.^_`|~0-9a-z-]+\/[!#$%&'*+.^_`|~0-9a-z-]+$/.test(essence) ? essence : undefined;
}

function delay(ms: number, signal: AbortSignal | undefined): Promise<void> {
  if (!signal) return new Promise((resolve) => setTimeout(resolve, ms));
  return new Promise((resolve, reject) => {
    let timer: ReturnType<typeof setTimeout>;
    const abort = () => { clearTimeout(timer); signal.removeEventListener("abort", abort); reject(new RuntimeClientError({ kind: "aborted", message: "Request was aborted" })); };
    const done = () => { signal.removeEventListener("abort", abort); resolve(); };
    timer = setTimeout(done, ms);
    if (signal.aborted) abort(); else signal.addEventListener("abort", abort, { once: true });
  });
}

function scopeError(scope: RequestScope): RuntimeClientError {
  return new RuntimeClientError({ kind: scope.deadline.getTime() <= Date.now() ? "deadline" : "aborted", message: scope.deadline.getTime() <= Date.now() ? "Request deadline exceeded" : "Request was aborted" });
}

function isRuntimeInfo(value: unknown): value is RuntimeInfo { return isRecord(value) && typeof value.version === "string" && Array.isArray(value.capabilities) && value.capabilities.every((v) => typeof v === "string") && typeof value.active_sessions === "number" && typeof value.queued_jobs === "number" && typeof value.uptime_ms === "number"; }
function isSessionState(value: unknown): value is SessionState { return isRecord(value) && typeof value.id === "string" && typeof value.profile === "string" && (typeof value.proxy === "string" || value.proxy === null) && Array.isArray(value.page_ids) && typeof value.created_at === "string" && typeof value.last_used_at === "string"; }
function isPageState(value: unknown): value is PageState { return isRecord(value) && typeof value.id === "string" && typeof value.session_id === "string" && (typeof value.url === "string" || value.url === null) && (value.mode === "Document" || value.mode === "Interactive" || value.mode === "Render") && typeof value.ready_state === "string" && typeof value.pending_requests === "number"; }
function isCommandOutcome(value: unknown): value is CommandOutcome { return isRecord(value) && typeof value.commandId === "string" && (value.status === "completed" ? Array.isArray(value.evidence) : value.status === "retryableFailure" || value.status === "policyDenied" || value.status === "failed" ? isCommandError(value.error) : value.status === "needsReconciliation" ? isCommandError(value.error) && Array.isArray(value.evidence) : value.status === "resourceExhausted" ? isCommandError(value.error) && typeof value.retryAfterMs === "number" : value.status === "restarted" ? typeof value.priorAttemptId === "string" && typeof value.attemptId === "string" && typeof value.reason === "string" : false); }
function isCommandError(value: unknown): boolean { return isRecord(value) && isCommandErrorCode(value.code) && typeof value.message === "string" && isErrorLayer(value.layer) && typeof value.retryable === "boolean"; }
function isCommandErrorCode(value: unknown): boolean { return value === "invalidRequest" || value === "notFound" || value === "deadlineExceeded" || value === "browserLaunchFailed" || value === "browserCommandFailed" || value === "verificationFailed" || value === "journalFailed" || value === "resourceExhausted" || value === "policyDenied" || value === "internal" || value === "targetNotFound" || value === "targetAmbiguous" || value === "frameNotFound" || value === "shadowRootUnavailable" || value === "targetDetached" || value === "waitConditionTimedOut" || value === "screenshotCaptureFailed" || value === "networkPolicyDenied" || value === "httpResponseTooLarge" || value === "httpTransferFailed" || value === "httpStateConflict" || value === "httpEquivalenceUnproven"; }
function isWorkflowCheckpoint(value: unknown): value is WorkflowCheckpoint { return isRecord(value) && typeof value.checkpointId === "string" && typeof value.workflowId === "string" && Array.isArray(value.evidence); }
function isRecoveryDecision(value: unknown): value is RecoveryDecision { return isRecord(value) && (value.status === "resumed" ? typeof value.checkpointId === "string" && typeof value.attemptId === "string" && Array.isArray(value.evidence) : value.status === "needsReconciliation" ? typeof value.checkpointId === "string" && typeof value.attemptId === "string" && typeof value.reason === "string" && Array.isArray(value.evidence) : value.status === "restarted" && typeof value.checkpointId === "string" && isRecord(value.lineage)); }

function verifiedArtifactStream(source: ReadableStream<Uint8Array>, reference: ArtifactReference, scope: RequestScope): ReadableStream<Uint8Array> {
  const reader = source.getReader();
  let data: Uint8Array | undefined;
  let delivered = false;
  let verification: Promise<void> | undefined;
  const verify = async () => {
    // `reference.bytes` is the broker's declared, caller-supplied upper bound. We never
    // retain more than that bounded amount, and do not expose any bytes before SHA-256 passes.
    let bytes = 0;
    const buffered = new Uint8Array(reference.bytes);
    while (true) {
      if (scope.signal.aborted) throw scopeError(scope);
      const next = await reader.read();
      if (next.done) break;
      bytes += next.value.byteLength;
      if (bytes > reference.bytes) throw new Error("artifact exceeds its declared bounded size");
      buffered.set(next.value, bytes - next.value.byteLength);
    }
    if (bytes !== reference.bytes) throw new Error("artifact byte count does not match its reference");
    const digest = Array.from(new Uint8Array(await crypto.subtle.digest("SHA-256", buffered))).map((byte) => byte.toString(16).padStart(2, "0")).join("");
    if (scope.signal.aborted) throw scopeError(scope);
    if (digest !== reference.sha256.toLowerCase()) throw new Error("artifact digest does not match its reference");
    data = buffered;
  };
  return new ReadableStream({
    async pull(controller) {
      try {
        verification ??= verify();
        await verification;
        if (delivered) { controller.close(); return; }
        delivered = true;
        controller.enqueue(data!);
      } catch (error) { controller.error(error); await reader.cancel(error); } finally { scope.dispose(); }
    },
    async cancel(reason) { scope.dispose(); await reader.cancel(reason); },
  });
}
