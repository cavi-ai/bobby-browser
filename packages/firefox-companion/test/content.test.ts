import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";

import {
  MAX_CONTROL_COUNT,
  MAX_VISIBLE_TEXT_LENGTH,
  executeContentAction,
  observeDocument,
} from "../src/content.js";

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
