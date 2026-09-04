import {
  NativeCompanionTransport,
  createEnrollProfileRequest,
  enrollOperatorMessage,
  parseNativeInboundMessage,
  type EnrollFailureCode,
  type NativePairRequest,
  type NativePort,
} from "./native-transport.js";
import {
  PROTOCOL_VERSION,
  type AttachmentGrant,
  type BrowserIdentity,
  type BrowserTarget,
  type CompanionCapabilities,
  type CompanionEvent,
} from "./protocol.js";
import { syncFingerprintRegistration } from "./fingerprint-registration.js";
import {
  claimFingerprintHostOwnership,
  releaseFingerprintHostOwnership,
  getFingerprintEnabled,
  getFingerprintOwner,
  getFingerprintProfile,
  type FingerprintProfile,
  type FingerprintStorage,
} from "./fingerprint.js";
import { buildPopupStatus, type EnrollPhase, type PopupStatus } from "./popup-status.js";

export const MAX_PAGE_LEASES = 256;
export const PAGE_LEASE_TTL_MS = 60_000;
export const ENROLL_PAIR_TIMEOUT_MS = 30_000;
const ENROLL_OPERATOR_FALLBACK = "Start bobby serve, then Pair again";
const OBSERVATION_RECEIVER_ATTEMPTS = 20;
const OBSERVATION_RECEIVER_DELAY_MS = 50;
const MAX_ID_COMPONENT_BYTES = 96;
const PAGE_BINDING_TITLE_PREFIX = "automation-runtime-binding:";

export type EnrollPairResult =
  | { ok: true }
  | { ok: false; code: string; message: string };

export type BackgroundConnectOptions = {
  companionId: string;
  profileId: string;
  identity: BrowserIdentity;
  capabilities: CompanionCapabilities;
};

export type DiscoveredTarget = {
  tabId: number;
  frameId: number;
};

type RuntimeSender = {
  id?: string;
  tab?: { id?: number };
  frameId?: number;
  url?: string;
};

type PageLease = {
  companionId: string;
  profileId: string;
  attachmentId: string;
  pageId: string;
  tabId: number;
  frameId: number;
  expiresAtUnixMs: number;
};

class ContentDeadlineError extends Error {}

type TabLifecycle = {
  generation: number;
  exists: boolean;
  pendingReconciliations: number;
};

type Transport = {
  start(listener: (message: unknown) => void | Promise<void>): void;
  send(message: unknown): void;
  stop(): void;
  isConnected?: () => boolean;
};

export type BackgroundDependencies = {
  transport: Transport;
  discoverTargets?: () => Promise<readonly DiscoveredTarget[]>;
  discoverTabTargets?: (tabId: number) => Promise<readonly DiscoveredTarget[]>;
  sendTabMessage(tabId: number, message: unknown, frameId: number): Promise<unknown>;
  navigateTab(tabId: number, url: string): Promise<void>;
  createTargetId?: (target: DiscoveredTarget) => string;
  now?: () => number;
  enrollTimeoutMs?: number;
  scheduleTimeout?: (callback: () => void, delayMs: number) => unknown;
  cancelTimeout?: (handle: unknown) => void;
  /** When true, Bobby worker owns fingerprint apply; extension registration clears. */
  setFingerprintManagedByHost?: (
    managed: boolean,
    profile?: FingerprintProfile,
  ) => Promise<void>;
};

function routeKey(attachmentId: string, pageId: string): string {
  return `${attachmentId}\u0000${pageId}`;
}

function browserRouteKey(tabId: number, frameId: number): string {
  return `${tabId}\u0000${frameId}`;
}

function byteLength(value: string): number {
  return new TextEncoder().encode(value).byteLength;
}

function boundedNonempty(value: unknown, maximum = MAX_ID_COMPONENT_BYTES): value is string {
  return typeof value === "string" && value.length > 0 && byteLength(value) <= maximum;
}

function uuid(value: unknown): value is string {
  return (
    boundedNonempty(value) &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value)
  );
}

function exactKeys(value: Record<string, unknown>, expected: readonly string[]): boolean {
  const actual = Object.keys(value).sort();
  const sortedExpected = [...expected].sort();
  return (
    actual.length === sortedExpected.length &&
    actual.every((key, index) => key === sortedExpected[index])
  );
}

function object(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function assertConnectOptions(options: BackgroundConnectOptions): void {
  if (
    !object(options) ||
    !exactKeys(options, ["companionId", "profileId", "identity", "capabilities"]) ||
    !boundedNonempty(options.companionId) ||
    !boundedNonempty(options.profileId) ||
    !object(options.identity) ||
    !exactKeys(options.identity, ["engine", "browserName", "browserVersion", "os", "profileLabel"]) ||
    !(["firefox", "chromium", "webKit"] as unknown[]).includes(options.identity.engine) ||
    !boundedNonempty(options.identity.browserName, 256) ||
    !boundedNonempty(options.identity.browserVersion, 256) ||
    !boundedNonempty(options.identity.os, 256) ||
    !boundedNonempty(options.identity.profileLabel, 256) ||
    !object(options.capabilities) ||
    !exactKeys(options.capabilities, [
      "observe",
      "navigate",
      "nativeInput",
      "tabs",
      "frames",
      "nativeDialogs",
    ]) ||
    !Object.values(options.capabilities).every((value) => typeof value === "boolean")
  ) {
    throw new Error("background connect options are invalid");
  }
}

function validBrowserId(value: unknown): value is number {
  return Number.isSafeInteger(value) && (value as number) >= 0;
}

function supportedFrameUrl(value: unknown): value is string {
  if (typeof value !== "string" || value.length === 0) return false;
  try {
    const url = new URL(value);
    return (url.protocol === "http:" || url.protocol === "https:") && !url.username && !url.password;
  } catch {
    return false;
  }
}

function navigationUrl(input: unknown): string {
  if (
    typeof input !== "object" ||
    input === null ||
    !("url" in input) ||
    typeof input.url !== "string"
  ) {
    throw new Error("navigate requires a URL");
  }
  const url = new URL(input.url);
  if (!(url.protocol === "http:" || url.protocol === "https:") || url.username || url.password) {
    throw new Error("navigate URL must be a credential-free HTTP or HTTPS URL");
  }
  return url.href;
}

export class CompanionBackground {
  readonly #dependencies: BackgroundDependencies;
  readonly #leases = new Map<string, PageLease>();
  readonly #targets = new Map<string, DiscoveredTarget & { kind: BrowserTarget["kind"] }>();
  readonly #targetIdsByRoute = new Map<string, string>();
  readonly #tabLifecycles = new Map<number, TabLifecycle>();
  #options: BackgroundConnectOptions | undefined;
  #paired = false;
  #started = false;
  #lastError: { code: string; message: string } | undefined;
  #unpairedReason = "waiting to pair";
  #enrollPhase: EnrollPhase = "idle";
  #enrollError: { code: string; message: string } | undefined;
  #enrollWaiter:
    | {
        resolve(result: EnrollPairResult): void;
        timeoutHandle: unknown;
      }
    | undefined;

  constructor(dependencies: BackgroundDependencies) {
    this.#dependencies = dependencies;
  }

  connect(options: BackgroundConnectOptions): void {
    assertConnectOptions(options);
    if (!this.#started) {
      this.#dependencies.transport.start((message) => this.receive(message));
      this.#started = true;
    }
    this.#options = structuredClone(options);
    this.#paired = false;
    this.#unpairedReason = "waiting to pair";
    this.#lastError = undefined;
    this.#leases.clear();
    this.#targets.clear();
    this.#targetIdsByRoute.clear();
    this.#tabLifecycles.clear();
    void this.#setFingerprintManagedByHost(false);
    const request: NativePairRequest = {
      kind: "pair",
      input: {
        protocolVersion: PROTOCOL_VERSION,
        companionId: options.companionId,
        profileId: options.profileId,
        identity: options.identity,
        capabilities: options.capabilities,
      },
    };
    this.#dependencies.transport.send(request);
  }

  stop(): void {
    this.#paired = false;
    this.#started = false;
    this.#unpairedReason = "disconnected";
    this.#leases.clear();
    this.#targets.clear();
    this.#targetIdsByRoute.clear();
    this.#tabLifecycles.clear();
    void this.#setFingerprintManagedByHost(false);
    this.#clearEnrollStateOnStop();
    this.#dependencies.transport.stop();
  }

  async enrollPair(): Promise<EnrollPairResult> {
    if (this.#enrollPhase === "pairing" && this.#enrollWaiter) {
      return new Promise((resolve) => {
        const prior = this.#enrollWaiter;
        this.#enrollWaiter = {
          timeoutHandle: prior?.timeoutHandle,
          resolve: (result) => {
            prior?.resolve(result);
            resolve(result);
          },
        };
      });
    }

    const options = this.#options;
    if (!options) {
      return this.#failEnroll("listenerUnavailable");
    }

    this.#enrollPhase = "pairing";
    this.#enrollError = undefined;

    // Fresh native port so enrollProfile is the first host frame (Task 5).
    this.#dependencies.transport.stop();
    this.#started = false;
    this.#paired = false;
    this.#unpairedReason = "waiting to pair";
    this.#leases.clear();
    this.#targets.clear();
    this.#targetIdsByRoute.clear();
    this.#tabLifecycles.clear();
    void this.#setFingerprintManagedByHost(false);

    this.#dependencies.transport.start((message) => this.receive(message));
    this.#started = true;

    try {
      this.#dependencies.transport.send(createEnrollProfileRequest());
    } catch {
      return this.#abortEnrollAfterHostStart("listenerUnavailable");
    }

    try {
      this.connect(options);
    } catch {
      // enrollProfile already reached the host — tear down so it is not left waiting.
      return this.#abortEnrollAfterHostStart("listenerUnavailable");
    }

    return await this.#waitForEnrollResult();
  }

  async getPopupStatus(storage: FingerprintStorage): Promise<PopupStatus> {
    const enabled = await getFingerprintEnabled(storage);
    const owner = await getFingerprintOwner(storage);
    let sessionId: string | undefined;
    let sessionSeed: number | undefined;
    if (owner === "host") {
      const profile = await getFingerprintProfile(storage);
      sessionId = profile.sessionId;
      sessionSeed = profile.sessionSeed;
    }
    return buildPopupStatus({
      paired: this.#paired,
      unpairedReason: this.#paired ? undefined : this.#unpairedReason,
      companionId: this.#options?.companionId,
      profileId: this.#options?.profileId,
      leaseCount: this.#leases.size,
      nativeConnected: this.#dependencies.transport.isConnected?.() ?? false,
      fingerprintEnabled: enabled,
      fingerprintOwner: owner,
      fingerprintSessionId: sessionId,
      fingerprintSessionSeed: sessionSeed,
      lastError: this.#lastError,
      enrollPhase: this.#enrollPhase,
      enrollError: this.#enrollError,
      protocolVersion: PROTOCOL_VERSION,
    });
  }

  async receiveRuntimeMessage(
    message: unknown,
    sender: RuntimeSender,
    extensionId: string,
  ): Promise<EnrollPairResult | void> {
    if (!object(message) || sender.id !== extensionId) {
      return;
    }
    if (exactKeys(message, ["type"]) && message.type === "enrollPair") {
      return this.enrollPair();
    }
    if (!validBrowserId(sender.tab?.id) || !validBrowserId(sender.frameId)) {
      return;
    }
    const frameReady = exactKeys(message, ["type"]) && message.type === "companionFrameReady";
    if (!frameReady || !supportedFrameUrl(sender.url)) {
      return;
    }

    const target = { tabId: sender.tab.id, frameId: sender.frameId };
    if (this.#registerTarget(target)) this.#sendDiscovery();
  }

  receiveTabUpdate(
    tabId: number,
    changeInfo: { title?: string },
    tab: { id?: number; url?: string; title?: string },
  ): void {
    if (!validBrowserId(tabId)) return;
    if (
      tab.id !== tabId ||
      tab.url !== "about:blank" ||
      typeof changeInfo.title !== "string" ||
      changeInfo.title !== tab.title ||
      !changeInfo.title.startsWith(PAGE_BINDING_TITLE_PREFIX)
    ) {
      return;
    }
    const bindingNonce = changeInfo.title.slice(PAGE_BINDING_TITLE_PREFIX.length);
    if (!uuid(bindingNonce)) return;
    this.#reportPageBinding({ tabId, frameId: 0 }, bindingNonce);
  }

  receiveTabRemoved(tabId: number): void {
    if (!validBrowserId(tabId)) return;
    const lifecycle = this.#advanceTabLifecycle(tabId, false);
    if (this.#removeTargets((target) => target.tabId === tabId)) this.#sendDiscovery();
    this.#retireTabTombstone(tabId, lifecycle);
  }

  async reconcileTab(tabId: number): Promise<void> {
    if (!validBrowserId(tabId) || !this.#dependencies.discoverTabTargets) return;
    const lifecycle = this.#advanceTabLifecycle(tabId, true);
    const generation = lifecycle.generation;
    lifecycle.pendingReconciliations += 1;
    try {
      const discovered = await this.#dependencies.discoverTabTargets(tabId);
      if (
        this.#tabLifecycles.get(tabId) !== lifecycle ||
        lifecycle.generation !== generation ||
        !lifecycle.exists
      ) {
        return;
      }
      const current = new Map<string, DiscoveredTarget>();
      for (const target of discovered) {
        if (
          object(target) &&
          exactKeys(target, ["tabId", "frameId"]) &&
          target.tabId === tabId &&
          validBrowserId(target.frameId)
        ) {
          current.set(browserRouteKey(target.tabId, target.frameId), target);
        }
      }
      let changed = this.#removeTargets(
        (target) =>
          target.tabId === tabId && !current.has(browserRouteKey(target.tabId, target.frameId)),
      );
      for (const target of current.values()) changed = this.#registerTarget(target) || changed;
      if (changed) this.#sendDiscovery();
    } finally {
      lifecycle.pendingReconciliations -= 1;
      this.#retireTabTombstone(tabId, lifecycle);
    }
  }

  #advanceTabLifecycle(tabId: number, exists: boolean): TabLifecycle {
    let lifecycle = this.#tabLifecycles.get(tabId);
    if (!lifecycle) {
      lifecycle = { generation: 0, exists, pendingReconciliations: 0 };
      this.#tabLifecycles.set(tabId, lifecycle);
    }
    lifecycle.generation += 1;
    lifecycle.exists = exists;
    return lifecycle;
  }

  #retireTabTombstone(tabId: number, lifecycle: TabLifecycle): void {
    if (
      !lifecycle.exists &&
      lifecycle.pendingReconciliations === 0 &&
      this.#tabLifecycles.get(tabId) === lifecycle
    ) {
      this.#tabLifecycles.delete(tabId);
    }
  }

  #reportPageBinding(target: DiscoveredTarget, bindingNonce: string): void {
    if (this.#registerTarget(target)) this.#sendDiscovery();
    if (!this.#paired || !this.#options) return;
    const targetId = this.#targetIdsByRoute.get(browserRouteKey(target.tabId, target.frameId));
    if (!targetId) return;
    const event: CompanionEvent = {
      kind: "pageBindingDiscovered",
      output: {
        protocolVersion: PROTOCOL_VERSION,
        profileId: this.#options.profileId,
        targetId,
        bindingNonce,
      },
    };
    this.#dependencies.transport.send(event);
  }

  async receive(message: unknown): Promise<void> {
    const incoming = parseNativeInboundMessage(message);
    if (incoming.kind === "paired") {
      await this.#acceptPairing(incoming);
      return;
    }
    if (incoming.kind === "ping") {
      this.#dependencies.transport.send({ kind: "pong" });
      return;
    }
    if (incoming.kind === "grant") {
      this.#acceptGrant(incoming.input);
      return;
    }
    if (incoming.kind === "nativeStatus") {
      this.#handleNativeStatus(incoming.output);
      return;
    }

    const { input } = incoming;
    const now = (this.#dependencies.now ?? Date.now)();
    this.#removeExpired(now);
    const lease = this.#leases.get(routeKey(input.attachmentId, input.pageId));
    if (
      !this.#options ||
      !this.#paired ||
      !lease ||
      lease.companionId !== this.#options.companionId ||
      lease.profileId !== this.#options.profileId ||
      lease.expiresAtUnixMs <= now
    ) {
      this.#sendFailure(
        input.commandId,
        "leaseExpired",
        "the page lease is missing or expired",
        false,
      );
      return;
    }
    if (input.deadlineUnixMs <= now) {
      this.#sendFailure(input.commandId, "deadlineExceeded", "the command deadline expired", false);
      return;
    }

    try {
      let output: unknown;
      if (input.operation === "navigate") {
        const url = navigationUrl(input.input);
        await this.#dependencies.navigateTab(lease.tabId, url);
        output = { url };
      } else {
        output = await this.#sendContentAction(
          lease,
          input.operation,
          input.input,
          input.deadlineUnixMs,
        );
      }
      const event: CompanionEvent = {
        kind: "actionCompleted",
        output: {
          commandId: input.commandId,
          interactionPath: "extensionApi",
          output,
        },
      };
      this.#dependencies.transport.send(event);
    } catch (error) {
      const deadlineExceeded = error instanceof ContentDeadlineError;
      this.#sendFailure(
        input.commandId,
        deadlineExceeded ? "deadlineExceeded" : "actionFailed",
        deadlineExceeded ? "the command deadline expired" : "the content action failed",
        input.operation !== "observe" && input.operation !== "a11yTree",
      );
    }
  }

  async #sendContentAction(
    lease: PageLease,
    operation: string,
    input: unknown,
    deadlineUnixMs: number,
  ): Promise<unknown> {
    const attempts = operation === "observe" ? OBSERVATION_RECEIVER_ATTEMPTS : 1;
    let lastError: unknown = new Error("the content receiver returned no result");
    for (let attempt = 0; attempt < attempts; attempt += 1) {
      const now = (this.#dependencies.now ?? Date.now)();
      const remaining = deadlineUnixMs - now;
      if (remaining <= 0) throw new ContentDeadlineError();
      let timeout: ReturnType<typeof setTimeout> | undefined;
      try {
        const output = await Promise.race([
          this.#dependencies.sendTabMessage(
            lease.tabId,
            { type: "companionAction", operation, input },
            lease.frameId,
          ),
          new Promise<never>((_resolve, reject) => {
            timeout = setTimeout(() => reject(new ContentDeadlineError()), remaining);
          }),
        ]);
        if (output !== undefined) return output;
        lastError = new Error("the content receiver returned no result");
      } catch (error) {
        if (error instanceof ContentDeadlineError) throw error;
        lastError = error;
      } finally {
        if (timeout !== undefined) clearTimeout(timeout);
      }
      if (attempt + 1 < attempts) {
        const delayRemaining = deadlineUnixMs - (this.#dependencies.now ?? Date.now)();
        if (delayRemaining <= 0) throw new ContentDeadlineError();
        await new Promise((resolve) =>
          setTimeout(resolve, Math.min(OBSERVATION_RECEIVER_DELAY_MS, delayRemaining)),
        );
      }
    }
    throw lastError;
  }

  async #acceptPairing(event: Extract<CompanionEvent, { kind: "paired" }>): Promise<void> {
    if (
      !this.#options ||
      event.output.companionId !== this.#options.companionId ||
      event.output.profileId !== this.#options.profileId
    ) {
      throw new Error("paired identity does not match the requested profile");
    }
    this.#paired = true;
    this.#lastError = undefined;
    this.#leases.clear();
    this.#targets.clear();
    this.#targetIdsByRoute.clear();
    this.#tabLifecycles.clear();
    await this.#setFingerprintManagedByHost(true, undefined);
    const targets = (await this.#dependencies.discoverTargets?.()) ?? [];
    for (const target of targets) {
      if (this.#targets.size >= MAX_PAGE_LEASES) break;
      this.#registerTarget(target);
    }
    this.#sendDiscovery();
    this.#resolveEnroll({ ok: true });
  }

  #handleNativeStatus(
    output:
      | { state: "invalidAuth" | "revoked" }
      | { state: "enrollOk" }
      | { state: "enrollFailed"; code: EnrollFailureCode },
  ): void {
    if (output.state === "enrollFailed") {
      this.#resolveEnroll({
        ok: false,
        code: output.code,
        message: enrollOperatorMessage(output.code) ?? ENROLL_OPERATOR_FALLBACK,
      });
      return;
    }
    if (output.state === "enrollOk") {
      // paired already resolves the waiter; enrollOk alone still counts as success.
      if (this.#paired) {
        this.#resolveEnroll({ ok: true });
      }
    }
  }

  #waitForEnrollResult(): Promise<EnrollPairResult> {
    return new Promise((resolve) => {
      const schedule =
        this.#dependencies.scheduleTimeout ??
        ((callback: () => void, delayMs: number) => setTimeout(callback, delayMs));
      const timeoutMs = this.#dependencies.enrollTimeoutMs ?? ENROLL_PAIR_TIMEOUT_MS;
      const timeoutHandle = schedule(() => {
        this.#resolveEnroll({
          ok: false,
          code: "timeout",
          message: enrollOperatorMessage("timeout") ?? "Pairing timed out",
        });
      }, timeoutMs);
      this.#enrollWaiter = { resolve, timeoutHandle };
    });
  }

  #failEnroll(code: EnrollFailureCode): EnrollPairResult {
    const result: EnrollPairResult = {
      ok: false,
      code,
      message: enrollOperatorMessage(code) ?? ENROLL_OPERATOR_FALLBACK,
    };
    this.#enrollPhase = "failed";
    this.#enrollError = { code: result.code, message: result.message };
    return result;
  }

  /** Stop the native port after enrollProfile may have reached the host. */
  #abortEnrollAfterHostStart(code: EnrollFailureCode): EnrollPairResult {
    this.#dependencies.transport.stop();
    this.#started = false;
    this.#paired = false;
    this.#unpairedReason = "waiting to pair";
    return this.#failEnroll(code);
  }

  /** Cancel an in-flight enroll timeout and drop stale enroll UI state. */
  #clearEnrollStateOnStop(): void {
    const waiter = this.#enrollWaiter;
    this.#enrollWaiter = undefined;
    if (waiter?.timeoutHandle !== undefined) {
      (this.#dependencies.cancelTimeout ?? clearTimeout)(waiter.timeoutHandle as never);
    }
    this.#enrollPhase = "idle";
    this.#enrollError = undefined;
    if (waiter) {
      waiter.resolve({
        ok: false,
        code: "listenerUnavailable",
        message:
          enrollOperatorMessage("listenerUnavailable") ?? ENROLL_OPERATOR_FALLBACK,
      });
    }
  }

  #resolveEnroll(result: EnrollPairResult): void {
    const waiter = this.#enrollWaiter;
    if (!waiter) {
      if (result.ok) {
        this.#enrollPhase = "idle";
        this.#enrollError = undefined;
      } else {
        this.#enrollPhase = "failed";
        this.#enrollError = { code: result.code, message: result.message };
      }
      return;
    }
    this.#enrollWaiter = undefined;
    if (waiter.timeoutHandle !== undefined) {
      (this.#dependencies.cancelTimeout ?? clearTimeout)(waiter.timeoutHandle as never);
    }
    if (result.ok) {
      this.#enrollPhase = "idle";
      this.#enrollError = undefined;
    } else {
      this.#enrollPhase = "failed";
      this.#enrollError = { code: result.code, message: result.message };
    }
    waiter.resolve(result);
  }

  async #setFingerprintManagedByHost(
    managed: boolean,
    profile?: FingerprintProfile,
  ): Promise<void> {
    const hook = this.#dependencies.setFingerprintManagedByHost;
    if (!hook) return;
    try {
      await hook(managed, profile);
    } catch {
      /* fingerprint ownership is best-effort relative to pairing */
    }
  }

  #registerTarget(target: DiscoveredTarget): boolean {
    if (!this.#paired || !this.#options) return false;
    if (!object(target) || !exactKeys(target, ["tabId", "frameId"])) return false;
    if (!validBrowserId(target.tabId) || !validBrowserId(target.frameId)) return false;
    const browserKey = browserRouteKey(target.tabId, target.frameId);
    if (this.#targetIdsByRoute.has(browserKey)) return false;
    if (this.#targets.size >= MAX_PAGE_LEASES) return false;
    const targetId =
      this.#dependencies.createTargetId?.(target) ?? globalThis.crypto?.randomUUID?.();
    if (!boundedNonempty(targetId, 256) || this.#targets.has(targetId)) return false;
    this.#targetIdsByRoute.set(browserKey, targetId);
    this.#targets.set(targetId, {
      ...target,
      kind: target.frameId === 0 ? "page" : "frame",
    });
    return true;
  }

  #removeTargets(predicate: (target: DiscoveredTarget) => boolean): boolean {
    const removedRoutes = new Set<string>();
    for (const [targetId, target] of this.#targets) {
      if (!predicate(target)) continue;
      const browserKey = browserRouteKey(target.tabId, target.frameId);
      removedRoutes.add(browserKey);
      this.#targets.delete(targetId);
      this.#targetIdsByRoute.delete(browserKey);
    }
    if (removedRoutes.size === 0) return false;
    for (const [key, lease] of this.#leases) {
      if (removedRoutes.has(browserRouteKey(lease.tabId, lease.frameId))) this.#leases.delete(key);
    }
    return true;
  }

  #sendDiscovery(): void {
    if (!this.#paired || !this.#options) return;
    const event: CompanionEvent = {
      kind: "targetsDiscovered",
      output: {
        protocolVersion: PROTOCOL_VERSION,
        profileId: this.#options.profileId,
        targets: [...this.#targets.entries()].map(([targetId, target]) => ({
          targetId,
          kind: target.kind,
        })),
      },
    };
    this.#dependencies.transport.send(event);
  }

  #acceptGrant(grant: AttachmentGrant): void {
    if (!this.#paired || !this.#options || grant.profileId !== this.#options.profileId) {
      throw new Error("attachment grant profile does not match the paired profile");
    }
    const now = (this.#dependencies.now ?? Date.now)();
    this.#removeExpired(now);
    if (grant.expiresAtUnixMs <= now) {
      throw new Error("attachment grant is expired");
    }
    // Merge, not replace: every runtime session holds its own attachment, and
    // dropping earlier attachments' leases would wedge their in-flight work.
    for (const [key, lease] of this.#leases) {
      if (lease.attachmentId === grant.attachmentId) this.#leases.delete(key);
    }
    for (const page of grant.pages) {
      const target = this.#targets.get(page.targetId);
      if (!target) throw new Error("attachment grant names an undiscovered browser target");
      const lease: PageLease = {
        companionId: this.#options.companionId,
        profileId: this.#options.profileId,
        attachmentId: grant.attachmentId,
        pageId: page.pageId,
        tabId: target.tabId,
        frameId: target.frameId,
        expiresAtUnixMs: grant.expiresAtUnixMs,
      };
      this.#leases.set(routeKey(lease.attachmentId, lease.pageId), lease);
    }
  }

  #removeExpired(now: number): void {
    for (const [key, lease] of this.#leases) {
      if (lease.expiresAtUnixMs <= now) this.#leases.delete(key);
    }
  }

  #sendFailure(
    commandId: string,
    code: string,
    message: string,
    effectUncertain: boolean,
  ): void {
    this.#lastError = { code, message };
    const event: CompanionEvent = {
      kind: "actionFailed",
      output: { commandId, code, message, effectUncertain },
    };
    this.#dependencies.transport.send(event);
  }
}

export type ProductionBrowserApi = {
  runtime: {
    id: string;
    connectNative(hostName: string): NativePort;
    onMessage: {
      addListener(listener: (message: unknown, sender: RuntimeSender) => unknown): void;
    };
    getBrowserInfo(): Promise<{ name: string; version: string }>;
    getPlatformInfo(): Promise<{ os: string }>;
  };
  storage: {
    local: {
      get(keys: readonly string[]): Promise<Record<string, unknown>>;
      set(values: Record<string, unknown>): Promise<void>;
    };
  };
  contentScripts?: {
    register(options: {
      matches: string[];
      js: Array<{ code: string }>;
      runAt: "document_start";
      allFrames: boolean;
      matchAboutBlank: boolean;
    }): Promise<{ unregister(): Promise<void> | void }>;
  };
  tabs: {
    onUpdated: {
      addListener(
        listener: (
          tabId: number,
          changeInfo: { title?: string },
          tab: { id?: number; url?: string; title?: string },
        ) => void,
      ): void;
    };
    onRemoved: {
      addListener(
        listener: (
          tabId: number,
          removeInfo: { windowId: number; isWindowClosing: boolean },
        ) => void,
      ): void;
    };
    query(query: Record<string, never>): Promise<Array<{ id?: number; url?: string; title?: string }>>;
    sendMessage(tabId: number, message: unknown, options: { frameId: number }): Promise<unknown>;
    update(tabId: number, properties: { url: string }): Promise<unknown>;
  };
  webNavigation: {
    onCommitted: {
      addListener(
        listener: (details: { tabId: number; frameId: number; url: string }) => void,
      ): void;
    };
    getAllFrames(details: { tabId: number }): Promise<Array<{ frameId: number; url: string }> | null>;
  };
};

function secureId(): string {
  const id = globalThis.crypto?.randomUUID?.();
  if (!id) throw new Error("secure browser identity generation is unavailable");
  return id;
}

async function loadIdentity(browserApi: ProductionBrowserApi): Promise<{
  companionId: string;
  profileId: string;
}> {
  const stored = await browserApi.storage.local.get(["companionId", "profileId"]);
  if (boundedNonempty(stored.companionId) && boundedNonempty(stored.profileId)) {
    return { companionId: stored.companionId, profileId: stored.profileId };
  }
  const created = { companionId: secureId(), profileId: secureId() };
  await browserApi.storage.local.set(created);
  return created;
}

export async function startProductionBackground(
  browserApi: ProductionBrowserApi,
): Promise<CompanionBackground> {
  const [stored, browserInfo, platformInfo] = await Promise.all([
    loadIdentity(browserApi),
    browserApi.runtime.getBrowserInfo(),
    browserApi.runtime.getPlatformInfo(),
  ]);
  const transport = new NativeCompanionTransport({
    connectNative: (hostName) => browserApi.runtime.connectNative(hostName),
  });
  const discoverTabTargets = async (
    tabId: number,
    fallbackUrl?: string,
  ): Promise<DiscoveredTarget[]> => {
    let frames: Array<{ frameId: number; url: string }> | null;
    try {
      frames = await browserApi.webNavigation.getAllFrames({ tabId });
    } catch {
      frames = supportedFrameUrl(fallbackUrl) ? [{ frameId: 0, url: fallbackUrl }] : [];
    }
    if (!frames?.length && supportedFrameUrl(fallbackUrl)) {
      frames = [{ frameId: 0, url: fallbackUrl }];
    }
    return (frames ?? []).flatMap((frame) =>
      validBrowserId(frame.frameId) && supportedFrameUrl(frame.url)
        ? [{ tabId, frameId: frame.frameId }]
        : [],
    );
  };
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => {
      const targets: DiscoveredTarget[] = [];
      const tabs = await browserApi.tabs.query({});
      for (const tab of tabs) {
        if (targets.length >= MAX_PAGE_LEASES) break;
        if (!validBrowserId(tab.id)) continue;
        try {
          const discovered = await discoverTabTargets(tab.id, tab.url);
          targets.push(...discovered.slice(0, MAX_PAGE_LEASES - targets.length));
        } catch {
          continue;
        }
      }
      return targets;
    },
    discoverTabTargets: (tabId) => discoverTabTargets(tabId),
    sendTabMessage: (tabId, message, frameId) =>
      browserApi.tabs.sendMessage(tabId, message, { frameId }),
    navigateTab: async (tabId, url) => {
      await browserApi.tabs.update(tabId, { url });
    },
    setFingerprintManagedByHost: async (managed, profile) => {
      if (managed) {
        await claimFingerprintHostOwnership(browserApi.storage, profile);
      } else {
        await releaseFingerprintHostOwnership(browserApi.storage);
      }
      await syncFingerprintRegistration(browserApi);
    },
  });
  browserApi.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
    background.receiveTabUpdate(tabId, changeInfo, tab);
  });
  browserApi.tabs.onRemoved.addListener((tabId) => {
    background.receiveTabRemoved(tabId);
  });
  browserApi.webNavigation.onCommitted.addListener((details) => {
    void background.reconcileTab(details.tabId).catch(() => undefined);
  });
  browserApi.runtime.onMessage.addListener((message, _sender) => {
    if (
      typeof message === "object" &&
      message !== null &&
      "type" in message &&
      message.type === "fingerprintSync"
    ) {
      return syncFingerprintRegistration(browserApi);
    }
    if (
      typeof message === "object" &&
      message !== null &&
      "type" in message &&
      message.type === "popupStatus"
    ) {
      return background.getPopupStatus(browserApi.storage);
    }
    if (
      typeof message === "object" &&
      message !== null &&
      "type" in message &&
      message.type === "enrollPair"
    ) {
      return background.receiveRuntimeMessage(
        message,
        _sender,
        browserApi.runtime.id,
      );
    }
    void background
      .receiveRuntimeMessage(message, _sender, browserApi.runtime.id)
      .catch(() => undefined);
    return undefined;
  });
  void syncFingerprintRegistration(browserApi).catch(() => undefined);
  background.connect({
    companionId: String(stored.companionId),
    profileId: String(stored.profileId),
    identity: {
      engine: "firefox",
      browserName: browserInfo.name,
      browserVersion: browserInfo.version,
      os: platformInfo.os,
      profileLabel: "firefox-profile",
    },
    capabilities: {
      observe: true,
      navigate: true,
      nativeInput: false,
      tabs: true,
      frames: true,
      nativeDialogs: false,
    },
  });
  return background;
}

declare const browser: ProductionBrowserApi | undefined;

if (typeof browser !== "undefined") {
  void startProductionBackground(browser).catch(() => undefined);
}
