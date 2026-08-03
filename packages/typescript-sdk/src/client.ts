import { INTERFACE_VERSION, type ArtifactReference, type CheckpointRequest, type CommandEnvelope, type CommandOutcome, type CreateSessionRequest, type EventOptions, type EventGap, type FormSnapshot, type FormSnapshotOptions, type InterfaceError, type InterfaceEvent, type OpenPageRequest, type RecoveryDecision, type RecoveryStatus, type RequestOptions, type RuntimeInfo, type SessionState, type PageState, type WorkflowCheckpoint } from "./contracts.js";
import { RuntimeClientError, type RuntimeErrorRedactor } from "./errors.js";
import { isInterfaceError } from "./events.js";
import { hasExactKeys, isCommandOutcome, isEventBatch, isEventGap, isFormSnapshot, isPageState, isRecoveryDecision, isRecoveryStatus, isRuntimeInfo, isSessionState, isSessionStateList, isUuid, isWorkflowCheckpoint } from "./validators.js";

const JSON_CONTENT_TYPE = /^application\/json(?:\s*;|$)/i;
const DEFAULT_TIMEOUT_MS = 30_000;
const DEFAULT_MAX_ARTIFACT_BYTES = 64 * 1024 * 1024;
const MAX_SDK_ARTIFACT_BYTES = 256 * 1024 * 1024;
const MAX_EVENT_LIMIT = 256;
const MAX_EVENT_TRANSPORT_RETRIES = 10;
const MAX_EVENT_RETRY_DELAY_MS = 60_000;

interface RequestScope { signal: AbortSignal; deadline: Date; redact: RuntimeErrorRedactor; dispose(): void; }
interface ScopedResponse { response: Response; scope: RequestScope; }

function deadlineFor(options: RequestOptions | undefined, redact: RuntimeErrorRedactor): Date {
  const absolute = options?.deadline === undefined ? undefined : new Date(options.deadline);
  const relative = options?.timeoutMs === undefined ? undefined : new Date(Date.now() + options.timeoutMs);
  const deadline = absolute && relative ? new Date(Math.min(absolute.getTime(), relative.getTime())) : absolute ?? relative ?? new Date(Date.now() + DEFAULT_TIMEOUT_MS);
  if (!Number.isFinite(deadline.getTime()) || deadline.getTime() <= Date.now()) throw new RuntimeClientError({ kind: "deadline", message: "Request deadline has already elapsed", redactor: redact });
  return deadline;
}

function composeSignal(caller: AbortSignal | undefined, deadline: Date, redact: RuntimeErrorRedactor): RequestScope {
  const controller = new AbortController();
  let timeout: ReturnType<typeof setTimeout> | undefined;
  let removeCaller = () => {};
  let disposed = false;
  const dispose = () => {
    if (disposed) return;
    disposed = true;
    if (timeout !== undefined) clearTimeout(timeout);
    removeCaller();
  };
  if (caller) {
    const abort = () => { controller.abort(caller.reason); dispose(); };
    if (caller.aborted) abort();
    else {
      caller.addEventListener("abort", abort, { once: true });
      removeCaller = () => caller.removeEventListener("abort", abort);
    }
  }
  if (!controller.signal.aborted) timeout = setTimeout(() => { controller.abort(new Error("deadline")); dispose(); }, Math.max(0, deadline.getTime() - Date.now()));
  return { signal: controller.signal, deadline, redact, dispose };
}

function contentType(response: Response): string | undefined {
  return response.headers.get("content-type")?.split(";", 1)[0]?.trim().toLowerCase();
}

/**
 * Construction options for {@link BrowserRuntimeClient}.
 *
 * `baseUrl` is the runtime origin. A trailing slash and a trailing `/v1` are
 * stripped so callers may pass either `http://127.0.0.1:7777` or `…/v1`.
 */
export interface BrowserRuntimeClientOptions {
  /** Runtime origin (trailing `/` and `/v1` are stripped). */
  baseUrl: string;
  /** Bearer token for `Authorization`. Never logged or retained on errors. */
  bearerToken: string;
  /** Override `globalThis.fetch` (tests, custom agents). */
  fetch?: typeof fetch;
  /**
   * Upper bound for a single artifact verification buffer.
   * Must be a positive safe integer ≤ 256 MiB. Defaults to 64 MiB.
   */
  maxArtifactBytes?: number;
}

/**
 * Authenticated HTTP client for the Bobby Browser `/v1` runtime interface.
 *
 * Every request sends `Authorization`, `x-interface-version`,
 * `x-correlation-id`, and `x-deadline`. Responses are shape-checked against
 * the published contracts; mismatches surface as {@link RuntimeClientError}
 * with `kind: "protocol"`.
 */
export class BrowserRuntimeClient {
  readonly #baseUrl: string;
  readonly #bearerToken: string;
  readonly #fetch: typeof fetch;
  readonly #maxArtifactBytes: number;
  readonly #redact: RuntimeErrorRedactor;

  constructor(options: BrowserRuntimeClientOptions) {
    if (!options.baseUrl || !options.bearerToken) throw new Error("baseUrl and bearerToken are required");
    this.#redact = (value) => value.split(options.bearerToken).join("");
    this.#baseUrl = options.baseUrl.replace(/\/+$/, "").replace(/\/v1$/, "");
    this.#bearerToken = options.bearerToken;
    this.#fetch = options.fetch ?? globalThis.fetch;
    this.#maxArtifactBytes = options.maxArtifactBytes ?? DEFAULT_MAX_ARTIFACT_BYTES;
    if (!Number.isSafeInteger(this.#maxArtifactBytes) || this.#maxArtifactBytes <= 0 || this.#maxArtifactBytes > MAX_SDK_ARTIFACT_BYTES) throw this.#protocol("maxArtifactBytes must be a positive safe integer within the SDK allocation cap");
  }

  /** Node inspect helper — never prints the bearer token. */
  [Symbol.for("nodejs.util.inspect.custom")](): string { return "BrowserRuntimeClient { bearerToken: [redacted] }"; }

  /** `GET /v1/runtime` — version, capabilities, and load counters. */
  async runtimeInfo(options?: RequestOptions): Promise<RuntimeInfo> { return this.#json("GET", "/v1/runtime", undefined, options, isRuntimeInfo); }
  /** `POST /v1/sessions` — create a browser session. */
  async createSession(input: CreateSessionRequest, options?: RequestOptions): Promise<SessionState> { return this.#json("POST", "/v1/sessions", input, options, isSessionState); }
  /** `GET /v1/sessions` — list active sessions. */
  async listSessions(options?: RequestOptions): Promise<SessionState[]> { return this.#json("GET", "/v1/sessions", undefined, options, isSessionStateList); }
  /** `DELETE /v1/sessions/{id}` — tear down a session (`204` on success). */
  async deleteSession(sessionId: string, options?: RequestOptions): Promise<void> {
    if (!isUuid(sessionId)) throw this.#protocol("session id must be a UUID");
    const scoped = await this.#request("DELETE", `/v1/sessions/${encodeURIComponent(sessionId)}`, undefined, options);
    try {
      if (scoped.response.status === 204) return;
      throw this.#responseError(scoped.response.status, await this.#readJson(scoped.response, scoped.scope));
    } finally { scoped.scope.dispose(); }
  }
  /** `POST /v1/pages` — open a page in a session. */
  async openPage(input: OpenPageRequest, options?: RequestOptions): Promise<PageState> { return this.#json("POST", "/v1/pages", input, options, isPageState); }
  /**
   * `GET /v1/sessions/{sessionId}/pages/{pageId}/forms` — semantic form snapshot.
   * @param options.maxControls Optional bound from 1 through 512.
   */
  async formSnapshot(sessionId: string, pageId: string, options: FormSnapshotOptions = {}): Promise<FormSnapshot> {
    if (!isUuid(sessionId) || !isUuid(pageId)) throw this.#protocol("session and page ids must be UUIDs");
    const { maxControls, ...requestOptions } = options;
    if (maxControls !== undefined && (!Number.isSafeInteger(maxControls) || maxControls < 1 || maxControls > 512)) throw this.#protocol("maxControls must be a safe integer between 1 and 512");
    const query = maxControls === undefined ? "" : `?maxControls=${maxControls}`;
    return this.#json("GET", `/v1/sessions/${encodeURIComponent(sessionId)}/pages/${encodeURIComponent(pageId)}/forms${query}`, undefined, requestOptions, isFormSnapshot);
  }

  /**
   * `POST /v1/commands` — submit a {@link CommandEnvelope} (primitive or intent).
   * Validates that the HTTP status matches the outcome `status` mapping.
   */
  async submit(input: CommandEnvelope, options?: RequestOptions): Promise<CommandOutcome> {
    return this.#consumeJson("POST", "/v1/commands", input, options, (response, payload) => {
      if (!isCommandOutcome(payload)) throw this.#responseError(response.status, payload);
      if (commandStatus(payload) !== response.status) throw this.#protocol("command outcome status does not match HTTP mapping", response.status);
      return payload;
    });
  }

  /** `POST /v1/checkpoints` — persist a workflow checkpoint. */
  async checkpoint(input: CheckpointRequest, options?: RequestOptions): Promise<WorkflowCheckpoint> { return this.#json("POST", "/v1/checkpoints", input, options, isWorkflowCheckpoint); }
  /** `GET /v1/recovery/{workflowId}` — current recovery status for a workflow. */
  async recoveryStatus(workflowId: string, options?: RequestOptions): Promise<RecoveryStatus> {
    if (!isUuid(workflowId)) throw this.#protocol("workflow id must be a UUID");
    return this.#json("GET", `/v1/recovery/${encodeURIComponent(workflowId)}`, undefined, options, isRecoveryStatus);
  }
  /**
   * `POST /v1/recovery/{workflowId}` — resume, reconcile, or restart a workflow.
   * `needsReconciliation` maps to HTTP `409`; other decisions to `200`.
   */
  async recover(workflowId: string, options?: RequestOptions): Promise<RecoveryDecision> {
    return this.#consumeJson("POST", `/v1/recovery/${encodeURIComponent(workflowId)}`, undefined, options, (response, payload) => {
      if (!isRecoveryDecision(payload)) throw this.#responseError(response.status, payload);
      if ((payload.status === "needsReconciliation" ? 409 : 200) !== response.status) throw this.#protocol("recovery decision status does not match HTTP mapping", response.status);
      return payload;
    });
  }

  /**
   * `GET /v1/events` — async iterator over interface events after `cursor`.
   *
   * Retries transport failures up to `maxTransportRetries` (default 2).
   * A cursor gap yields {@link RuntimeClientError} with `kind: "http"`,
   * status `409`, and an {@link EventGap} payload.
   */
  async *events(cursor: number, options: EventOptions = {}): AsyncIterable<InterfaceEvent> {
    if (!validEventOptions(cursor, options)) throw this.#protocol("event cursor or retry options are outside SDK bounds");
    const scope = composeSignal(options.signal, deadlineFor(options, this.#redact), this.#redact);
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
        await delay(options.retryDelayMs ?? 50, scope);
        continue;
      }
      failures = 0;
      const payload = await this.#readJson(response, scope);
      if (response.status === 409 && hasExactKeys(payload, ["error", "gap"])) {
        if (!isBrokerEventGapError(payload.error) || !isEventGap(payload.gap)) throw this.#protocol("event gap response has an unexpected shape", response.status);
        throw new RuntimeClientError({ kind: "http", status: response.status, interfaceError: payload.error, eventGap: payload.gap, redactor: this.#redact });
      }
      if (response.status !== 200 || !isEventBatch(payload, after, limit)) throw this.#responseError(response.status, payload);
      for (const event of payload.events) { after = event.cursor; yield event; }
    }} finally { scope.dispose(); }
  }

  /**
   * `GET /v1/artifacts/{artifactId}` — verified artifact body as a byte stream.
   *
   * Checks `Content-Type` and `Content-Length` against the reference, then
   * buffers within `reference.bytes` and verifies SHA-256 before any bytes
   * are delivered to the caller.
   */
  async artifact(reference: ArtifactReference, options?: RequestOptions): Promise<ReadableStream<Uint8Array>> {
    if (!validArtifactReference(reference, this.#maxArtifactBytes)) throw this.#protocol("artifact reference is outside the client hard bound");
    const scoped = await this.#request("GET", `/v1/artifacts/${encodeURIComponent(reference.artifactId)}`, undefined, options);
    const response = scoped.response;
    if (response.status !== 200 || !response.body) {
      try { throw this.#responseError(response.status, await this.#readJson(response, scoped.scope)); } finally { scoped.scope.dispose(); }
    }
    if (contentType(response) !== mediaTypeEssence(reference.mediaType)) { scoped.scope.dispose(); throw this.#protocol("artifact media type does not match its reference", response.status); }
    const length = response.headers.get("content-length");
    if (length === null || !/^\d+$/.test(length) || !Number.isSafeInteger(Number(length)) || Number(length) !== reference.bytes) { scoped.scope.dispose(); throw this.#protocol("artifact content length does not match its reference", response.status); }
    try { return verifiedArtifactStream(response.body, reference, scoped.scope); }
    catch { scoped.scope.dispose(); throw this.#protocol("artifact body could not be consumed", response.status); }
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
    const scope = existing ?? composeSignal(options?.signal, deadlineFor(options, this.#redact), this.#redact);
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
        throw new RuntimeClientError({ kind, message: kind === "deadline" ? "Request deadline exceeded" : "Request was aborted", redactor: this.#redact });
      }
      throw new RuntimeClientError({ kind: "transport", message: "Runtime transport request failed", redactor: this.#redact });
    }
  }

  async #readJson(response: Response, scope: RequestScope): Promise<unknown> {
    if (!JSON_CONTENT_TYPE.test(response.headers.get("content-type") ?? "")) throw this.#protocol("response content type must be application/json", response.status);
    try { const value = await response.json(); if (scope.signal.aborted) throw scopeError(scope); return value; } catch (error) { if (scope.signal.aborted) throw scopeError(scope); throw this.#protocol("response body is not valid JSON", response.status); }
  }

  #responseError(status: number, payload: unknown): RuntimeClientError {
    if (hasExactKeys(payload, ["error"]) && isInterfaceError(payload.error)) {
      if (!interfaceErrorStatusMatches(payload.error, status)) return this.#protocol("interface error status does not match HTTP mapping", status);
      return new RuntimeClientError({ kind: "http", status, interfaceError: payload.error, redactor: this.#redact });
    }
    return this.#protocol("response has an unexpected status or shape", status);
  }
  #protocol(message: string, status?: number): RuntimeClientError { return new RuntimeClientError({ kind: "protocol", status, message, redactor: this.#redact }); }
}

function commandStatus(outcome: CommandOutcome): number {
  if (outcome.status === "completed" || outcome.status === "restarted") return 200;
  if (outcome.status === "retryableFailure") return 503;
  if (outcome.status === "needsReconciliation") return 409;
  if (outcome.status === "policyDenied") return 403;
  if (outcome.status === "resourceExhausted") return 429;
  return outcome.error.code === "invalidRequest" ? 422 : 500;
}

function interfaceErrorStatus(code: InterfaceError["code"]): number {
  if (code === "authenticationFailed" || code === "tokenExpired") return 401;
  if (code === "missingCapability" || code === "malformedScope") return 403;
  if (code === "artifactDenied" || code === "notFound") return 404;
  if (code === "deadlineExceeded") return 408;
  if (code === "idempotencyConflict") return 409;
  if (code === "resourceExhausted") return 429;
  if (code === "internal") return 500;
  return 422;
}

function interfaceErrorStatusMatches(error: InterfaceError, status: number): boolean {
  if (error.reconciliationRequired) return status === 409;
  if (error.code === "invalidRequest") return status === 413 || status === 422;
  return interfaceErrorStatus(error.code) === status;
}

function isBrokerEventGapError(value: unknown): value is InterfaceError {
  return isInterfaceError(value)
    && value.code === "invalidRequest"
    && value.layer === "interface"
    && value.message === "event history has a cursor gap"
    && value.commandId === null
    && value.retryable === false
    && value.retryAfterMs === null
    && value.reconciliationRequired === false
    && value.requiredCapability === null;
}

function isBoundedInteger(value: unknown, minimum: number, maximum: number): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= minimum && value <= maximum;
}

function validEventOptions(cursor: number, options: EventOptions): boolean {
  return isBoundedInteger(cursor, 0, Number.MAX_SAFE_INTEGER)
    && (options.limit === undefined || isBoundedInteger(options.limit, 1, MAX_EVENT_LIMIT))
    && (options.maxTransportRetries === undefined || isBoundedInteger(options.maxTransportRetries, 0, MAX_EVENT_TRANSPORT_RETRIES))
    && (options.retryDelayMs === undefined || isBoundedInteger(options.retryDelayMs, 0, MAX_EVENT_RETRY_DELAY_MS));
}

function validArtifactReference(reference: ArtifactReference, maximum: number): boolean {
  return isUuid(reference.referenceId) && Number.isSafeInteger(reference.bytes) && reference.bytes >= 0 && reference.bytes <= maximum && /^[0-9a-f]{64}$/.test(reference.sha256) && reference.artifactId.length > 0 && reference.artifactId.length <= 128 && /^[0-9A-Fa-f-]+$/.test(reference.artifactId) && mediaTypeEssence(reference.mediaType) !== undefined;
}

function mediaTypeEssence(value: string): string | undefined {
  const essence = value.split(";", 1)[0]?.trim().toLowerCase();
  return essence && /^[!#$%&'*+.^_`|~0-9a-z-]+\/[!#$%&'*+.^_`|~0-9a-z-]+$/.test(essence) ? essence : undefined;
}

function delay(ms: number, scope: RequestScope): Promise<void> {
  return new Promise((resolve, reject) => {
    let timer: ReturnType<typeof setTimeout>;
    const abort = () => { clearTimeout(timer); scope.signal.removeEventListener("abort", abort); reject(scopeError(scope)); };
    const done = () => { scope.signal.removeEventListener("abort", abort); resolve(); };
    timer = setTimeout(done, ms);
    if (scope.signal.aborted) abort(); else scope.signal.addEventListener("abort", abort, { once: true });
  });
}

function scopeError(scope: RequestScope): RuntimeClientError {
  const deadlineAbort = scope.signal.reason instanceof Error && scope.signal.reason.message === "deadline";
  return new RuntimeClientError({ kind: deadlineAbort ? "deadline" : "aborted", message: deadlineAbort ? "Request deadline exceeded" : "Request was aborted", redactor: scope.redact });
}

function artifactProtocolError(scope: RequestScope): RuntimeClientError {
  return new RuntimeClientError({ kind: "protocol", message: "Artifact verification failed", redactor: scope.redact });
}

function raceWithScope<T>(operation: Promise<T>, scope: RequestScope): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    let settled = false;
    const finish = (action: () => void) => {
      if (settled) return;
      settled = true;
      scope.signal.removeEventListener("abort", abort);
      action();
    };
    const abort = () => finish(() => reject(scopeError(scope)));
    if (scope.signal.aborted) abort();
    else scope.signal.addEventListener("abort", abort, { once: true });
    operation.then(
      (value) => finish(() => resolve(value)),
      (error) => finish(() => reject(error)),
    );
  });
}

function verifiedArtifactStream(source: ReadableStream<Uint8Array>, reference: ArtifactReference, scope: RequestScope): ReadableStream<Uint8Array> {
  const reader = source.getReader();
  let data: Uint8Array | undefined;
  let delivered = false;
  let verification: Promise<void> | undefined;
  const verify = async () => {
    // `reference.bytes` is the declared upper bound. Buffer at most that many
    // bytes and do not deliver any until SHA-256 verification succeeds.
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
    const digestBytes = await raceWithScope(crypto.subtle.digest("SHA-256", buffered), scope);
    const digest = Array.from(new Uint8Array(digestBytes)).map((byte) => byte.toString(16).padStart(2, "0")).join("");
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
      } catch (error) {
        const failure = scope.signal.aborted ? scopeError(scope) : error instanceof RuntimeClientError ? error : artifactProtocolError(scope);
        controller.error(failure);
        try { await reader.cancel(failure); } catch { /* keep the sanitized RuntimeClientError */ }
      } finally { scope.dispose(); }
    },
    async cancel() {
      scope.dispose();
      try { await reader.cancel(); }
      catch { throw artifactProtocolError(scope); }
    },
  });
}
