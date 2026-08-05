import type { NorthstarApi } from "../api.js";
import { element, pageHeader } from "../components.js";

export async function dashboardPage(document: Document, api: NorthstarApi): Promise<HTMLElement> {
  const page = element(document, "section", { className: "page" });
  page.append(pageHeader(document, "Workspace overview", "Good afternoon, Maya", "Your customer operations, documents, and reports in one calm workspace."));
  try {
    const summary = await api.dashboard();
    const grid = element(document, "div", { className: "metric-grid" });
    for (const [label, value] of [
      ["Active customers", summary.activeCustomers],
      ["Pending onboarding", summary.pendingOnboarding],
      ["Documents processed", summary.documentsProcessed],
      ["Reports ready", summary.reportsReady],
    ] as const) {
      const card = element(document, "article", { className: "metric-card" });
      card.append(element(document, "p", { text: label }), element(document, "strong", { text: String(value) }));
      grid.append(card);
    }
    page.append(grid);
  } catch {
    page.append(element(document, "p", { className: "error-panel", text: "Dashboard data is temporarily unavailable." }));
  }
  return page;
}
