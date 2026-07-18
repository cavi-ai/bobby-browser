import type { InterfaceScenarioDriver, InterfaceScenarioStep } from "./scenario.js";

export type TypeScriptSdkScenarioTransport = (request: {
  interface: "typescript-sdk";
  steps: readonly InterfaceScenarioStep[];
  implicitBoundaryReplay: false;
}) => Promise<unknown>;

export function typescriptSdkDriver(transport: TypeScriptSdkScenarioTransport): InterfaceScenarioDriver {
  return { execute: steps => transport({ interface: "typescript-sdk", steps, implicitBoundaryReplay: false }) };
}
