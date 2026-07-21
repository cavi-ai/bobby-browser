import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";

import {
  MAX_CONTROL_COUNT,
  MAX_CONTROL_VISITED_NODES,
  MAX_ELEMENT_TEXT_VISITED_NODES,
  MAX_VISIBLE_TEXT_LENGTH,
  MAX_VISIBLE_TEXT_VISITED_NODES,
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

function countWalkerVisits(document: Document): Map<number, number> {
  const visits = new Map<number, number>();
  const createTreeWalker = document.createTreeWalker.bind(document);
  Object.defineProperty(document, "createTreeWalker", {
    configurable: true,
    value(root: Node, whatToShow: number) {
      const walker = createTreeWalker(root, whatToShow);
      const nextNode = walker.nextNode.bind(walker);
      walker.nextNode = () => {
        visits.set(whatToShow, (visits.get(whatToShow) ?? 0) + 1);
        return nextNode();
      };
      return walker;
    },
  });
  return visits;
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

test("observe action scopes selector and target output and only includes sanitized bounded HTML on request", () => {
  const document = documentFor(`
    <main id="wanted" onclick="steal()">
      <p>Wanted text</p>
      <span data-authorization="Bearer private-token">private-token</span>
      <input id="secret" type="password" value="opaque-secret">
      <button id="inside">Inside</button>
      <script>window.exfiltrate("opaque-secret")</script>
    </main>
    <section id="other"><p>Other text</p><button id="outside">Outside</button></section>
  `);

  const selected = executeContentAction(document, "observe", {
    selector: "#wanted",
    target: null,
    includeHtml: true,
  }) as ReturnType<typeof observeDocument> & { html?: string };
  assert.match(selected.visibleText, /Wanted text/);
  assert.doesNotMatch(selected.visibleText, /Other text/);
  assert.equal(selected.controls.length, 2);
  assert.doesNotMatch(selected.controls[0]?.cssPath ?? "", /secret/i);
  assert.equal(selected.controls[1]?.cssPath, "#inside");
  assert.ok(selected.html);
  assert.match(selected.html, /Wanted text/);
  assert.doesNotMatch(selected.html, /opaque-secret|private-token|authorization|onclick|script/i);
  assert.ok(new TextEncoder().encode(JSON.stringify(selected)).byteLength <= EXPECTED_MAX_OBSERVATION_BYTES);

  const targeted = executeContentAction(document, "observe", {
    selector: null,
    target: {
      css: "#other",
      testId: null,
      role: null,
      accessibleName: null,
      label: null,
      text: null,
      attributes: {},
      framePath: [],
      shadowPath: [],
      ordinal: null,
      allowBestMatch: false,
    },
    includeHtml: false,
  }) as ReturnType<typeof observeDocument> & { html?: string };
  assert.equal(targeted.visibleText, "Other text Outside");
  assert.equal(targeted.controls[0]?.cssPath, "#outside");
  assert.equal("html" in targeted, false);
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

test("sanitized HTML fails closed when unsafe content sits beyond the traversal budget", () => {
  const filler = "<i></i>".repeat(MAX_CONTROL_VISITED_NODES + 8);
  const document = documentFor(
    `<main id="bounded">${filler}<input value="z7Q4-vault-material"></main>`,
  );

  const observed = executeContentAction(document, "observe", {
    selector: "#bounded",
    target: null,
    includeHtml: true,
  }) as { html?: string };

  assert.ok(observed.html);
  assert.doesNotMatch(observed.html, /z7Q4-vault-material/);
  assert.ok(new TextEncoder().encode(observed.html).byteLength <= 128 * 1024);
});

test("sanitized HTML uses a strict structural attribute allowlist", () => {
  const credential = "z7Q4-9Lm2";
  const document = documentFor(`
    <main id="${credential}" class="${credential}" data-session="${credential}" custom="${credential}">
      <a href="https://example.test/${credential}" ping="https://example.test/${credential}">Link</a>
      <img src="https://example.test/${credential}" srcset="https://example.test/${credential} 2x" alt="${credential}">
      <video poster="https://example.test/${credential}"></video>
      <object data="https://example.test/${credential}"></object>
      <button role="button" aria-expanded="true" data-testid="${credential}">Continue</button>
      <input type="checkbox" checked disabled value="${credential}">
    </main>
  `);

  const observed = executeContentAction(document, "observe", {
    selector: "main",
    target: null,
    includeHtml: true,
  }) as { html?: string };
  assert.ok(observed.html);
  assert.equal(observed.html.includes(credential), false);
  assert.doesNotMatch(
    observed.html,
    /\s(?:id|class|data-[^=\s]*|custom|href|ping|src|srcset|alt|poster|data|value)=/i,
  );
  assert.match(observed.html, /role="button"/);
  assert.match(observed.html, /aria-expanded="true"/);
  assert.match(observed.html, /type="checkbox"/);
  assert.match(observed.html, /checked=""/);
  assert.match(observed.html, /disabled=""/);
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

test("huge hidden text cannot exhaust the visible-text walker budget", () => {
  const hidden = '<span hidden>ignored hidden payload</span>'.repeat(
    MAX_VISIBLE_TEXT_VISITED_NODES + 256,
  );
  const document = documentFor(`<p>kept text</p>${hidden}`);
  const visits = countWalkerVisits(document);

  const observed = observeDocument(document);

  assert.match(observed.visibleText, /kept text/);
  assert.doesNotMatch(observed.visibleText, /ignored hidden payload/);
  assert.ok((visits.get(4) ?? 0) <= MAX_VISIBLE_TEXT_VISITED_NODES);
  assert.ok(new TextEncoder().encode(JSON.stringify(observed)).byteLength < MAX_COMPANION_PAYLOAD_BYTES);
});

test("huge nonmatching DOM cannot exhaust the control walker budget", () => {
  const document = documentFor("<div></div>".repeat(MAX_CONTROL_VISITED_NODES + 256));
  const visits = countWalkerVisits(document);

  const observed = observeDocument(document);

  assert.deepEqual(observed.controls, []);
  assert.ok((visits.get(1) ?? 0) <= MAX_CONTROL_VISITED_NODES);
  assert.ok(MAX_ELEMENT_TEXT_VISITED_NODES < MAX_CONTROL_VISITED_NODES);
  assert.ok(new TextEncoder().encode(JSON.stringify(observed)).byteLength < MAX_COMPANION_PAYLOAD_BYTES);
});

test("huge sibling sets cannot bypass the css-path helper budget", () => {
  const siblingBudget = 128;
  const document = documentFor(
    `<div id="bounded-parent">${"<span>noise</span>".repeat(siblingBudget + 256)}<button>Last</button></div>`,
  );
  const parent = document.getElementById("bounded-parent");
  assert.ok(parent);
  const siblings = parent.children;
  const item = siblings.item.bind(siblings);
  let siblingVisits = 0;
  Object.defineProperty(siblings, "item", {
    configurable: true,
    value(index: number) {
      siblingVisits += 1;
      return item(index);
    },
  });

  observeDocument(document);

  assert.ok(
    siblingVisits <= siblingBudget,
    `css-path sibling work exceeded its budget: ${siblingVisits}`,
  );
});

test("huge label sets use the bounded label index instead of a document query", () => {
  const noise = Array.from(
    { length: 2_048 },
    (_, index) => `<label hidden for="noise-${index}">noise</label>`,
  ).join("");
  const document = documentFor(`${noise}<label for="target">Target label</label><input id="target">`);
  const querySelector = document.querySelector.bind(document);
  let documentQueries = 0;
  Object.defineProperty(document, "querySelector", {
    configurable: true,
    value(selector: string) {
      documentQueries += 1;
      return querySelector(selector);
    },
  });

  const observed = observeDocument(document);

  assert.equal(documentQueries, 0);
  assert.equal(observed.controls[0]?.label, "Target label");
});

test("hidden controls do not consume the observed control cap", () => {
  const hidden = Array.from(
    { length: MAX_CONTROL_COUNT },
    (_, index) => `<button hidden>hidden-${index}</button>`,
  ).join("");
  const document = documentFor(`${hidden}<button id="visible-target">Visible target</button>`);

  const observed = observeDocument(document);

  assert.equal(observed.controls.length, 1);
  assert.equal(observed.controls[0]?.cssPath, "#visible-target");
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
