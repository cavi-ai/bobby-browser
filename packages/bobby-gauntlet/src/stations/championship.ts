import { failed, passed, seededNumber, type Difficulty, type GauntletStation, type StationResult } from "../station.js";

export interface ChampionshipState { readonly steps: readonly string[]; }
export interface ChampionshipSubmission { readonly steps?: unknown; }

export const championshipStation: GauntletStation<ChampionshipState, ChampionshipSubmission> = {
  id: "championship",
  version: "1",
  mutationVersion: "1",
  supportedDifficulties: Object.freeze(["foundation"]),
  title: "Combined multi-step submission",
  capabilities: Object.freeze(["form-fill", "click", "submission"]),
  setup(seed: string, _difficulty: Difficulty): Readonly<ChampionshipState> {
    const mutation = seededNumber(`${seed}:championship`).toString(36);
    return Object.freeze({ steps: Object.freeze([`observe-${mutation}`, `confirm-${mutation}`, `submit-${mutation}`]) });
  },
  verify(state, submission): StationResult {
    return submission !== null && typeof submission === "object" && Array.isArray(submission.steps) && submission.steps.length === state.steps.length && submission.steps.every((step, index) => step === state.steps[index])
      ? passed("championship-steps-verified", "championship:verified")
      : failed("postconditionFailed", "station", "complete-championship-steps", "championship:rejected", true);
  },
  reset(): void {},
};

