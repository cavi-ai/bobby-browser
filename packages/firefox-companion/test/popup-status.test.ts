import assert from "node:assert/strict";
import test from "node:test";
import { buildPopupStatus } from "../src/popup-status.js";

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
