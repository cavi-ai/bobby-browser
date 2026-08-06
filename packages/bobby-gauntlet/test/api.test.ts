import assert from "node:assert/strict";
import test from "node:test";

import { ApiError, NorthstarApi } from "../src/api.js";
import type { OnboardingInput } from "../src/models.js";

const onboarding: OnboardingInput = {
  fullName: "Maya Chen",
  email: "maya@atlas.example",
  companyName: "Atlas Labs",
  postalCode: "02110",
  plan: "growth",
  billingCycle: "annual",
};

test("structured field rejection preserves status, code, and field guidance", async () => {
  const api = new NorthstarApi("run-17", async () => Response.json({
    code: "postal_rejected",
    message: "Review the highlighted field.",
    fields: { postalCode: "Use 10001 for this account." },
  }, { status: 422 }));

  await assert.rejects(api.onboard(onboarding), (error: unknown) => {
    assert.ok(error instanceof ApiError);
    assert.equal(error.status, 422);
    assert.equal(error.code, "postal_rejected");
    assert.deepEqual(error.fields, { postalCode: "Use 10001 for this account." });
    return true;
  });
});

test("every API request carries the isolated run identity", async () => {
  const requests: Request[] = [];
  const api = new NorthstarApi("run-17", async (input, init) => {
    requests.push(new Request(input, init));
    return Response.json({ id: "onb_17", status: "complete" });
  });

  const receipt = await api.onboard(onboarding);

  assert.deepEqual(receipt, { id: "onb_17", status: "complete" });
  assert.equal(requests.length, 1);
  assert.equal(requests[0]?.headers.get("x-northstar-run"), "run-17");
  assert.equal(requests[0]?.method, "POST");
  assert.equal(new URL(requests[0]?.url ?? "https://invalid.test").pathname, "/api/onboarding");
});

test("Level 2 onboarding sends the reCAPTCHA response without changing the customer record", async () => {
  const requests: Request[] = [];
  const api = new NorthstarApi("run-level-two", async (input, init) => {
    requests.push(new Request(input, init));
    return Response.json({ id: "onb_level_two", status: "complete" });
  });

  await api.onboard(onboarding, "verified-widget-token");

  assert.deepEqual(await requests[0]?.json(), {
    ...onboarding,
    recaptchaResponse: "verified-widget-token",
  });
});

test("a non-JSON failure remains a structured API error", async () => {
  const api = new NorthstarApi("run-17", async () => new Response("gateway unavailable", {
    status: 502,
    headers: { "content-type": "text/plain" },
  }));

  await assert.rejects(api.dashboard(), (error: unknown) => {
    assert.ok(error instanceof ApiError);
    assert.equal(error.status, 502);
    assert.equal(error.code, "http_error");
    assert.equal(error.message, "Request failed with status 502.");
    return true;
  });
});

test("browser API requests resolve against the active application origin", async () => {
  const requests: Request[] = [];
  Object.defineProperty(globalThis, "location", {
    configurable: true,
    value: new URL("https://workspace.example/customers"),
  });
  const api = new NorthstarApi("run-origin", async (input, init) => {
    requests.push(new Request(input, init));
    return Response.json({ activeCustomers: 1, pendingOnboarding: 0, documentsProcessed: 0, reportsReady: 0 });
  });

  await api.dashboard();

  assert.equal(new URL(requests[0]?.url ?? "https://invalid.test").origin, "https://workspace.example");
  Reflect.deleteProperty(globalThis, "location");
});

test("the injected fetch function is invoked without rebinding its receiver", async () => {
  let receiver: unknown = "not-called";
  const fetcher = function (this: unknown): Promise<Response> {
    receiver = this;
    return Promise.resolve(Response.json({ activeCustomers: 1, pendingOnboarding: 0, documentsProcessed: 0, reportsReady: 0 }));
  } as typeof fetch;
  const api = new NorthstarApi("run-fetch-receiver", fetcher);

  await api.dashboard();

  assert.equal(receiver, undefined);
});
