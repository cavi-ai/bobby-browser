import { type Difficulty, FOUNDATION_DIFFICULTY } from "./station.js";

export interface ManifestStation {
  readonly id: string;
  readonly version: string;
  readonly capabilities: readonly string[];
  readonly mutationVersion: string;
}

export interface GauntletManifest {
  readonly courseVersion: string;
  readonly seed: string;
  readonly difficulty: Difficulty;
  readonly stations: readonly ManifestStation[];
}

export const FOUNDATION_STATION_MANIFEST: readonly ManifestStation[] = Object.freeze([
  Object.freeze({ id: "route", version: "1", capabilities: Object.freeze(["navigation"]), mutationVersion: "1" }),
  Object.freeze({ id: "dom-drift", version: "1", capabilities: Object.freeze(["dom-observation"]), mutationVersion: "1" }),
  Object.freeze({ id: "semantic-form", version: "1", capabilities: Object.freeze(["form-fill"]), mutationVersion: "1" }),
  Object.freeze({ id: "validation", version: "1", capabilities: Object.freeze(["form-fill", "validation"]), mutationVersion: "1" }),
]);

export const CHAMPIONSHIP_COURSE_VERSION = "course-v1";
export const CHAMPIONSHIP_STATION_MANIFEST: readonly ManifestStation[] = Object.freeze([
  ...FOUNDATION_STATION_MANIFEST,
  Object.freeze({ id: "iframe", version: "1", capabilities: Object.freeze(["iframe", "click"]), mutationVersion: "1" }),
  Object.freeze({ id: "shadow-root", version: "1", capabilities: Object.freeze(["shadow-dom", "click"]), mutationVersion: "1" }),
  Object.freeze({ id: "popup", version: "1", capabilities: Object.freeze(["popup", "click"]), mutationVersion: "1" }),
  Object.freeze({ id: "file-attachment", version: "1", capabilities: Object.freeze(["file-upload"]), mutationVersion: "1" }),
  Object.freeze({ id: "download", version: "1", capabilities: Object.freeze(["download"]), mutationVersion: "1" }),
  Object.freeze({ id: "championship", version: "1", capabilities: Object.freeze(["form-fill", "click", "submission"]), mutationVersion: "1" }),
]);

const MAX_SEED_LENGTH = 256;
const COURSE_VERSION = /^[a-z0-9][a-z0-9._-]{0,63}$/i;
const DIFFICULTIES = new Set<Difficulty>([FOUNDATION_DIFFICULTY, "advanced", "adversarial"]);

export function createManifest(
  courseVersion: string,
  seed: string,
  difficulty: Difficulty,
  stations: readonly ManifestStation[] = FOUNDATION_STATION_MANIFEST,
): Readonly<GauntletManifest> {
  if (!COURSE_VERSION.test(courseVersion)) {
    throw new Error("courseVersion must be a bounded version identifier");
  }
  if (seed.length === 0 || seed.length > MAX_SEED_LENGTH) {
    throw new Error("seed must contain between 1 and 256 characters");
  }
  if (!DIFFICULTIES.has(difficulty)) {
    throw new Error("difficulty is not supported");
  }
  if (stations.length === 0 || new Set(stations.map((station) => station.id)).size !== stations.length) {
    throw new Error("manifest stations must be non-empty and unique");
  }
  for (const station of stations) {
    if (!COURSE_VERSION.test(station.id) || !COURSE_VERSION.test(station.version) || !COURSE_VERSION.test(station.mutationVersion) || station.capabilities.length === 0 || station.capabilities.some((capability) => !COURSE_VERSION.test(capability))) {
      throw new Error("manifest station configuration is invalid");
    }
  }

  return deepFreeze({
    courseVersion,
    seed,
    difficulty,
    stations: stations.map((station) => ({
      id: station.id,
      version: station.version,
      capabilities: [...station.capabilities],
      mutationVersion: station.mutationVersion,
    })),
  });
}

function deepFreeze<T>(value: T): Readonly<T> {
  if (value !== null && typeof value === "object" && !Object.isFrozen(value)) {
    for (const child of Object.values(value)) {
      deepFreeze(child);
    }
    Object.freeze(value);
  }
  return value;
}

