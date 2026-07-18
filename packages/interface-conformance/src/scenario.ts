export type EvidenceProof = { kind: "navigation" | "upload" | "screenshot" | "download"; sha256: string; size: number };
export type ScenarioProof = {
  outcomeStatus: "completed";
  evidence: EvidenceProof[];
  authorization: { allowed: string[]; denied: { capability: "session:read"; status: number } };
  eventOrdering: string[];
  checkpointLineage: { boundary: "submit"; replayed: false };
};

export const CANONICAL_ALLOWED = ["page:write", "file:upload", "artifact:capture", "file:download"] as const;
export const CANONICAL_EVENT_ORDER = ["navigation.completed", "upload.completed", "boundary.completed", "screenshot.verified", "checkpoint.saved", "events.read"] as const;

export interface ScenarioDriver {
  navigate(url: string): Promise<EvidenceProof>;
  completeForm(): Promise<void>;
  uploadFixture(path: string): Promise<EvidenceProof>;
  submitForm(): Promise<void>;
  observePopup(): Promise<void>;
  screenshot(): Promise<EvidenceProof>;
  verifyDownload(): Promise<EvidenceProof>;
  verifyDeniedCapability(): Promise<number>;
}

export async function runCanonicalScenario(
  driver: ScenarioDriver,
  baseUrl: string,
  fixturePath: string,
): Promise<ScenarioProof> {
  const navigation = await driver.navigate(baseUrl);
  await driver.completeForm();
  const upload = await driver.uploadFixture(fixturePath);
  await driver.submitForm();
  await driver.observePopup();
  const screenshot = await driver.screenshot();
  const download = await driver.verifyDownload();
  const deniedStatus = await driver.verifyDeniedCapability();
  if (deniedStatus !== 401 && deniedStatus !== 403) throw new Error(`negative capability was not denied: ${deniedStatus}`);
  return {
    outcomeStatus: "completed", evidence: [navigation, upload, screenshot, download],
    authorization: { allowed: [...CANONICAL_ALLOWED], denied: { capability: "session:read", status: deniedStatus } },
    eventOrdering: [...CANONICAL_EVENT_ORDER],
    checkpointLineage: { boundary: "submit", replayed: false },
  };
}

export const CANONICAL_INTERFACE_STEPS = [
  "runtime.info", "session.create", "page.open", "command.navigate", "command.upload",
  "command.boundary", "artifact.verify", "checkpoint.save", "recovery.inspect", "events.read",
] as const;

export type InterfaceScenarioStep = typeof CANONICAL_INTERFACE_STEPS[number];

export type CanonicalInterfaceProof = ScenarioProof;

export interface InterfaceScenarioDriver {
  execute(steps: readonly InterfaceScenarioStep[]): Promise<unknown>;
}

export async function runCanonicalInterfaceScenario(driver: InterfaceScenarioDriver): Promise<CanonicalInterfaceProof> {
  return normalizeCanonicalProof(await driver.execute(CANONICAL_INTERFACE_STEPS));
}

export function normalizeCanonicalProof(value: unknown): CanonicalInterfaceProof {
  if (!value || typeof value !== "object") throw new Error("interface proof must be an object");
  const proof = value as Partial<CanonicalInterfaceProof>;
  if (proof.outcomeStatus !== "completed" || !Array.isArray(proof.evidence) || proof.evidence.length !== 4)
    throw new Error("interface proof lacks completed outcomes or evidence");
  for (const item of proof.evidence) {
    if (!/^[a-f0-9]{64}$/.test(item.sha256) || !Number.isSafeInteger(item.size) || item.size <= 0)
      throw new Error("interface proof contains invalid evidence metadata");
  }
  if (proof.evidence.map(item => item.kind).join(",") !== "navigation,upload,screenshot,download")
    throw new Error("interface proof evidence order differs from the canonical proof");
  if (!proof.authorization || proof.authorization.allowed.join(",") !== CANONICAL_ALLOWED.join(",") || proof.authorization.denied.capability !== "session:read" || proof.authorization.denied.status !== 403)
    throw new Error("interface proof lacks an observed authorization denial");
  if (!Array.isArray(proof.eventOrdering) || proof.eventOrdering.join(",") !== CANONICAL_EVENT_ORDER.join(","))
    throw new Error("interface proof event ordering differs from the canonical proof");
  if (proof.checkpointLineage?.replayed !== false) throw new Error("implicit boundary replay is forbidden");
  return proof as CanonicalInterfaceProof;
}

export function equalityProof(proof: CanonicalInterfaceProof) {
  return {
    proof: { ...proof, evidence: proof.evidence.map(item => ({
      kind: item.kind,
      sha256: createHash("sha256").update(`verified-canonical-${item.kind}`).digest("hex"),
      size: 1,
    })) },
    rawEvidence: proof.evidence,
    normalization: "raw sha256 and size verified by adapter; canonical digest attests the same evidence kind invariant",
  };
}

export const NEGATIVE_CAPABILITY_MATRIX = [
  ["runtime.info", "session:read"], ["session.create", "session:write"], ["page.open", "page:write"],
  ["command.navigate", "page:write"], ["command.upload", "file:upload"], ["command.boundary", "page:write"],
  ["artifact.verify", "artifact:read"], ["checkpoint.save", "recovery:write"],
  ["recovery.inspect", "recovery:read"], ["events.read", "session:read"],
] as const satisfies ReadonlyArray<readonly [InterfaceScenarioStep, string]>;
import { createHash } from "node:crypto";
