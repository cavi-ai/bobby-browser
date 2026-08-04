import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";
import { applyStatusOrFallback, bindPairButton, renderPopup } from "../src/popup.js";
import type { PopupStatus } from "../src/popup-status.js";

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

function baseStatus(overrides: Partial<PopupStatus> = {}): PopupStatus {
  return {
    paired: false,
    leaseCount: 0,
    nativeConnected: true,
    fingerprint: { enabled: false, owner: "popup" },
    humanize: "unknown",
    protocolVersion: 1,
    enrollPhase: "idle",
    ...overrides,
  };
}

function mount(): Document {
  const dom = new JSDOM(`<!DOCTYPE html><html><body>
    <section id="connection"><div class="status"></div><button id="pair-button" type="button">Pair</button></section>
    <section id="session"><div class="status"></div></section>
    <label><input id="toggle" type="checkbox" /></label>
    <p id="fingerprint-status"></p>
    <p id="humanize-status"></p>
    <section id="debug"><div class="status"></div></section>
  </body></html>`);
  return dom.window.document;
}

test("renderPopup disables fingerprint when host-owned", () => {
  const document = mount();
  const status: PopupStatus = {
    paired: true,
    companionId: "c1",
    profileId: "p1",
    leaseCount: 1,
    nativeConnected: true,
    fingerprint: { enabled: true, owner: "host", sessionId: "fp_1", seedHex: "2a" },
    humanize: "unknown",
    protocolVersion: 1,
  };
  renderPopup(document, status);
  const toggle = document.getElementById("toggle") as HTMLInputElement;
  assert.equal(toggle.disabled, true);
  assert.equal(toggle.checked, true);
  assert.match(document.getElementById("fingerprint-status")!.textContent ?? "", /Managed by Bobby/);
  assert.equal(
    document.getElementById("humanize-status")!.textContent,
    "Unknown — set by session policy",
  );
});

test("renderPopup host-owned shows checked and disabled when enabled is false", () => {
  const document = mount();
  renderPopup(document, {
    paired: true,
    companionId: "c1",
    profileId: "p1",
    leaseCount: 1,
    nativeConnected: true,
    fingerprint: { enabled: false, owner: "host", sessionId: "fp_1", seedHex: "2a" },
    humanize: "unknown",
    protocolVersion: 1,
  });
  const toggle = document.getElementById("toggle") as HTMLInputElement;
  assert.equal(toggle.disabled, true);
  assert.equal(toggle.checked, true);
});

test("renderPopup shows unpaired reason when not paired", () => {
  const document = mount();
  renderPopup(document, {
    paired: false,
    unpairedReason: "waiting to pair",
    leaseCount: 0,
    nativeConnected: false,
    fingerprint: { enabled: true, owner: "popup" },
    humanize: "unknown",
    protocolVersion: 1,
  });
  assert.match(document.getElementById("connection")!.textContent ?? "", /waiting to pair/);
});

test("renderPopup escapes HTML in unpaired reason", () => {
  const document = mount();
  const malicious = '<img src=x onerror="alert(1)">';
  renderPopup(document, {
    paired: false,
    unpairedReason: malicious,
    leaseCount: 0,
    nativeConnected: false,
    fingerprint: { enabled: true, owner: "popup" },
    humanize: "unknown",
    protocolVersion: 1,
  });
  const connection = document.querySelector("#connection .status")!;
  assert.equal(connection.querySelector("img"), null);
  assert.match(connection.textContent ?? "", new RegExp(malicious.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
});

test("renderPopup shows Pair when unpaired", () => {
  const document = mount();
  renderPopup(document, baseStatus({ paired: false, enrollPhase: "idle" }));
  const button = document.querySelector("#pair-button") as HTMLButtonElement;
  assert.ok(button);
  assert.equal(button.textContent, "Pair");
  assert.equal(button.disabled, false);
});

test("renderPopup shows Re-pair when paired", () => {
  const document = mount();
  renderPopup(
    document,
    baseStatus({
      paired: true,
      companionId: "c1",
      profileId: "p1",
      enrollPhase: "idle",
    }),
  );
  const button = document.querySelector("#pair-button") as HTMLButtonElement;
  assert.equal(button.textContent, "Re-pair");
  assert.equal(button.disabled, false);
});

test("renderPopup disables button while pairing", () => {
  const document = mount();
  renderPopup(document, baseStatus({ enrollPhase: "pairing", paired: false }));
  const button = document.querySelector("#pair-button") as HTMLButtonElement;
  assert.equal(button.disabled, true);
  assert.match(document.querySelector("#connection .status")?.textContent ?? "", /Pairing/i);
});

test("renderPopup shows enroll error message on failure", () => {
  const document = mount();
  renderPopup(
    document,
    baseStatus({
      enrollPhase: "failed",
      enrollError: { code: "enroll_failed", message: "Enable remote debugging in about:config" },
    }),
  );
  assert.match(
    document.querySelector("#connection .status")?.textContent ?? "",
    /Enable remote debugging/,
  );
});

test("bindPairButton sends enrollPair and reloads status", async () => {
  const document = mount();
  renderPopup(document, baseStatus());
  let enrollCalled = false;
  let statusCalls = 0;
  const browserApi = {
    storage: memoryStorage().storage,
    runtime: {
      async sendMessage(message: unknown) {
        if (
          typeof message === "object" &&
          message !== null &&
          "type" in message &&
          message.type === "enrollPair"
        ) {
          enrollCalled = true;
          return { ok: true };
        }
        if (
          typeof message === "object" &&
          message !== null &&
          "type" in message &&
          message.type === "popupStatus"
        ) {
          statusCalls += 1;
          return baseStatus({ paired: true, companionId: "c1", profileId: "p1" });
        }
      },
    },
  };
  await bindPairButton(browserApi, document);
  const button = document.querySelector("#pair-button") as HTMLButtonElement;
  button.click();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(enrollCalled, true);
  assert.equal(statusCalls, 1);
  assert.match(document.querySelector("#connection .status")?.textContent ?? "", /Paired/);
});

test("applyStatusOrFallback binds fingerprint when status unavailable", async () => {
  const document = mount();
  const { storage } = memoryStorage();
  const browserApi = {
    storage,
    runtime: { sendMessage: async () => undefined },
  };
  await applyStatusOrFallback(browserApi, document, undefined);
  const connection = document.querySelector("#connection .status");
  assert.equal(connection?.textContent, "Status unavailable");
  const toggle = document.getElementById("toggle") as HTMLInputElement;
  assert.equal(toggle.disabled, false);
  assert.equal(toggle.checked, true);
});
