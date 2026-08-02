import { failed, passed, seededPick, type Difficulty, type GauntletStation, type StationResult } from "../station.js";

export interface SemanticFormState {
  readonly fields: Readonly<{ name: string; email: string; plan: string }>;
  readonly labels: Readonly<{ name: string; email: string; plan: string; terms: string }>;
  readonly order: readonly ("name" | "email" | "plan" | "terms")[];
}

export interface SemanticFormSubmission {
  readonly values?: unknown;
}

const FIELD_SETS = [
  { name: "full-name", email: "email-address", plan: "membership-plan" },
  { name: "contact-name", email: "contact-email", plan: "service-plan" },
] as const;

const PRESENTATIONS = [
  {
    labels: { name: "Full name", email: "Email address", plan: "Plan", terms: "Accept terms" },
    order: ["name", "email", "plan", "terms"],
  },
  {
    labels: { name: "Applicant name", email: "Contact email", plan: "Membership level", terms: "Agree to terms" },
    order: ["email", "plan", "terms", "name"],
  },
] as const;

export const semanticFormStation: GauntletStation<SemanticFormState, SemanticFormSubmission> = {
  id: "semantic-form",
  version: "1",
  mutationVersion: "2",
  supportedDifficulties: Object.freeze(["foundation"]),
  title: "Semantic multi-field form",
  capabilities: Object.freeze(["form-fill"]),
  setup(seed: string, _difficulty: Difficulty): Readonly<SemanticFormState> {
    const presentation = seededPick(`${seed}:semantic-form:presentation`, PRESENTATIONS);
    return Object.freeze({
      fields: Object.freeze({ ...seededPick(`${seed}:semantic-form`, FIELD_SETS) }),
      labels: Object.freeze({ ...presentation.labels }),
      order: Object.freeze([...presentation.order]),
    });
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
