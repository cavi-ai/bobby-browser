import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";

import { executeContentAction, observeDocument } from "../src/content.js";
import { NativeCompanionTransport } from "../src/native-transport.js";
import { containsSecretMaterial } from "../src/secret-material.js";

const ORDINARY = [
  "Authors",
  "Author: Jane Doe",
  "Forgot password?",
  "Password",
  "Reset your password",
  "Two-factor authentication",
  "Token gating explained",
  "Secret Santa sign-up",
  "API key management",
  "Credentials",
  // Free text is never URL-parsed as a whole, on either side of the channel.
  "Status:online",
  "mailto:someone@example.test",
  "Note:important",
];
const BOUNDARY = [
  "Basic info and settings",
  "Bearer of bad news",
  "private key backup",
  "wss://127.0.0.1:9876/session?token=abc",
  "ftp://example.test/file",
  "https://user:pw@example.test/",
  "https://example.test/?access_token=abc",
];
const SECRET = [
  "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
  "Bearer abc123def456ghi789",
  "Basic dXNlcjpwYXNz",
  "password: hunter2!",
  "api_key=9f8e7d6c5b4a",
  "token = abcd1234",
  "-----BEGIN RSA PRIVATE KEY-----",
  "ghp_0123456789abcdefghijABCDEFGHIJ012345",
  "sk-live-0123456789abcdefghij",
  "AKIAIOSFODNN7EXAMPLE",
  "aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bC1dE3fG5hJ7kL9mN1pQ3",
];

function escape(text: string): string {
  return text.replaceAll("&", "&amp;").replaceAll('"', "&quot;").replaceAll("<", "&lt;");
}

function pageFor(text: string): Document {
  const t = escape(text);
  return new JSDOM(
    `<!doctype html><html><head><title>Example</title></head><body><main><h1>${t}</h1>` +
      `<a href="https://example.test/x">${t}</a><button>${t}</button>` +
      `<label for="i">${t}</label><input id="i" type="text"><p>${t}</p></main></body></html>`,
    { url: "https://example.test/p" },
  ).window.document;
}

function outputs(text: string): { observe: string; a11y: string } {
  const document = pageFor(text);
  return {
    observe: JSON.stringify(observeDocument(document)),
    a11y: JSON.stringify(executeContentAction(document, "a11yTree", { maxNodes: 64 })),
  };
}

for (const text of ORDINARY) {
  test(`ordinary text is not redacted: ${text}`, () => {
    const { observe, a11y } = outputs(text);
    assert.equal(observe.includes("[redacted]"), false);
    assert.equal(a11y.includes("[redacted]"), false);
    assert.ok(observe.includes(text) && a11y.includes(text));
    assert.equal(containsSecretMaterial(text), false);
  });
}

for (const text of [...BOUNDARY, ...SECRET]) {
  test(`redacted in observe and a11yTree: ${text}`, () => {
    const { observe, a11y } = outputs(text);
    assert.ok(observe.includes("[redacted]"));
    assert.ok(a11y.includes("[redacted]"));
    assert.equal(observe.includes(text), false);
    assert.equal(a11y.includes(text), false);
  });
}

for (const text of SECRET) {
  test(`secret material is detected: ${text}`, () => {
    assert.equal(containsSecretMaterial(text), true);
  });
}

test("every redaction-test result passes the real outbound validator", () => {
  const listeners = { addListener() {} };
  const transport = new NativeCompanionTransport({
    connectNative: () => ({
      postMessage() {},
      onMessage: listeners,
      onDisconnect: listeners,
      disconnect() {},
    }),
  });
  transport.start(() => {});
  for (const text of [...ORDINARY, ...BOUNDARY, ...SECRET]) {
    const document = pageFor(text);
    for (const output of [
      observeDocument(document),
      executeContentAction(document, "a11yTree", { maxNodes: 64 }),
    ]) {
      assert.doesNotThrow(
        () =>
          transport.send({
            kind: "actionCompleted",
            output: {
              commandId: "4c4dfe8c-7c69-4b33-a13e-1fcdf18f2952",
              interactionPath: "extensionApi",
              output,
            },
          }),
        text,
      );
    }
  }
});

const LONG_ID = "1A2b3C4d5E6f7G8h9I0jK1l2M3n4O5p6Q7r8S9t0UvWx";

function observedUrl(url: string): string {
  return observeDocument(
    new JSDOM("<!doctype html><html><head><title>Example</title></head><body><p>x</p></body></html>", {
      url,
    }).window.document,
  ).url;
}

test("an ordinary page URL with a long mixed-case id stays visible", () => {
  const url = `https://docs.example.test/document/d/${LONG_ID}/edit`;
  assert.equal(observedUrl(url), url);
  assert.equal(containsSecretMaterial(url), false);
});

for (const url of [
  "https://example.test/reset/eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
  "https://example.test/t/ghp_0123456789abcdefghijABCDEFGHIJ012345",
]) {
  test(`a URL carrying a credential is redacted: ${url.slice(0, 40)}`, () => {
    assert.equal(observedUrl(url), "[redacted]");
    assert.equal(containsSecretMaterial(url), true);
  });
}

test("the same long id outside a URL is still redacted", () => {
  assert.equal(containsSecretMaterial(LONG_ID), true);
  const document = pageFor(LONG_ID);
  assert.ok(JSON.stringify(observeDocument(document)).includes("[redacted]"));
  assert.equal(JSON.stringify(observeDocument(document)).includes(LONG_ID), false);
});

function controlsOf(body: string): { observe: ReturnType<typeof observeDocument>["controls"]; a11y: string } {
  const document = new JSDOM(
    `<!doctype html><html><head><title>E</title></head><body>${body}</body></html>`,
    { url: "https://example.test/p" },
  ).window.document;
  return {
    observe: observeDocument(document).controls,
    a11y: JSON.stringify(executeContentAction(document, "a11yTree", { maxNodes: 64 })),
  };
}

const AUTHOR_FIELDS = [
  '<label for="author">Author name</label><input type="text" id="author" name="author" value="Jane Doe">',
  '<span id="author-label">Author name</span><input type="text" value="Jane Doe" aria-labelledby="author-label">',
  '<label for="a">Author name</label><input type="text" id="a" name="authority" value="Jane Doe">',
];

AUTHOR_FIELDS.forEach((body, index) => {
  test(`author-family fields are not classified sensitive: ${index}`, () => {
    const { observe, a11y } = controlsOf(body);
    assert.equal(observe[0]?.name, "Author name");
    assert.equal(observe[0]?.value, "Jane Doe");
    assert.ok(a11y.includes('"name":"Author name"'));
    assert.ok(a11y.includes('"value":"Jane Doe"'));
  });
});

for (const attribute of [
  'name="auth_code"',
  'name="authCode"',
  'id="oauth-token"',
  'data-authorization="x"',
  'name="authentication"',
  'type="password"',
]) {
  test(`auth-family fields stay sensitive: ${attribute}`, () => {
    const { observe, a11y } = controlsOf(`<input ${attribute} value="Jane Doe" aria-label="Field">`);
    assert.equal(observe[0]?.value, "[redacted]");
    assert.equal(a11y.includes("Jane Doe"), false);
  });
}
