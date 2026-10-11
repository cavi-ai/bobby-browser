import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { JSDOM } from "jsdom";

function page(html: string) {
  const dom = new JSDOM(html, { runScripts: "outside-only" });
  Object.defineProperty(dom.window.Element.prototype, "getClientRects", { value: () => [{ width: 20, height: 20 }] });
  Object.defineProperty(dom.window.Element.prototype, "getBoundingClientRect", { value: () => ({ width: 20, height: 20 }) });
  return dom.window;
}

type Gathered = {role:string;name:string;css:string;tag:string;attributes:Record<string,string>};
type Gather = (root:ParentNode, options:Record<string,unknown>) => { candidates:Gathered[]; truncated:boolean };
type Match = (root:ParentNode, spec:{role:string;name:string}, gather:Gather, limit?:number) => Element[] | "too-many";

function gather(window: ReturnType<typeof page>, options = {}) {
  const script = readFileSync(new URL("../../../crates/dom-engine/src/candidate_gather.js", import.meta.url), "utf8");
  const collect=window.eval("("+script+")") as Gather;
  const result=collect(window.document,{controlsOnly:true,redact:true,maxNodes:4096,maxCandidates:128,...options});
  return JSON.parse(JSON.stringify({ controls: result.candidates, truncated: result.truncated })) as { controls: Array<{role:string;name:string;css:string;tag:string;attributes:Record<string,string>}>;truncated:boolean };

}

test("shared gathering infers native roles and reads labelled-by names", () => {
  const window = page(`<span id="label">Remember me</span><input type="checkbox" aria-labelledby="label" aria-label="Fallback"><input placeholder="Search terms" type="search">`);
  const result = gather(window);
  assert.deepEqual(result.controls.map(({ role, name }) => ({ role, name })), [
    { role: "checkbox", name: "Remember me" }, { role: "searchbox", name: "Search terms" },
  ]);
});

test("shared gathering redacts sensitive labels and refuses partial bounded sets", () => {
  const secret = "secret: sample-canary-2836";
  const window = page(`<input type="password" aria-label="${secret}" value="${secret}"><button>Save</button>`);
  assert.equal(gather(window).controls[0]?.name, "[redacted]");
  assert.equal(JSON.stringify(gather(window)).includes(secret), false);
  assert.deepEqual(gather(window, { maxCandidates:1 }), { controls:[], truncated:true });
  assert.deepEqual(gather(window, { maxNodes:1 }), { controls:[], truncated:true });
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
  const gatherScript = readFileSync(new URL("../../../crates/dom-engine/src/candidate_gather.js", import.meta.url), "utf8");
  const matchScript = readFileSync(new URL("../../../crates/dom-engine/src/shadow_segment.js", import.meta.url), "utf8");
  const collect=window.eval("("+gatherScript+")") as Gather;
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
