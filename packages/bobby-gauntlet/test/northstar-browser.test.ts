import assert from "node:assert/strict";
import test from "node:test";

import { JSDOM } from "jsdom";

import { NorthstarApi } from "../src/api.js";
import { mountNorthstar } from "../src/app.js";
import type { DocumentReceipt, ReportInput, ReportState } from "../src/models.js";

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

test("server validation preserves accepted onboarding values and focuses the rejected field", async () => {
  const fetcher: typeof fetch = async (input) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    if (url.pathname === "/api/onboarding") return response({
      code: "postal_rejected",
      message: "Review the highlighted field.",
      fields: { postalCode: "Use 10001 for this account." },
    }, 422);
    return response({ code: "not_found", message: "Not found" }, 404);
  };
  const window = new JSDOM("<div id='app'></div>", { url: "https://northstar.test/onboarding" }).window;
  Object.defineProperty(globalThis, "window", { configurable: true, value: window });
  Object.defineProperty(globalThis, "document", { configurable: true, value: window.document });
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.ok(root);
  const app = mountNorthstar(root, new NorthstarApi("run-onboarding", fetcher));
  await app.navigate("/onboarding");

  const values: Record<string, string> = {
    "Full name": "Maya Chen",
    "Work email": "maya@atlas.example",
    "Company name": "Atlas Labs",
    "Postal code": "02110",
  };
  for (const [label, value] of Object.entries(values)) {
    const input = window.document.querySelector<HTMLInputElement>(`input[aria-label='${label}']`);
    assert.ok(input, `${label} input`);
    input.value = value;
  }
  const plan = window.document.querySelector<HTMLSelectElement>("select[aria-label='Plan']");
  assert.ok(plan);
  plan.value = "growth";
  plan.dispatchEvent(new window.Event("change", { bubbles: true }));
  const billing = await eventually<HTMLSelectElement>(window.document, "select[aria-label='Billing cycle']");
  billing.value = "annual";
  window.document.querySelector<HTMLFormElement>("form[aria-label='Customer onboarding']")?.requestSubmit();

  const postal = await eventually<HTMLInputElement>(window.document, "input[aria-invalid='true']");
  assert.equal(postal.getAttribute("aria-label"), "Postal code");
  assert.equal(postal.value, "02110");
  assert.equal(window.document.querySelector<HTMLInputElement>("input[aria-label='Company name']")?.value, "Atlas Labs");
  assert.match(root.textContent ?? "", /Use 10001 for this account/);
});

test("popup authorization refreshes the connected identity after a trusted completion message", async () => {
  let connected = false;
  const fetcher: typeof fetch = async (input) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    if (url.pathname === "/api/integrations/ledger-cloud") return response(connected
      ? { connected: true, identity: "finance@atlas.example" }
      : { connected: false, authorizationUrl: "/authorize/ledger-cloud" });
    return response({ code: "not_found", message: "Not found" }, 404);
  };
  const window = new JSDOM("<div id='app'></div>", { url: "https://northstar.test/integrations" }).window;
  Object.defineProperty(globalThis, "window", { configurable: true, value: window });
  Object.defineProperty(globalThis, "document", { configurable: true, value: window.document });
  let opened = "";
  window.open = ((url?: string | URL) => { opened = String(url); return null; }) as typeof window.open;
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.ok(root);
  const app = mountNorthstar(root, new NorthstarApi("run-integration", fetcher));
  await app.navigate("/integrations");
  const connect = await eventually<HTMLButtonElement>(window.document, "button[aria-label='Connect Ledger Cloud']");
  connect.click();
  assert.match(opened, /\/authorize\/ledger-cloud/);
  connected = true;
  window.dispatchEvent(new window.MessageEvent("message", {
    origin: "https://northstar.test",
    data: { type: "northstar.authorization.complete" },
  }));
  await eventually(window.document, "[data-connected='true']");
  assert.match(root.textContent ?? "", /Connected as finance@atlas.example/);
});

test("document upload renders the server preview in a labelled frame", async () => {
  class DocumentApi extends NorthstarApi {
    override async uploadDocument(customerId: string, file: File): Promise<DocumentReceipt> {
      assert.equal(customerId, "cus_atlas");
      assert.equal(file.name, "approved-upload.txt");
      return { id: "doc_17", customerId, filename: file.name, mediaType: "text/plain", sha256: "abc123", previewUrl: "/api/documents/doc_17/preview" };
    }
  }
  const window = new JSDOM("<div id='app'></div>", { url: "https://northstar.test/customers/cus_atlas/documents" }).window;
  Object.defineProperty(globalThis, "window", { configurable: true, value: window });
  Object.defineProperty(globalThis, "document", { configurable: true, value: window.document });
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.ok(root);
  const app = mountNorthstar(root, new DocumentApi("run-documents"));
  await app.navigate("/customers/cus_atlas/documents");
  const input = await eventually<HTMLInputElement>(window.document, "input[aria-label='Customer document']");
  const file = new window.File(["approved bytes"], "approved-upload.txt", { type: "text/plain" });
  Object.defineProperty(input, "files", { configurable: true, value: { 0: file, length: 1, item: () => file } });
  window.document.querySelector<HTMLFormElement>("form[aria-label='Upload customer document']")?.requestSubmit();
  const frame = await eventually<HTMLIFrameElement>(window.document, "iframe[title='Preview of approved-upload.txt']");
  assert.equal(frame.getAttribute("src"), "/api/documents/doc_17/preview");
  assert.match(root.textContent ?? "", /Upload complete/);
});

test("completed report exposes an ordinary download link", async () => {
  class ReportApi extends NorthstarApi {
    override async createReport(input: ReportInput): Promise<ReportState> {
      assert.deepEqual(input, { customerId: "cus_atlas", format: "csv" });
      return { id: "rep_17", status: "pending" };
    }
    override async report(id: string): Promise<ReportState> {
      assert.equal(id, "rep_17");
      return { id, status: "complete", filename: "atlas-operations.csv", mediaType: "text/csv", downloadUrl: "/api/reports/rep_17/download", sha256: "def456" };
    }
  }
  const window = new JSDOM("<div id='app'></div>", { url: "https://northstar.test/reports" }).window;
  Object.defineProperty(globalThis, "window", { configurable: true, value: window });
  Object.defineProperty(globalThis, "document", { configurable: true, value: window.document });
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.ok(root);
  const app = mountNorthstar(root, new ReportApi("run-report"));
  await app.navigate("/reports");
  window.document.querySelector<HTMLFormElement>("form[aria-label='Generate report']")?.requestSubmit();
  const download = await eventually<HTMLAnchorElement>(window.document, "a[download='atlas-operations.csv']");
  assert.equal(download.getAttribute("href"), "/api/reports/rep_17/download");
  assert.match(root.textContent ?? "", /Report ready/);
});

test("customer search exposes the transport failure instead of hiding it", async () => {
  const api = new NorthstarApi("run-search-error", async () => {
    throw new TypeError("Network request could not be constructed");
  });
  const window = new JSDOM("<div id='app'></div>", { url: "https://northstar.test/customers" }).window;
  Object.defineProperty(globalThis, "window", { configurable: true, value: window });
  Object.defineProperty(globalThis, "document", { configurable: true, value: window.document });
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.ok(root);
  const app = mountNorthstar(root, api);
  await app.navigate("/customers");
  window.document.querySelector<HTMLFormElement>("form[aria-label='Customer search']")?.requestSubmit();

  await eventually(window.document, ".error-panel");

  assert.match(root.textContent ?? "", /Network request could not be constructed/);
});
