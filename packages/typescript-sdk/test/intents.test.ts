import assert from "node:assert/strict";
import test from "node:test";

import { DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS, MAX_INTENT_PURPOSE_BYTES } from "../src/contracts.js";
import {
  assertIntentPurpose,
  dismissObstructionEnvelope,
  dismissObstructionRuntimeCommand,
  extractEnvelope,
  extractRuntimeCommand,
  fillEnvelope,
  followEnvelope,
  followRuntimeCommand,
  locateEnvelope,
  locateRuntimeCommand,
  submitAndVerifyEnvelope,
  waitForStateEnvelope,
} from "../src/intents.js";

const META = {
  commandId: "00000000-0000-4000-8000-000000000001",
  workflowId: "00000000-0000-4000-8000-000000000002",
  attemptId: "00000000-0000-4000-8000-000000000003",
  sessionId: "00000000-0000-4000-8000-000000000004",
  pageId: "00000000-0000-4000-8000-000000000005",
  deadline: "2026-07-16T12:00:00Z",
};

test("locateRuntimeCommand matches Rust golden nested wire shape", () => {
  const command = locateRuntimeCommand({ purpose: "Continue" });
  assert.deepEqual(command, {
    kind: "intent",
    input: {
      kind: "locate",
      input: {
        purpose: "Continue",
        hints: {
          role: null,
          nearText: null,
          framePath: [],
          shadowPath: [],
          allowBestMatch: false,
        },
      },
    },
  });
});

test("locateEnvelope builds a full CommandEnvelope for agents", () => {
  const envelope = locateEnvelope(META, "Continue");
  assert.equal(envelope.schemaVersion, 2);
  assert.equal(envelope.commandId, META.commandId);
  assert.equal(envelope.pageId, META.pageId);
  assert.deepEqual(envelope.command, locateRuntimeCommand({ purpose: "Continue" }));
});

test("fill / submitAndVerify / waitForState helpers nest correctly", () => {
  const fill = fillEnvelope(META, "Email", { kind: "text", text: "a@b.co" });
  assert.deepEqual(fill.command, {
    kind: "intent",
    input: {
      kind: "fill",
      input: {
        purpose: "Email",
        hints: {
          role: null,
          nearText: null,
          framePath: [],
          shadowPath: [],
          allowBestMatch: false,
        },
        value: { kind: "text", text: "a@b.co", clearFirst: false },
      },
    },
  });

  const submit = submitAndVerifyEnvelope(META, "Submit application", {
    condition: { kind: "url", matcher: { kind: "contains", value: "/thanks" } },
    timeoutMs: 5_000,
  });
  assert.deepEqual(submit.command.kind, "intent");
  if (submit.command.kind !== "intent") throw new Error("expected intent");
  assert.equal(submit.command.input.kind, "submitAndVerify");

  const wait = waitForStateEnvelope(META, { kind: "document", ready: "interactive" }, 5_000);
  assert.deepEqual(wait.command, {
    kind: "intent",
    input: {
      kind: "waitForState",
      input: {
        condition: { kind: "document", ready: "interactive" },
        timeoutMs: 5_000,
      },
    },
  });
});

test("followRuntimeCommand matches Rust golden nested wire shape", () => {
  const command = followRuntimeCommand({
    purpose: "Details",
    expectedDestination: {
      condition: { kind: "url", matcher: { kind: "contains", value: "/details" } },
      timeoutMs: 5_000,
    },
  });
  assert.deepEqual(command, {
    kind: "intent",
    input: {
      kind: "follow",
      input: {
        purpose: "Details",
        hints: {
          role: null,
          nearText: null,
          framePath: [],
          shadowPath: [],
          allowBestMatch: false,
        },
        expectedDestination: {
          condition: { kind: "url", matcher: { kind: "contains", value: "/details" } },
          timeoutMs: 5_000,
        },
        boundary: false,
      },
    },
  });
});

test("followEnvelope forwards boundary:true verbatim", () => {
  const envelope = followEnvelope(
    META,
    "Sign out",
    { condition: { kind: "url", matcher: { kind: "contains", value: "/signed-out" } }, timeoutMs: 5_000 },
    { boundary: true },
  );
  assert.deepEqual(envelope.command, {
    kind: "intent",
    input: {
      kind: "follow",
      input: {
        purpose: "Sign out",
        hints: {
          role: null,
          nearText: null,
          framePath: [],
          shadowPath: [],
          allowBestMatch: false,
        },
        expectedDestination: {
          condition: { kind: "url", matcher: { kind: "contains", value: "/signed-out" } },
          timeoutMs: 5_000,
        },
        boundary: true,
      },
    },
  });
});

test("dismissObstructionRuntimeCommand matches Rust golden nested wire shape", () => {
  const command = dismissObstructionRuntimeCommand({ purpose: "Cookie notice close button" });
  assert.deepEqual(command, {
    kind: "intent",
    input: {
      kind: "dismissObstruction",
      input: {
        purpose: "Cookie notice close button",
        hints: {
          role: null,
          nearText: null,
          framePath: [],
          shadowPath: [],
          allowBestMatch: false,
        },
        timeoutMs: DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS,
      },
    },
  });
});

test("dismissObstructionEnvelope forwards an explicit timeoutMs verbatim", () => {
  const envelope = dismissObstructionEnvelope(META, "Cookie notice close button", {
    timeoutMs: 3_000,
  });
  assert.deepEqual(envelope.command, {
    kind: "intent",
    input: {
      kind: "dismissObstruction",
      input: {
        purpose: "Cookie notice close button",
        hints: {
          role: null,
          nearText: null,
          framePath: [],
          shadowPath: [],
          allowBestMatch: false,
        },
        timeoutMs: 3_000,
      },
    },
  });
});

test("extractRuntimeCommand matches Rust golden nested wire shape", () => {
  const command = extractRuntimeCommand({
    purpose: "Profile summary",
    fields: [
      { name: "displayName", purpose: "Display name", value: { kind: "text" } },
      { name: "profileLink", purpose: "Profile link", value: { kind: "href" } },
    ],
  });
  assert.deepEqual(command, {
    kind: "intent",
    input: {
      kind: "extract",
      input: {
        purpose: "Profile summary",
        fields: [
          {
            name: "displayName",
            purpose: "Display name",
            hints: {
              role: null,
              nearText: null,
              framePath: [],
              shadowPath: [],
              allowBestMatch: false,
            },
            value: { kind: "text" },
          },
          {
            name: "profileLink",
            purpose: "Profile link",
            hints: {
              role: null,
              nearText: null,
              framePath: [],
              shadowPath: [],
              allowBestMatch: false,
            },
            value: { kind: "href" },
          },
        ],
      },
    },
  });
});

test("extractRuntimeCommand rejects an empty field list", () => {
  assert.throws(() => extractRuntimeCommand({ purpose: "Profile summary", fields: [] }), /at least one field/);
});

test("extractRuntimeCommand rejects duplicate field names", () => {
  assert.throws(
    () =>
      extractRuntimeCommand({
        purpose: "Profile summary",
        fields: [
          { name: "displayName", purpose: "Display name", value: { kind: "text" } },
          { name: "displayName", purpose: "Secondary name", value: { kind: "text" } },
        ],
      }),
    /duplicate extract field name/,
  );
});

test("extractRuntimeCommand rejects an empty field name", () => {
  assert.throws(
    () =>
      extractRuntimeCommand({
        purpose: "Profile summary",
        fields: [{ name: "   ", purpose: "Display name", value: { kind: "text" } }],
      }),
    /field name must not be empty/,
  );
});

test("extractEnvelope builds a full CommandEnvelope with an attribute field", () => {
  const envelope = extractEnvelope(META, "Profile summary", [
    { name: "userId", purpose: "User id", value: { kind: "attribute", attribute: "data-user-id" } },
  ]);
  assert.deepEqual(envelope.command, {
    kind: "intent",
    input: {
      kind: "extract",
      input: {
        purpose: "Profile summary",
        fields: [
          {
            name: "userId",
            purpose: "User id",
            hints: {
              role: null,
              nearText: null,
              framePath: [],
              shadowPath: [],
              allowBestMatch: false,
            },
            value: { kind: "attribute", attribute: "data-user-id" },
          },
        ],
      },
    },
  });
});

test("assertIntentPurpose enforces the 256-byte bound", () => {
  assert.doesNotThrow(() => assertIntentPurpose("a".repeat(MAX_INTENT_PURPOSE_BYTES)));
  assert.throws(() => assertIntentPurpose(""), /non-empty/);
  assert.throws(() => assertIntentPurpose("a".repeat(MAX_INTENT_PURPOSE_BYTES + 1)), /256/);
  assert.throws(() => locateEnvelope(META, "a".repeat(257)), /256/);
});
