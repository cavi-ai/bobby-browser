/** Registers the shared init script at document_start when the popup toggle is on. */

import {
  buildInitScript,
  getFingerprintEnabled,
  getFingerprintOwner,
  getFingerprintProfile,
} from "./fingerprint.js";

export type FingerprintRegistrationStorage = {
  local: {
    get(keys: readonly string[]): Promise<Record<string, unknown>>;
    set(values: Record<string, unknown>): Promise<void>;
  };
};

export type RegisteredFingerprintScript = {
  unregister(): Promise<void> | void;
};

export type FingerprintContentScriptsApi = {
  register(options: {
    matches: string[];
    js: Array<{ code: string }>;
    runAt: "document_start";
    allFrames: boolean;
    matchAboutBlank: boolean;
  }): Promise<RegisteredFingerprintScript>;
};

export type FingerprintRegistrationApi = {
  storage: FingerprintRegistrationStorage;
  contentScripts?: FingerprintContentScriptsApi;
};

let activeRegistration: RegisteredFingerprintScript | null = null;

/** Unregister any prior script, then register when popup-owned and enabled. */
export async function syncFingerprintRegistration(
  api: FingerprintRegistrationApi,
): Promise<"registered" | "cleared" | "managed" | "unsupported"> {
  if (activeRegistration) {
    try {
      await activeRegistration.unregister();
    } catch {
      /* ignore */
    }
    activeRegistration = null;
  }

  if ((await getFingerprintOwner(api.storage)) === "host") {
    return "managed";
  }
  if (!(await getFingerprintEnabled(api.storage))) {
    return "cleared";
  }
  if (!api.contentScripts?.register) {
    return "unsupported";
  }

  const profile = await getFingerprintProfile(api.storage);
  const code = buildInitScript({ ...profile, injectChrome: false });
  activeRegistration = await api.contentScripts.register({
    matches: ["http://*/*", "https://*/*"],
    js: [{ code }],
    runAt: "document_start",
    allFrames: true,
    matchAboutBlank: true,
  });
  return "registered";
}
