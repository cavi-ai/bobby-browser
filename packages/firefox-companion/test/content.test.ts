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
import { NativeCompanionTransport } from "../src/native-transport.js";
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
      <input id="email" name="email" type="email" value="user@example.test" required autocomplete="email">
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
    attributes: { autocomplete: "email", name: "email", required: "true", type: "email" },
    disabled: false,
  });
  assert.equal(observed.controls[1]?.cssPath, '[data-testid="submit-login"]');
  assert.equal(observed.controls[1]?.role, "button");
  assert.equal(observed.controls[1]?.name, "Continue");
});

test("named iframes are observable targets", () => {
  const document = documentFor('<iframe title="Card details"></iframe>');
  const observed = observeDocument(document);
  assert.equal(observed.controls[0]?.role, "iframe");
  assert.equal(observed.controls[0]?.name, "Card details");
  const snapshot = executeContentAction(document, "a11yTree", { maxNodes: 32 });
  assert.match(JSON.stringify(snapshot), /Card details/);
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
  assert.equal(observed.controls[0]?.name, "Password");
  assert.equal(observed.controls[0]?.value, "[redacted]");
});

test("public authentication field labels remain targetable while their values stay redacted", () => {
  const document = documentFor(
    '<label for="authentication-code">Authentication code</label><input id="authentication-code" value="opaque-code" autocomplete="one-time-code">',
  );
  const observed = observeDocument(document);
  assert.equal(observed.controls[0]?.name, "Authentication code");
  assert.equal(observed.controls[0]?.value, "[redacted]");
  assert.equal(JSON.stringify(observed).includes("opaque-code"), false);
  const tree = executeContentAction(document, "a11yTree", { maxNodes: 64 });
  assert.match(JSON.stringify(tree), /"name":"Authentication code"/);
});

test("file picker observations never expose the browser's local path", () => {
  const document = documentFor('<label for="upload">Customer document</label><input id="upload" type="file">');
  const input = document.querySelector("input")!;
  Object.defineProperty(input, "value", { value: "C:\\fakepath\\approved-upload.txt" });

  const observed = observeDocument(document);
  assert.equal(observed.controls[0]?.name, "Customer document");
  assert.equal(observed.controls[0]?.value, undefined);
  assert.equal(JSON.stringify(observed).includes("fakepath"), false);
});

test("search inputs observe as searchbox and file inputs as button with their selected names", () => {
  const document = documentFor(
    '<input id="q" type="search" placeholder="Search products"><label for="upload">Customer document</label><input id="upload" type="file">',
  );
  const upload = document.querySelector<HTMLInputElement>("#upload")!;
  Object.defineProperty(upload, "files", {
    value: [{ name: "resume.txt" }],
  });

  const observed = observeDocument(document);
  const search = observed.controls.find((control) => control.cssPath === "#q");
  const file = observed.controls.find((control) => control.cssPath === "#upload");
  assert.equal(search?.role, "searchbox");
  assert.equal(search?.name, "Search products");
  assert.equal(file?.role, "button");
  assert.equal(file?.value, "resume.txt");
});

test("invalid required fields retain the accessibility invalid state for resolution", () => {
  const observed = observeDocument(documentFor('<input aria-label="Full name" required>'));
  assert.equal(observed.controls[0]?.name, "Full name");
  assert.equal(observed.controls[0]?.attributes["aria-invalid"], "true");
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

test("sanitized HTML removes comments and unsupported nodes before serialization", () => {
  const credential = "z7Q4-comment-secret";
  const document = documentFor("<main><p>Visible</p></main>");
  const main = document.querySelector("main");
  assert.ok(main);
  main.append(document.createComment(credential));
  main.append(document.createProcessingInstruction("opaque", credential));

  const observed = executeContentAction(document, "observe", {
    selector: "main",
    target: null,
    includeHtml: true,
  }) as { html?: string };

  assert.ok(observed.html);
  assert.equal(observed.html.includes(credential), false);
  assert.doesNotMatch(observed.html, /<!--|<\?/);
  assert.match(observed.html, /<p>Visible<\/p>/);
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

test("a11yTree builds a bounded, hidden-aware, redacting accessibility tree", () => {
  const document = documentFor(`
    <main>
      <h1>Sign in</h1>
      <form>
        <label for="email">Email address</label>
        <input id="email" type="email" value="user@example.test">
        <input id="secret" type="password" value="hunter2">
        <button>Continue</button>
      </form>
      <span hidden>not visible</span>
    </main>
  `);

  const result = executeContentAction(document, "a11yTree", { maxNodes: 64 }) as {
    nodes: Array<{ role?: string; name?: string; children?: unknown[] }>;
    truncated: boolean;
  };

  assert.equal(result.truncated, false);
  const root = result.nodes[0]!;
  assert.equal(root.role, "main");
  const serialized = JSON.stringify(result.nodes);
  assert.match(serialized, /"role":"heading"/);
  assert.match(serialized, /"role":"textbox"/);
  assert.match(serialized, /"role":"button"/);
  assert.match(serialized, /Email address/);
  assert.doesNotMatch(serialized, /hunter2/);
  assert.doesNotMatch(serialized, /not visible/);

  const bounded = executeContentAction(document, "a11yTree", { maxNodes: 1 }) as {
    truncated: boolean;
  };
  assert.equal(bounded.truncated, true);
});

test("a11yTree reports visible page text as StaticText only when asked", () => {
  const document = documentFor(`
    <main>
      <h1>Documents</h1>
      <label for="hidden-file">Attach resume</label>
      <input type="file" id="hidden-file" style="display:none">
      <input type="file" id="visible-file" aria-label="Visible attachment">
      <pre id="out">hidden-file=hidden-bytes-7731;</pre>
      <ul><li>Post text</li></ul>
      <button>Continue</button>
      <span hidden>not visible</span>
      <p>key sk_live_abcdefghijklmnop1234</p>
      <pre>-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAx7Qk2LmZr9VtW4YbNc6HdJf5GsAePuXo7TiKq
-----END RSA PRIVATE KEY-----</pre>
      <select><option>Option text</option></select>
      <textarea>Draft text</textarea>
      <script>const inline = "script text";</script>
    </main>
  `);
  type Node = { role?: string; name?: string; children?: Node[] };
  const flat = (nodes: Node[]): Node[] => nodes.flatMap((node) => [node, ...flat(node.children ?? [])]);
  const tree = executeContentAction(document, "a11yTree", { maxNodes: 64, includeText: true }) as {
    nodes: Node[];
    truncated: boolean;
  };
  assert.equal(tree.truncated, false);
  const texts = flat(tree.nodes)
    .filter((node) => node.role === "StaticText")
    .map((node) => node.name);
  assert.deepEqual(texts, ["Attach resume", "hidden-file=hidden-bytes-7731;", "[redacted]", "[redacted]"]);
  const main = tree.nodes[0]!;
  assert.equal(main.role, "main");
  assert.ok(main.children?.some((node) => node.role === "StaticText" && node.name === "hidden-file=hidden-bytes-7731;"));
  assert.ok(flat(tree.nodes).some((node) => node.role === "listitem" && node.name === "Post text"));
  assert.doesNotMatch(JSON.stringify(tree), /sk_live_|MIIEowIBAAKCAQEA|not visible|script text/);

  const plain = executeContentAction(document, "a11yTree", { maxNodes: 64 }) as { nodes: Node[] };
  assert.equal(flat(plain.nodes).filter((node) => node.role === "StaticText").length, 0);
});

test("a11yTree includes a same-origin iframe's controls with the frame hop in their targets", () => {
  const document = documentFor(
    '<button>Open</button><iframe title="Widget frame"></iframe><iframe title="Widget frame"></iframe>',
  );
  const frames = document.querySelectorAll("iframe");
  frames[0]!.contentDocument!.body.innerHTML = "<button>Frame action</button>";
  frames[1]!.contentDocument!.body.innerHTML = "<button>Frame action</button>";
  type Node = {
    role?: string;
    name?: string;
    target?: { ordinal?: number; framePath?: Array<{ role: string; accessibleName: string; ordinal?: number }> };
    children?: Node[];
  };
  const tree = executeContentAction(document, "a11yTree", { maxNodes: 32 }) as { nodes: Node[] };
  const flat = (nodes: Node[]): Node[] => nodes.flatMap((node) => [node, ...flat(node.children ?? [])]);
  const inFrame = flat(tree.nodes).filter((node) => node.name === "Frame action");
  assert.equal(inFrame.length, 2);
  assert.deepEqual(inFrame[0]!.target?.framePath, [
    { role: "iframe", accessibleName: "Widget frame", ordinal: 0 },
  ]);
  assert.deepEqual(inFrame[1]!.target?.framePath, [
    { role: "iframe", accessibleName: "Widget frame", ordinal: 1 },
  ]);
  assert.equal(inFrame[0]!.target?.ordinal, undefined);
  const open = flat(tree.nodes).find((node) => node.name === "Open");
  assert.equal(open?.target?.framePath, undefined);
});

test("a11yTree marks a control invalid when the page flags it aria-invalid", () => {
  const document = documentFor(
    '<label>Name <input name="name" aria-invalid="true"></label><label>City <input name="city"></label>',
  );
  const tree = executeContentAction(document, "a11yTree", { maxNodes: 32 }) as {
    nodes: Array<{ role?: string; name?: string; invalid?: boolean }>;
  };
  const flat = (nodes: typeof tree.nodes): typeof tree.nodes =>
    nodes.flatMap((node) => [node, ...flat((node as { children?: typeof tree.nodes }).children ?? [])]);
  const byName = (name: string) => flat(tree.nodes).find((node) => node.name === name);
  assert.equal(byName("Name")?.invalid, true);
  assert.equal(byName("City")?.invalid, false);
});

test("a11yTree exposes bounded form state without leaking sensitive values", () => {
  const secret = "vault-secret-92";
  const document = documentFor(`
    <form>
      <label for="email">Email address</label>
      <input id="email" type="email" value="broken" required autocomplete="email">
      <label><input id="terms" type="checkbox" checked disabled> Accept terms</label>
      <label for="password">Password</label>
      <input id="password" type="password" value="${secret}" required>
    </form>
  `);

  const result = executeContentAction(document, "a11yTree", { maxNodes: 64 }) as {
    nodes: Array<Record<string, unknown>>;
    truncated: boolean;
  };
  const encoded = JSON.stringify(result.nodes);

  assert.match(encoded, /"name":"Email address"/);
  assert.match(encoded, /"value":"broken"/);
  assert.match(encoded, /"required":true/);
  assert.match(encoded, /"invalid":true/);
  assert.match(encoded, /"autocomplete":"email"/);
  assert.match(encoded, /"checked":true/);
  assert.match(encoded, /"disabled":true/);
  assert.match(encoded, /"value":"\[redacted\]"/);
  assert.match(encoded, /"name":"Password"/);
  assert.equal(encoded.includes(secret), false);
});

test("a11yTree keeps the global ordinal when a duplicate is truncated", () => {
  const document = documentFor(`
    <label for="home-phone">Phone</label><input id="home-phone">
    <label for="work-phone">Phone</label><input id="work-phone">
  `);

  const result = executeContentAction(document, "a11yTree", { maxNodes: 1 }) as {
    nodes: Array<{
      target?: { role: string; accessibleName: string; ordinal?: number };
    }>;
    truncated: boolean;
  };

  assert.equal(result.truncated, true);
  assert.equal(result.nodes.length, 1);
  assert.deepEqual(result.nodes[0]?.target, {
    role: "textbox",
    accessibleName: "Phone",
    ordinal: 0,
  });
});

function sendA11yThroughChannel(document: Document, maxNodes: number): {
  nodes: Array<{ name?: string }>;
  truncated: boolean;
  posted: unknown[];
} {
  const result = executeContentAction(document, "a11yTree", { maxNodes }) as {
    nodes: Array<{ name?: string }>;
    truncated: boolean;
  };
  const posted: unknown[] = [];
  const listeners = { addListener() {} };
  const transport = new NativeCompanionTransport({
    connectNative: () => ({
      postMessage: (message: unknown) => void posted.push(message),
      onMessage: listeners,
      onDisconnect: listeners,
      disconnect() {},
    }),
  });
  transport.start(() => {});
  transport.send({
    kind: "actionCompleted",
    output: {
      commandId: "4c4dfe8c-7c69-4b33-a13e-1fcdf18f2952",
      interactionPath: "extensionApi",
      output: result,
    },
  });
  return { ...result, posted };
}

function collectNames(nodes: Array<{ name?: string; children?: unknown }>): string[] {
  return nodes.flatMap((node) => [
    ...(node.name === undefined ? [] : [node.name]),
    ...collectNames((node.children ?? []) as Array<{ name?: string }>),
  ]);
}

test("a11yTree result is transmissible for a 40-level nested page", () => {
  const body = `${"<section>".repeat(40)}<button>Deep leaf</button>${"</section>".repeat(40)}`;
  const sent = sendA11yThroughChannel(documentFor(body), 256);
  assert.equal(sent.posted.length, 1);
  assert.equal(sent.truncated, true);
});

test("a11yTree result is transmissible for names that match the outbound secret patterns", () => {
  const body =
    '<button>Basic info and settings</button><button>private key backup</button>' +
    '<button>Open mailbox</button>';
  const sent = sendA11yThroughChannel(documentFor(body), 256);
  assert.equal(sent.posted.length, 1);
  assert.ok(collectNames(sent.nodes).includes("Open mailbox"));
});

test("a11yTree result is transmissible for names that parse as non-http or secret-query URLs", () => {
  const body =
    '<button>mailto:someone@example.test</button><button>Status:online</button>' +
    '<a href="https://example.test/path?key=abc">https://example.test/path?key=abc</a>' +
    '<button aria-label="Status:online">x</button>';
  const sent = sendA11yThroughChannel(documentFor(body), 256);
  assert.equal(sent.posted.length, 1);
});

test("a11yTree result stays transmissible at the maximum node budget", () => {
  const filler = "w".repeat(250);
  const body = Array.from(
    { length: 12 },
    (_, section) =>
      `<section>${Array.from(
        { length: 250 },
        (_, index) => `<button>${section}-${index} ${filler}</button>`,
      ).join("")}</section>`,
  ).join("");
  const sent = sendA11yThroughChannel(documentFor(body), 100_000);
  assert.equal(sent.posted.length, 1);
  assert.equal(sent.truncated, true);
  assert.ok(Buffer.byteLength(JSON.stringify(sent.posted[0])) <= MAX_COMPANION_PAYLOAD_BYTES);
});

function largeBody(visible: number, after: string): string {
  const hidden = Array.from({ length: 3000 }, (_, index) => `<a href="#h${index}">Hidden ${index}</a>`).join("");
  const buttons = Array.from({ length: visible }, (_, index) => `<button>Item ${index}</button>`).join("");
  return `<div style="display:none">${hidden}</div><main>${buttons}${after}</main>`;
}

test("a11yTree flags a sibling list longer than its child bound as truncated", () => {
  const document = documentFor(largeBody(300, ""));
  const result = executeContentAction(document, "a11yTree", { maxNodes: 1024 }) as {
    truncated: boolean;
  };
  assert.equal(result.truncated, true);
});

test("locateTarget finds a link past thousands of visible and hidden nodes", () => {
  const document = documentFor(largeBody(4500, '<a href="/all">Show all</a>'));
  const located = executeContentAction(document, "locateTarget", {
    target: { role: "link", accessibleName: "Show all" },
  }) as { found: boolean; ambiguous: boolean; cssPath?: string; name?: string };
  assert.equal(located.found, true);
  assert.equal(located.name, "Show all");
  assert.equal(document.querySelector(located.cssPath as string)?.textContent, "Show all");
});

test("locateTarget reports an absent target and an ambiguous one", () => {
  const document = documentFor(
    largeBody(300, '<a href="/a">Same</a><a href="/b">Same</a>'),
  );
  const absent = executeContentAction(document, "locateTarget", {
    target: { role: "link", accessibleName: "Missing" },
  }) as { found: boolean; ambiguous: boolean };
  assert.deepEqual(absent, { found: false, ambiguous: false });
  const ambiguous = executeContentAction(document, "locateTarget", {
    target: { role: "link", accessibleName: "Same" },
  }) as { found: boolean; ambiguous: boolean };
  assert.deepEqual(ambiguous, { found: false, ambiguous: true });
  const second = executeContentAction(document, "locateTarget", {
    target: { role: "link", accessibleName: "Same", ordinal: 1 },
  }) as { found: boolean; cssPath?: string };
  assert.equal(second.found, true);
  assert.equal(document.querySelector(second.cssPath as string)?.getAttribute("href"), "/b");
});

test("a11yTree keeps main and its first children when the budget ends inside main", () => {
  const buttons = Array.from({ length: 200 }, (_, index) => `<button>Item ${index}</button>`).join("");
  const document = documentFor(`<header><a href="/home">Home</a></header><main><div>${buttons}</div></main>`);

  const result = executeContentAction(document, "a11yTree", { maxNodes: 60 }) as {
    nodes: Array<{ role?: string; name?: string; children?: unknown[] }>;
    truncated: boolean;
  };

  assert.equal(result.truncated, true);
  const serialized = JSON.stringify(result.nodes);
  assert.match(serialized, /"role":"main"/);
  assert.match(serialized, /"name":"Item 0"/);
  assert.doesNotMatch(serialized, /"name":"Item 199"/);
  const count = (nodes: Array<{ children?: unknown[] }>): number =>
    nodes.reduce((total, node) => total + 1 + count((node.children ?? []) as never), 0);
  assert.ok(count(result.nodes) <= 60);
});

test("a11yTree names containers only from aria attributes and keeps listitem text", () => {
  const document = documentFor(`
    <header>Site banner text</header>
    <nav>Navigation words <a href="/a">Alpha</a></nav>
    <main><p>Main body words</p>
      <form>Form words <input placeholder="Search"></form>
      <ul><li>First post text</li></ul>
      <section aria-label="Labelled region">Region words</section>
    </main>
    <footer>Footer words</footer>
  `);

  const result = executeContentAction(document, "a11yTree", { maxNodes: 64 }) as {
    nodes: unknown[];
  };
  const found = new Map<string, Array<string | undefined>>();
  const walk = (nodes: Array<{ role?: string; name?: string; children?: unknown[] }>): void => {
    for (const node of nodes) {
      if (node.role) found.set(node.role, [...(found.get(node.role) ?? []), node.name]);
      walk((node.children ?? []) as never);
    }
  };
  walk(result.nodes as never);

  for (const role of ["banner", "navigation", "main", "form", "list", "contentinfo"]) {
    assert.deepEqual(found.get(role), [undefined], `${role} must not be named from content`);
  }
  assert.deepEqual(found.get("region"), ["Labelled region"]);
  assert.deepEqual(found.get("listitem"), ["First post text"]);
  assert.deepEqual(found.get("link"), ["Alpha"]);
});
