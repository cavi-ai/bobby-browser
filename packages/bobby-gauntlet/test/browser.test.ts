import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { JSDOM } from "jsdom";

import { mountGauntlet } from "../src/app.js";

function mount(path: string) {
  const window = new JSDOM("<main id=app></main>", { url: `https://gauntlet.test${path}?seed=browser-seed` }).window;
  const document = window.document;
  const root = document.querySelector<HTMLElement>("#app");
  return { root: requireElement(root, "application root", (element) => mountGauntlet(element, path, "?seed=browser-seed")), window };
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

test("semantic form station is completed through labelled browser controls", () => {
  const { root } = mount("/station/semantic-form/");
  requireElement(root.querySelector<HTMLInputElement>("[aria-label='Full name']"), "full name input").value = "Bobby";
  requireElement(root.querySelector<HTMLInputElement>("[aria-label='Email address']"), "email input").value = "bobby@example.test";
  const plan = requireElement(root.querySelector<HTMLSelectElement>("[aria-label='Plan']"), "plan selector");
  plan.value = "pro";

  root.querySelector<HTMLButtonElement>("button[type=submit]")?.click();
  assert.match(result(root), /passed/i);
});

test("validation station publishes corrective feedback and derives a correction from browser constraints", () => {
  const { root } = mount("/station/validation/");
  const accepted = requireElement(root.querySelector<HTMLInputElement>("[aria-label='Accepted reference']"), "accepted input");
  const rejected = requireElement(root.querySelector<HTMLInputElement>("[aria-label='Rejected value']"), "rejected input");
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

