import {
  FINGERPRINT_PROFILE_KEY,
  getFingerprintEnabled,
  getFingerprintOwner,
  getFingerprintProfile,
  setFingerprintEnabled,
} from "./fingerprint.js";
import type { PopupStatus } from "./popup-status.js";

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

function truncateId(id: string, max = 12): string {
  if (id.length <= max) return id;
  return `${id.slice(0, max)}…`;
}

function sectionStatus(root: ParentNode, id: string): HTMLElement | null {
  const section = root.querySelector(`#${id} .status`);
  if (!section || !("textContent" in section)) return null;
  return section as HTMLElement;
}

function fingerprintStatusText(status: PopupStatus["fingerprint"]): string {
  if (status.owner === "host") {
    let text = "Managed by Bobby worker — BiDi owns spoofing";
    if (status.sessionId !== undefined && status.seedHex !== undefined) {
      text += `\nSession ${status.sessionId} · seed 0x${status.seedHex}`;
    }
    return text;
  }
  return status.enabled
    ? "On — document_start script registered for new loads"
    : "Off — real browser fingerprints exposed";
}

function humanizeLabel(humanize: PopupStatus["humanize"]): string {
  switch (humanize) {
    case "on":
      return "On";
    case "off":
      return "Off";
    case "unknown":
      return "Unknown — set by session policy";
  }
}

function renderConnection(el: HTMLElement, status: PopupStatus): void {
  const doc = el.ownerDocument;
  el.replaceChildren();

  if (status.enrollPhase === "pairing") {
    el.appendChild(doc.createTextNode("Pairing…"));
    return;
  }

  const badge = doc.createElement("span");
  if (status.paired) {
    badge.className = "badge badge-paired";
    badge.textContent = "Paired";
    el.appendChild(badge);
    const ids: string[] = [];
    if (status.companionId) ids.push(`companion ${truncateId(status.companionId)}`);
    if (status.profileId) ids.push(`profile ${truncateId(status.profileId)}`);
    if (ids.length > 0) {
      el.appendChild(doc.createTextNode(`\n${ids.join(" · ")}`));
    }
  } else {
    badge.className = "badge badge-unpaired";
    badge.textContent = "Unpaired";
    el.appendChild(badge);
    const reason = status.unpairedReason ?? "Not paired";
    el.appendChild(doc.createTextNode(`\n${reason}`));
  }

  if (status.enrollPhase === "failed" && status.enrollError) {
    el.appendChild(doc.createTextNode(`\n${status.enrollError.message}`));
  }
}

function renderPairButton(root: ParentNode, status: PopupStatus): void {
  const button = root.querySelector("#pair-button") as HTMLButtonElement | null;
  if (!button) return;
  button.textContent = status.paired ? "Re-pair" : "Pair";
  button.disabled = status.enrollPhase === "pairing";
}

export function renderPopup(root: ParentNode, status: PopupStatus): void {
  const connection = sectionStatus(root, "connection");
  if (connection) {
    renderConnection(connection, status);
  }
  renderPairButton(root, status);

  const session = sectionStatus(root, "session");
  if (session) {
    let text = `Leases: ${status.leaseCount}`;
    if (
      status.fingerprint.sessionId !== undefined &&
      status.fingerprint.seedHex !== undefined
    ) {
      text += `\nSeed session ${status.fingerprint.sessionId} · 0x${status.fingerprint.seedHex}`;
    }
    session.textContent = text;
  }

  const toggle = root.querySelector("#toggle") as HTMLInputElement | null;
  const fingerprintStatus = root.querySelector("#fingerprint-status");
  if (toggle) {
    if (status.fingerprint.owner === "host") {
      toggle.checked = true;
      toggle.disabled = true;
    } else {
      toggle.checked = status.fingerprint.enabled;
      toggle.disabled = false;
    }
  }
  if (fingerprintStatus) {
    fingerprintStatus.textContent = fingerprintStatusText(status.fingerprint);
  }

  const humanizeStatus = root.querySelector("#humanize-status");
  if (humanizeStatus) {
    humanizeStatus.textContent = humanizeLabel(status.humanize);
  }

  const debug = sectionStatus(root, "debug");
  if (debug) {
    const lines = [
      `Native: ${status.nativeConnected ? "connected" : "disconnected"}`,
      `Protocol v${status.protocolVersion}`,
    ];
    if (status.lastError) {
      lines.push(`Error: ${status.lastError.code} — ${status.lastError.message}`);
    }
    debug.textContent = lines.join("\n");
  }
}

export function showStatusUnavailable(root: ParentNode): void {
  const connection = sectionStatus(root, "connection");
  if (connection) {
    connection.textContent = "Status unavailable";
  }
}

export async function applyStatusOrFallback(
  browserApi: BrowserApi,
  root: ParentNode,
  status: PopupStatus | undefined,
): Promise<void> {
  if (!status) {
    showStatusUnavailable(root);
    await bindFingerprintToggle(browserApi, root);
    await bindPairButton(browserApi, root);
    return;
  }
  renderPopup(root, status);
  if (status.fingerprint.owner === "popup") {
    await bindFingerprintToggle(browserApi, root);
  }
  await bindPairButton(browserApi, root);
}

async function loadStatus(browserApi: BrowserApi): Promise<PopupStatus | undefined> {
  try {
    return (await browserApi.runtime.sendMessage({ type: "popupStatus" })) as PopupStatus;
  } catch {
    return undefined;
  }
}

export async function bindPairButton(
  browserApi: BrowserApi,
  root: ParentNode,
): Promise<void> {
  const button = root.querySelector("#pair-button") as HTMLButtonElement | null;
  if (!button || button.dataset.bound === "true") return;
  button.dataset.bound = "true";

  button.addEventListener("click", async () => {
    button.disabled = true;
    try {
      await browserApi.runtime.sendMessage({ type: "enrollPair" });
    } catch {
      /* background may be restarting */
    }
    const status = await loadStatus(browserApi);
    if (status) {
      renderPopup(root, status);
    } else {
      showStatusUnavailable(root);
      button.disabled = false;
    }
  });
}

export async function bindFingerprintToggle(
  browserApi: BrowserApi,
  root: ParentNode,
): Promise<void> {
  const toggle = root.querySelector("#toggle") as HTMLInputElement | null;
  const statusEl = root.querySelector("#fingerprint-status");
  if (!toggle || !statusEl) return;

  const owner = await getFingerprintOwner(browserApi.storage);
  if (owner === "host") {
    toggle.checked = true;
    toggle.disabled = true;
    const stored = await browserApi.storage.local.get([FINGERPRINT_PROFILE_KEY]);
    const hasStoredProfile =
      stored[FINGERPRINT_PROFILE_KEY] !== undefined &&
      typeof stored[FINGERPRINT_PROFILE_KEY] === "object";
    let statusText = "Managed by Bobby worker — BiDi owns spoofing";
    if (hasStoredProfile) {
      const profile = await getFingerprintProfile(browserApi.storage);
      const seedHex = profile.sessionSeed.toString(16);
      statusText += `\nSession ${profile.sessionId} · seed 0x${seedHex}`;
    }
    statusEl.textContent = statusText;
    return;
  }

  const enabled = await getFingerprintEnabled(browserApi.storage);
  toggle.checked = enabled;
  toggle.disabled = false;
  statusEl.textContent = enabled
    ? "On — document_start script registered for new loads"
    : "Off — real browser fingerprints exposed";

  toggle.addEventListener("change", async () => {
    if ((await getFingerprintOwner(browserApi.storage)) === "host") {
      toggle.checked = true;
      toggle.disabled = true;
      statusEl.textContent = "Managed by Bobby worker — BiDi owns spoofing";
      return;
    }
    const next = toggle.checked;
    await setFingerprintEnabled(browserApi.storage, next);
    try {
      await browserApi.runtime.sendMessage({ type: "fingerprintSync" });
    } catch {
      /* background may be restarting */
    }
    statusEl.textContent = next
      ? "On — document_start script registered for new loads"
      : "Off — real browser fingerprints exposed";
  });
}

async function main(): Promise<void> {
  const status = await loadStatus(browser);
  await applyStatusOrFallback(browser, document, status);
}

const inNodeTest =
  typeof process !== "undefined" && process.env.NODE_TEST_CONTEXT !== undefined;
if (!inNodeTest) {
  void main();
}
