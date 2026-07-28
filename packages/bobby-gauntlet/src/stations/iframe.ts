import { failed, passed, seededNumber, type Difficulty, type GauntletStation, type StationResult } from "../station.js";

export interface IframeState { readonly action: string; }
export interface IframeSubmission { readonly action?: unknown; }

export const iframeStation: GauntletStation<IframeState, IframeSubmission> = {
  id: "iframe",
  version: "1",
  mutationVersion: "1",
  supportedDifficulties: Object.freeze(["foundation"]),
  title: "Nested frame action",
  capabilities: Object.freeze(["iframe", "click"]),
  setup(seed: string, _difficulty: Difficulty): Readonly<IframeState> {
    return Object.freeze({ action: `frame-${seededNumber(`${seed}:iframe`).toString(36)}` });
  },
  verify(state, submission): StationResult {
    return submission !== null && typeof submission === "object" && submission.action === state.action
      ? passed("iframe-action-verified", "iframe:verified")
      : failed("postconditionFailed", "station", "complete-embedded-action", "iframe:rejected", true);
  },
  reset(): void {},
};

