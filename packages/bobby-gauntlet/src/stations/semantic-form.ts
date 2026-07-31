import { failed, passed, seededPick, type Difficulty, type GauntletStation, type StationResult } from "../station.js";

export interface SemanticFormState {
  readonly fields: Readonly<{ name: string; email: string; plan: string }>;
}

export interface SemanticFormSubmission {
  readonly values?: unknown;
}

const FIELD_SETS = [
  { name: "full-name", email: "email-address", plan: "membership-plan" },
  { name: "contact-name", email: "contact-email", plan: "service-plan" },
] as const;

export const semanticFormStation: GauntletStation<SemanticFormState, SemanticFormSubmission> = {
  id: "semantic-form",
  version: "1",
  mutationVersion: "1",
  supportedDifficulties: Object.freeze(["foundation"]),
  title: "Semantic multi-field form",
  capabilities: Object.freeze(["form-fill"]),
  setup(seed: string, _difficulty: Difficulty): Readonly<SemanticFormState> {
    return Object.freeze({ fields: Object.freeze({ ...seededPick(`${seed}:semantic-form`, FIELD_SETS) }) });
  },
  verify(state: Readonly<SemanticFormState>, submission: SemanticFormSubmission): StationResult {
    const values = submission.values;
    if (values === null || typeof values !== "object" || Array.isArray(values)) {
      return failed("postconditionFailed", "station", "complete-semantic-fields", "semantic-form:missing-values", true);
    }
    const form = values as Record<string, unknown>;
    const name = form[state.fields.name];
    const email = form[state.fields.email];
    const plan = form[state.fields.plan];
    const acceptedTerms = form["accept-terms"];
    if (typeof name === "string" && name.trim().length > 0 && typeof email === "string" && /^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email) && plan === "pro" && acceptedTerms === true) {
      return passed("semantic-fields-verified", "semantic-form:verified");
    }
    return failed("postconditionFailed", "station", "complete-semantic-fields", "semantic-form:invalid", true);
  },
  reset(): void {},
};
