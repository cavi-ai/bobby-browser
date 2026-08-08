import assert from "node:assert/strict";
import test from "node:test";

import { JSDOM } from "jsdom";

import { NorthstarApi } from "../src/api.js";
import { mountNorthstar } from "../src/app.js";
import type { DocumentReceipt, ReportInput, ReportState, RunConfig } from "../src/models.js";

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

test("an older customer search response cannot replace newer results", async () => {
  let resolveSlow: ((value: Response) => void) | undefined;
  const slowResponse = new Promise<Response>((resolve) => { resolveSlow = resolve; });
  const fetcher: typeof fetch = async (input) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    if (url.pathname === "/api/customers" && url.searchParams.get("q") === "slow") return slowResponse;
    if (url.pathname === "/api/customers" && url.searchParams.get("q") === "Atlas") {
      return response([{ id: "cus_atlas", name: "Atlas Labs", email: "ops@atlas.example", priority: "normal", status: "active" }]);
    }
    return response({ code: "not_found", message: "Not found" }, 404);
  };
  const window = new JSDOM("<div id='app'></div>", { url: "https://northstar.test/customers" }).window;
  Object.defineProperty(globalThis, "window", { configurable: true, value: window });
  Object.defineProperty(globalThis, "document", { configurable: true, value: window.document });
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.ok(root);
  const app = mountNorthstar(root, new NorthstarApi("run-search-order", fetcher));
  await app.navigate("/customers");
  const input = window.document.querySelector<HTMLInputElement>("input[aria-label='Search customers']");
  const form = window.document.querySelector<HTMLFormElement>("form[aria-label='Customer search']");
  assert.ok(input);
  assert.ok(form);

  input.value = "slow";
  form.requestSubmit();
  input.value = "Atlas";
  form.requestSubmit();
  await eventually(window.document, "a[href='/customers/cus_atlas']");
  resolveSlow?.(response([{ id: "cus_old", name: "Old Result", email: "old@example.test", priority: "low", status: "active" }]));
  await new Promise<void>((resolve) => setTimeout(resolve, 0));

  assert.match(root.textContent ?? "", /Atlas Labs/);
  assert.doesNotMatch(root.textContent ?? "", /Old Result/);
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

test("Level 2 renders an irregular form and submits the real reCAPTCHA widget response", async () => {
  let body: unknown;
  const fetcher: typeof fetch = async (input, init) => {
    const request = new Request(input, init);
    body = await request.json();
    return response({ id: "onb_level_two", status: "complete" });
  };
  const config: RunConfig = {
    level: 2,
    seed: "level-two-browser",
    traps: {
      extraModal: true,
      extraPopup: true,
      reversedIdentityFields: true,
      delayedControlMs: 1,
    },
    recaptchaSiteKey: "public-site-key",
  };
  const window = new JSDOM("<div id='app'></div>", { url: "https://northstar.test/onboarding" }).window;
  Object.defineProperty(globalThis, "window", { configurable: true, value: window });
  Object.defineProperty(globalThis, "document", { configurable: true, value: window.document });
  let renderedSiteKey = "";
  Object.defineProperty(window, "grecaptcha", {
    configurable: true,
    value: {
      render(container: HTMLElement, options: { sitekey: string; callback: (token: string) => void }): number {
        renderedSiteKey = options.sitekey;
        const response = window.document.createElement("textarea");
        response.name = "g-recaptcha-response";
        response.value = "verified-widget-token";
        container.append(response);
        options.callback(response.value);
        return 7;
      },
      getResponse(widgetId: number): string {
        assert.equal(widgetId, 7);
        return "verified-widget-token";
      },
    },
  });
  let checkpoint = "";
  window.open = ((url?: string | URL) => { checkpoint = String(url); return null; }) as typeof window.open;
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.ok(root);
  const app = mountNorthstar(root, new NorthstarApi("run-level-two", fetcher), config);
  await app.navigate("/onboarding");

  const interruption = window.document.querySelector<HTMLElement>("[role='dialog'][aria-label='Workflow interruption']");
  assert.ok(interruption);
  interruption.querySelector<HTMLButtonElement>("button")?.click();
  assert.match(checkpoint, /\/level-two-checkpoint\?seed=level-two-browser/);
  assert.equal(window.document.querySelector("[role='dialog'][aria-label='Workflow interruption']"), null);
  const widget = window.document.querySelector<HTMLElement>(".g-recaptcha");
  assert.equal(widget?.dataset.sitekey, "public-site-key");
  await eventually(window.document, "textarea[name='g-recaptcha-response']");
  assert.equal(renderedSiteKey, "public-site-key");
  assert.ok(window.document.querySelector("script[src='https://www.google.com/recaptcha/api.js?render=explicit']"));
  const identityLabels = [...window.document.querySelectorAll(".form-grid > label")]
    .map((label) => label.firstChild?.textContent);
  assert.deepEqual(identityLabels.slice(0, 2), ["Work email", "Full name"]);
  await eventually(window.document, "input[aria-label='Confirm work email']");

  const values: Record<string, string> = {
    "Full name": "Maya Chen",
    "Work email": "maya@atlas.example",
    "Confirm work email": "maya@atlas.example",
    "Company name": "Atlas Labs",
    "Postal code": "02110",
  };
  for (const [label, value] of Object.entries(values)) {
    const input = window.document.querySelector<HTMLInputElement>(`input[aria-label='${label}']`);
    assert.ok(input, `${label} input`);
    input.value = value;
  }
  window.document.querySelector<HTMLFormElement>("form[aria-label='Customer onboarding']")?.requestSubmit();

  await eventually(window.document, "[role='status']");
  assert.deepEqual(body, {
    fullName: "Maya Chen",
    email: "maya@atlas.example",
    companyName: "Atlas Labs",
    postalCode: "02110",
    plan: "starter",
    billingCycle: "monthly",
    recaptchaResponse: "verified-widget-token",
  });
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

test("revisiting Integrations does not duplicate authorization refreshes", async () => {
  let connected = false;
  let stateRequests = 0;
  const fetcher: typeof fetch = async (input) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    if (url.pathname === "/api/integrations/ledger-cloud") {
      stateRequests += 1;
      return response(connected
        ? { connected: true, identity: "finance@atlas.example" }
        : { connected: false, authorizationUrl: "/authorize/ledger-cloud" });
    }
    return response({ code: "not_found", message: "Not found" }, 404);
  };
  const window = new JSDOM("<div id='app'></div>", { url: "https://northstar.test/integrations" }).window;
  Object.defineProperty(globalThis, "window", { configurable: true, value: window });
  Object.defineProperty(globalThis, "document", { configurable: true, value: window.document });
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.ok(root);
  const app = mountNorthstar(root, new NorthstarApi("run-integration-revisit", fetcher));

  await app.navigate("/integrations");
  await app.navigate("/integrations");
  assert.equal(stateRequests, 2);
  connected = true;
  window.dispatchEvent(new window.MessageEvent("message", {
    origin: "https://northstar.test",
    data: { type: "northstar.authorization.complete" },
  }));

  await eventually(window.document, "[data-connected='true']");
  assert.equal(stateRequests, 3);
});

test("a failed Integrations revisit preserves authorization refresh on the visible page", async () => {
  let connected = false;
  let failNextStateRequest = false;
  const fetcher: typeof fetch = async (input) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    if (url.pathname !== "/api/integrations/ledger-cloud") {
      return response({ code: "not_found", message: "Not found" }, 404);
    }
    if (failNextStateRequest) {
      failNextStateRequest = false;
      return response({ code: "integration_unavailable", message: "Integration unavailable." }, 503);
    }
    return response(connected
      ? { connected: true, identity: "finance@atlas.example" }
      : { connected: false, authorizationUrl: "/authorize/ledger-cloud" });
  };
  const window = new JSDOM("<div id='app'></div>", { url: "https://northstar.test/integrations" }).window;
  Object.defineProperty(globalThis, "window", { configurable: true, value: window });
  Object.defineProperty(globalThis, "document", { configurable: true, value: window.document });
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.ok(root);
  const app = mountNorthstar(root, new NorthstarApi("run-integration-failed-revisit", fetcher));
  await app.navigate("/integrations");
  failNextStateRequest = true;
  await assert.rejects(app.navigate("/integrations"), /Integration unavailable/);

  connected = true;
  window.dispatchEvent(new window.MessageEvent("message", {
    origin: "https://northstar.test",
    data: { type: "northstar.authorization.complete" },
  }));

  await eventually(window.document, "[data-connected='true']");
  assert.match(root.textContent ?? "", /Connected as finance@atlas.example/);
});

test("out-of-order Integrations revisits keep authorization refresh on the visible page", async () => {
  let requestNumber = 0;
  let connected = false;
  let resolveSlow: ((value: Response) => void) | undefined;
  let resolveFast: ((value: Response) => void) | undefined;
  const slowResponse = new Promise<Response>((resolve) => { resolveSlow = resolve; });
  const fastResponse = new Promise<Response>((resolve) => { resolveFast = resolve; });
  const fetcher: typeof fetch = async (input) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    if (url.pathname !== "/api/integrations/ledger-cloud") {
      return response({ code: "not_found", message: "Not found" }, 404);
    }
    requestNumber += 1;
    if (requestNumber === 2) return slowResponse;
    if (requestNumber === 3) return fastResponse;
    return response(connected
      ? { connected: true, identity: "finance@atlas.example" }
      : { connected: false, authorizationUrl: "/authorize/ledger-cloud" });
  };
  const window = new JSDOM("<div id='app'></div>", { url: "https://northstar.test/integrations" }).window;
  Object.defineProperty(globalThis, "window", { configurable: true, value: window });
  Object.defineProperty(globalThis, "document", { configurable: true, value: window.document });
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.ok(root);
  const app = mountNorthstar(root, new NorthstarApi("run-integration-order", fetcher));
  await app.navigate("/integrations");

  const slowNavigation = app.navigate("/integrations");
  const fastNavigation = app.navigate("/integrations");
  resolveFast?.(response({ connected: false, authorizationUrl: "/authorize/ledger-cloud" }));
  await fastNavigation;
  resolveSlow?.(response({ connected: false, authorizationUrl: "/authorize/ledger-cloud" }));
  await slowNavigation;
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

test("top-level Documents navigation renders the upload workflow", async () => {
  const window = new JSDOM("<div id='app'></div>", { url: "https://northstar.test/" }).window;
  Object.defineProperty(globalThis, "window", { configurable: true, value: window });
  Object.defineProperty(globalThis, "document", { configurable: true, value: window.document });
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.ok(root);
  const app = mountNorthstar(root, new NorthstarApi("run-documents-route", async () => response({
    activeCustomers: 48,
    pendingOnboarding: 6,
    documentsProcessed: 127,
    reportsReady: 9,
  })));

  await app.navigate("/documents");

  assert.ok(window.document.querySelector("form[aria-label='Upload customer document']"));
  assert.match(root.textContent ?? "", /Upload source material/);
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

test("priority update failure is visible and leaves the form retryable", async () => {
  let updateAttempts = 0;
  const fetcher: typeof fetch = async (input, init) => {
    const request = new Request(input, init);
    const url = new URL(request.url);
    if (url.pathname === "/api/customers/cus_atlas" && request.method === "GET") {
      return response({
        id: "cus_atlas",
        name: "Atlas Labs",
        email: "ops@atlas.example",
        company: "Atlas Labs",
        joinedAt: "2026-01-15",
        priority: "normal",
        status: "active",
      });
    }
    if (url.pathname === "/api/customers/cus_atlas/priority") {
      updateAttempts += 1;
      if (updateAttempts === 1) {
        return response({ code: "save_failed", message: "Priority service is unavailable." }, 500);
      }
      return response({
        id: "cus_atlas",
        name: "Atlas Labs",
        email: "ops@atlas.example",
        company: "Atlas Labs",
        joinedAt: "2026-01-15",
        priority: "normal",
        status: "active",
      });
    }
    return response({ code: "not_found", message: "Not found" }, 404);
  };
  const window = new JSDOM("<div id='app'></div>", { url: "https://northstar.test/customers/cus_atlas" }).window;
  Object.defineProperty(globalThis, "window", { configurable: true, value: window });
  Object.defineProperty(globalThis, "document", { configurable: true, value: window.document });
  const root = window.document.querySelector<HTMLElement>("#app");
  assert.ok(root);
  const app = mountNorthstar(root, new NorthstarApi("run-priority-error", fetcher));
  await app.navigate("/customers/cus_atlas");

  const form = window.document.querySelector<HTMLFormElement>("form[aria-label='Update customer priority']");
  const save = window.document.querySelector<HTMLButtonElement>("button[type='submit']");
  assert.ok(form);
  assert.ok(save);
  form.requestSubmit();

  const error = await eventually<HTMLElement>(window.document, ".error-panel");
  assert.match(error.textContent ?? "", /Priority service is unavailable/);
  assert.equal(save.disabled, false);

  form.requestSubmit();
  await eventually(window.document, "[role='status']");
  assert.equal(window.document.querySelector(".error-panel"), null);
  assert.equal(save.disabled, false);
});
