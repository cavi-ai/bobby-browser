import type { CommandOutcome } from "./contracts.js";

/** Evidence strength, independent of optional follow-up observations.
 * `conditionObserved` proves only the recorded wait condition; `settled` does
 * not prove that a server accepted a submission. Neither authorizes a retry.
 */
export type VerificationStatus = "notCompleted" | "notReported" | "conditionObserved" | "settled" | "validationRejected";

export function commandVerificationStatus(outcome: CommandOutcome): VerificationStatus {
  if (outcome.status !== "completed") return "notCompleted";
  if (outcome.evidence.some(item => item.kind === "submitSettlement" && item.outcome === "validationRejected")) return "validationRejected";
  if (outcome.evidence.some(item => item.kind === "submitSettlement" && item.outcome === "settled")) return "settled";
  if (outcome.evidence.some(item => item.kind === "wait")) return "conditionObserved";
  return "notReported";
}
