import { failed, passed, seededNumber, sha256Hex, type Difficulty, type GauntletStation, type StationResult } from "../station.js";

export interface DownloadState { readonly digest: string; readonly bytes: string; }
export interface DownloadSubmission { readonly digest?: unknown; readonly downloaded?: unknown; }

export const downloadStation: GauntletStation<DownloadState, DownloadSubmission> = {
  id: "download",
  version: "1",
  mutationVersion: "1",
  supportedDifficulties: Object.freeze(["foundation"]),
  title: "Generated download",
  capabilities: Object.freeze(["download"]),
  setup(seed: string, _difficulty: Difficulty): Readonly<DownloadState> {
    const bytes = `bobby-download:${seededNumber(`${seed}:download`).toString(36)}`;
    return Object.freeze({ bytes, digest: sha256Hex(bytes) });
  },
  verify(state, submission): StationResult {
    return submission !== null && typeof submission === "object" && submission.downloaded === true && submission.digest === state.digest
      ? passed("generated-download-verified", "download:verified")
      : failed("postconditionFailed", "station", "confirm-generated-download", "download:rejected", true);
  },
  reset(): void {},
};

