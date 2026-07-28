import { CHAMPIONSHIP_COURSE_VERSION, CHAMPIONSHIP_STATION_MANIFEST, createManifest, type GauntletManifest } from "./manifest.js";
import { normalizeStationResult, sha256Hex, type EvidenceRef, type GauntletFailure, type StationResult } from "./station.js";

export interface SkillVersion { readonly id: string; readonly version: string; }

export interface ChampionshipTelemetry {
  readonly engine: string;
  readonly activeSkills: readonly SkillVersion[];
  readonly recoveryCount: number;
  readonly strategyChanges: readonly string[];
  readonly durationMs: number;
}

export interface VerifiedStationResult {
  readonly id: string;
  readonly version: string;
  readonly mutationVersion: string;
  readonly manifestDigest: string;
  readonly result: StationResult;
}

export interface StationScore {
  readonly id: string;
  readonly version: string;
  readonly mutationVersion: string;
  readonly passed: boolean;
  readonly postconditions?: readonly string[];
  readonly failure?: GauntletFailure;
  readonly evidence: readonly EvidenceRef[];
}

export interface GauntletScorecard {
  readonly manifest: Readonly<GauntletManifest>;
  readonly manifestDigest: string;
  readonly results: Readonly<Record<string, StationResult>>;
  readonly passed: boolean;
  readonly stations: readonly StationScore[];
  readonly recoveryCount: number;
  readonly strategyChanges: readonly string[];
  readonly engine: string;
  readonly activeSkills: readonly SkillVersion[];
  readonly durationMs: number;
  readonly evidence: readonly EvidenceRef[];
  readonly terminalFailure?: GauntletFailure;
}

const IDENTIFIER = /^[a-z][a-z0-9._:-]{0,95}$/i;
const VERSION = /^[a-z0-9][a-z0-9._:-]{0,95}$/i;

export function manifestDigest(manifest: Readonly<GauntletManifest>): string {
  return sha256Hex(JSON.stringify({
    courseVersion: manifest.courseVersion,
    seed: manifest.seed,
    difficulty: manifest.difficulty,
    stations: manifest.stations.map((station) => ({ id: station.id, version: station.version, mutationVersion: station.mutationVersion, capabilities: [...station.capabilities] })),
  }));
}

/** Builds the only complete championship scorecard shape. Inputs are bound to the exact immutable manifest. */
export function finalizeScorecard(
  manifest: Readonly<GauntletManifest>,
  results: readonly VerifiedStationResult[],
  telemetry: ChampionshipTelemetry,
): Readonly<GauntletScorecard> {
  const validatedManifest = validateManifest(manifest);
  assertChampionshipCourse(validatedManifest);
  const digest = manifestDigest(validatedManifest);
  const byId = new Map<string, VerifiedStationResult>();
  if (!Array.isArray(results)) throw new Error("scorecard results must be an array of controller-verified entries");
  for (const entry of results) {
    if (entry === null || typeof entry !== "object" || Array.isArray(entry)) throw new Error("scorecard result is malformed");
    if (!IDENTIFIER.test(entry.id)) throw new Error("scorecard result id is malformed");
    if (byId.has(entry.id)) throw new Error("scorecard contains duplicate station results");
    if (entry.manifestDigest !== digest) throw new Error("scorecard result does not bind the exact manifest");
    byId.set(entry.id, entry);
  }
  const stations = validatedManifest.stations.map((descriptor) => {
    const entry = byId.get(descriptor.id);
    if (entry === undefined) throw new Error(`scorecard is missing mandatory station result: ${descriptor.id}`);
    if (entry.version !== descriptor.version || entry.mutationVersion !== descriptor.mutationVersion) {
      throw new Error(`scorecard station version does not match manifest: ${descriptor.id}`);
    }
    const result = canonicalResult(entry.result);
    return result.passed
      ? { id: descriptor.id, version: descriptor.version, mutationVersion: descriptor.mutationVersion, passed: true, postconditions: result.postconditions, evidence: result.evidence }
      : { id: descriptor.id, version: descriptor.version, mutationVersion: descriptor.mutationVersion, passed: false, failure: result.failure, evidence: result.evidence };
  });
  for (const id of byId.keys()) {
    if (!validatedManifest.stations.some((station) => station.id === id)) throw new Error(`scorecard contains unknown station result: ${id}`);
  }
  const normalizedTelemetry = normalizeTelemetry(telemetry);
  const scorecardResults = Object.fromEntries(stations.map((station) => [station.id, station.passed
    ? { passed: true, postconditions: station.postconditions ?? [], evidence: station.evidence }
    : { passed: false, failure: station.failure!, evidence: station.evidence },
  ])) as Record<string, StationResult>;
  const terminalFailure = stations.find((station) => !station.passed)?.failure;
  return deepFreeze({
    manifest: validatedManifest,
    manifestDigest: digest,
    results: scorecardResults,
    passed: terminalFailure === undefined,
    stations,
    recoveryCount: normalizedTelemetry.recoveryCount,
    strategyChanges: normalizedTelemetry.strategyChanges,
    engine: normalizedTelemetry.engine,
    activeSkills: normalizedTelemetry.activeSkills,
    durationMs: normalizedTelemetry.durationMs,
    evidence: stations.flatMap((station) => station.evidence).sort((a, b) => a.id.localeCompare(b.id)),
    ...(terminalFailure === undefined ? {} : { terminalFailure }),
  });
}

function validateManifest(value: Readonly<GauntletManifest>): Readonly<GauntletManifest> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error("scorecard manifest is malformed");
  const manifest = value as GauntletManifest;
  try {
    return createManifest(manifest.courseVersion, manifest.seed, manifest.difficulty, manifest.stations);
  } catch {
    throw new Error("scorecard manifest is invalid");
  }
}

function assertChampionshipCourse(manifest: Readonly<GauntletManifest>): void {
  if (manifest.courseVersion !== CHAMPIONSHIP_COURSE_VERSION || manifest.difficulty !== "foundation" || !sameStationManifest(manifest.stations, CHAMPIONSHIP_STATION_MANIFEST)) {
    throw new Error("scorecard manifest does not match the immutable championship course");
  }
}

function sameStationManifest(actual: readonly { id: string; version: string; mutationVersion: string; capabilities: readonly string[] }[], expected: readonly { id: string; version: string; mutationVersion: string; capabilities: readonly string[] }[]): boolean {
  return actual.length === expected.length && actual.every((station, index) => station.id === expected[index]?.id && station.version === expected[index]?.version && station.mutationVersion === expected[index]?.mutationVersion && station.capabilities.length === expected[index]?.capabilities.length && station.capabilities.every((capability, capabilityIndex) => capability === expected[index]?.capabilities[capabilityIndex]));
}

function canonicalResult(value: unknown): StationResult {
  const result = normalizeStationResult(value);
  const evidence = [...result.evidence].sort((a, b) => a.id.localeCompare(b.id));
  return result.passed
    ? { passed: true, postconditions: [...result.postconditions].sort(), evidence }
    : { passed: false, failure: result.failure, evidence };
}

function normalizeTelemetry(value: ChampionshipTelemetry): ChampionshipTelemetry {
  if (value === null || typeof value !== "object" || !IDENTIFIER.test(value.engine) || !Number.isSafeInteger(value.recoveryCount) || value.recoveryCount < 0 || !Number.isSafeInteger(value.durationMs) || value.durationMs < 0 || !Array.isArray(value.strategyChanges) || !Array.isArray(value.activeSkills)) {
    throw new Error("championship telemetry is malformed");
  }
  const strategyChanges = value.strategyChanges.map((item) => {
    if (typeof item !== "string" || !IDENTIFIER.test(item)) throw new Error("championship telemetry strategy is malformed");
    return item;
  }).sort();
  const activeSkills = value.activeSkills.map((skill) => {
    if (skill === null || typeof skill !== "object" || !IDENTIFIER.test(skill.id) || !VERSION.test(skill.version)) throw new Error("championship skill telemetry is malformed");
    return { id: skill.id, version: skill.version };
  }).sort((a, b) => `${a.id}:${a.version}`.localeCompare(`${b.id}:${b.version}`));
  return { engine: value.engine, activeSkills, recoveryCount: value.recoveryCount, strategyChanges, durationMs: value.durationMs };
}

function deepFreeze<T>(value: T): Readonly<T> {
  if (value !== null && typeof value === "object" && !Object.isFrozen(value)) {
    for (const child of Object.values(value)) deepFreeze(child);
    Object.freeze(value);
  }
  return value;
}

