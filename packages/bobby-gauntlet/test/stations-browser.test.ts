import assert from "node:assert/strict";
import test from "node:test";
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

