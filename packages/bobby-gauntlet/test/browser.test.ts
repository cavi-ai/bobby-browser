import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { JSDOM } from "jsdom";

import { mountGauntlet } from "../src/app.js";

function mount(path: string, seed = "browser-seed") {
  const window = new JSDOM("<main id=app></main>", { url: `https://gauntlet.test${path}?seed=${seed}` }).window;
  const document = window.document;
  const root = document.querySelector<HTMLElement>("#app");
  return { root: requireElement(root, "application root", (element) => mountGauntlet(element, path, `?seed=${seed}`)), window };
}

function result(root: HTMLElement): string {
  return root.querySelector<HTMLElement>("[data-testid=result]")?.textContent ?? "";
}

function requireElement<T extends Element>(
  element: T | null,
  description: string,
  use?: (element: T) => void,
): T {
  assert.notEqual(element, null, `${description} is missing`);
  if (element === null) throw new Error(`${description} is missing`);
  use?.(element);
  return element;
}

test("route station uses a separate static redirect document instead of self-fulfilling history state", async () => {
  const { root } = mount("/station/route/");
  assert.equal(root.querySelector("pre"), null);
  assert.doesNotMatch(root.textContent ?? "", /checkpoint|browser-seed/);

  const redirect = requireElement(root.querySelector<HTMLAnchorElement>("a[data-testid=route-redirect]"), "redirect link");
  assert.equal(redirect.getAttribute("href"), "./redirect/");
  const redirectDocument = await readFile(new URL("../route-redirect.html", import.meta.url), "utf8");
  assert.match(redirectDocument, /window\.location\.replace\(`\.\.\/complete\/\?checkpoint=/);
  assert.match(redirectDocument, /http-equiv="refresh"/);
});

test("DOM drift station rejects an actionable stale target before accepting its replacement", async () => {
  const { root } = mount("/station/dom-drift/");
  const initial = requireElement(root.querySelector<HTMLButtonElement>("button[data-testid=initial-target]"), "initial target");
  await new Promise((resolve) => setTimeout(resolve, 20));

  initial.click();
  assert.match(result(root), /reobserve/i);
  const replacement = requireElement(root.querySelector<HTMLButtonElement>("button[data-testid=replacement-target]"), "replacement target");
  replacement.click();
  assert.match(result(root), /passed/i);
});

test("semantic form station exposes no fixture-specific targeting hooks", () => {
  const { root } = mount("/station/semantic-form/");
  const fullName = requireElement(root.querySelector<HTMLInputElement>("input[autocomplete='name']"), "full name input");
  const email = requireElement(root.querySelector<HTMLInputElement>("input[autocomplete='email']"), "email input");
  assert.equal(root.querySelector("[data-testid^='semantic-']"), null);
  assert.equal(root.querySelector("button[type=submit]")?.getAttribute("aria-label"), "Submit form");
  fullName.value = "Bobby";
  email.value = "bobby@example.test";
  const plan = requireElement(root.querySelector<HTMLSelectElement>("select"), "plan selector");
  plan.value = "pro";
  requireElement(root.querySelector<HTMLInputElement>("input[type='checkbox']"), "terms checkbox").checked = true;

  root.querySelector<HTMLButtonElement>("button[type=submit]")?.click();
  assert.match(result(root), /passed/i);
});

test("semantic form station mutates labels and field order across seeds", () => {
  const signatures = new Set<string>();
  for (let index = 0; index < 16; index += 1) {
    const { root } = mount("/station/semantic-form/", `semantic-mutation-${index}`);
    const form = requireElement(root.querySelector("form"), "semantic form");
    signatures.add(Array.from(form.querySelectorAll("label")).map((label) => label.textContent?.trim()).join("|"));
  }
  assert.ok(signatures.size > 1, "semantic labels and ordering must vary by seed");
});

test("validation station publishes corrective feedback and derives a correction from browser constraints", () => {
  const { root } = mount("/station/validation/");
  const accepted = requireElement(root.querySelector<HTMLInputElement>("[aria-label='Accepted reference']"), "accepted input");
  const rejected = requireElement(root.querySelector<HTMLInputElement>("[aria-label='Rejected value']"), "rejected input");
  assert.equal(rejected.dataset.testid, "validation-rejected");
  assert.equal(root.querySelector<HTMLButtonElement>("button[type=submit]")?.dataset.testid, "validation-submit");
  const acceptedBefore = accepted.value;

  root.querySelector<HTMLButtonElement>("button[type=submit]")?.click();
  const feedback = requireElement(root.querySelector<HTMLElement>("[role=alert]"), "validation feedback");
  assert.match(feedback.textContent ?? "", /five-digit/i);
  assert.equal(accepted.value, acceptedBefore);
  rejected.value = "0".repeat(rejected.minLength);
  root.querySelector<HTMLButtonElement>("button[type=submit]")?.click();
  assert.equal(rejected.checkValidity(), true);
  assert.equal(feedback.textContent, "");
  assert.match(result(root), /passed/i);
});
