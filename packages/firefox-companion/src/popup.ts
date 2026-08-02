import {
  FINGERPRINT_PROFILE_KEY,
  getFingerprintEnabled,
  getFingerprintOwner,
  getFingerprintProfile,
  setFingerprintEnabled,
} from "./fingerprint.js";

type BrowserApi = {
  storage: {
    local: {
      get(keys: readonly string[]): Promise<Record<string, unknown>>;
      set(values: Record<string, unknown>): Promise<void>;
    };
  };
  runtime: {
    sendMessage(message: unknown): Promise<unknown>;
  };
};

declare const browser: BrowserApi;

async function main(): Promise<void> {
  const toggle = document.getElementById("toggle") as HTMLInputElement | null;
  const status = document.getElementById("status");
  if (!toggle || !status) return;

  const owner = await getFingerprintOwner(browser.storage);
  if (owner === "host") {
    toggle.checked = true;
    toggle.disabled = true;
    const stored = await browser.storage.local.get([FINGERPRINT_PROFILE_KEY]);
    const hasStoredProfile =
      stored[FINGERPRINT_PROFILE_KEY] !== undefined &&
      typeof stored[FINGERPRINT_PROFILE_KEY] === "object";
    let statusText = "Managed by Bobby worker — BiDi owns spoofing";
    if (hasStoredProfile) {
      const profile = await getFingerprintProfile(browser.storage);
      const seedHex = profile.sessionSeed.toString(16);
      statusText += `\nSession ${profile.sessionId} · seed 0x${seedHex}`;
    }
    status.textContent = statusText;
    return;
  }

  const enabled = await getFingerprintEnabled(browser.storage);
  toggle.checked = enabled;
  toggle.disabled = false;
  status.textContent = enabled
    ? "On — document_start script registered for new loads"
    : "Off — real browser fingerprints exposed";

  toggle.addEventListener("change", async () => {
    if ((await getFingerprintOwner(browser.storage)) === "host") {
      toggle.checked = true;
      toggle.disabled = true;
      status.textContent =
        "Managed by Bobby worker — BiDi owns spoofing";
      return;
    }
    const next = toggle.checked;
    await setFingerprintEnabled(browser.storage, next);
    try {
      await browser.runtime.sendMessage({ type: "fingerprintSync" });
    } catch {
      /* background may be restarting */
    }
    status.textContent = next
      ? "On — document_start script registered for new loads"
      : "Off — real browser fingerprints exposed";
  });
}

void main();
