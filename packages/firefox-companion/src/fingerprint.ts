/** Fingerprint storage + shared init-script builder for companion extensions. */

import { DEFAULT_FINGERPRINT_PROFILE as DEFAULT_PROFILE_RAW } from "./default-fingerprint-profile.js";
import {
  INIT_SCRIPT_TEMPLATE,
  PROFILE_PLACEHOLDER,
  WORKER_BOOTSTRAP_PLACEHOLDER,
  WORKER_BOOTSTRAP_TEMPLATE,
  WORKER_PROFILE_PLACEHOLDER,
} from "./init-script-template.js";

export {
  INIT_SCRIPT_TEMPLATE,
  PROFILE_PLACEHOLDER,
  WORKER_BOOTSTRAP_PLACEHOLDER,
  WORKER_BOOTSTRAP_TEMPLATE,
  WORKER_PROFILE_PLACEHOLDER,
};

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

export type FingerprintStorage = {
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
  const profile =
    value && typeof value === "object"
      ? { ...DEFAULT_FINGERPRINT_PROFILE, ...(value as FingerprintProfile) }
      : DEFAULT_FINGERPRINT_PROFILE;
  return geckoPersona(profile);
}

/** The shared default profile is a Chromium persona (Chrome UA, client
 * hints). Serving it on Gecko is a cross-channel lie any detector sees:
 * the engine is Firefox. Rewrite the engine-visible fields to a Firefox
 * persona of the same version and platform; the init script gates its
 * Chrome-only surfaces on engine detection.
 */
function geckoPersona(profile: FingerprintProfile): FingerprintProfile {
  const version = profile.clientHints.fullVersion.split(".")[0] || "131";
  const platformUa =
    profile.platform === "MacIntel"
      ? "Macintosh; Intel Mac OS X 10.15"
      : profile.platform === "Linux x86_64"
        ? "X11; Linux x86_64"
        : "Windows NT 10.0; Win64; x64";
  return {
    ...profile,
    userAgent: `Mozilla/5.0 (${platformUa}; rv:${version}.0) Gecko/20100101 Firefox/${version}.0`,
    clientHints: { ...profile.clientHints, brands: [], fullVersionList: [] },
  };
}

export async function setFingerprintProfile(
  storage: FingerprintStorage,
  profile: FingerprintProfile,
): Promise<void> {
  await storage.local.set({ [FINGERPRINT_PROFILE_KEY]: profile });
}

/** Collapse whitespace / line comments outside of string literals. */
function minifyJs(source: string): string {
  let out = "";  let i = 0;
  let inSquote = false;
  let inDquote = false;
  let inTemplate = false;
  let inLineComment = false;
  let inBlockComment = false;
  let lastEmit: string | null = null;

  while (i < source.length) {
    const c = source[i]!;
    const next = source[i + 1];

    if (inLineComment) {
      if (c === "\n") {
        inLineComment = false;
        if (lastEmit && /[A-Za-z0-9_$]/.test(lastEmit)) {
          out += "\n";
          lastEmit = "\n";
        }
      }
      i += 1;
      continue;
    }
    if (inBlockComment) {
      if (c === "*" && next === "/") {
        inBlockComment = false;
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }

    if (inSquote || inDquote || inTemplate) {
      out += c;
      lastEmit = c;
      if (c === "\\" && i + 1 < source.length) {
        const escaped = source[i + 1]!;
        out += escaped;
        lastEmit = escaped;
        i += 2;
        continue;
      }
      if (inSquote && c === "'") inSquote = false;
      else if (inDquote && c === '"') inDquote = false;
      else if (inTemplate && c === "`") inTemplate = false;
      i += 1;
      continue;
    }

    if (c === "/" && next === "/") {
      inLineComment = true;
      i += 2;
      continue;
    }
    if (c === "/" && next === "*") {
      inBlockComment = true;
      i += 2;
      continue;
    }

    if (c === "'") {
      inSquote = true;
      out += c;
      lastEmit = c;
      i += 1;
      continue;
    }
    if (c === '"') {
      inDquote = true;
      out += c;
      lastEmit = c;
      i += 1;
      continue;
    }
    if (c === "`") {
      inTemplate = true;
      out += c;
      lastEmit = c;
      i += 1;
      continue;
    }

    if (/\s/.test(c)) {
      let j = i + 1;
      while (j < source.length && /\s/.test(source[j]!)) j += 1;
      const nxt = source[j];
      const need =
        !!lastEmit &&
        nxt !== undefined &&
        /[A-Za-z0-9_$/]/.test(lastEmit) &&
        /[A-Za-z0-9_$/]/.test(nxt);
      if (need) {
        out += " ";
        lastEmit = " ";
      }
      i = j;
      continue;
    }

    out += c;
    lastEmit = c;
    i += 1;
  }
  return out;
}

/** Embed a profile into the shared Rust/TS init-script template. */
export function buildInitScript(profile: FingerprintProfile): string {
  if (!INIT_SCRIPT_TEMPLATE.includes(PROFILE_PLACEHOLDER)) {
    throw new Error("init script template missing profile placeholder");
  }
  if (!INIT_SCRIPT_TEMPLATE.includes(WORKER_BOOTSTRAP_PLACEHOLDER)) {
    throw new Error("init script template missing worker bootstrap placeholder");
  }
  if (!WORKER_BOOTSTRAP_TEMPLATE.includes(WORKER_PROFILE_PLACEHOLDER)) {
    throw new Error("worker bootstrap template missing profile placeholder");
  }

  const workerProfile = {
    userAgent: profile.userAgent,
    platform: profile.platform,
    locale: profile.locale,
    hardwareConcurrency: profile.hardwareConcurrency,
    deviceMemory: profile.deviceMemory,
    maxTouchPoints: profile.maxTouchPoints,
    timezoneId: profile.timezoneId,
    webgl: {
      vendor: profile.webgl?.vendor || "",
      renderer: profile.webgl?.renderer || "",
      maxTextureSize: profile.webgl?.maxTextureSize || 16384,
    },
    clientHints: profile.clientHints || {},
    injectChrome: false,
  };
  const worker = minifyJs(
    WORKER_BOOTSTRAP_TEMPLATE.replace(
      WORKER_PROFILE_PLACEHOLDER,
      JSON.stringify(workerProfile),
    ),
  );
  const script = INIT_SCRIPT_TEMPLATE.replace(
    PROFILE_PLACEHOLDER,
    JSON.stringify(profile),
  ).replace(WORKER_BOOTSTRAP_PLACEHOLDER, JSON.stringify(worker));
  return minifyJs(script);
}
