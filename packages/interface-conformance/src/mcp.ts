import type { InterfaceScenarioDriver, InterfaceScenarioStep } from "./scenario.js";

export type McpScenarioTransport = (request: {
  jsonrpc: "2.0"; method: "tools/call"; tool: string;
  step: InterfaceScenarioStep; implicitBoundaryReplay: false;
}) => Promise<unknown>;

const TOOLS: Record<InterfaceScenarioStep, string> = {
  "runtime.info": "runtime_info", "session.create": "session_create", "page.open": "page_open",
  "command.navigate": "command_execute", "command.upload": "command_execute", "command.boundary": "command_execute",
  "artifact.verify": "resources/read", "checkpoint.save": "checkpoint_save",
  "recovery.inspect": "workflow_recover", "events.read": "events_read",
};

export function mcpDriver(transport: McpScenarioTransport): InterfaceScenarioDriver {
  return { execute: async steps => {
    let observation: unknown;
    for (const step of steps) observation = await transport({ jsonrpc: "2.0", method: "tools/call", tool: TOOLS[step], step, implicitBoundaryReplay: false });
    return observation;
  } };
}
