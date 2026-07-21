import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import * as backgroundModule from "../src/background.js";
import { CompanionBackground, type DiscoveredTarget } from "../src/background.js";

const CONNECT_OPTIONS = {
  companionId: "dbb47eb5-e32f-41f7-812d-24b051fbac52",
  profileId: "8ec6d155-8d88-4107-87a5-744660228b65",
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

const ATTACHMENT_ID = "af4e851e-630a-4a28-8fae-4cd56a9df787";

function pageId(tabId: number, frameId: number): string {
  return `00000000-0000-4000-8000-${String(tabId * 1_000 + frameId).padStart(12, "0")}`;
}

function targetId(tabId: number, frameId: number): string {
  return `target-${tabId}-${frameId}`;
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

async function grant(
  background: CompanionBackground,
  targets: readonly DiscoveredTarget[],
  expiresAtUnixMs = 61_000,
): Promise<void> {
  await background.receive({
    kind: "grant",
    input: {
      protocolVersion: 1,
      attachmentId: ATTACHMENT_ID,
      profileId: CONNECT_OPTIONS.profileId,
      expiresAtUnixMs,
      pages: targets.map((target) => ({
        targetId: targetId(target.tabId, target.frameId),
        pageId: pageId(target.tabId, target.frameId),
      })),
    },
  });
}

function deferred<T>(): {
  promise: Promise<T>;
  resolve(value: T): void;
} {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((completion) => {
    resolve = completion;
  });
  return { promise, resolve };
}

function discoveredTargetIds(transport: FakeTransport): string[] {
  const discoveries = transport.sent.filter(
    (message): message is {
      kind: "targetsDiscovered";
      output: { targets: Array<{ targetId: string }> };
    } =>
      typeof message === "object" &&
      message !== null &&
      "kind" in message &&
      message.kind === "targetsDiscovered",
  );
  return discoveries.at(-1)?.output.targets.map((target) => target.targetId).sort() ?? [];
}

test("discovery is not a grant and only an explicit UUID grant can route", async () => {
  const transport = new FakeTransport();
  const routed: unknown[] = [];
  let now = 1_000;
  const options = {
    ...CONNECT_OPTIONS,
    companionId: "dbb47eb5-e32f-41f7-812d-24b051fbac52",
    profileId: "8ec6d155-8d88-4107-87a5-744660228b65",
  };
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [{ tabId: 9, frameId: 4 }],
    createTargetId: () => "opaque-subframe-target",
    async sendTabMessage(tabId, _message, frameId) {
      routed.push({ tabId, frameId });
      return {};
    },
    async navigateTab() {},
    now: () => now,
  });
  background.connect(options);
  await background.receive({
    kind: "paired",
    output: { companionId: options.companionId, profileId: options.profileId },
  });

  assert.deepEqual(transport.sent.at(-1), {
    kind: "targetsDiscovered",
    output: {
      protocolVersion: 1,
      profileId: options.profileId,
      targets: [{ targetId: "opaque-subframe-target", kind: "frame" }],
    },
  });
  await background.receive({
    kind: "action",
    input: {
      protocolVersion: 1,
      attachmentId: "af4e851e-630a-4a28-8fae-4cd56a9df787",
      commandId: "4c4dfe8c-7c69-4b33-a13e-1fcdf18f2952",
      pageId: "1531e810-2d39-4902-a0c8-6f635d3d4730",
      operation: "observe",
      input: {},
      deadlineUnixMs: 10_000,
    },
  });
  assert.deepEqual(routed, []);

  const grant = {
    kind: "grant",
    input: {
      protocolVersion: 1,
      attachmentId: "af4e851e-630a-4a28-8fae-4cd56a9df787",
      profileId: options.profileId,
      expiresAtUnixMs: 2_000,
      pages: [
        {
          targetId: "opaque-subframe-target",
          pageId: "1531e810-2d39-4902-a0c8-6f635d3d4730",
        },
      ],
    },
  } as const;
  await assert.rejects(
    background.receive({
      ...grant,
      input: {
        ...grant.input,
        profileId: "42d294d1-336e-4552-b087-424ed92fdcc7",
      },
    }),
    /grant.*profile|profile.*grant/i,
  );
  await background.receive(grant);
  await background.receive({
    kind: "action",
    input: {
      protocolVersion: 1,
      attachmentId: grant.input.attachmentId,
      commandId: "4c4dfe8c-7c69-4b33-a13e-1fcdf18f2952",
      pageId: grant.input.pages[0].pageId,
      operation: "observe",
      input: {},
      deadlineUnixMs: 10_000,
    },
  });
  assert.deepEqual(routed, [{ tabId: 9, frameId: 4 }]);

  now = 2_001;
  await background.receive({
    ...grant,
    input: { ...grant.input, expiresAtUnixMs: 4_000 },
  });
  await background.receive({
    kind: "action",
    input: {
      protocolVersion: 1,
      attachmentId: grant.input.attachmentId,
      commandId: "4c4dfe8c-7c69-4b33-a13e-1fcdf18f2952",
      pageId: grant.input.pages[0].pageId,
      operation: "observe",
      input: {},
      deadlineUnixMs: 10_000,
    },
  });
  assert.deepEqual(routed, [
    { tabId: 9, frameId: 4 },
    { tabId: 9, frameId: 4 },
  ]);
});

test("manifest installs content receivers in subframes and newly opened blank contexts", async () => {
  const manifest = JSON.parse(
    await readFile(new URL("../manifest.json", import.meta.url), "utf8"),
  ) as {
    manifest_version?: number;
    permissions?: string[];
    content_scripts?: Array<{
      all_frames?: boolean;
      match_about_blank?: boolean;
      run_at?: string;
    }>;
  };

  assert.equal(manifest.manifest_version, 2);
  assert.equal(manifest.permissions?.includes("http://*/*"), true);
  assert.equal(manifest.permissions?.includes("https://*/*"), true);
  assert.equal(manifest.content_scripts?.[0]?.all_frames, true);
  assert.equal(manifest.content_scripts?.[0]?.match_about_blank, true);
  assert.equal(manifest.content_scripts?.[0]?.run_at, "document_start");
});

test("read-only observation retries a receiver that initially returns no result", async () => {
  const transport = new FakeTransport();
  let attempts = 0;
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [{ tabId: 9, frameId: 4 }],
    createTargetId: (target) => targetId(target.tabId, target.frameId),
    async sendTabMessage() {
      attempts += 1;
      return attempts === 1 ? undefined : { controls: [] };
    },
    async navigateTab() {},
    now: () => 1_000,
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);
  await grant(background, [{ tabId: 9, frameId: 4 }]);

  await background.receive(action(9, 4));

  assert.equal(attempts, 2);
  assert.equal((transport.sent.at(-1) as { kind: string }).kind, "actionCompleted");
});

test("read-only observation cannot retry after its command deadline", async () => {
  const transport = new FakeTransport();
  let attempts = 0;
  let now = 1_000;
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [{ tabId: 9, frameId: 4 }],
    createTargetId: (target) => targetId(target.tabId, target.frameId),
    async sendTabMessage() {
      attempts += 1;
      now = 120_001;
      return undefined;
    },
    async navigateTab() {},
    now: () => now,
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);
  await grant(background, [{ tabId: 9, frameId: 4 }]);

  await background.receive(action(9, 4));

  assert.equal(attempts, 1);
  assert.deepEqual(transport.sent.at(-1), {
    kind: "actionFailed",
    output: {
      commandId: "command-1",
      code: "deadlineExceeded",
      message: "the command deadline expired",
      effectUncertain: false,
    },
  });
});

test("mutating content actions never retry an absent response", async () => {
  const transport = new FakeTransport();
  let attempts = 0;
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [{ tabId: 9, frameId: 4 }],
    createTargetId: (target) => targetId(target.tabId, target.frameId),
    async sendTabMessage() {
      attempts += 1;
      return undefined;
    },
    async navigateTab() {},
    now: () => 1_000,
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);
  await grant(background, [{ tabId: 9, frameId: 4 }]);

  await background.receive(action(9, 4, { operation: "click", input: { cssPath: "#submit" } }));

  assert.equal(attempts, 1);
  assert.deepEqual(transport.sent.at(-1), {
    kind: "actionFailed",
    output: {
      commandId: "command-1",
      code: "actionFailed",
      message: "the content action failed",
      effectUncertain: true,
    },
  });
});

test("paired discovery accepts profile-bound grants and rejects unrelated routes", async () => {
  const transport = new FakeTransport();
  const routed: unknown[] = [];
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [{ tabId: 9, frameId: 4 }],
    createTargetId: (target) => targetId(target.tabId, target.frameId),
    async sendTabMessage(tabId, message, frameId) {
      routed.push({ tabId, message, frameId });
      return { controls: [] };
    },
    async navigateTab() {},
    now: () => 1_000,
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);
  await grant(background, [{ tabId: 9, frameId: 4 }]);

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
    createTargetId: (target) => targetId(target.tabId, target.frameId),
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
    createTargetId: (target) => targetId(target.tabId, target.frameId),
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
    {
      id: "trusted-extension",
      tab: { id: 10 },
      frameId: 3,
      url: "https://example.test/subframe",
    },
    "trusted-extension",
  );
  await background.receiveRuntimeMessage(
    { type: "companionFrameReady" },
    { id: "trusted-extension", tab: { id: 11 }, frameId: 3, url: "about:blank" },
    "trusted-extension",
  );
  await grant(background, [{ tabId: 10, frameId: 3 }]);
  await background.receive(action(10, 3));

  assert.deepEqual(routed, [{ tabId: 10, frameId: 3 }]);
});

test("runtime messages cannot submit page-visible binding nonces", async () => {
  const transport = new FakeTransport();
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [],
    createTargetId: (target) => targetId(target.tabId, target.frameId),
    async sendTabMessage() {
      return {};
    },
    async navigateTab() {},
    now: () => 1_000,
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);
  const sentBeforeMarkers = transport.sent.length;

  const bindingNonce = "b5f6319a-6b36-43cb-9464-d337fc9d8201";
  await background.receiveRuntimeMessage(
    { type: "companionPageBinding", bindingNonce },
    {
      id: "trusted-extension",
      tab: { id: 10 },
      frameId: 3,
      url: "about:blank",
    },
    "trusted-extension",
  );
  await background.receiveRuntimeMessage(
    { type: "companionPageBinding", bindingNonce: "page-controlled-value" },
    {
      id: "trusted-extension",
      tab: { id: 11 },
      frameId: 0,
      url: "https://example.test/forged",
    },
    "trusted-extension",
  );

  assert.equal(transport.sent.length, sentBeforeMarkers);
});

test("lease capacity is bounded and leases expire", async () => {
  const transport = new FakeTransport();
  let now = 1_000;
  const routed: number[] = [];
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () =>
      Array.from({ length: 257 }, (_, tabId) => ({ tabId, frameId: 0 })),
    createTargetId: (target) => targetId(target.tabId, target.frameId),
    async sendTabMessage(tabId) {
      routed.push(tabId);
      return {};
    },
    async navigateTab() {},
    now: () => now,
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);
  await grant(
    background,
    Array.from({ length: 256 }, (_, tabId) => ({ tabId, frameId: 0 })),
  );

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

test("tab removal invalidates an older asynchronous frame snapshot", async () => {
  const transport = new FakeTransport();
  const snapshots: Array<ReturnType<typeof deferred<readonly DiscoveredTarget[]>>> = [];
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [{ tabId: 21, frameId: 0 }],
    discoverTabTargets: async () => {
      const snapshot = deferred<readonly DiscoveredTarget[]>();
      snapshots.push(snapshot);
      return snapshot.promise;
    },
    createTargetId: (target) => targetId(target.tabId, target.frameId),
    async sendTabMessage() {
      return {};
    },
    async navigateTab() {},
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);

  const stale = background.reconcileTab(21);
  background.receiveTabRemoved(21);
  const pendingSnapshot = snapshots.at(0);
  assert.ok(pendingSnapshot);
  pendingSnapshot.resolve([
    { tabId: 21, frameId: 0 },
    { tabId: 21, frameId: 4 },
  ]);
  await stale;

  assert.deepEqual(discoveredTargetIds(transport), []);
});

test("the newest overlapping navigation reconciliation wins regardless of resolution order", async () => {
  const transport = new FakeTransport();
  const snapshots: Array<ReturnType<typeof deferred<readonly DiscoveredTarget[]>>> = [];
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [{ tabId: 22, frameId: 0 }],
    discoverTabTargets: async () => {
      const snapshot = deferred<readonly DiscoveredTarget[]>();
      snapshots.push(snapshot);
      return snapshot.promise;
    },
    createTargetId: (target) => targetId(target.tabId, target.frameId),
    async sendTabMessage() {
      return {};
    },
    async navigateTab() {},
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);

  const older = background.reconcileTab(22);
  const newer = background.reconcileTab(22);
  const newerSnapshot = snapshots.at(1);
  assert.ok(newerSnapshot);
  newerSnapshot.resolve([
    { tabId: 22, frameId: 0 },
    { tabId: 22, frameId: 8 },
  ]);
  await newer;
  const olderSnapshot = snapshots.at(0);
  assert.ok(olderSnapshot);
  olderSnapshot.resolve([
    { tabId: 22, frameId: 0 },
    { tabId: 22, frameId: 7 },
  ]);
  await older;

  assert.deepEqual(discoveredTargetIds(transport), [targetId(22, 0), targetId(22, 8)]);
});

test("a page-binding marker does not invalidate an authoritative frame snapshot", async () => {
  const transport = new FakeTransport();
  const snapshot = deferred<readonly DiscoveredTarget[]>();
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [],
    discoverTabTargets: async () => snapshot.promise,
    createTargetId: (target) => targetId(target.tabId, target.frameId),
    async sendTabMessage() {
      return {};
    },
    async navigateTab() {},
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);

  const stale = background.reconcileTab(23);
  const nonce = "b5f6319a-6b36-43cb-9464-d337fc9d8201";
  background.receiveTabUpdate(
    23,
    { title: `automation-runtime-binding:${nonce}` },
    { id: 23, url: "about:blank", title: `automation-runtime-binding:${nonce}` },
  );
  snapshot.resolve([
    { tabId: 23, frameId: 0 },
    { tabId: 23, frameId: 9 },
  ]);
  await stale;

  assert.deepEqual(discoveredTargetIds(transport), [targetId(23, 0), targetId(23, 9)]);
});

test("a normal title update does not prevent an in-flight snapshot from pruning routes", async () => {
  const transport = new FakeTransport();
  const snapshot = deferred<readonly DiscoveredTarget[]>();
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [
      { tabId: 24, frameId: 0 },
      { tabId: 24, frameId: 4 },
    ],
    discoverTabTargets: async () => snapshot.promise,
    createTargetId: (target) => targetId(target.tabId, target.frameId),
    async sendTabMessage() {
      return {};
    },
    async navigateTab() {},
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);

  const reconciliation = background.reconcileTab(24);
  background.receiveTabUpdate(
    24,
    { title: "A normal page title" },
    { id: 24, url: "https://example.test/", title: "A normal page title" },
  );
  snapshot.resolve([{ tabId: 24, frameId: 0 }]);
  await reconciliation;

  assert.deepEqual(discoveredTargetIds(transport), [targetId(24, 0)]);
});

test("frame-ready does not invalidate the snapshot that prunes disappeared routes", async () => {
  const transport = new FakeTransport();
  const snapshot = deferred<readonly DiscoveredTarget[]>();
  const background = new CompanionBackground({
    transport,
    discoverTargets: async () => [
      { tabId: 25, frameId: 0 },
      { tabId: 25, frameId: 4 },
    ],
    discoverTabTargets: async () => snapshot.promise,
    createTargetId: (target) => targetId(target.tabId, target.frameId),
    async sendTabMessage() {
      return {};
    },
    async navigateTab() {},
  });
  background.connect(CONNECT_OPTIONS);
  await pair(background);

  const reconciliation = background.reconcileTab(25);
  await background.receiveRuntimeMessage(
    { type: "companionFrameReady" },
    {
      id: "trusted-extension",
      tab: { id: 25 },
      frameId: 8,
      url: "https://example.test/frame",
    },
    "trusted-extension",
  );
  snapshot.resolve([{ tabId: 25, frameId: 0 }]);
  await reconciliation;

  assert.deepEqual(discoveredTargetIds(transport), [targetId(25, 0)]);
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
  const tabUpdates = new ListenerSet<(
    tabId: number,
    changeInfo: { title?: string },
    tab: { id?: number; url?: string; title?: string },
  ) => void>();
  const tabRemovals = new ListenerSet<(
    tabId: number,
    removeInfo: { windowId: number; isWindowClosing: boolean },
  ) => void>();
  const navigationCommits = new ListenerSet<(
    details: { tabId: number; frameId: number; url: string },
  ) => void>();
  let frameDiscoveryFails = false;
  let currentFrames = [
    { frameId: 0, url: "https://example.test/" },
    { frameId: 4, url: "https://example.test/frame" },
    { frameId: 5, url: "about:blank" },
    { frameId: 6, url: "" },
    { frameId: 7, url: "moz-extension://untrusted/" },
  ];
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
          return {
            companionId: CONNECT_OPTIONS.companionId,
            profileId: CONNECT_OPTIONS.profileId,
          };
        },
        async set() {},
      },
    },
    tabs: {
      onUpdated: tabUpdates,
      onRemoved: tabRemovals,
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
      onCommitted: navigationCommits,
      async getAllFrames({ tabId }: { tabId: number }) {
        frames.push(tabId);
        if (frameDiscoveryFails) throw new Error("frame discovery unavailable");
        return currentFrames;
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
    output: {
      companionId: CONNECT_OPTIONS.companionId,
      profileId: CONNECT_OPTIONS.profileId,
    },
  });
  await new Promise((resolve) => setImmediate(resolve));
  const discovery = port.sent.find(
    (message): message is {
      kind: "targetsDiscovered";
      output: { targets: Array<{ targetId: string; kind: string }> };
    } =>
      typeof message === "object" &&
      message !== null &&
      "kind" in message &&
      message.kind === "targetsDiscovered",
  );
  assert.ok(discovery);
  assert.equal(discovery.output.targets.length, 2);
  const subframe = discovery.output.targets.find((target) => target.kind === "frame");
  assert.ok(subframe);
  port.onMessage.emit({
    kind: "grant",
    input: {
      protocolVersion: 1,
      attachmentId: ATTACHMENT_ID,
      profileId: CONNECT_OPTIONS.profileId,
      expiresAtUnixMs: Date.now() + 60_000,
      pages: [{ targetId: subframe.targetId, pageId: pageId(9, 4) }],
    },
  });
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

  const bindingNonce = "b5f6319a-6b36-43cb-9464-d337fc9d8201";
  tabUpdates.emit(
    12,
    { title: `automation-runtime-binding:${bindingNonce}` },
    { id: 12, url: "about:blank", title: `automation-runtime-binding:${bindingNonce}` },
  );
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(
    port.sent.some(
      (message) =>
        typeof message === "object" &&
        message !== null &&
        "kind" in message &&
        message.kind === "pageBindingDiscovered" &&
        "output" in message &&
        typeof message.output === "object" &&
        message.output !== null &&
        "bindingNonce" in message.output &&
        message.output.bindingNonce === bindingNonce,
    ),
    true,
  );

  currentFrames = [{ frameId: 0, url: "https://example.test/replaced" }];
  navigationCommits.emit({
    tabId: 9,
    frameId: 0,
    url: "https://example.test/replaced",
  });
  await new Promise((resolve) => setImmediate(resolve));
  port.onMessage.emit(action(9, 4, { deadlineUnixMs: Date.now() + 60_000 }));
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(routed.length, 1, "navigation reconciliation must revoke disappeared frame leases");
  const reconciledDiscoveries = port.sent.filter(
    (message): message is {
      kind: "targetsDiscovered";
      output: { targets: Array<{ targetId: string; kind: string }> };
    } =>
      typeof message === "object" &&
      message !== null &&
      "kind" in message &&
      message.kind === "targetsDiscovered",
  );
  assert.equal(reconciledDiscoveries.at(-1)?.output.targets.length, 2);
  assert.equal(
    reconciledDiscoveries.at(-1)?.output.targets.every((target) => target.kind === "page"),
    true,
  );

  frameDiscoveryFails = true;
  navigationCommits.emit({
    tabId: 9,
    frameId: 0,
    url: "https://example.test/unavailable",
  });
  await new Promise((resolve) => setImmediate(resolve));
  const failedDiscoveries = port.sent.filter(
    (message): message is {
      kind: "targetsDiscovered";
      output: { targets: Array<{ targetId: string; kind: string }> };
    } =>
      typeof message === "object" &&
      message !== null &&
      "kind" in message &&
      message.kind === "targetsDiscovered",
  );
  assert.equal(
    failedDiscoveries.at(-1)?.output.targets.length,
    1,
    "failed frame discovery must revoke stale routes instead of retaining their leases",
  );

  const lifetimeNonces = new Set<string>();
  for (let index = 0; index < 300; index += 1) {
    const tabId = 1_000 + index;
    const nonce = `00000000-0000-4000-8000-${String(index).padStart(12, "0")}`;
    lifetimeNonces.add(nonce);
    tabUpdates.emit(
      tabId,
      { title: `automation-runtime-binding:${nonce}` },
      { id: tabId, url: "about:blank", title: `automation-runtime-binding:${nonce}` },
    );
    tabRemovals.emit(tabId, { windowId: 1, isWindowClosing: false });
  }
  const reportedLifetimeNonces = new Set(
    port.sent.flatMap((message) => {
      if (
        typeof message !== "object" ||
        message === null ||
        !("kind" in message) ||
        message.kind !== "pageBindingDiscovered" ||
        !("output" in message) ||
        typeof message.output !== "object" ||
        message.output === null ||
        !("bindingNonce" in message.output) ||
        typeof message.output.bindingNonce !== "string"
      ) {
        return [];
      }
      return lifetimeNonces.has(message.output.bindingNonce) ? [message.output.bindingNonce] : [];
    }),
  );
  assert.equal(
    reportedLifetimeNonces.size,
    lifetimeNonces.size,
    "trusted tab removal events must reclaim target capacity across the browser lifetime",
  );
});
