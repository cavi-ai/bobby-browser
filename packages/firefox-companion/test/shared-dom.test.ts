import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { JSDOM } from "jsdom";
import { controlLabel } from "../../../crates/dom-engine/src/control_label.js";
import {
  composedQuery,
  composedQueryAll,
  isHidden,
  modalDialogOpen,
  obscuredByOpenModal,
} from "../../../crates/dom-engine/src/page_dom.js";

function page(html: string) {
  const dom = new JSDOM(html, { runScripts: "outside-only" });
  Object.defineProperty(dom.window.Element.prototype, "getClientRects", { value: () => [{ width: 20, height: 20 }] });
  Object.defineProperty(dom.window.Element.prototype, "getBoundingClientRect", { value: () => ({ width: 20, height: 20 }) });
  return dom.window;
}

type Gathered = {role:string;name:string;css:string;tag:string;attributes:Record<string,string>};
type Gather = (root:ParentNode, options:Record<string,unknown>) => { candidates:Gathered[]; truncated:boolean };
type Match = (root:ParentNode, spec:{role?:string;name?:string;selector?:string;ordinal?:number}, gather:Gather, limit?:number) => Element[] | "too-many";

function gatherScript() {
  return readFileSync(new URL("../../../crates/dom-engine/src/candidate_gather.js", import.meta.url), "utf8")
    .replace("__CONTROL_LABEL_FUNCTION__", controlLabel.toString());
}

test("associated label bounds withhold incomplete names", () => {
  const window = page(`<label>${"x".repeat(2049)}<input></label>`);
  const input = window.document.querySelector("input")!;
  assert.equal(controlLabel(input), null);
  input.parentElement!.replaceChildren(window.document.createTextNode("Field"), input);
  assert.equal(controlLabel(input, () => false), null);
  assert.equal(controlLabel(input), "Field");
});

function gather(window: ReturnType<typeof page>, options = {}) {
  const script = gatherScript();
  const collect=window.eval("("+script+")") as Gather;
  const result=collect(window.document,{controlsOnly:true,redact:true,maxNodes:4096,maxCandidates:128,...options});
  return JSON.parse(JSON.stringify({ controls: result.candidates, truncated: result.truncated })) as { controls: Array<{id:string;role:string;name:string;css:string;tag:string;attributes:Record<string,string>}>;truncated:boolean };

}

test("shared gathering infers native roles and reads labelled-by names", () => {
  const window = page(`<span id="label">Remember me</span><input type="checkbox" aria-labelledby="label" aria-label="Fallback"><input placeholder="Search terms" type="search">`);
  const result = gather(window);
  assert.deepEqual(result.controls.map(({ role, name }) => ({ role, name })), [
    { role: "checkbox", name: "Remember me" }, { role: "searchbox", name: "Search terms" },
  ]);
});

test("shared gathering names button inputs from their labels without exposing field values", () => {
  const window = page(`<input type="button" value="Save"><input type="submit" value="Send"><input type="reset" value="Clear"><input type="submit" aria-label="Publish" value="Send"><span id="label">Confirm</span><input type="button" aria-labelledby="label" value="Continue"><input role="button" value="private-field-value">`);
  assert.deepEqual(gather(window, { redact: false }).controls.map(({ role, name }) => ({ role, name })), [
    { role: "button", name: "Save" },
    { role: "button", name: "Send" },
    { role: "button", name: "Clear" },
    { role: "button", name: "Publish" },
    { role: "button", name: "Confirm" },
    { role: "button", name: null },
  ]);
  assert.equal(JSON.stringify(gather(window)).includes("private-field-value"), false);
});

test("shared gathering names image inputs from alt text and preserves ARIA precedence", () => {
  const window = page(`<input type="image" alt="Upload image" value="private-image-value"><input type="image" alt="Fallback" aria-label="Publish"><span id="image-label">Confirm</span><input type="image" alt="Fallback" aria-labelledby="image-label" aria-label="Other"><input role="button" alt="Not a button label" value="private-field-value">`);
  assert.deepEqual(gather(window, { redact: false }).controls.map(({ role, name }) => ({ role, name })), [
    { role: "button", name: "Upload image" },
    { role: "button", name: "Publish" },
    { role: "button", name: "Confirm" },
    { role: "button", name: null },
  ]);
  assert.equal(JSON.stringify(gather(window)).includes("private-image-value"), false);
  assert.equal(JSON.stringify(gather(window)).includes("private-field-value"), false);
});

test("shared gathering redacts sensitive labels and refuses partial bounded sets", () => {
  const secret = "secret: sample-canary-2836";
  const window = page(`<input type="password" aria-label="${secret}" value="${secret}"><button>Save</button>`);
  assert.equal(gather(window).controls[0]?.name, "[redacted]");
  assert.equal(JSON.stringify(gather(window)).includes(secret), false);
  assert.deepEqual(gather(window, { maxCandidates:1 }), { controls:[], truncated:true });
  assert.deepEqual(gather(window, { maxNodes:1 }), { controls:[], truncated:true });
});

test("shared gathering keeps an element's stamped id and gives a copy a fresh one", () => {
  const window = page(`<button>Save</button><button>Cancel</button>`);
  const ids = (prefix: string) => gather(window, { prefix }).controls.map(({ id }) => id);
  const first = ids("bobby-1-");
  assert.deepEqual(first, ["bobby-1-1", "bobby-1-2"]);
  assert.deepEqual(ids("bobby-2-"), first);
  const save = window.document.querySelector("button")!;
  save.after(save.cloneNode(true));
  assert.deepEqual(ids("bobby-3-"), ["bobby-1-1", "bobby-3-1", "bobby-1-2"]);
});

test("shared gathering preserves input kind and validation metadata for local resolution", () => {
  const window=page(`<input type="email" aria-label="Email" aria-invalid="true" required>`);
  const input=gather(window,{redact:false}).controls[0]!;
  assert.equal(input.tag,"input");
  assert.equal(input.role,"textbox");
  assert.equal(input.attributes.type,"email");
  assert.equal(input.attributes["aria-invalid"],"true");
  assert.equal(input.attributes.required,"true");
});

test("shared gathering reaches controls inside an unnamed open shadow host", () => {
  const window=page(`<div></div>`);
  window.document.querySelector("div")!.attachShadow({mode:"open"}).innerHTML=`<input type="checkbox" aria-label="Remember">`;
  const result=gather(window);
  assert.equal(result.controls.length,1);
  assert.equal(result.controls[0]?.role,"checkbox");
  assert.equal(result.controls[0]?.name,"Remember");
  assert.match(result.controls[0]!.css,/ >>> /);
});

test("shared shadow matching stays inside the selected root and uses native roles", () => {
  const window = page(`<span id="label">Outside</span><span id="document-only">Document name</span><div></div>`);
  const host = window.document.querySelector("div")!;
  const root = host.attachShadow({ mode:"open" });
  root.innerHTML = `<span id="label">Inside</span><input type="checkbox" aria-labelledby="label" aria-label="Fallback"><input type="checkbox" aria-labelledby="document-only"><div></div>`;
  root.querySelector("div")!.attachShadow({mode:"open"}).innerHTML = `<input type="checkbox" aria-label="Inside">`;
  const script = gatherScript();
  const matchScript = readFileSync(new URL("../../../crates/dom-engine/src/shadow_segment.js", import.meta.url), "utf8");
  const collect=window.eval("("+script+")") as Gather;
  const match=window.eval("("+matchScript+")") as Match;
  const matches=match(root,{role:"checkbox",name:"Inside"},collect);
  assert.ok(Array.isArray(matches));
  assert.equal(matches.length, 1);
  assert.equal(matches[0], root.querySelector("input"));
  assert.equal(match(root,{role:"checkbox",name:"Outside"},collect).length, 0);
  assert.equal(match(root,{role:"checkbox",name:"Document name"},collect).length, 0);
  root.append(root.querySelector("input")!.cloneNode());
  assert.equal(match(root,{role:"checkbox",name:"Inside"},collect).length, 2);
});

test("shadow matching applies ordinals within the selected root and refuses missing indices", () => {
  const window = page(`<input aria-label="Email"><div id="host"></div>`);
  const root = window.document.getElementById("host")!.attachShadow({mode:"open"});
  root.innerHTML = `<input aria-label="Email"><input aria-label="Email"><input aria-label="Email">`;
  const collect = window.eval("(" + gatherScript() + ")") as Gather;
  const match = window.eval("(" + readFileSync(new URL("../../../crates/dom-engine/src/shadow_segment.js", import.meta.url), "utf8") + ")") as Match;
  for (const spec of [{role:"textbox", name:"Email"}, {selector:"input"}]) {
    for (const ordinal of [0, 1, 2]) {
      const matches = match(root, {...spec, ordinal}, collect);
      assert.ok(Array.isArray(matches));
      assert.equal(matches.length, 1);
      assert.equal(matches[0], root.querySelectorAll("input")[ordinal]);
    }
    for (const ordinal of [-1, 0.5, 3]) assert.equal(match(root, {...spec, ordinal}, collect).length, 0);
    assert.equal(match(root, spec, collect).length, 2);
  }
});

test("composed query hops open shadow roots and stops without one", () => {
  const window = page(`<div id="host"></div><div id="plain"></div>`);
  const host = window.document.getElementById("host")!;
  const shadow = host.attachShadow({ mode: "open" });
  shadow.innerHTML = `<span class="inner">Inside</span><b class="inner">Also</b>`;
  assert.equal(composedQuery(window.document, "#host >>> .inner")?.textContent, "Inside");
  assert.equal(composedQueryAll(window.document, "#host >>> .inner").length, 2);
  assert.equal(composedQuery(window.document, "#plain >>> .inner"), null);
  assert.throws(() => composedQuery(window.document, "#host >>> ["));
});

test("hidden modes disagree on aria-hidden and agree on display none", () => {
  const window = page(`<div id="aria" aria-hidden="true"><span id="leaf">Text</span></div><div id="css" style="display:none"><span id="gone">Gone</span></div><span id="self" style="display:none">Self</span>`);
  const leaf = window.document.getElementById("leaf")!;
  const gone = window.document.getElementById("gone")!;
  const self = window.document.getElementById("self")!;
  assert.equal(isHidden(leaf, "a11y"), true);
  assert.equal(isHidden(leaf, "hit-ready"), false);
  assert.equal(isHidden(leaf, "rendered"), false);
  assert.equal(isHidden(gone, "a11y"), true);
  assert.equal(isHidden(gone, "hit-ready"), true);
  assert.equal(isHidden(gone, "rendered"), false);
  assert.equal(isHidden(self, "rendered"), true);
});

test("a visible aria-modal dialog is open and a hidden one is not", () => {
  const open = page(`<div role="dialog" aria-modal="true">Open</div>`);
  const shut = page(`<div role="dialog" aria-modal="true" hidden>Shut</div>`);
  assert.equal(modalDialogOpen(open.document), true);
  assert.equal(modalDialogOpen(shut.document), false);
  assert.equal(obscuredByOpenModal(open.document, open.document.body), true);
  assert.equal(obscuredByOpenModal(shut.document, shut.document.body), false);
  assert.equal(obscuredByOpenModal(open.document, null), false);
});
