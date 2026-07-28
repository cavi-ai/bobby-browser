import { failed, passed, seededNumber, type Difficulty, type GauntletStation, type StationResult } from "../station.js";

export interface PopupState { readonly completion: string; }
export interface PopupSubmission { readonly completion?: unknown; }

export const popupStation: GauntletStation<PopupState, PopupSubmission> = {
  id: "popup",
  version: "1",
  mutationVersion: "1",
  supportedDifficulties: Object.freeze(["foundation"]),
  title: "Popup completion",
  capabilities: Object.freeze(["popup", "click"]),
  setup(seed: string, _difficulty: Difficulty): Readonly<PopupState> {
    return Object.freeze({ completion: `popup-${seededNumber(`${seed}:popup`).toString(36)}` });
  },
  verify(state, submission): StationResult {
    return submission !== null && typeof submission === "object" && submission.completion === state.completion
      ? passed("popup-completion-verified", "popup:verified")
      : failed("postconditionFailed", "station", "complete-popup-action", "popup:rejected", true);
  },
  reset(): void {},
};

