import { failed, passed, sha256Hex, type Difficulty, type GauntletStation, type StationResult } from "../station.js";

export const APPROVED_UPLOAD_NAME = "approved-upload.txt";
export const APPROVED_UPLOAD_DIGEST = sha256Hex("approved upload for Bobby\n");

export interface FileAttachmentState { readonly name: string; readonly digest: string; }
export interface FileAttachmentSubmission { readonly name?: unknown; readonly digest?: unknown; }

export const fileAttachmentStation: GauntletStation<FileAttachmentState, FileAttachmentSubmission> = {
  id: "file-attachment",
  version: "1",
  mutationVersion: "1",
  supportedDifficulties: Object.freeze(["foundation"]),
  title: "Approved file attachment",
  capabilities: Object.freeze(["file-upload"]),
  setup(_seed: string, _difficulty: Difficulty): Readonly<FileAttachmentState> {
    return Object.freeze({ name: APPROVED_UPLOAD_NAME, digest: APPROVED_UPLOAD_DIGEST });
  },
  verify(state, submission): StationResult {
    return submission !== null && typeof submission === "object" && submission.name === state.name && submission.digest === state.digest
      ? passed("approved-file-bytes-verified", "file-attachment:verified")
      : failed("postconditionFailed", "station", "attach-approved-file", "file-attachment:rejected", true);
  },
  reset(): void {},
};

