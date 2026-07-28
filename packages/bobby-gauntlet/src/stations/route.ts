import { failed, passed, seededNumber, type Difficulty, type GauntletStation, type StationResult } from "../station.js";

export interface RouteState {
  readonly canonicalUrl: string;
}

export interface RouteSubmission {
  readonly url?: unknown;
}

export const routeStation: GauntletStation<RouteState, RouteSubmission> = {
  id: "route",
  version: "1",
  mutationVersion: "1",
  supportedDifficulties: Object.freeze(["foundation"]),
  title: "Canonical navigation",
  capabilities: Object.freeze(["navigation"]),
  setup(seed: string, _difficulty: Difficulty): Readonly<RouteState> {
    const checkpoint = seededNumber(`${seed}:route`).toString(36);
    return Object.freeze({ canonicalUrl: `/station/route/complete/?checkpoint=${checkpoint}` });
  },
  verify(state: Readonly<RouteState>, submission: RouteSubmission): StationResult {
    return submission.url === state.canonicalUrl
      ? passed("canonical-route-reached", "route:canonical")
      : failed("postconditionFailed", "station", "inspect-canonical-route", "route:not-canonical", true);
  },
  reset(): void {},
};

