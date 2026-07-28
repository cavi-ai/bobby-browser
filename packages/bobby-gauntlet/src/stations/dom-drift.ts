import { failed, passed, seededNumber, type Difficulty, type GauntletStation, type StationResult } from "../station.js";

export interface DomDriftState {
  readonly initialTargetId: string;
  readonly replacementTargetId: string;
}

export interface DomDriftSubmission {
  readonly targetId?: unknown;
}

export const domDriftStation: GauntletStation<DomDriftState, DomDriftSubmission> = {
  id: "dom-drift",
  version: "1",
  mutationVersion: "1",
  supportedDifficulties: Object.freeze(["foundation"]),
  title: "Delayed and replaced DOM",
  capabilities: Object.freeze(["dom-observation"]),
  setup(seed: string, _difficulty: Difficulty): Readonly<DomDriftState> {
    const mutation = seededNumber(`${seed}:dom-drift`).toString(36);
    return Object.freeze({ initialTargetId: `stale-${mutation}`, replacementTargetId: `replacement-${mutation}` });
  },
  verify(state: Readonly<DomDriftState>, submission: DomDriftSubmission): StationResult {
    if (submission.targetId === state.replacementTargetId) {
      return passed("replacement-target-verified", "dom-drift:replacement");
    }
    return failed("targetDrift", "station", "reobserve-replacement-target", "dom-drift:stale", true);
  },
  reset(): void {},
};

