import assert from "node:assert/strict";
import test from "node:test";
import { CompanionBackground } from "../src/background.js";
import {
  claimFingerprintHostOwnership,
  DEFAULT_FINGERPRINT_PROFILE,
} from "../src/fingerprint.js";
import { buildPopupStatus } from "../src/popup-status.js";

function memoryStorage(initial: Record<string, unknown> = {}) {
  const store = new Map<string, unknown>(Object.entries(initial));
  return {
    storage: {
      local: {
        async get(keys: readonly string[]) {
          const out: Record<string, unknown> = {};
          for (const key of keys) {
            if (store.has(key)) out[key] = store.get(key);
          }
          return out;
        },
        async set(values: Record<string, unknown>) {
          for (const [key, value] of Object.entries(values)) store.set(key, value);
        },
      },
    },
  };
}

function fakeTransport(connected = true) {
  return {
    start() {},
    send() {},
    stop() {},
    isConnected: () => connected,
  };
}

const CONNECT = {
  companionId: "11111111-1111-4111-8111-111111111111",
  profileId: "22222222-2222-4222-8222-222222222222",
  identity: {
    engine: "firefox" as const,
    browserName: "Firefox",
    browserVersion: "128.0",
    os: "mac",
    profileLabel: "test",
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

test("buildPopupStatus unpaired omits ids and sets humanize unknown", () => {
  const status = buildPopupStatus({
    paired: false,
    unpairedReason: "waiting to pair",
    leaseCount: 0,
    nativeConnected: true,
    fingerprintEnabled: true,
    fingerprintOwner: "popup",
    protocolVersion: 1,
  });
  assert.equal(status.paired, false);
  assert.equal(status.unpairedReason, "waiting to pair");
  assert.equal(status.companionId, undefined);
  assert.equal(status.humanize, "unknown");
  assert.equal(status.leaseCount, 0);
  assert.equal(status.nativeConnected, true);
  assert.deepEqual(status.fingerprint, { enabled: true, owner: "popup" });
});

test("buildPopupStatus paired includes ids, seed hex, and lastError", () => {
  const status = buildPopupStatus({
    paired: true,
    companionId: "c1",
    profileId: "p1",
    leaseCount: 2,
    nativeConnected: true,
    fingerprintEnabled: true,
    fingerprintOwner: "host",
    fingerprintSessionId: "fp_1",
    fingerprintSessionSeed: 0xb0b5f1d,
    lastError: { code: "leaseExpired", message: "the page lease is missing or expired" },
    protocolVersion: 1,
  });
  assert.equal(status.paired, true);
  assert.equal(status.companionId, "c1");
  assert.equal(status.profileId, "p1");
  assert.equal(status.leaseCount, 2);
  assert.equal(status.fingerprint.owner, "host");
  assert.equal(status.fingerprint.sessionId, "fp_1");
  assert.equal(status.fingerprint.seedHex, "b0b5f1d");
  assert.deepEqual(status.lastError, {
    code: "leaseExpired",
    message: "the page lease is missing or expired",
  });
  assert.equal(status.humanize, "unknown");
});

test("getPopupStatus reports unpaired waiting to pair", async () => {
  const { storage } = memoryStorage();
  const background = new CompanionBackground({
    transport: fakeTransport(true),
    sendTabMessage: async () => undefined,
    navigateTab: async () => undefined,
  });
  background.connect(CONNECT);
  const status = await background.getPopupStatus(storage);
  assert.equal(status.paired, false);
  assert.equal(status.unpairedReason, "waiting to pair");
  assert.equal(status.nativeConnected, true);
  assert.equal(status.humanize, "unknown");
});

test("getPopupStatus host fingerprint is reflected", async () => {
  const { storage } = memoryStorage();
  await claimFingerprintHostOwnership(storage, {
    ...DEFAULT_FINGERPRINT_PROFILE,
    sessionId: "fp_host",
    sessionSeed: 42,
  });
  const background = new CompanionBackground({
    transport: fakeTransport(false),
    sendTabMessage: async () => undefined,
    navigateTab: async () => undefined,
  });
  background.connect(CONNECT);
  const status = await background.getPopupStatus(storage);
  assert.equal(status.fingerprint.owner, "host");
  assert.equal(status.fingerprint.sessionId, "fp_host");
  assert.equal(status.fingerprint.seedHex, "2a");
  assert.equal(status.nativeConnected, false);
});
