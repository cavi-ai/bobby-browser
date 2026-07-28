export type Difficulty = "foundation" | "advanced" | "adversarial";

export const FOUNDATION_DIFFICULTY: Difficulty = "foundation";

export type FailureCode =
  | "unsupportedCapability"
  | "configurationConflict"
  | "deadlineExceeded"
  | "targetDrift"
  | "postconditionFailed"
  | "effectUncertain"
  | "checkpointMismatch"
  | "strategyExhausted"
  | "engineUnavailable";

export interface EvidenceRef {
  readonly id: string;
}

export interface GauntletFailure {
  readonly code: FailureCode;
  readonly layer: "controller" | "station";
  readonly retryable: boolean;
  readonly guidance: GuidanceCode;
}

export type GuidanceCode =
  | "inspect-canonical-route"
  | "reobserve-replacement-target"
  | "complete-semantic-fields"
  | "correct-invalid-input"
  | "complete-embedded-action"
  | "complete-popup-action"
  | "attach-approved-file"
  | "confirm-generated-download"
  | "complete-championship-steps"
  | "invalid-station-id"
  | "station-not-in-run"
  | "invalid-station-result";

export type StationResult =
  | {
      readonly passed: true;
      readonly postconditions: readonly string[];
      readonly evidence: readonly EvidenceRef[];
    }
  | {
      readonly passed: false;
      readonly failure: GauntletFailure;
      readonly evidence: readonly EvidenceRef[];
    };

export interface GauntletStation<S extends object, I> {
  readonly id: string;
  readonly version: string;
  readonly mutationVersion: string;
  readonly supportedDifficulties: readonly Difficulty[];
  readonly title: string;
  readonly capabilities: readonly string[];
  setup(seed: string, difficulty: Difficulty): Readonly<S>;
  verify(state: Readonly<S>, submission: I): StationResult;
  reset(): void;
}

export type RegisteredStation = GauntletStation<object, unknown>;

const EVIDENCE_ID = /^[a-z][a-z0-9:_-]{0,95}$/;
const POSTCONDITION = /^[a-z][a-z0-9-]{0,127}$/;
const FAILURE_CODES = new Set<FailureCode>([
  "unsupportedCapability",
  "configurationConflict",
  "deadlineExceeded",
  "targetDrift",
  "postconditionFailed",
  "effectUncertain",
  "checkpointMismatch",
  "strategyExhausted",
  "engineUnavailable",
]);
const GUIDANCE_CODES = new Set<GuidanceCode>([
  "inspect-canonical-route",
  "reobserve-replacement-target",
  "complete-semantic-fields",
  "correct-invalid-input",
  "complete-embedded-action",
  "complete-popup-action",
  "attach-approved-file",
  "confirm-generated-download",
  "complete-championship-steps",
  "invalid-station-id",
  "station-not-in-run",
  "invalid-station-result",
]);

export function passed(postcondition: string, evidenceId: string): StationResult {
  return normalizeStationResult({
    passed: true,
    postconditions: Object.freeze([postcondition]),
    evidence: Object.freeze([{ id: evidenceId }]),
  });
}

export function failed(
  code: FailureCode,
  layer: GauntletFailure["layer"],
  guidance: GuidanceCode,
  evidenceId: string,
  retryable = false,
): StationResult {
  return normalizeStationResult({
    passed: false,
    failure: { code, layer, retryable, guidance },
    evidence: Object.freeze([{ id: evidenceId }]),
  });
}

export function normalizeStationResult(result: unknown): StationResult {
  if (result === null || typeof result !== "object" || Array.isArray(result)) {
    throw new Error("station result must be an object");
  }
  const candidate = result as Record<string, unknown>;
  const evidence = normalizeEvidence(candidate.evidence);
  if (candidate.passed === true) {
    const postconditions = normalizePostconditions(candidate.postconditions);
    return deepFreeze({ passed: true, postconditions, evidence });
  }
  if (candidate.passed === false) {
    return deepFreeze({ passed: false, failure: normalizeFailure(candidate.failure), evidence });
  }
  throw new Error("station result must declare a boolean passed flag");
}

function normalizeEvidence(value: unknown): readonly EvidenceRef[] {
  if (!Array.isArray(value) || value.length === 0 || value.length > 20) {
    throw new Error("station result evidence must be a bounded non-empty array");
  }
  return value.map((evidence) => {
    if (evidence === null || typeof evidence !== "object" || Array.isArray(evidence)) {
      throw new Error("station evidence must be an object");
    }
    const id = (evidence as Record<string, unknown>).id;
    if (typeof id !== "string" || !EVIDENCE_ID.test(id)) {
      throw new Error("station evidence id must be an opaque bounded identifier");
    }
    return { id };
  });
}

function normalizePostconditions(value: unknown): readonly string[] {
  if (!Array.isArray(value) || value.length === 0 || value.length > 20 || value.some((item) => typeof item !== "string" || !POSTCONDITION.test(item))) {
    throw new Error("station postconditions must be bounded identifiers");
  }
  return [...value] as string[];
}

function normalizeFailure(value: unknown): GauntletFailure {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("station failure must be an object");
  }
  const failure = value as Record<string, unknown>;
  if (
    typeof failure.code !== "string" ||
    !FAILURE_CODES.has(failure.code as FailureCode) ||
    (failure.layer !== "controller" && failure.layer !== "station") ||
    typeof failure.retryable !== "boolean" ||
    typeof failure.guidance !== "string" ||
    !GUIDANCE_CODES.has(failure.guidance as GuidanceCode)
  ) {
    throw new Error("station failure is malformed");
  }
  return { code: failure.code as FailureCode, layer: failure.layer, retryable: failure.retryable, guidance: failure.guidance as GuidanceCode };
}

function deepFreeze<T>(value: T): Readonly<T> {
  if (value !== null && typeof value === "object" && !Object.isFrozen(value)) {
    for (const child of Object.values(value)) deepFreeze(child);
    Object.freeze(value);
  }
  return value;
}

/** A small checked-in generator: deterministic across browsers and runtimes. */
export function seededNumber(seed: string): number {
  let value = 0x811c9dc5;
  for (const character of seed) {
    value ^= character.charCodeAt(0);
    value = Math.imul(value, 0x01000193);
  }
  value ^= value << 13;
  value ^= value >>> 17;
  value ^= value << 5;
  return value >>> 0;
}

export function seededPick<T>(seed: string, values: readonly T[]): T {
  if (values.length === 0) {
    throw new Error("seededPick requires at least one value");
  }
  return values[seededNumber(seed) % values.length] as T;
}

/** Browser-compatible SHA-256 over exact UTF-8 input bytes. */
export function sha256Hex(value: string): string {
  const bytes = new TextEncoder().encode(value);
  const paddedLength = bytes.length + 1 + ((64 - ((bytes.length + 1 + 8) % 64)) % 64) + 8;
  const padded = new Uint8Array(paddedLength);
  padded.set(bytes);
  padded[bytes.length] = 0x80;
  let bitLength = BigInt(bytes.length) * 8n;
  for (let index = paddedLength - 1; index >= paddedLength - 8; index -= 1) {
    padded[index] = Number(bitLength & 0xffn);
    bitLength >>= 8n;
  }

  const hash = new Uint32Array([0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19]);
  const constants = SHA256_CONSTANTS;
  for (let offset = 0; offset < padded.length; offset += 64) {
    const schedule = new Uint32Array(64);
    for (let index = 0; index < 16; index += 1) {
      const wordOffset = offset + index * 4;
      schedule[index] = ((padded[wordOffset]! << 24) | (padded[wordOffset + 1]! << 16) | (padded[wordOffset + 2]! << 8) | padded[wordOffset + 3]!) >>> 0;
    }
    for (let index = 16; index < 64; index += 1) {
      const a = schedule[index - 15]!;
      const b = schedule[index - 2]!;
      const sigma0 = ((a >>> 7) | (a << 25)) ^ ((a >>> 18) | (a << 14)) ^ (a >>> 3);
      const sigma1 = ((b >>> 17) | (b << 15)) ^ ((b >>> 19) | (b << 13)) ^ (b >>> 10);
      schedule[index] = (schedule[index - 16]! + sigma0 + schedule[index - 7]! + sigma1) >>> 0;
    }
    let [a, b, c, d, e, f, g, h] = hash;
    for (let index = 0; index < 64; index += 1) {
      const sigma1 = ((e! >>> 6) | (e! << 26)) ^ ((e! >>> 11) | (e! << 21)) ^ ((e! >>> 25) | (e! << 7));
      const choice = (e! & f!) ^ (~e! & g!);
      const temp1 = (h! + sigma1 + choice + constants[index]! + schedule[index]!) >>> 0;
      const sigma0 = ((a! >>> 2) | (a! << 30)) ^ ((a! >>> 13) | (a! << 19)) ^ ((a! >>> 22) | (a! << 10));
      const majority = (a! & b!) ^ (a! & c!) ^ (b! & c!);
      const temp2 = (sigma0 + majority) >>> 0;
      h = g; g = f; f = e; e = (d! + temp1) >>> 0; d = c; c = b; b = a; a = (temp1 + temp2) >>> 0;
    }
    hash[0] = (hash[0]! + a!) >>> 0; hash[1] = (hash[1]! + b!) >>> 0; hash[2] = (hash[2]! + c!) >>> 0; hash[3] = (hash[3]! + d!) >>> 0;
    hash[4] = (hash[4]! + e!) >>> 0; hash[5] = (hash[5]! + f!) >>> 0; hash[6] = (hash[6]! + g!) >>> 0; hash[7] = (hash[7]! + h!) >>> 0;
  }
  return [...hash].map((word) => word.toString(16).padStart(8, "0")).join("");
}

const SHA256_CONSTANTS = new Uint32Array([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
  0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
  0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
  0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
  0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]);

