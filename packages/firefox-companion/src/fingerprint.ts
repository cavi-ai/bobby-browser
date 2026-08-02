/** Fingerprint storage + shared init-script builder for companion extensions. */

import { DEFAULT_FINGERPRINT_PROFILE as DEFAULT_PROFILE_RAW } from "./default-fingerprint-profile.js";
import {
  INIT_SCRIPT_TEMPLATE,
  PROFILE_PLACEHOLDER,
} from "./init-script-template.js";

export { INIT_SCRIPT_TEMPLATE, PROFILE_PLACEHOLDER };

export const FINGERPRINT_ENABLED_KEY = "fingerprintEnabled";
export const FINGERPRINT_PROFILE_KEY = "fingerprintProfile";
/** Who owns apply: popup (standalone) or host (Bobby worker BiDi/CDP). */
export const FINGERPRINT_OWNER_KEY = "fingerprintOwner";

export type FingerprintOwner = "popup" | "host";

export type FingerprintProfile = {
  sessionId: string;
  sessionSeed: number;
  canvasHash: string;
  canvasNoiseAmplitude: number;
  webgl: { vendor: string; renderer: string; hash: string; maxTextureSize: number };
  audioHash: string;
  audioNoiseScale: number;
  fontList: string[];
  screenResolution: {
    width: number;
    height: number;
    availableWidth: number;
    availableHeight: number;
    colorDepth: number;
    pixelRatio: number;
  };
  userAgent: string;
  platform: string;
  locale: string;
  timezoneId: string;
  hardwareConcurrency: number;
  deviceMemory: number;
  maxTouchPoints: number;
  injectChrome: boolean;
  clientHints: {
    brands: Array<{ brand: string; version: string }>;
    fullVersionList: Array<{ brand: string; version: string }>;
    platform: string;
    platformVersion: string;
    architecture: string;
    bitness: string;
    model: string;
    mobile: boolean;
    fullVersion: string;
  };
};

/** Golden-locked to Rust create_session — see default-fingerprint-profile.json. */
export const DEFAULT_FINGERPRINT_PROFILE: FingerprintProfile = DEFAULT_PROFILE_RAW;

type FingerprintStorage = {
  local: {
    get(keys: readonly string[]): Promise<Record<string, unknown>>;
    set(values: Record<string, unknown>): Promise<void>;
  };
};

export async function getFingerprintEnabled(storage: FingerprintStorage): Promise<boolean> {
  const stored = await storage.local.get([FINGERPRINT_ENABLED_KEY]);
  const value = stored[FINGERPRINT_ENABLED_KEY];
  return value === undefined ? true : value === true;
}

export async function setFingerprintEnabled(
  storage: FingerprintStorage,
  enabled: boolean,
): Promise<void> {
  await storage.local.set({ [FINGERPRINT_ENABLED_KEY]: enabled });
}

export async function getFingerprintOwner(
  storage: FingerprintStorage,
): Promise<FingerprintOwner> {
  const stored = await storage.local.get([FINGERPRINT_OWNER_KEY]);
  return stored[FINGERPRINT_OWNER_KEY] === "host" ? "host" : "popup";
}

export async function setFingerprintOwner(
  storage: FingerprintStorage,
  owner: FingerprintOwner,
): Promise<void> {
  await storage.local.set({ [FINGERPRINT_OWNER_KEY]: owner });
}

/** Bobby worker is paired — BiDi preload owns spoofing; optionally persist session profile. */
export async function claimFingerprintHostOwnership(
  storage: FingerprintStorage,
  profile?: FingerprintProfile,
): Promise<void> {
  await setFingerprintOwner(storage, "host");
  if (profile) await setFingerprintProfile(storage, profile);
}

/** Native host gone — popup may register again from stored toggle preference. */
export async function releaseFingerprintHostOwnership(
  storage: FingerprintStorage,
): Promise<void> {
  await setFingerprintOwner(storage, "popup");
}

export async function getFingerprintProfile(
  storage: FingerprintStorage,
): Promise<FingerprintProfile> {
  const stored = await storage.local.get([FINGERPRINT_PROFILE_KEY]);
  const value = stored[FINGERPRINT_PROFILE_KEY];
  if (value && typeof value === "object") {
    return { ...DEFAULT_FINGERPRINT_PROFILE, ...(value as FingerprintProfile) };
  }
  return DEFAULT_FINGERPRINT_PROFILE;
}

export async function setFingerprintProfile(
  storage: FingerprintStorage,
  profile: FingerprintProfile,
): Promise<void> {
  await storage.local.set({ [FINGERPRINT_PROFILE_KEY]: profile });
}

/** Embed a profile into the shared Rust/TS init-script template. */
export function buildInitScript(profile: FingerprintProfile): string {
  if (!INIT_SCRIPT_TEMPLATE.includes(PROFILE_PLACEHOLDER)) {
    throw new Error("init script template missing profile placeholder");
  }
  return INIT_SCRIPT_TEMPLATE.replace(PROFILE_PLACEHOLDER, JSON.stringify(profile));
}
