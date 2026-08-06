import { NorthstarApi } from "./api.js";
import { element } from "./components.js";
import { dashboardPage } from "./pages/dashboard.js";
import { customerDetailPage, customersPage } from "./pages/customers.js";
import { documentsPage } from "./pages/documents.js";
import { integrationsPage } from "./pages/integrations.js";
import { onboardingPage } from "./pages/onboarding.js";
import { LEVEL_ONE_RUN_CONFIG, type RunConfig } from "./models.js";
import { reportsPage } from "./pages/reports.js";
import { createRouter, type AppRouter, type Route } from "./router.js";
import { levelTwoInterruption } from "./traps.js";

export interface NorthstarApp { navigate(path: string): Promise<void>; }

export function mountNorthstar(root: HTMLElement, api: NorthstarApi, config: RunConfig = LEVEL_ONE_RUN_CONFIG): NorthstarApp {
  const document = root.ownerDocument;
  const window = document.defaultView;
  if (window === null) throw new Error("Northstar requires a browser window");
  const router = createRouter(window);
  let interruptionShown = false;
  const render = async (route: Route): Promise<void> => {
    const shell = applicationShell(document, await northstarPage(document, route, api, router, config), router);
    if (!interruptionShown) {
      const interruption = levelTwoInterruption(document, config);
      if (interruption !== null) shell.append(interruption);
      interruptionShown = true;
    }
    root.replaceChildren(shell);
  };
  router.subscribe(render);
  return { navigate: router.navigate };
}

async function northstarPage(document: Document, route: Route, api: NorthstarApi, router: AppRouter, config: RunConfig): Promise<HTMLElement> {
  if (route.segments[0] === "customers" && route.segments[1] !== undefined && route.segments[2] === "documents") return documentsPage(document, route.segments[1], api);
  if (route.segments[0] === "customers" && route.segments[1] !== undefined) return customerDetailPage(document, route.segments[1], api);
  if (route.segments[0] === "customers") return customersPage(document, api, router);
  if (route.segments[0] === "onboarding") return onboardingPage(document, api, config);
  if (route.segments[0] === "integrations") return integrationsPage(document, api);
  if (route.segments[0] === "reports") return reportsPage(document, api);
  return dashboardPage(document, api);
}

function applicationShell(document: Document, content: HTMLElement, router: AppRouter): HTMLElement {
  const shell = element(document, "div", { className: "app-shell" });
  const sidebar = element(document, "aside", { className: "sidebar" });
  const brand = element(document, "a", { className: "brand", text: "Northstar Ops" });
  brand.href = "/";
  const navigation = element(document, "nav", { ariaLabel: "Primary navigation" });
  for (const [label, path] of [["Overview", "/"], ["Customers", "/customers"], ["Onboarding", "/onboarding"], ["Documents", "/documents"], ["Integrations", "/integrations"], ["Reports", "/reports"]] as const) {
    const link = element(document, "a", { text: label });
    link.href = path;
    link.addEventListener("click", (event) => { event.preventDefault(); void router.navigate(path); });
    navigation.append(link);
  }
  sidebar.append(brand, navigation);
  const main = element(document, "main", { className: "main-content" });
  main.append(content);
  shell.append(sidebar, main);
  return shell;
}

if (typeof document !== "undefined") {
  const root = document.querySelector<HTMLElement>("#app") ?? document.body;
  const runId = new URLSearchParams(window.location.search).get("run") ?? window.sessionStorage.getItem("northstar.run") ?? "demo";
  window.sessionStorage.setItem("northstar.run", runId);
  const api = new NorthstarApi(runId);
  void api.runConfig().then((config) => mountNorthstar(root, api, config).navigate(window.location.pathname)).catch((error: unknown) => {
    const message = error instanceof Error ? error.message : "Unknown application error";
    root.replaceChildren(element(document, "p", { className: "error-panel", text: `Northstar failed to load: ${message}` }));
  });
}
