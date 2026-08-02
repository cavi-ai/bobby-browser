import {
  getFingerprintEnabled,
  setFingerprintEnabled,
} from "./fingerprint.js";

type BrowserStorage = {
  storage: {
    local: {
      get(keys: readonly string[]): Promise<Record<string, unknown>>;
      set(values: Record<string, unknown>): Promise<void>;
    };
  };
};

declare const browser: BrowserStorage;

async function main(): Promise<void> {
  const toggle = document.getElementById("toggle") as HTMLInputElement | null;
  const status = document.getElementById("status");
  if (!toggle || !status) return;

  const enabled = await getFingerprintEnabled(browser.storage);
  toggle.checked = enabled;
  status.textContent = enabled
    ? "On — spoofing applies to new page loads"
    : "Off — real browser fingerprints exposed";

  toggle.addEventListener("change", async () => {
    const next = toggle.checked;
    await setFingerprintEnabled(browser.storage, next);
    status.textContent = next
      ? "On — spoofing applies to new page loads"
      : "Off — real browser fingerprints exposed";
  });
}

void main();
