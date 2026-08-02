import assert from "node:assert/strict";
import test from "node:test";

import {
  DEFAULT_FINGERPRINT_PROFILE,
  FINGERPRINT_ENABLED_KEY,
  applyFingerprintProfile,
  getFingerprintEnabled,
  setFingerprintEnabled,
} from "../src/fingerprint.js";

test("fingerprint enabled defaults to true", async () => {
  const store = new Map<string, unknown>();
  const storage = {
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
  };
  assert.equal(await getFingerprintEnabled(storage), true);
  await setFingerprintEnabled(storage, false);
  assert.equal(store.get(FINGERPRINT_ENABLED_KEY), false);
  assert.equal(await getFingerprintEnabled(storage), false);
});

test("applyFingerprintProfile is idempotent and sets flag", () => {
  const g = globalThis as typeof globalThis & { __bobbyFingerprintApplied?: boolean };
  delete g.__bobbyFingerprintApplied;
  applyFingerprintProfile(DEFAULT_FINGERPRINT_PROFILE);
  assert.equal(g.__bobbyFingerprintApplied, true);
  applyFingerprintProfile(DEFAULT_FINGERPRINT_PROFILE);
  assert.equal(g.__bobbyFingerprintApplied, true);
});
