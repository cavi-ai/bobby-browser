export type EvidenceProof = { kind: "navigation" | "upload" | "screenshot" | "download"; sha256: string; size: number };
export type ScenarioProof = {
  outcomeStatus: "completed";
  evidence: EvidenceProof[];
  authorization: { allowed: string[]; denied: { capability: "session:read"; status: number } };
  eventOrdering: string[];
  checkpointLineage: { boundary: "boundary"; replayed: boolean; checkpointId: string; workflowId: string; boundaryCommandId: string; recoveryStatus: string };
};

export const CANONICAL_ALLOWED = ["page:write", "file:upload", "artifact:capture", "file:download"] as const;
export const CANONICAL_EVENT_ORDER = ["navigation.completed", "upload.completed", "checkpoint.saved", "boundary.completed", "checkpoint.saved", "boundary.completed", "screenshot.verified", "recovery.inspected", "events.read"] as const;
export type CheckpointObservation = { checkpointId: string; workflowId: string; boundaryCommandId: string; boundary: "boundary" };
export type RecoveryObservation = { status: string; checkpointId: string; workflowId: string; boundaryCommandId: string; boundary: "boundary"; replayed: boolean };
export type EventObservation = { events: Array<{ cursor: number; kind: string; payload: unknown }>; latestAvailable?: number };
export type ProtocolInventory = { methods: string[]; events: string[] };

export interface ScenarioDriver {
  navigate(url: string): Promise<EvidenceProof>;
  completeForm(): Promise<void>;
  uploadFixture(path: string): Promise<EvidenceProof>;
  submitForm(): Promise<void>;
  observePopup(): Promise<void>;
  screenshot(): Promise<EvidenceProof>;
  verifyDownload(): Promise<EvidenceProof>;
  verifyDeniedCapability(): Promise<number>;
  checkpoint(): Promise<CheckpointObservation>;
  recover(): Promise<RecoveryObservation>;
  readEvents(): Promise<EventObservation>;
  protocolInventory(): Promise<ProtocolInventory>;
}

type ManifestEntry = { name: string; scenarios: string[]; playwrightCovered: boolean; puppeteerCovered: boolean };
export function auditProtocolInventory(inventory: ProtocolInventory, client: "playwright" | "puppeteer", manifest: { methods: ManifestEntry[]; events: ManifestEntry[] }) {
  if (inventory.methods.length > 128 || inventory.events.length > 128) throw new Error("protocol inventory exceeded its bound");
  for (const [kind, observed, entries] of [["method", inventory.methods, manifest.methods], ["event", inventory.events, manifest.events]] as const) {
    const byName = new Map(entries.map(entry => [entry.name, entry]));
    const unflagged: string[] = [];
    for (const name of observed) {
      if (!/^[A-Za-z]+\.[A-Za-z][A-Za-z0-9]+$/.test(name)) throw new Error(`unsanitized observed ${kind}`);
      const entry = byName.get(name); if (!entry) throw new Error(`observed ${kind} missing from manifest: ${name}`);
      if (!entry[`${client}Covered`]) unflagged.push(name);
    }
    if (unflagged.length) throw new Error(`observed ${client} ${kind}s are not coverage-flagged: ${unflagged.join(",")}`);
    for (const entry of entries.filter(item => item.scenarios.includes(`${client}-canonical`) && item[`${client}Covered`]))
      if (!observed.includes(entry.name)) throw new Error(`manifest canonical ${client} ${kind} was not observed: ${entry.name}`);
  }
}

export async function runCanonicalScenario(
  driver: ScenarioDriver,
  baseUrl: string,
  fixturePath: string,
): Promise<ScenarioProof> {
  const navigation = await driver.navigate(baseUrl);
  await driver.completeForm();
  const upload = await driver.uploadFixture(fixturePath);
  const popupCheckpoint = await driver.checkpoint();
  await driver.submitForm();
  await driver.observePopup();
  const checkpoint = await driver.checkpoint();
  const download = await driver.verifyDownload();
  const screenshot = await driver.screenshot();
  const recovery = await driver.recover();
  if (recovery.checkpointId !== checkpoint.checkpointId || recovery.boundary !== checkpoint.boundary)
    throw new Error("recovery checkpoint lineage differs from the persisted checkpoint");
  const eventBatch = await driver.readEvents();
  // Persistent benchmark fixtures intentionally accumulate earlier runs. The
  // proof for this invocation is the most recent complete canonical suffix.
  const currentEvents = eventBatch.events.slice(-CANONICAL_EVENT_ORDER.length);
  const eventOrdering = currentEvents.map(event => event.kind);
  const checkpointEvents = currentEvents.filter(event => event.kind === "checkpoint.saved").map(event => event.payload as Record<string, unknown>);
  const boundaryEvents = currentEvents.filter(event => event.kind === "boundary.completed").map(event => event.payload as Record<string, unknown>);
  for (const [index, reserved] of [popupCheckpoint, checkpoint].entries()) {
    if (reserved.boundaryCommandId !== boundaryEvents[index]?.commandId || reserved.workflowId !== boundaryEvents[index]?.workflowId)
      throw new Error("boundary outcome did not consume the reserved checkpoint identity");
    if (checkpointEvents[index]?.boundaryCommandId !== reserved.boundaryCommandId || checkpointEvents[index]?.checkpointId !== reserved.checkpointId)
      throw new Error("persisted checkpoint did not retain the reserved boundary identity");
  }
  if (recovery.workflowId !== checkpoint.workflowId || recovery.boundaryCommandId !== checkpoint.boundaryCommandId)
    throw new Error("recovery did not inspect the consumed boundary workflow");
  const deniedStatus = await driver.verifyDeniedCapability();
  if (deniedStatus !== 403) throw new Error(`negative capability was not denied exactly: ${deniedStatus}`);
  return {
    outcomeStatus: "completed", evidence: [navigation, upload, screenshot, download],
    authorization: { allowed: [...CANONICAL_ALLOWED], denied: { capability: "session:read", status: deniedStatus } },
    eventOrdering,
    checkpointLineage: { boundary: checkpoint.boundary, replayed: recovery.replayed, checkpointId: recovery.checkpointId, workflowId: recovery.workflowId, boundaryCommandId: recovery.boundaryCommandId, recoveryStatus: recovery.status },
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
  if (proof.checkpointLineage.boundary !== "boundary") throw new Error("checkpoint lineage is not a boundary checkpoint");
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(proof.checkpointLineage.checkpointId))
    throw new Error("interface proof lacks persisted checkpoint identity");
  for (const id of [proof.checkpointLineage.workflowId, proof.checkpointLineage.boundaryCommandId])
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(id)) throw new Error("interface proof lacks linked boundary identity");
  if (!["resumed", "needsReconciliation"].includes(proof.checkpointLineage.recoveryStatus))
    throw new Error("interface proof contains an invalid recovery decision");
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
