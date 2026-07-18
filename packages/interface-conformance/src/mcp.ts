import type { InterfaceScenarioDriver, InterfaceScenarioStep } from "./scenario.js";

export type McpScenarioTransport = (request: {
  jsonrpc: "2.0"; method: "tools/call"; tool: "runtime_conformance";
  steps: readonly InterfaceScenarioStep[]; implicitBoundaryReplay: false;
}) => Promise<unknown>;

export function mcpDriver(transport: McpScenarioTransport): InterfaceScenarioDriver {
  return { execute: steps => transport({ jsonrpc: "2.0", method: "tools/call", tool: "runtime_conformance", steps, implicitBoundaryReplay: false }) };
}
