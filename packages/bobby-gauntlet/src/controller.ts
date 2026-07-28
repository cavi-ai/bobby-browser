import { createManifest, type GauntletManifest, type ManifestStation } from "./manifest.js";
import { finalizeScorecard, manifestDigest, type ChampionshipTelemetry, type GauntletScorecard, type StationScore, type VerifiedStationResult } from "./scorecard.js";
import { failed, normalizeStationResult, type GauntletStation, type RegisteredStation, type StationResult } from "./station.js";

type StationRecord = {
  readonly station: RegisteredStation;
  state: Readonly<object>;
};

export class GauntletController<TStates extends { [K in keyof TStates]: object }> {
  readonly manifest: Readonly<GauntletManifest>;
  private readonly stations = new Map<string, StationRecord>();
  private readonly results = new Map<string, StationResult>();
  private readonly registeredStations: readonly RegisteredStation[];

  constructor(manifest: Readonly<GauntletManifest>, registeredStations: readonly RegisteredStation[]) {
    this.manifest = manifest;
    const expected = new Set(manifest.stations.map((station) => station.id));
    if (registeredStations.length !== expected.size) {
      throw new Error("station registry must match the manifest exactly");
    }
    for (const station of registeredStations) {
      const descriptor = manifest.stations.find((candidate) => candidate.id === station.id);
      if (
        descriptor === undefined ||
        descriptor.version !== station.version ||
        descriptor.mutationVersion !== station.mutationVersion ||
        !sameItems(descriptor.capabilities, station.capabilities) ||
        !expected.delete(station.id)
      ) {
        throw new Error("station registry must match immutable manifest identifiers, versions, capabilities, and mutations");
      }
      if (!Array.isArray(station.supportedDifficulties) || !station.supportedDifficulties.includes(manifest.difficulty)) {
        throw new Error("station registry does not support manifest difficulty");
      }
      this.stations.set(station.id, {
        station,
        state: station.setup(manifest.seed, manifest.difficulty),
      });
    }
    if (expected.size !== 0) {
      throw new Error("station registry omitted a required manifest station");
    }
    this.registeredStations = Object.freeze([...registeredStations]);
  }

  stateFor<TId extends keyof TStates & string>(stationId: TId): Readonly<TStates[TId]> {
    const record = this.stations.get(stationId);
    if (record === undefined) {
      throw new Error(`unknown station: ${stationId}`);
    }
    return record.state as Readonly<TStates[TId]>;
  }

  verify(stationId: unknown, submission: unknown): StationResult {
    if (typeof stationId !== "string" || !/^[a-z][a-z0-9-]{0,63}$/.test(stationId)) {
      return failed("configurationConflict", "controller", "invalid-station-id", "controller:invalid-station");
    }
    const record = this.stations.get(stationId);
    if (record === undefined) {
      return failed("configurationConflict", "controller", "station-not-in-run", "controller:unknown-station");
    }
    try {
      const result = normalizeStationResult(record.station.verify(record.state, submission));
      this.results.set(stationId, result);
      return result;
    } catch {
      const result = failed("configurationConflict", "controller", "invalid-station-result", "controller:invalid-result");
      this.results.set(stationId, result);
      return result;
    }
  }

  reset(stationId: string): void {
    const record = this.stations.get(stationId);
    if (record === undefined) {
      throw new Error(`unknown station: ${stationId}`);
    }
    record.station.reset();
    record.state = record.station.setup(this.manifest.seed, this.manifest.difficulty);
    this.results.delete(stationId);
  }

  scorecard(): Readonly<GauntletScorecard> {
    const results = Object.fromEntries(
      this.manifest.stations.flatMap((station) => {
        const result = this.results.get(station.id);
        return result === undefined ? [] : [[station.id, normalizeStationResult(result)]];
      }),
    ) as Record<string, StationResult>;
    const required = this.manifest.stations.map((station) => results[station.id]);
    const passed = required.length === this.manifest.stations.length && required.every((result) => result?.passed === true);
    const stations: StationScore[] = [];
    for (const station of this.manifest.stations) {
      const result = results[station.id];
      if (result === undefined) continue;
      if (result.passed) stations.push({ id: station.id, version: station.version, mutationVersion: station.mutationVersion, passed: true, postconditions: result.postconditions, evidence: result.evidence });
      else stations.push({ id: station.id, version: station.version, mutationVersion: station.mutationVersion, passed: false, failure: result.failure, evidence: result.evidence });
    }
    const failedStation = stations.find((station) => !station.passed);
    return deepFreeze({ manifest: this.manifest, manifestDigest: manifestDigest(this.manifest), results, passed, stations, recoveryCount: 0, strategyChanges: [], engine: "unreported", activeSkills: [], durationMs: 0, evidence: stations.flatMap((station) => station.evidence), ...(failedStation?.failure === undefined ? {} : { terminalFailure: failedStation.failure }) });
  }

  verifiedResults(): readonly VerifiedStationResult[] {
    const digest = manifestDigest(this.manifest);
    return deepFreeze(this.manifest.stations.flatMap((station) => {
      const result = this.results.get(station.id);
      return result === undefined ? [] : [{ id: station.id, version: station.version, mutationVersion: station.mutationVersion, manifestDigest: digest, result: normalizeStationResult(result) }];
    }));
  }

  finalizeScorecard(telemetry: ChampionshipTelemetry): Readonly<GauntletScorecard> {
    return finalizeScorecard(this.manifest, this.verifiedResults(), telemetry);
  }

  withStation<S extends object, I>(station: GauntletStation<S, I>): GauntletController<Record<string, object>> {
    const descriptor: ManifestStation = { id: station.id, version: station.version, mutationVersion: station.mutationVersion, capabilities: [...station.capabilities] };
    return new GauntletController<Record<string, object>>(
      createManifest(this.manifest.courseVersion, this.manifest.seed, this.manifest.difficulty, [...this.manifest.stations, descriptor]),
      [...this.registeredStations, station as RegisteredStation],
    );
  }
}

function sameItems(expected: readonly string[], actual: readonly string[]): boolean {
  return expected.length === actual.length && expected.every((item, index) => actual[index] === item);
}

function deepFreeze<T>(value: T): Readonly<T> {
  if (value !== null && typeof value === "object" && !Object.isFrozen(value)) {
    for (const child of Object.values(value)) deepFreeze(child);
    Object.freeze(value);
  }
  return value;
}

