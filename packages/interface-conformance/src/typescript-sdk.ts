import type { InterfaceScenarioDriver, InterfaceScenarioStep } from "./scenario.js";

export type TypeScriptSdkScenarioTransport = (request: {
  interface: "typescript-sdk";
  step: InterfaceScenarioStep;
  implicitBoundaryReplay: false;
}) => Promise<unknown>;

export function typescriptSdkDriver(transport: TypeScriptSdkScenarioTransport): InterfaceScenarioDriver {
  return { execute: async steps => {
    let observation: unknown;
    for (const step of steps) observation = await transport({ interface: "typescript-sdk", step, implicitBoundaryReplay: false });
    return observation;
  } };
}
