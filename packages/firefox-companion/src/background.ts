import { NativeCompanionTransport, type NativePairRequest } from "./native-transport.js";
import {
  PROTOCOL_VERSION,
  parseCompanionRequest,
  type BrowserIdentity,
  type CompanionCapabilities,
  type CompanionEvent,
} from "./protocol.js";

export type BackgroundConnectOptions = {
  companionId: string;
  profileId: string;
  identity: BrowserIdentity;
  capabilities: CompanionCapabilities;
};

export type PageLease = {
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

type BackgroundDependencies = {
  transport: Transport;
  sendTabMessage(tabId: number, message: unknown, frameId: number): Promise<unknown>;
  navigateTab(tabId: number, url: string): Promise<void>;
  now?: () => number;
};

function routeKey(attachmentId: string, pageId: string): string {
  return `${attachmentId}\u0000${pageId}`;
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
  if (!(["http:", "https:"] as string[]).includes(url.protocol)) {
    throw new Error("navigate URL must use HTTP or HTTPS");
  }
  return url.href;
}

export class CompanionBackground {
  readonly #dependencies: BackgroundDependencies;
  readonly #leases = new Map<string, PageLease>();
  #started = false;

  constructor(dependencies: BackgroundDependencies) {
    this.#dependencies = dependencies;
  }

  connect(options: BackgroundConnectOptions): void {
    if (!this.#started) {
      this.#dependencies.transport.start((message) => this.receive(message));
      this.#started = true;
    }
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

  leasePage(lease: PageLease): void {
    if (
      !Number.isInteger(lease.tabId) ||
      !Number.isInteger(lease.frameId) ||
      !Number.isSafeInteger(lease.expiresAtUnixMs)
    ) {
      throw new Error("page lease is invalid");
    }
    this.#leases.set(routeKey(lease.attachmentId, lease.pageId), lease);
  }

  async receive(message: unknown): Promise<void> {
    const payload = typeof message === "string" ? message : JSON.stringify(message);
    const request = parseCompanionRequest(payload);
    if (request.kind === "ping") {
      this.#dependencies.transport.send({ kind: "pong" });
      return;
    }
    if (request.kind !== "action") return;

    const { input } = request;
    const lease = this.#leases.get(routeKey(input.attachmentId, input.pageId));
    const now = (this.#dependencies.now ?? Date.now)();
    if (!lease || lease.expiresAtUnixMs <= now) {
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
    } catch (error) {
      this.#sendFailure(
        input.commandId,
        "actionFailed",
        error instanceof Error ? error.message : "action failed",
        input.operation !== "observe",
      );
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

type BrowserApi = {
  runtime: {
    connectNative(hostName: string): import("./native-transport.js").NativePort;
    onMessage: { addListener(listener: (message: unknown) => unknown): void };
  };
  tabs: {
    sendMessage(tabId: number, message: unknown, options: { frameId: number }): Promise<unknown>;
    update(tabId: number, properties: { url: string }): Promise<unknown>;
  };
};

declare const browser: BrowserApi | undefined;

if (typeof browser !== "undefined") {
  const transport = new NativeCompanionTransport({
    connectNative: (hostName) => browser.runtime.connectNative(hostName),
  });
  const background = new CompanionBackground({
    transport,
    sendTabMessage: (tabId, message, frameId) =>
      browser.tabs.sendMessage(tabId, message, { frameId }),
    navigateTab: async (tabId, url) => {
      await browser.tabs.update(tabId, { url });
    },
  });
  browser.runtime.onMessage.addListener((message) => {
    if (typeof message !== "object" || message === null || !("type" in message)) return;
    if (message.type === "connectCompanion" && "options" in message) {
      background.connect(message.options as BackgroundConnectOptions);
    } else if (message.type === "leasePage" && "lease" in message) {
      background.leasePage(message.lease as PageLease);
    }
  });
}
