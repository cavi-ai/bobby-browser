import assert from "node:assert/strict";
import { test } from "node:test";
import { expectedCanonicalInterfaceProof, runCanonicalInterfaceScenario } from "../src/scenario.js";
import { mcpDriver } from "../src/mcp.js";

test("MCP consumes the canonical scenario through one bounded tool request", async () => {
  const proof = await runCanonicalInterfaceScenario(mcpDriver(async request => {
    assert.deepEqual([request.jsonrpc, request.method, request.tool], ["2.0", "tools/call", "runtime_conformance"]);
    assert.equal(request.implicitBoundaryReplay, false);
    return expectedCanonicalInterfaceProof;
  }));
  assert.deepEqual(proof, expectedCanonicalInterfaceProof);
});

test("MCP proof normalization rejects weakened assertions", async () => {
  await assert.rejects(runCanonicalInterfaceScenario(mcpDriver(async () => ({ ...expectedCanonicalInterfaceProof, implicitBoundaryReplay: true }))));
});
