import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";

import {
  MAX_CONTROL_COUNT,
  MAX_VISIBLE_TEXT_LENGTH,
  executeContentAction,
  observeDocument,
} from "../src/content.js";
import { MAX_COMPANION_PAYLOAD_BYTES } from "../src/protocol.js";

const EXPECTED_MAX_CONTROL_FIELD_LENGTH = 2 * 1024;
const EXPECTED_MAX_SELECTOR_LENGTH = 512;
const EXPECTED_MAX_OBSERVATION_BYTES = MAX_COMPANION_PAYLOAD_BYTES - 64 * 1024;

function documentFor(body: string, url = "https://example.test/login"): Document {
  return new JSDOM(
    `<!doctype html><html><head><title>Example</title></head><body>${body}</body></html>`,
    { url },
  ).window.document;
}

test("observeDocument returns page identity, visible text, labels, roles, and stable targets", () => {
  const document = documentFor(`
    <main>
      <h1>Sign in</h1>
      <label for="email">Email address</label>
      <input id="email" name="email" type="email" value="user@example.test">
      <button data-testid="submit-login">Continue</button>
      <span hidden>not visible</span>
    </main>
  `);

  const observed = observeDocument(document);

  assert.equal(observed.url, "https://example.test/login");
  assert.equal(observed.title, "Example");
  assert.match(observed.visibleText, /Sign in/);
  assert.doesNotMatch(observed.visibleText, /not visible/);
  assert.deepEqual(observed.controls[0], {
    cssPath: "#email",
    role: "textbox",
    name: "Email address",
    label: "Email address",
    value: "user@example.test",
    disabled: false,
  });
  assert.equal(observed.controls[1]?.cssPath, '[data-testid="submit-login"]');
  assert.equal(observed.controls[1]?.role, "button");
  assert.equal(observed.controls[1]?.name, "Continue");
});

test("password values never enter observations", () => {
  const document = documentFor(
    '<label for="p">Password</label><input id="p" type="password" value="secret">',
  );
  const observed = observeDocument(document);
  assert.equal(JSON.stringify(observed).includes("secret"), false);
  assert.equal(observed.controls[0]?.value, "[redacted]");
});

test("unlabelled password values cannot become accessible names", () => {
  const observed = observeDocument(documentFor('<input type="password" value="name-leak">'));

  assert.equal(JSON.stringify(observed).includes("name-leak"), false);
  assert.equal(observed.controls[0]?.name, undefined);
  assert.equal(observed.controls[0]?.value, "[redacted]");
});

test("password controls redact opaque secrets from every metadata field", () => {
  const secret = "opaque-value-77";
  const observed = observeDocument(
    documentFor(
      `<label for="${secret}">${secret}</label><input id="${secret}" type="password" value="${secret}" aria-label="${secret}" title="${secret}" alt="${secret}">`,
    ),
  );
  const encoded = JSON.stringify(observed);

  assert.equal(encoded.includes(secret), false);
  assert.equal(observed.controls[0]?.name, "[redacted]");
  assert.equal(observed.controls[0]?.label, "[redacted]");
  assert.equal(observed.controls[0]?.value, "[redacted]");
  assert.doesNotMatch(observed.controls[0]?.cssPath ?? "", /opaque-value/);
});

test("authorization-like values and attributes never enter observations", () => {
  const document = documentFor(
    '<label for="bearer">Authorization</label><input id="bearer" name="authorization" data-authorization="Bearer private-token" value="Bearer private-token">',
  );

  const observed = observeDocument(document);
  const encoded = JSON.stringify(observed);

  assert.equal(encoded.includes("private-token"), false);
  assert.equal(encoded.includes("data-authorization"), false);
  assert.equal(observed.controls[0]?.value, "[redacted]");
});

test("every observation field redacts secrets from accessibility and URL metadata", () => {
  const secret = "private-token-73a9";
  const document = new JSDOM(
    `<!doctype html><html><head><title>Bearer ${secret}</title></head><body>
      <label id="label-token" for="Bearer-${secret}">Bearer ${secret}</label>
      <input
        id="Bearer-${secret}"
        type="password"
        value="${secret}"
        aria-label="Bearer ${secret}"
        aria-labelledby="label-token"
        alt="Bearer ${secret}"
        title="Bearer ${secret}"
        data-testid="Bearer-${secret}"
      >
    </body></html>`,
    {
      url: `https://user:${secret}@example.test/login?authorization=Bearer%20${secret}#${secret}`,
    },
  ).window.document;

  const observed = observeDocument(document);
  const encoded = JSON.stringify(observed);

  assert.equal(encoded.includes(secret), false);
  assert.equal(encoded.toLowerCase().includes("bearer"), false);
  assert.equal(observed.url, "https://example.test/login");
  assert.equal(observed.title, "[redacted]");
  assert.equal(observed.controls[0]?.name, "[redacted]");
  assert.equal(observed.controls[0]?.label, "[redacted]");
  assert.equal(observed.controls[0]?.value, "[redacted]");
  assert.doesNotMatch(observed.controls[0]?.cssPath ?? "", /token|bearer/i);
});

test("malformed URL encoding cannot restore stripped credentials", () => {
  const credential = "opaque-value-77";
  const document = documentFor(
    "<button>Continue</button>",
    `https://user:${credential}@example.test/%E0%A4%A`,
  );

  const observed = observeDocument(document);

  assert.equal(JSON.stringify(observed).includes(credential), false);
  assert.equal(observed.url, "https://example.test/%E0%A4%A");
});

test("observations are bounded", () => {
  const document = documentFor(
    `<p>${"x".repeat(MAX_VISIBLE_TEXT_LENGTH + 100)}</p>${Array.from(
      { length: MAX_CONTROL_COUNT + 20 },
      (_, index) => `<button id="button-${index}">Button ${index}</button>`,
    ).join("")}`,
  );

  const observed = observeDocument(document);

  assert.equal(observed.visibleText.length, MAX_VISIBLE_TEXT_LENGTH);
  assert.equal(observed.controls.length, MAX_CONTROL_COUNT);
});

test("adversarial 512-control observations stay below the native ceiling", () => {
  const longId = "selector".repeat(700);
  const longName = "accessible name ".repeat(400);
  const longTitle = "control title ".repeat(400);
  const document = documentFor(
    Array.from(
      { length: MAX_CONTROL_COUNT },
      (_, index) =>
        `<button id="${longId}-${index}" aria-label="${longName}-${index}" title="${longTitle}-${index}">${longTitle}-${index}</button>`,
    ).join(""),
  );

  const observed = observeDocument(document);
  const size = new TextEncoder().encode(JSON.stringify(observed)).byteLength;

  assert.equal(observed.controls.length, MAX_CONTROL_COUNT);
  assert.ok(size <= EXPECTED_MAX_OBSERVATION_BYTES, `${size} exceeds observation budget`);
  assert.ok(size < MAX_COMPANION_PAYLOAD_BYTES, `${size} reaches native ceiling`);
  for (const control of observed.controls) {
    assert.ok(control.cssPath.length <= EXPECTED_MAX_SELECTOR_LENGTH);
    for (const value of [control.role, control.name, control.label, control.value]) {
      assert.ok((value?.length ?? 0) <= EXPECTED_MAX_CONTROL_FIELD_LENGTH);
    }
  }
});

test("content fallback actions resolve stable targets inside the isolated document", () => {
  const document = documentFor(
    '<button data-testid="confirm">Confirm</button><input id="name">',
  );
  let clicked = false;
  document.querySelector("button")?.addEventListener("click", () => {
    clicked = true;
  });

  executeContentAction(document, "click", { cssPath: '[data-testid="confirm"]' });
  executeContentAction(document, "type", { cssPath: "#name", text: "Ada" });

  assert.equal(clicked, true);
  assert.equal((document.querySelector("#name") as HTMLInputElement).value, "Ada");
});
