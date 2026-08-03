import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";
import { renderPopup } from "../src/popup.js";
import type { PopupStatus } from "../src/popup-status.js";

function mount(): Document {
  const dom = new JSDOM(`<!DOCTYPE html><html><body>
    <section id="connection"><div class="status"></div></section>
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
  assert.match(document.getElementById("humanize-status")!.textContent ?? "", /unknown/i);
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
