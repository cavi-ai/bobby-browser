import assert from "node:assert/strict";
import test from "node:test";

import {
  DEFAULT_FINGERPRINT_PROFILE,
  FINGERPRINT_ENABLED_KEY,
  FINGERPRINT_OWNER_KEY,
  FINGERPRINT_PROFILE_KEY,
  PROFILE_PLACEHOLDER,
  buildInitScript,
  claimFingerprintHostOwnership,
  getFingerprintEnabled,
  getFingerprintOwner,
  getFingerprintProfile,
  releaseFingerprintHostOwnership,
  setFingerprintEnabled,
  setFingerprintProfile,
} from "../src/fingerprint.js";
import { syncFingerprintRegistration } from "../src/fingerprint-registration.js";

function memoryStorage(initial: Record<string, unknown> = {}) {
  const store = new Map<string, unknown>(Object.entries(initial));
  return {
    store,
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

test("fingerprint enabled defaults to true", async () => {
  const { store, storage } = memoryStorage();
  assert.equal(await getFingerprintEnabled(storage), true);
  await setFingerprintEnabled(storage, false);
  assert.equal(store.get(FINGERPRINT_ENABLED_KEY), false);
  assert.equal(await getFingerprintEnabled(storage), false);
});

test("fingerprint owner defaults to popup and flips with claim/release", async () => {
  const { store, storage } = memoryStorage();
  assert.equal(await getFingerprintOwner(storage), "popup");
  await claimFingerprintHostOwnership(storage);
  assert.equal(store.get(FINGERPRINT_OWNER_KEY), "host");
  assert.equal(await getFingerprintOwner(storage), "host");
  await releaseFingerprintHostOwnership(storage);
  assert.equal(await getFingerprintOwner(storage), "popup");
});

test("DEFAULT_FINGERPRINT_PROFILE matches Rust golden session", () => {
  assert.equal(DEFAULT_FINGERPRINT_PROFILE.sessionSeed, 0xb0b5f1d);
  assert.equal(DEFAULT_FINGERPRINT_PROFILE.sessionId, "fp_185294621");
  assert.notEqual(DEFAULT_FINGERPRINT_PROFILE.canvasHash, "0".repeat(64));
  assert.notEqual(DEFAULT_FINGERPRINT_PROFILE.audioHash, "0".repeat(64));
  assert.notEqual(DEFAULT_FINGERPRINT_PROFILE.webgl.hash, "0".repeat(64));
  assert.equal(DEFAULT_FINGERPRINT_PROFILE.injectChrome, false);
});

test("claimFingerprintHostOwnership persists optional profile", async () => {
  const { store, storage } = memoryStorage();
  const profile = {
    ...DEFAULT_FINGERPRINT_PROFILE,
    sessionId: "fp_test_host",
    sessionSeed: 42,
  };
  await claimFingerprintHostOwnership(storage, profile);
  assert.equal(store.get(FINGERPRINT_OWNER_KEY), "host");
  assert.deepEqual(store.get(FINGERPRINT_PROFILE_KEY), profile);
  assert.deepEqual(await getFingerprintProfile(storage), profile);
});

test("claimFingerprintHostOwnership without profile leaves stored profile unchanged", async () => {
  const existing = { ...DEFAULT_FINGERPRINT_PROFILE, sessionId: "fp_existing" };
  const { store, storage } = memoryStorage({ [FINGERPRINT_PROFILE_KEY]: existing });
  await claimFingerprintHostOwnership(storage);
  assert.equal(store.get(FINGERPRINT_OWNER_KEY), "host");
  assert.deepEqual(store.get(FINGERPRINT_PROFILE_KEY), existing);
});

test("buildInitScript embeds profile into shared template", () => {
  const script = buildInitScript(DEFAULT_FINGERPRINT_PROFILE);
  assert.ok(!script.includes(PROFILE_PLACEHOLDER));
  assert.ok(script.includes(DEFAULT_FINGERPRINT_PROFILE.userAgent));
  assert.ok(script.includes('Symbol.for("bobby.fp.applied")'));
  assert.ok(!script.includes("__bobbyFingerprintApplied"));
  assert.equal(DEFAULT_FINGERPRINT_PROFILE.injectChrome, false);
});

test("syncFingerprintRegistration registers and clears", async () => {
  const { store, storage } = memoryStorage({ [FINGERPRINT_ENABLED_KEY]: true });
  let registered = 0;
  let unregistered = 0;
  const api = {
    storage,
    contentScripts: {
      async register() {
        registered += 1;
        return {
          async unregister() {
            unregistered += 1;
          },
        };
      },
    },
  };

  assert.equal(await syncFingerprintRegistration(api), "registered");
  assert.equal(registered, 1);
  store.set(FINGERPRINT_ENABLED_KEY, false);
  assert.equal(await syncFingerprintRegistration(api), "cleared");
  assert.equal(unregistered, 1);
});

test("host ownership clears registration even when enabled", async () => {
  const { storage } = memoryStorage({ [FINGERPRINT_ENABLED_KEY]: true });
  let registered = 0;
  let unregistered = 0;
  const api = {
    storage,
    contentScripts: {
      async register() {
        registered += 1;
        return {
          async unregister() {
            unregistered += 1;
          },
        };
      },
    },
  };

  assert.equal(await syncFingerprintRegistration(api), "registered");
  await claimFingerprintHostOwnership(storage);
  assert.equal(await syncFingerprintRegistration(api), "managed");
  assert.equal(registered, 1);
  assert.equal(unregistered, 1);

  await releaseFingerprintHostOwnership(storage);
  assert.equal(await syncFingerprintRegistration(api), "registered");
  assert.equal(registered, 2);
});
