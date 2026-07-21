import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_COMPANION_PAYLOAD_BYTES,
  parseCompanionEvent,
  parseCompanionRequest,
  serializeCompanionEvent,
} from "../src/protocol.js";
import { CompanionBackground } from "../src/background.js";

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

test("protocol version 1 action requests preserve the Rust JSON shape", () => {
  const request = parseCompanionRequest(
    JSON.stringify({
      kind: "action",
      input: {
        protocolVersion: 1,
        attachmentId: "attachment-1",
        commandId: "command-1",
        pageId: "page-1",
        operation: "observe",
        input: {},
        deadlineUnixMs: 1_800_000_000_000,
      },
    }),
  );

  assert.deepEqual(request, {
    kind: "action",
    input: {
      protocolVersion: 1,
      attachmentId: "attachment-1",
      commandId: "command-1",
      pageId: "page-1",
      operation: "observe",
      input: {},
      deadlineUnixMs: 1_800_000_000_000,
    },
  });
});

test("parseCompanionEvent rejects unknown protocol versions", () => {
  assert.throws(
    () =>
      parseCompanionEvent(
        JSON.stringify({ protocolVersion: 2, kind: "pong" }),
      ),
    /protocol version/i,
  );
});

test("parseCompanionEvent rejects unknown event kinds", () => {
  assert.throws(
    () => parseCompanionEvent(JSON.stringify({ kind: "surprise" })),
    /event kind/i,
  );
});

test("parseCompanionEvent rejects missing command IDs", () => {
  assert.throws(
    () =>
      parseCompanionEvent(
        JSON.stringify({
          kind: "actionCompleted",
          output: { interactionPath: "extensionApi", output: {} },
        }),
      ),
    /commandId/,
  );
});

test("parseCompanionEvent rejects payloads larger than 1 MiB", () => {
  const oversized = JSON.stringify({
    kind: "actionFailed",
    output: {
      commandId: "command-1",
      code: "failed",
      message: "x".repeat(MAX_COMPANION_PAYLOAD_BYTES),
      effectUncertain: false,
    },
  });

  assert.throws(() => parseCompanionEvent(oversized), /1 MiB/);
});

test("serialized completion events match the Rust tagged representation", () => {
  assert.equal(
    serializeCompanionEvent({
      kind: "actionCompleted",
      output: {
        commandId: "command-1",
        interactionPath: "extensionApi",
        output: { observed: true },
      },
    }),
    '{"kind":"actionCompleted","output":{"commandId":"command-1","interactionPath":"extensionApi","output":{"observed":true}}}',
  );
});

test("the background connects through the native host without receiving pairing material", () => {
  const transport = new FakeTransport();
  const background = new CompanionBackground({
    transport,
    async sendTabMessage() {
      return {};
    },
    async navigateTab() {},
  });

  background.connect({
    companionId: "companion-1",
    profileId: "profile-1",
    identity: {
      engine: "firefox",
      browserName: "Firefox",
      browserVersion: "stable",
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
  });

  assert.deepEqual(transport.sent, [
    {
      kind: "pair",
      input: {
        protocolVersion: 1,
        companionId: "companion-1",
        profileId: "profile-1",
        identity: {
          engine: "firefox",
          browserName: "Firefox",
          browserVersion: "stable",
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
      },
    },
  ]);
  assert.equal(JSON.stringify(transport.sent).includes("pair-once"), false);
  assert.equal(JSON.stringify(transport.sent).includes("pairingCode"), false);
});

test("leased page actions route only to the matching tab and frame", async () => {
  const transport = new FakeTransport();
  const routed: unknown[] = [];
  const background = new CompanionBackground({
    transport,
    async sendTabMessage(tabId, message, frameId) {
      routed.push({ tabId, message, frameId });
      return { url: "https://example.test/", title: "Example", visibleText: "Hi", controls: [] };
    },
    async navigateTab() {},
    now: () => 1_000,
  });
  background.connect({
    companionId: "companion-1",
    profileId: "profile-1",
    identity: {
      engine: "firefox",
      browserName: "Firefox",
      browserVersion: "stable",
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
  });
  background.leasePage({
    attachmentId: "attachment-1",
    pageId: "page-1",
    tabId: 9,
    frameId: 4,
    expiresAtUnixMs: 2_000,
  });

  await background.receive(
    JSON.stringify({
      kind: "action",
      input: {
        protocolVersion: 1,
        attachmentId: "attachment-1",
        commandId: "command-1",
        pageId: "page-1",
        operation: "observe",
        input: {},
        deadlineUnixMs: 1_500,
      },
    }),
  );

  assert.deepEqual(routed, [
    {
      tabId: 9,
      frameId: 4,
      message: { type: "companionAction", operation: "observe", input: {} },
    },
  ]);
  assert.deepEqual(transport.sent.at(-1), {
    kind: "actionCompleted",
    output: {
      commandId: "command-1",
      interactionPath: "extensionApi",
      output: {
        url: "https://example.test/",
        title: "Example",
        visibleText: "Hi",
        controls: [],
      },
    },
  });
});

test("expired page leases cannot route commands", async () => {
  const transport = new FakeTransport();
  let routed = false;
  const background = new CompanionBackground({
    transport,
    async sendTabMessage() {
      routed = true;
      return {};
    },
    async navigateTab() {},
    now: () => 2_001,
  });
  background.connect({
    companionId: "companion-1",
    profileId: "profile-1",
    identity: {
      engine: "firefox",
      browserName: "Firefox",
      browserVersion: "stable",
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
  });
  background.leasePage({
    attachmentId: "attachment-1",
    pageId: "page-1",
    tabId: 9,
    frameId: 4,
    expiresAtUnixMs: 2_000,
  });

  await background.receive(
    JSON.stringify({
      kind: "action",
      input: {
        protocolVersion: 1,
        attachmentId: "attachment-1",
        commandId: "command-1",
        pageId: "page-1",
        operation: "observe",
        input: {},
        deadlineUnixMs: 3_000,
      },
    }),
  );

  assert.equal(routed, false);
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
