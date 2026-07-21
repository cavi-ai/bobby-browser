import assert from "node:assert/strict";
import test from "node:test";

import * as backgroundModule from "../src/background.js";
import { CompanionBackground } from "../src/background.js";

const CONNECT_OPTIONS = {
  companionId: "companion-1",
  profileId: "profile-1",
  identity: {
    engine: "firefox" as const,
    browserName: "Firefox",
    browserVersion: "128.0",
    os: "macos",
    profileLabel: "default-release",
  },
  capabilities: {
    observe: true,
    navigate: true,
    nativeInput: false,
    tabs: true,
    frames: true,
    nativeDialogs: false,
  },
};

const ATTACHMENT_ID = "attachment:companion-1:profile-1";

function pageId(tabId: number, frameId: number): string {
  return `page:${tabId}:${frameId}`;
}

function action(tabId: number, frameId: number, overrides: Record<string, unknown> = {}) {
  return {
    kind: "action",
    input: {
      protocolVersion: 1,
      attachmentId: ATTACHMENT_ID,
      commandId: "command-1",
      pageId: pageId(tabId, frameId),
      operation: "observe",
      input: {},
      deadlineUnixMs: 120_000,
      ...overrides,
    },
  };
}

class FakeTransport {
  readonly sent: unknown[] = [];
  listener: ((message: unknown) => void | Promise<void>) | undefined;

  start(listener: (message: unknown) => void | Promise<void>): void {
    this.listener = listener;
  }

  send(message: unknown): void {
    this.sent.push(message);
  }

  stop(): void {}
}

async function pair(background: CompanionBackground): Promise<void> {
  await background.receive({
    kind: "paired",
    output: { companionId: CONNECT_OPTIONS.companionId, profileId: CONNECT_OPTIONS.profileId },
  });
}

test("paired discovery creates profile-bound leases and rejects unrelated routes", async () => {
  const transport = new FakeTransport();
  const routed: unknown[] = [];
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [{ tabId: 9, frameId: 4 }],
    async sendTabMessage(tabId, message, frameId) {
      routed.push({ tabId, message, frameId });
      return { controls: [] };
    },
    async navigateTab() {},
    now: () => 1_000,
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);

  await background.receive(action(9, 4));
  await background.receive(action(9, 4, { attachmentId: "attachment-unrelated" }));

  assert.equal(routed.length, 1);
  assert.deepEqual(routed[0], {
    tabId: 9,
    frameId: 4,
    message: { type: "companionAction", operation: "observe", input: {} },
  });
  assert.deepEqual(transport.sent.at(-1), {
    kind: "actionFailed",
    output: {
      commandId: "command-1",
      code: "leaseExpired",
      message: "the page lease is missing or expired",
      effectUncertain: false,
    },
  });
});

test("spoofed pairing and sender IDs cannot mint routes", async () => {
  const transport = new FakeTransport();
  let routed = false;
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [{ tabId: -1, frameId: 0 }, { tabId: 8, frameId: -2 }],
    async sendTabMessage() {
      routed = true;
      return {};
    },
    async navigateTab() {},
    now: () => 1_000,
  });
  background.connect(CONNECT_OPTIONS);

  await assert.rejects(
    background.receive({
      kind: "paired",
      output: { companionId: "spoofed", profileId: CONNECT_OPTIONS.profileId },
    }),
    /paired|identity|profile/i,
  );
  await pair(background);
  await background.receiveRuntimeMessage(
    { type: "companionFrameReady", tabId: 41, frameId: 12 },
    { id: "evil-extension", tab: { id: 10 }, frameId: 3 },
    "trusted-extension",
  );
  await background.receiveRuntimeMessage(
    { type: "companionFrameReady", tabId: 41, frameId: 12 },
    { id: "trusted-extension", tab: { id: -10 }, frameId: -3 },
    "trusted-extension",
  );

  await background.receive(action(10, 3));
  await background.receive(action(41, 12));

  assert.equal(routed, false);
});

test("trusted runtime sender metadata creates the route without trusting payload IDs", async () => {
  const transport = new FakeTransport();
  const routed: unknown[] = [];
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [],
    async sendTabMessage(tabId, _message, frameId) {
      routed.push({ tabId, frameId });
      return {};
    },
    async navigateTab() {},
    now: () => 1_000,
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);

  await background.receiveRuntimeMessage(
    { type: "companionFrameReady" },
    { id: "trusted-extension", tab: { id: 10 }, frameId: 3 },
    "trusted-extension",
  );
  await background.receive(action(10, 3));

  assert.deepEqual(routed, [{ tabId: 10, frameId: 3 }]);
});

test("lease capacity is bounded and leases expire", async () => {
  const transport = new FakeTransport();
  let now = 1_000;
  const routed: number[] = [];
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () =>
      Array.from({ length: 257 }, (_, tabId) => ({ tabId, frameId: 0 })),
    async sendTabMessage(tabId) {
      routed.push(tabId);
      return {};
    },
    async navigateTab() {},
    now: () => now,
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);

  await background.receive(action(255, 0));
  await background.receive(action(256, 0));
  now = 61_001;
  await background.receive(action(0, 0));

  assert.deepEqual(routed, [255]);
  assert.equal(
    transport.sent.filter(
      (message) =>
        typeof message === "object" && message !== null && "kind" in message && message.kind === "actionFailed",
    ).length,
    2,
  );
});

class ListenerSet<T extends (...arguments_: never[]) => unknown> {
  readonly listeners: T[] = [];

  addListener(listener: T): void {
    this.listeners.push(listener);
  }

  emit(...arguments_: Parameters<T>): void {
    for (const listener of this.listeners) listener(...arguments_);
  }
}

class FakeNativePort {
  readonly sent: unknown[] = [];
  readonly onMessage = new ListenerSet<(message: unknown) => void>();
  readonly onDisconnect = new ListenerSet<() => void>();

  postMessage(message: unknown): void {
    this.sent.push(message);
  }

  disconnect(): void {}
}

test("production startup pairs, discovers tabs and frames, leases, and routes", async () => {
  const port = new FakeNativePort();
  const runtimeMessages = new ListenerSet<(
    message: unknown,
    sender: { id?: string; tab?: { id?: number }; frameId?: number },
  ) => unknown>();
  const routed: unknown[] = [];
  const queried: unknown[] = [];
  const frames: number[] = [];
  const browserApi = {
    runtime: {
      id: "trusted-extension",
      connectNative: () => port,
      onMessage: runtimeMessages,
      async getBrowserInfo() {
        return { name: "Firefox", version: "128.0" };
      },
      async getPlatformInfo() {
        return { os: "mac" };
      },
    },
    storage: {
      local: {
        async get() {
          return { companionId: "companion-1", profileId: "profile-1" };
        },
        async set() {},
      },
    },
    tabs: {
      async query(query: unknown) {
        queried.push(query);
        return [
          { id: -1, url: "https://invalid.test/", title: "Invalid" },
          { id: 9, url: "https://example.test/", title: "Example" },
        ];
      },
      async sendMessage(tabId: number, message: unknown, options: { frameId: number }) {
        routed.push({ tabId, message, frameId: options.frameId });
        return { controls: [] };
      },
      async update() {},
    },
    webNavigation: {
      async getAllFrames({ tabId }: { tabId: number }) {
        frames.push(tabId);
        return [
          { frameId: 0, url: "https://example.test/" },
          { frameId: 4, url: "https://example.test/frame" },
        ];
      },
    },
  };
  const startProductionBackground = (
    backgroundModule as typeof backgroundModule & {
      startProductionBackground(api: unknown): Promise<CompanionBackground>;
    }
  ).startProductionBackground;

  await startProductionBackground(browserApi);
  assert.equal(port.sent[0] && (port.sent[0] as { kind?: string }).kind, "pair");
  port.onMessage.emit({
    kind: "paired",
    output: { companionId: "companion-1", profileId: "profile-1" },
  });
  await new Promise((resolve) => setImmediate(resolve));
  port.onMessage.emit(action(9, 4, { deadlineUnixMs: Date.now() + 60_000 }));
  await new Promise((resolve) => setImmediate(resolve));

  assert.deepEqual(queried, [{}]);
  assert.deepEqual(frames, [9]);
  assert.deepEqual(routed, [
    {
      tabId: 9,
      frameId: 4,
      message: { type: "companionAction", operation: "observe", input: {} },
    },
  ]);
  assert.equal(
    port.sent.some(
      (message) =>
        typeof message === "object" &&
        message !== null &&
        "kind" in message &&
        message.kind === "actionCompleted",
    ),
    true,
  );
});
