import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import { JSDOM, type DOMWindow } from "jsdom";

import { mountGauntlet } from "../src/app.js";

function mount(path: string) {
  const window = new JSDOM("<main id=app></main>", { url: `https://gauntlet.test${path}?seed=station-seed&difficulty=foundation` }).window;
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.notEqual(root, null);
  if (root === null) throw new Error("missing root");
  mountGauntlet(root, path, window.location.search);
  return { window, root };
}

function outcome(root: HTMLElement): string {
  return root.querySelector<HTMLElement>("[data-testid=result]")?.textContent ?? "";
}

test("iframe station requires a real nested-frame click and does not expose a checkpoint", async () => {
  const { root } = mount("/station/iframe/");
  assert.doesNotMatch(root.textContent ?? "", /station-seed|checkpoint/i);
  await new Promise((resolve) => setTimeout(resolve, 20));
  const frame = root.querySelector<HTMLIFrameElement>("iframe[data-testid=iframe-challenge]");
  assert.notEqual(frame, null);
  assert.equal(frame?.name, "bobby-iframe-challenge");
  const nestedButton = frame?.contentDocument?.querySelector<HTMLButtonElement>("button[data-testid=iframe-submit]");
  assert.notEqual(nestedButton, null);
  nestedButton?.click();
  assert.match(outcome(root), /passed/i);
});

test("shadow-root station requires the visible button inside an open shadow root", () => {
  const { root } = mount("/station/shadow-root/");
  const host = root.querySelector<HTMLElement>("[data-testid=shadow-host]");
  assert.notEqual(host?.shadowRoot, null);
  host?.shadowRoot?.querySelector<HTMLButtonElement>("button[data-testid=shadow-submit]")?.click();
  assert.match(outcome(root), /passed/i);
});

test("a verified station publishes a controller-produced scorecard receipt", async () => {
  const { root } = mount("/station/dom-drift/");
  await new Promise((resolve) => setTimeout(resolve, 20));
  root.querySelector<HTMLButtonElement>("button[data-testid=replacement-target]")?.click();
  const receipt = root.querySelector<HTMLScriptElement>("script[data-testid=station-scorecard]");
  assert.notEqual(receipt, null);
  const scorecard = JSON.parse(receipt?.textContent ?? "") as { manifestDigest: string; stations: { id: string; passed: boolean }[] };
  assert.match(scorecard.manifestDigest, /^[a-f0-9]{64}$/);
  assert.deepEqual(scorecard.stations, [{
    id: "dom-drift",
    version: "1",
    mutationVersion: "1",
    passed: true,
    postconditions: ["replacement-target-verified"],
    evidence: [{ id: "dom-drift:replacement" }],
  }]);
});

test("popup station completes through a controlled same-origin companion window", () => {
  const { window, root } = mount("/station/popup/");
  const popup = new JSDOM("<main></main>", { url: "https://gauntlet.test/companion" }).window;
  Object.defineProperty(window, "open", { value: () => popup });
  root.querySelector<HTMLButtonElement>("button[data-testid=popup-open]")?.click();
  popup.document.querySelector<HTMLButtonElement>("button")?.click();
  assert.match(outcome(root), /passed/i);
});

test("popup station accepts a writable inherited about-blank companion", () => {
  const { window, root } = mount("/station/popup/");
  const popup = new JSDOM("<main></main>", { url: "about:blank" }).window;
  Object.defineProperty(window, "open", { value: () => popup });
  root.querySelector<HTMLButtonElement>("button[data-testid=popup-open]")?.click();
  popup.document.querySelector<HTMLButtonElement>("button")?.click();
  assert.match(outcome(root), /passed/i);
});

test("popup station fails closed when the popup is blocked or cross-origin", () => {
  const blocked = mount("/station/popup/");
  Object.defineProperty(blocked.window, "open", { value: () => null });
  blocked.root.querySelector<HTMLButtonElement>("button[data-testid=popup-open]")?.click();
  assert.match(outcome(blocked.root), /popup/i);
  assert.equal(blocked.root.querySelector("[data-testid=popup-complete]"), null);

  const crossOrigin = mount("/station/popup/");
  Object.defineProperty(crossOrigin.window, "open", { value: () => ({ get document(): never { throw new DOMException("cross origin", "SecurityError"); } }) });
  crossOrigin.root.querySelector<HTMLButtonElement>("button[data-testid=popup-open]")?.click();
  assert.match(outcome(crossOrigin.root), /popup/i);
});

test("file attachment verifies bytes, not a claimed pass or filename alone", async () => {
  const { window, root } = mount("/station/file-attachment/");
  const input = root.querySelector<HTMLInputElement>("input[type=file]");
  assert.notEqual(input, null);
  const file = new window.File(["approved upload for Bobby\n"], "approved-upload.txt", { type: "text/plain" });
  Object.defineProperty(input, "files", { value: [file] });
  input?.dispatchEvent(new window.Event("change", { bubbles: true }));
  await new Promise((resolve) => setTimeout(resolve, 20));
  root.querySelector<HTMLButtonElement>("button[type=submit]")?.click();
  assert.match(outcome(root), /passed/i);
});

test("file attachment ignores a stale slow valid read after a fast tampered selection", async () => {
  const { window, root } = mount("/station/file-attachment/");
  const input = requireFileInput(root);
  const slowValid = controlledFile("approved-upload.txt");
  const fastTampered = controlledFile("approved-upload.txt");
  selectFile(window, input, slowValid.file);
  selectFile(window, input, fastTampered.file);
  fastTampered.resolve("tampered");
  await settleControlledRead();
  slowValid.resolve("approved upload for Bobby\n");
  await settleControlledRead();
  root.querySelector<HTMLButtonElement>("button[type=submit]")?.click();
  assert.match(outcome(root), /attach-approved-file/i);
});

test("file attachment ignores a stale slow tampered read after a fast valid selection", async () => {
  const { window, root } = mount("/station/file-attachment/");
  const input = requireFileInput(root);
  const slowTampered = controlledFile("approved-upload.txt");
  const fastValid = controlledFile("approved-upload.txt");
  selectFile(window, input, slowTampered.file);
  selectFile(window, input, fastValid.file);
  fastValid.resolve("approved upload for Bobby\n");
  await settleControlledRead();
  slowTampered.resolve("tampered");
  await settleControlledRead();
  root.querySelector<HTMLButtonElement>("button[type=submit]")?.click();
  assert.match(outcome(root), /passed/i);
});

test("download station keeps Confirm disabled until the reattached byte receipt is ready, then passes without reload", async () => {
  const { window, root } = mount("/station/download/");
  root.querySelector<HTMLButtonElement>("button[data-testid=download-generate]")?.click();
  const link = root.querySelector<HTMLAnchorElement>("a[download]");
  assert.notEqual(link, null);
  link?.addEventListener("click", (event) => event.preventDefault());
  link?.click();
  const downloaded = new window.File([decodeDownloadedBytes(link?.href ?? "")], "bobby-artifact.txt", { type: "text/plain" });
  const input = root.querySelector<HTMLInputElement>("input[type=file]");
  assert.notEqual(input, null);
  Object.defineProperty(input, "files", { value: [downloaded] });
  input?.dispatchEvent(new window.Event("change", { bubbles: true }));
  const confirm = root.querySelector<HTMLButtonElement>("button[data-testid=download-confirm]");
  assert.equal(confirm?.disabled, true, "automation may click immediately only after the receipt is ready");
  confirm?.click();
  assert.doesNotMatch(outcome(root), /passed/i);
  await waitFor(() => confirm?.disabled === false);
  confirm?.click();
  assert.match(outcome(root), /passed/i);
});

test("download station rejects a receipt derived from tampered downloaded bytes", async () => {
  const { window, root } = mount("/station/download/");
  root.querySelector<HTMLButtonElement>("button[data-testid=download-generate]")?.click();
  const link = root.querySelector<HTMLAnchorElement>("a[download]");
  link?.addEventListener("click", (event) => event.preventDefault());
  link?.click();
  const input = root.querySelector<HTMLInputElement>("input[type=file]");
  Object.defineProperty(input, "files", { configurable: true, value: [new window.File(["tampered"], "bobby-artifact.txt", { type: "text/plain" })] });
  input?.dispatchEvent(new window.Event("change", { bubbles: true }));
  const confirm = root.querySelector<HTMLButtonElement>("button[data-testid=download-confirm]");
  await waitFor(() => confirm?.disabled === false);
  confirm?.click();
  assert.match(outcome(root), /download/i);
  Object.defineProperty(input, "files", { configurable: true, value: [new window.File([decodeDownloadedBytes(link?.href ?? "")], "bobby-artifact.txt", { type: "text/plain" })] });
  input?.dispatchEvent(new window.Event("change", { bubbles: true }));
  assert.equal(confirm?.disabled, true);
  await waitFor(() => confirm?.disabled === false);
  confirm?.click();
  assert.match(outcome(root), /passed/i);
});

test("download ignores a stale slow valid receipt after a fast tampered selection", async () => {
  const { window, root } = mount("/station/download/");
  const { input, confirm, bytes } = openDownloadedArtifact(window, root);
  const slowValid = controlledFile("bobby-artifact.txt");
  const fastTampered = controlledFile("bobby-artifact.txt");
  selectFile(window, input, slowValid.file);
  selectFile(window, input, fastTampered.file);
  fastTampered.resolve("tampered");
  await settleControlledRead();
  slowValid.resolve(bytes);
  await settleControlledRead();
  assert.equal(confirm.disabled, false);
  confirm.click();
  assert.match(outcome(root), /download/i);
});

test("download ignores a stale slow tampered receipt after a fast valid selection", async () => {
  const { window, root } = mount("/station/download/");
  const { input, confirm, bytes } = openDownloadedArtifact(window, root);
  const slowTampered = controlledFile("bobby-artifact.txt");
  const fastValid = controlledFile("bobby-artifact.txt");
  selectFile(window, input, slowTampered.file);
  selectFile(window, input, fastValid.file);
  fastValid.resolve(bytes);
  await settleControlledRead();
  slowTampered.resolve("tampered");
  await settleControlledRead();
  assert.equal(confirm.disabled, false);
  confirm.click();
  assert.match(outcome(root), /passed/i);
});

test("championship direct route surfaces every mandatory station without leaking answers", () => {
  const { root } = mount("/championship");
  assert.equal(root.querySelectorAll("[data-station-id]").length, 10);
  assert.doesNotMatch(root.textContent ?? "", /station-seed|canonicalUrl|correctedValue/i);
});

test("championship route delegates genuine navigation to an owned child and verifies its observed final URL", async () => {
  const mounted = mount("/championship");
  const route = mounted.root.querySelector<HTMLElement>("[data-station-id=route]")!;
  const frame = route.querySelector<HTMLIFrameElement>("iframe[data-testid=route-challenge]");
  assert.notEqual(frame, null);
  assert.match(frame?.src ?? "", /\/station\/route\/\?seed=station-seed&difficulty=foundation$/);
  assert.equal(route.querySelector("a[data-testid=route-redirect]"), null, "parent must not impersonate navigation");
  assert.equal(mounted.window.location.pathname, "/championship");

  const childDom = new JSDOM("<main id=child></main>", { url: "https://gauntlet.test/station/route/?seed=station-seed&difficulty=foundation" });
  const child = childDom.window;
  Object.defineProperty(frame, "contentWindow", { configurable: true, value: child });
  const childRoot = child.document.querySelector<HTMLElement>("#child")!;
  mountGauntlet(childRoot, "/station/route/", "?seed=station-seed&difficulty=foundation");
  const redirect = childRoot.querySelector<HTMLAnchorElement>("a[data-testid=route-redirect]")!;
  const event = new mounted.window.Event("click", { bubbles: true, cancelable: true });
  redirect.dispatchEvent(event);
  assert.equal(event.defaultPrevented, false, "normal child anchor navigation must not be intercepted");
  const artifact = await readFile(new URL("../route-redirect.html", import.meta.url), "utf8");
  assert.match(artifact, /window\.location\.replace\(`\.\.\/complete\/\?checkpoint=/);
  assert.doesNotMatch(artifact, /pushState|preventDefault/);

  const initialDocument = child.document;
  child.history.replaceState(null, "", "/station/route/complete/?checkpoint=15a98k0");
  frame?.dispatchEvent(new mounted.window.Event("load"));
  assert.strictEqual(child.document, initialDocument, "test simulation retains the owned child document only because jsdom cannot navigate");
  assert.equal(route.querySelector<HTMLElement>("[data-testid=result]")?.textContent, "Passed");
  assert.equal(mounted.window.location.pathname, "/championship", "parent controller document must survive child navigation");

  const tampered = mount("/championship");
  const tamperedFrame = tampered.root.querySelector<HTMLIFrameElement>("[data-station-id=route] iframe")!;
  const tamperedChild = new JSDOM("<main></main>", { url: "https://gauntlet.test/station/route/complete/?checkpoint=tampered" }).window;
  Object.defineProperty(tamperedFrame, "contentWindow", { configurable: true, value: tamperedChild });
  tamperedFrame.dispatchEvent(new tampered.window.Event("load"));
  assert.notEqual(tampered.root.querySelector<HTMLElement>("[data-station-id=route] [data-testid=result]")?.textContent, "Passed");
});

test("championship route publishes one final app-produced aggregate only after its shared ledger passes", async () => {
  const { window, root } = mount("/championship");
  const section = (id: string) => root.querySelector<HTMLElement>(`[data-station-id='${id}']`)!;
  const click = (id: string, selector: string) => section(id).querySelector<HTMLElement>(selector)?.click();

  const routeFrame = section("route").querySelector<HTMLIFrameElement>("iframe[data-testid=route-challenge]")!;
  const routeChild = new JSDOM("<main></main>", { url: "https://gauntlet.test/station/route/complete/?checkpoint=15a98k0" }).window;
  Object.defineProperty(routeFrame, "contentWindow", { configurable: true, value: routeChild });
  routeFrame.dispatchEvent(new window.Event("load"));
  await new Promise((resolve) => setTimeout(resolve, 20));
  click("dom-drift", "[data-testid=replacement-target]");

  const semantic = section("semantic-form");
  semantic.querySelector<HTMLInputElement>("[aria-label='Full name']")!.value = "Bobby";
  semantic.querySelector<HTMLInputElement>("[aria-label='Email address']")!.value = "bobby@example.test";
  semantic.querySelector<HTMLSelectElement>("[aria-label='Plan']")!.value = "pro";
  click("semantic-form", "button[type=submit]");

  section("validation").querySelector<HTMLInputElement>("[aria-label='Rejected value']")!.value = "00000";
  click("validation", "button[type=submit]");
  await new Promise((resolve) => setTimeout(resolve, 5));
  section("iframe").querySelector<HTMLIFrameElement>("iframe")?.contentDocument?.querySelector<HTMLButtonElement>("button")?.click();
  section("shadow-root").querySelector<HTMLElement>("[data-testid=shadow-host]")?.shadowRoot?.querySelector<HTMLButtonElement>("button")?.click();

  const popup = new JSDOM("<main></main>", { url: "https://gauntlet.test/companion" }).window;
  Object.defineProperty(window, "open", { value: () => popup });
  click("popup", "[data-testid=popup-open]");
  popup.document.querySelector<HTMLButtonElement>("button")?.click();

  const upload = section("file-attachment").querySelector<HTMLInputElement>("input[type=file]")!;
  Object.defineProperty(upload, "files", { value: [new window.File(["approved upload for Bobby\n"], "approved-upload.txt")] });
  upload.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitFor(() => section("file-attachment").querySelector("[data-testid=station-scorecard]") === null);
  await new Promise((resolve) => setTimeout(resolve, 10));
  click("file-attachment", "button[type=submit]");

  click("download", "[data-testid=download-generate]");
  const link = section("download").querySelector<HTMLAnchorElement>("a[download]")!;
  link.addEventListener("click", (event) => event.preventDefault());
  link.click();
  const downloadInput = section("download").querySelector<HTMLInputElement>("input[type=file]")!;
  Object.defineProperty(downloadInput, "files", { configurable: true, value: [new window.File([decodeDownloadedBytes(link.href)], "bobby-artifact.txt")] });
  downloadInput.dispatchEvent(new window.Event("change", { bubbles: true }));
  const confirm = section("download").querySelector<HTMLButtonElement>("[data-testid=download-confirm]")!;
  await waitFor(() => !confirm.disabled);
  confirm.click();

  for (let step = 1; step <= 3; step += 1) click("championship", `[data-testid=championship-step-${step}]`);

  const receipt = root.querySelector<HTMLScriptElement>("script[data-testid=championship-scorecard]");
  assert.notEqual(receipt, null);
  const scorecard = JSON.parse(receipt?.textContent ?? "") as { passed: boolean; manifestDigest: string; stations: { id: string; passed: boolean }[] };
  assert.equal(scorecard.passed, true);
  assert.match(scorecard.manifestDigest, /^[a-f0-9]{64}$/);
  assert.deepEqual(scorecard.stations.map(({ id, passed }) => ({ id, passed })), [
    "route", "dom-drift", "semantic-form", "validation", "iframe", "shadow-root", "popup", "file-attachment", "download", "championship",
  ].map((id) => ({ id, passed: true })));
});

function decodeDownloadedBytes(href: string): string {
  const encoded = href.match(/^data:text\/plain;base64,(.+)$/)?.[1];
  assert.notEqual(encoded, undefined, "download link must expose actual generated bytes");
  return Buffer.from(encoded ?? "", "base64").toString("utf8");
}

async function waitFor(predicate: () => boolean): Promise<void> {
  for (let attempts = 0; attempts < 50; attempts += 1) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  assert.fail("receipt readiness did not settle");
}

function requireFileInput(root: HTMLElement): HTMLInputElement {
  const input = root.querySelector<HTMLInputElement>("input[type=file]");
  assert.notEqual(input, null, "file input is missing");
  if (input === null) throw new Error("file input is missing");
  return input;
}

function selectFile(window: DOMWindow, input: HTMLInputElement, file: File): void {
  Object.defineProperty(input, "files", { configurable: true, value: [file] });
  input.dispatchEvent(new window.Event("change", { bubbles: true }));
}

function controlledFile(name: string): { file: File; resolve(value: string): void } {
  let resolveText: ((value: string) => void) | undefined;
  const pending = new Promise<string>((resolve) => { resolveText = resolve; });
  return { file: { name, text: () => pending } as unknown as File, resolve: (value) => resolveText?.(value) };
}

async function settleControlledRead(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

function openDownloadedArtifact(window: DOMWindow, root: HTMLElement): { input: HTMLInputElement; confirm: HTMLButtonElement; bytes: string } {
  root.querySelector<HTMLButtonElement>("button[data-testid=download-generate]")?.click();
  const link = root.querySelector<HTMLAnchorElement>("a[download]");
  link?.addEventListener("click", (event) => event.preventDefault());
  link?.click();
  const input = requireFileInput(root);
  const confirm = root.querySelector<HTMLButtonElement>("button[data-testid=download-confirm]");
  assert.notEqual(confirm, null, "download confirmation is missing");
  if (confirm === null) throw new Error("download confirmation is missing");
  return { input, confirm, bytes: decodeDownloadedBytes(link?.href ?? "") };
}

