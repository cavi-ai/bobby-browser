import { failed, passed, seededPick, type Difficulty, type GauntletStation, type StationResult } from "../station.js";

export interface ValidationState {
  readonly validField: string;
  readonly validValue: string;
  readonly invalidField: string;
  readonly invalidValue: string;
  readonly correctedValue: string;
}

export interface ValidationSubmission {
  readonly values?: unknown;
}

const VARIATIONS = [
  { validField: "account-id", validValue: "acct-42", invalidField: "postal-code", invalidValue: "12", correctedValue: "02139" },
  { validField: "reference-id", validValue: "ref-73", invalidField: "security-code", invalidValue: "7", correctedValue: "02139" },
] as const;

export const validationStation: GauntletStation<ValidationState, ValidationSubmission> = {
  id: "validation",
  version: "1",
  mutationVersion: "1",
  supportedDifficulties: Object.freeze(["foundation"]),
  title: "Validation diagnosis",
  capabilities: Object.freeze(["form-fill", "validation"]),
  setup(seed: string, _difficulty: Difficulty): Readonly<ValidationState> {
    return Object.freeze({ ...seededPick(`${seed}:validation`, VARIATIONS) });
  },
  verify(state: Readonly<ValidationState>, submission: ValidationSubmission): StationResult {
    const values = submission.values;
    if (values === null || typeof values !== "object" || Array.isArray(values)) {
      return failed("postconditionFailed", "station", "correct-invalid-input", "validation:missing-values", true);
    }
    const form = values as Record<string, unknown>;
    const correction = form[state.invalidField];
    if (form[state.validField] === state.validValue && typeof correction === "string" && /^[0-9]{5}$/.test(correction)) {
      return passed("validation-corrected-with-valid-input-preserved", "validation:verified");
    }
    return failed("postconditionFailed", "station", "correct-invalid-input", "validation:rejected", true);
  },
  reset(): void {},
};

