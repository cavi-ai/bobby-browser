import type { NorthstarApi } from "../api.js";
import { element, pageHeader } from "../components.js";
import { chatOverlay, showToast } from "../overlays.js";

const authorizationListeners = new WeakMap<Window, (event: MessageEvent) => void>();

export async function integrationsPage(document: Document, api: NorthstarApi): Promise<HTMLElement> {
  const page = element(document, "section", { className: "page" });
  page.append(pageHeader(document, "Connected systems", "Integrations", "Bring financial context into customer operations without leaving the workspace."));
  const card = element(document, "article", { className: "integration-card" });
  page.append(card);
  const render = async (): Promise<void> => {
    const state = await api.integrationState();
    card.replaceChildren();
    card.dataset.connected = String(state.connected);
    card.append(element(document, "h2", { text: "Ledger Cloud" }));
    if (state.connected) {
      card.append(element(document, "p", { className: "connection-state", text: `Connected as ${state.identity ?? "authorized account"}` }));
      showToast(document, "Ledger Cloud is syncing balances.");
      document.querySelector(".chat-overlay")?.remove();
      document.querySelector(".app-shell")?.append(chatOverlay(document));
      return;
    }
    const connect = element(document, "button", { text: "Connect Ledger Cloud", ariaLabel: "Connect Ledger Cloud" });
    connect.addEventListener("click", () => {
      if (state.authorizationUrl !== undefined) document.defaultView?.open(state.authorizationUrl, "northstar-ledger-authorization", "popup,width=520,height=680");
    });
    card.append(element(document, "p", { text: "Connect account balances and reconciliation status." }), connect);
  };
  const window = document.defaultView;
  if (window === null) throw new Error("Integrations requires a browser window");
  const authorizationListener = (event: MessageEvent) => {
    if (event.origin !== window.location.origin || !isAuthorizationMessage(event.data)) return;
    void render();
  };
  await render();
  const previousListener = authorizationListeners.get(window);
  if (previousListener !== undefined) window.removeEventListener("message", previousListener);
  authorizationListeners.set(window, authorizationListener);
  window.addEventListener("message", authorizationListener);
  return page;
}

function isAuthorizationMessage(value: unknown): boolean {
  return typeof value === "object" && value !== null && "type" in value && value.type === "northstar.authorization.complete";
}
