import assert from "node:assert/strict";
import { test } from "node:test";
import { auditProtocolInventory, normalizeCanonicalProof } from "../src/scenario.js";

const uuid = "00000000-0000-4000-8000-000000000001";
const proof = {
  outcomeStatus: "completed", evidence: ["navigation", "upload", "screenshot", "download"].map(kind => ({ kind, sha256: "a".repeat(64), size: 1 })),
  authorization: { allowed: ["page:write", "file:upload", "artifact:capture", "file:download"], denied: { capability: "session:read", status: 403 } },
  eventOrdering: ["navigation.completed", "upload.completed", "checkpoint.saved", "boundary.completed", "checkpoint.saved", "boundary.completed", "screenshot.verified", "recovery.inspected", "events.read"],
  checkpointLineage: { boundary: "boundary", replayed: false, checkpointId: uuid, workflowId: uuid, boundaryCommandId: uuid, recoveryStatus: "needsReconciliation" },
};

test("negative proof corpus fails closed at every trust-bearing field", () => {
  assert.equal(normalizeCanonicalProof(structuredClone(proof)).outcomeStatus, "completed");
  const attacks: Array<(value: any) => void> = [
    value => { value.outcomeStatus = "accepted"; },
    value => { value.evidence[0].sha256 = "../secret"; },
    value => { value.evidence[0].size = Number.MAX_SAFE_INTEGER + 1; },
    value => { value.authorization.allowed.push("javascript:evaluate"); },
    value => { value.authorization.denied.status = 200; },
    value => { value.eventOrdering.reverse(); },
    value => { value.checkpointLineage.replayed = true; },
    value => { value.checkpointLineage.checkpointId = "../../checkpoint"; },
    value => { value.checkpointLineage.recoveryStatus = "restarted"; },
  ];
  for (const attack of attacks) { const candidate = structuredClone(proof); attack(candidate); assert.throws(() => normalizeCanonicalProof(candidate)); }
});

test("protocol inventory rejects unknown, oversized, and malicious method observations", () => {
  const manifest = { methods: [{ name: "Target.getTargets", scenarios: ["playwright-canonical"], playwrightCovered: true, puppeteerCovered: true }], events: [] };
  auditProtocolInventory({ methods: ["Target.getTargets"], events: [] }, "playwright", manifest);
  for (const methods of [["Runtime.evaluate"], ["Target.getTargets\nAuthorization.secret"], Array(129).fill("Target.getTargets")])
    assert.throws(() => auditProtocolInventory({ methods, events: [] }, "playwright", manifest));
});
