import assert from "node:assert/strict";
import test from "node:test";

import { JSDOM } from "jsdom";

import { NorthstarApi } from "../src/api.js";
import { mountNorthstar } from "../src/app.js";

function response(body: unknown, status = 200): Response {
  return Response.json(body, { status });
}

async function eventually<T extends Element>(document: Document, selector: string): Promise<T> {
  const deadline = Date.now() + 1_000;
  while (Date.now() < deadline) {
    const element = document.querySelector<T>(selector);
    if (element !== null) return element;
    await new Promise<void>((resolve) => setTimeout(resolve, 1));
  }
  throw new Error(`Timed out waiting for ${selector}`);
}

test("customer search replaces loading content and a saved priority remains visible", async () => {
  let priority = "normal";
  const fetcher: typeof fetch = async (input, init) => {
    const request = new Request(input, init);
    const url = new URL(request.url);
    if (url.pathname === "/api/customers" && url.searchParams.get("q") === "Atlas") {
      return response([{ id: "cus_atlas", name: "Atlas Labs", email: "ops@atlas.example", priority, status: "active" }]);
    }
    if (url.pathname === "/api/customers/cus_atlas/priority") {
      priority = String((await request.json() as { priority: string }).priority);
      return response({ id: "cus_atlas", name: "Atlas Labs", email: "ops@atlas.example", company: "Atlas Labs", joinedAt: "2026-01-15", priority, status: "active" });
    }
    if (url.pathname === "/api/customers/cus_atlas") {
      return response({ id: "cus_atlas", name: "Atlas Labs", email: "ops@atlas.example", company: "Atlas Labs", joinedAt: "2026-01-15", priority, status: "active" });
    }
    return response({ code: "not_found", message: "Not found" }, 404);
  };
  const window = new JSDOM("<div id='app'></div>", { url: "https://northstar.test/customers" }).window;
  Object.defineProperty(globalThis, "window", { configurable: true, value: window });
  Object.defineProperty(globalThis, "document", { configurable: true, value: window.document });
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.ok(root);
  const app = mountNorthstar(root, new NorthstarApi("run-customer", fetcher));
  await app.navigate("/customers");

  const input = await eventually<HTMLInputElement>(window.document, "input[aria-label='Search customers']");
  input.value = "Atlas";
  window.document.querySelector<HTMLFormElement>("form[aria-label='Customer search']")?.requestSubmit();
  assert.ok(window.document.querySelector("[aria-busy='true']"));
  const customer = await eventually<HTMLAnchorElement>(window.document, "a[href='/customers/cus_atlas']");
  assert.equal(customer.textContent, "Atlas Labs");
  assert.equal(window.document.querySelector("[aria-busy='true']"), null);

  customer.click();
  const prioritySelect = await eventually<HTMLSelectElement>(window.document, "select[aria-label='Customer priority']");
  prioritySelect.value = "high";
  window.document.querySelector<HTMLFormElement>("form[aria-label='Update customer priority']")?.requestSubmit();
  await eventually(window.document, "[role='status']");
  assert.match(root.textContent ?? "", /Priority saved/i);
  assert.equal(priority, "high");
});
