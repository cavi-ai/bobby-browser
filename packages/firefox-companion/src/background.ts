import {
  NativeCompanionTransport,
  parseNativeInboundMessage,
  type NativePairRequest,
  type NativePort,
} from "./native-transport.js";
import {
  PROTOCOL_VERSION,
  type BrowserIdentity,
  type CompanionCapabilities,
  type CompanionEvent,
} from "./protocol.js";

export const MAX_PAGE_LEASES = 256;
export const PAGE_LEASE_TTL_MS = 60_000;
const MAX_ID_COMPONENT_BYTES = 96;

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

type Transport = {
  start(listener: (message: unknown) => void | Promise<void>): void;
  send(message: unknown): void;
  stop(): void;
};

export type BackgroundDependencies = {
  transport: Transport;
  discoverTargets?: () => Promise<readonly DiscoveredTarget[]>;
  sendTabMessage(tabId: number, message: unknown, frameId: number): Promise<unknown>;
  navigateTab(tabId: number, url: string): Promise<void>;
  now?: () => number;
};

function routeKey(attachmentId: string, pageId: string): string {
  return `${attachmentId}\u0000${pageId}`;
}

function attachmentId(options: BackgroundConnectOptions): string {
  return `attachment:${options.companionId}:${options.profileId}`;
}

function pageId(tabId: number, frameId: number): string {
  return `page:${tabId}:${frameId}`;
}

function byteLength(value: string): number {
  return new TextEncoder().encode(value).byteLength;
}

function boundedNonempty(value: unknown, maximum = MAX_ID_COMPONENT_BYTES): value is string {
  return typeof value === "string" && value.length > 0 && byteLength(value) <= maximum;
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
  #options: BackgroundConnectOptions | undefined;
  #paired = false;
  #started = false;

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
    this.#leases.clear();
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
    this.#leases.clear();
    this.#dependencies.transport.stop();
  }

  async receiveRuntimeMessage(
    message: unknown,
    sender: RuntimeSender,
    extensionId: string,
  ): Promise<void> {
    if (
      !object(message) ||
      !exactKeys(message, ["type"]) ||
      message.type !== "companionFrameReady" ||
      sender.id !== extensionId ||
      !validBrowserId(sender.tab?.id) ||
      !validBrowserId(sender.frameId)
    ) {
      return;
    }
    this.#leaseTarget({ tabId: sender.tab.id, frameId: sender.frameId });
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
        output = await this.#dependencies.sendTabMessage(
          lease.tabId,
          { type: "companionAction", operation: input.operation, input: input.input },
          lease.frameId,
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
    } catch {
      this.#sendFailure(
        input.commandId,
        "actionFailed",
        "the content action failed",
        input.operation !== "observe",
      );
    }
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
    this.#leases.clear();
    const targets = (await this.#dependencies.discoverTargets?.()) ?? [];
    for (const target of targets) {
      if (this.#leases.size >= MAX_PAGE_LEASES) break;
      this.#leaseTarget(target);
    }
  }

  #leaseTarget(target: DiscoveredTarget): void {
    if (!this.#paired || !this.#options) return;
    if (!object(target) || !exactKeys(target, ["tabId", "frameId"])) return;
    if (!validBrowserId(target.tabId) || !validBrowserId(target.frameId)) return;
    const now = (this.#dependencies.now ?? Date.now)();
    this.#removeExpired(now);
    const id = attachmentId(this.#options);
    const page = pageId(target.tabId, target.frameId);
    const key = routeKey(id, page);
    if (!this.#leases.has(key) && this.#leases.size >= MAX_PAGE_LEASES) return;
    this.#leases.set(key, {
      companionId: this.#options.companionId,
      profileId: this.#options.profileId,
      attachmentId: id,
      pageId: page,
      tabId: target.tabId,
      frameId: target.frameId,
      expiresAtUnixMs: now + PAGE_LEASE_TTL_MS,
    });
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
    const event: CompanionEvent = {
      kind: "actionFailed",
      output: { commandId, code, message, effectUncertain },
    };
    this.#dependencies.transport.send(event);
  }
}

type StoredIdentity = {
  companionId?: unknown;
  profileId?: unknown;
};

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
      get(keys: readonly string[]): Promise<StoredIdentity>;
      set(values: { companionId: string; profileId: string }): Promise<void>;
    };
  };
  tabs: {
    query(query: Record<string, never>): Promise<Array<{ id?: number; url?: string; title?: string }>>;
    sendMessage(tabId: number, message: unknown, options: { frameId: number }): Promise<unknown>;
    update(tabId: number, properties: { url: string }): Promise<unknown>;
  };
  webNavigation: {
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
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => {
      const targets: DiscoveredTarget[] = [];
      const tabs = await browserApi.tabs.query({});
      for (const tab of tabs) {
        if (targets.length >= MAX_PAGE_LEASES) break;
        if (!validBrowserId(tab.id)) continue;
        let frames: Array<{ frameId: number }> | null = null;
        try {
          frames = await browserApi.webNavigation.getAllFrames({ tabId: tab.id });
        } catch {
          frames = null;
        }
        if (!frames?.length) frames = [{ frameId: 0 }];
        for (const frame of frames) {
          if (targets.length >= MAX_PAGE_LEASES) break;
          if (validBrowserId(frame.frameId)) targets.push({ tabId: tab.id, frameId: frame.frameId });
        }
      }
      return targets;
    },
    sendTabMessage: (tabId, message, frameId) =>
      browserApi.tabs.sendMessage(tabId, message, { frameId }),
    navigateTab: async (tabId, url) => {
      await browserApi.tabs.update(tabId, { url });
    },
  });
  browserApi.runtime.onMessage.addListener((message, sender) => {
    void background
      .receiveRuntimeMessage(message, sender, browserApi.runtime.id)
      .catch(() => undefined);
    return undefined;
  });
  background.connect({
    companionId: stored.companionId,
    profileId: stored.profileId,
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
