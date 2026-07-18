export type ScenarioProof = {
  submitted: boolean;
  popupObserved: boolean;
  downloadVerified: boolean;
};

export interface ScenarioDriver {
  navigate(url: string): Promise<void>;
  completeForm(): Promise<void>;
  uploadFixture(path: string): Promise<void>;
  submitForm(): Promise<void>;
  observePopup(): Promise<void>;
  screenshot(): Promise<void>;
  verifyDownload(): Promise<void>;
}

export async function runCanonicalScenario(
  driver: ScenarioDriver,
  baseUrl: string,
  fixturePath: string,
): Promise<ScenarioProof> {
  await driver.navigate(baseUrl);
  await driver.completeForm();
  await driver.uploadFixture(fixturePath);
  await driver.submitForm();
  await driver.observePopup();
  await driver.screenshot();
  await driver.verifyDownload();
  return { submitted: true, popupObserved: true, downloadVerified: true };
}

export const CANONICAL_INTERFACE_STEPS = [
  "runtime.info", "session.create", "page.open", "command.navigate", "command.upload",
  "command.boundary", "artifact.verify", "checkpoint.save", "recovery.inspect", "events.read",
] as const;

export type InterfaceScenarioStep = typeof CANONICAL_INTERFACE_STEPS[number];

export type CanonicalInterfaceProof = {
  outcomeStatus: "completed";
  evidence: ReadonlyArray<{ kind: "navigation" | "upload" | "screenshot" | "download"; sha256: string }>;
  authorization: ReadonlyArray<"allow:session:write" | "allow:page:write" | "allow:artifact:capture" | "deny:javascript:evaluate">;
  eventOrdering: readonly ["command.accepted", "command.completed", "checkpoint.saved"];
  checkpointLineage: readonly ["checkpoint-1", "attempt-1", "boundary-1"];
  implicitBoundaryReplay: false;
};

export interface InterfaceScenarioDriver {
  execute(steps: readonly InterfaceScenarioStep[]): Promise<unknown>;
}

const HASHES = {
  navigation: "a".repeat(64), upload: "b".repeat(64), screenshot: "c".repeat(64), download: "d".repeat(64),
} as const;

export const expectedCanonicalInterfaceProof: CanonicalInterfaceProof = {
  outcomeStatus: "completed",
  evidence: [
    { kind: "navigation", sha256: HASHES.navigation }, { kind: "upload", sha256: HASHES.upload },
    { kind: "screenshot", sha256: HASHES.screenshot }, { kind: "download", sha256: HASHES.download },
  ],
  authorization: ["allow:session:write", "allow:page:write", "allow:artifact:capture", "deny:javascript:evaluate"],
  eventOrdering: ["command.accepted", "command.completed", "checkpoint.saved"],
  checkpointLineage: ["checkpoint-1", "attempt-1", "boundary-1"],
  implicitBoundaryReplay: false,
};

export async function runCanonicalInterfaceScenario(driver: InterfaceScenarioDriver): Promise<CanonicalInterfaceProof> {
  return normalizeCanonicalProof(await driver.execute(CANONICAL_INTERFACE_STEPS));
}

export function normalizeCanonicalProof(value: unknown): CanonicalInterfaceProof {
  const encoded = JSON.stringify(value);
  const expected = JSON.stringify(expectedCanonicalInterfaceProof);
  if (encoded !== expected) throw new Error("interface proof did not match the canonical behavioral contract");
  return value as CanonicalInterfaceProof;
}

export const NEGATIVE_CAPABILITY_MATRIX = [
  ["runtime.info", "session:read"], ["session.create", "session:write"], ["page.open", "page:write"],
  ["command.navigate", "page:write"], ["command.upload", "file:upload"], ["command.boundary", "page:write"],
  ["artifact.verify", "artifact:read"], ["checkpoint.save", "recovery:write"],
  ["recovery.inspect", "recovery:read"], ["events.read", "session:read"],
] as const satisfies ReadonlyArray<readonly [InterfaceScenarioStep, string]>;
