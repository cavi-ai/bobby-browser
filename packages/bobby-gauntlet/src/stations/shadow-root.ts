import { failed, passed, seededNumber, type Difficulty, type GauntletStation, type StationResult } from "../station.js";

export interface ShadowRootState { readonly action: string; }
export interface ShadowRootSubmission { readonly action?: unknown; }

export const shadowRootStation: GauntletStation<ShadowRootState, ShadowRootSubmission> = {
  id: "shadow-root",
  version: "1",
  mutationVersion: "1",
  supportedDifficulties: Object.freeze(["foundation"]),
  title: "Shadow-root action",
  capabilities: Object.freeze(["shadow-dom", "click"]),
  setup(seed: string, _difficulty: Difficulty): Readonly<ShadowRootState> {
    return Object.freeze({ action: `shadow-${seededNumber(`${seed}:shadow-root`).toString(36)}` });
  },
  verify(state, submission): StationResult {
    return submission !== null && typeof submission === "object" && submission.action === state.action
      ? passed("shadow-root-action-verified", "shadow-root:verified")
      : failed("postconditionFailed", "station", "complete-embedded-action", "shadow-root:rejected", true);
  },
  reset(): void {},
};

