import type { NorthstarApi } from "../api.js";
import { element, pageHeader, status } from "../components.js";
import type { AppRouter } from "../router.js";
import type { CustomerSummary, Priority } from "../models.js";
import { showToast } from "../overlays.js";
import { combobox, listbox } from "../widgets.js";

const ROW_HEIGHT = 48;
const WINDOW_SIZE = 8;

export async function customersPage(document: Document, api: NorthstarApi, router: AppRouter): Promise<HTMLElement> {
  const page = element(document, "section", { className: "page" });
  page.append(pageHeader(document, "Customer operations", "Customers", "Find an account in a dense ledger, then open the durable record."));
  const results = element(document, "div", { className: "customer-results" });
  let customers: CustomerSummary[] = [];
  let searchGeneration = 0;
  const refresh = async (query: string) => {
    const generation = ++searchGeneration;
    results.replaceChildren(loading(document, "Searching customer records"));
    try {
      const next = await api.customers(query);
      if (generation !== searchGeneration) return;
      customers = next;
      results.replaceChildren(virtualTable(document, customers, router));
    } catch (error: unknown) {
      if (generation !== searchGeneration) return;
      const detail = error instanceof Error ? ` ${error.message}` : "";
      results.replaceChildren(element(document, "p", { className: "error-panel", text: `Customer search failed.${detail}` }));
    }
  };
  const search = combobox(document, {
    label: "Search customers",
    placeholder: "Name, company, or email",
    onQuery: async (value) => {
      const matches = await api.customers(value);
      return matches.map((customer) => ({ id: customer.id, label: customer.name }));
    },
    onSelect: (id, label) => {
      void refresh(label).then(() => {
        const row = results.querySelector(`[data-customer-id='${id}']`);
        row?.scrollIntoView({ block: "center" });
      });
    },
    onSearch: (value) => { void refresh(value); },
  });
  page.append(search, results);
  void refresh("");
  return page;
}

function loading(document: Document, label: string): HTMLElement {
  const node = element(document, "div", { className: "skeleton-panel", text: label });
  node.setAttribute("aria-busy", "true");
  return node;
}

function virtualTable(document: Document, customers: CustomerSummary[], router: AppRouter): HTMLElement {
  const viewport = element(document, "div", { className: "virtual-table", ariaLabel: "Customer ledger" });
  viewport.tabIndex = 0;
  const spacer = element(document, "div", { className: "virtual-spacer" });
  spacer.style.height = `${customers.length * ROW_HEIGHT}px`;
  const windowNode = element(document, "div", { className: "virtual-window" });
  spacer.append(windowNode);
  viewport.append(spacer);
  let pulse = 0;
  const render = () => {
    const start = Math.min(customers.length, Math.floor(viewport.scrollTop / ROW_HEIGHT));
    const visible = customers.slice(start, start + WINDOW_SIZE);
    windowNode.style.transform = `translateY(${start * ROW_HEIGHT}px)`;
    windowNode.replaceChildren();
    for (const [offset, customer] of visible.entries()) {
      const row = element(document, "div", { className: "virtual-row" });
      row.dataset.customerId = customer.id;
      const link = element(document, "a", { text: customer.name });
      link.href = `/customers/${customer.id}`;
      link.addEventListener("click", (event) => { event.preventDefault(); void router.navigate(link.pathname); });
      const statusLabel = customer.id === "cus_decoy_03" && pulse % 2 === 1 ? "paused" : customer.status;
      row.append(link, element(document, "span", { text: customer.email }), element(document, "span", { text: statusLabel }));
      windowNode.append(row);
      void offset;
    }
  };
  viewport.addEventListener("scroll", render);
  if (document.defaultView?.navigator.userAgent.includes("jsdom") !== true) {
    const timer = document.defaultView?.setInterval(() => {
      if (!viewport.isConnected) {
        if (timer !== undefined) document.defaultView?.clearInterval(timer);
        return;
      }
      pulse += 1;
      render();
    }, 2000);
  }
  render();
  return viewport;
}

export async function customerDetailPage(document: Document, id: string, api: NorthstarApi): Promise<HTMLElement> {
  const customer = await api.customer(id);
  const page = element(document, "section", { className: "page" });
  page.append(pageHeader(document, "Customer profile", customer.name, `${customer.email} · Joined ${customer.joinedAt}`));
  const card = element(document, "article", { className: "detail-card" });
  const form = element(document, "form", { className: "priority-form", ariaLabel: "Update customer priority" });
  const feedback = element(document, "div", { className: "priority-feedback" });
  let priority = customer.priority;
  const picker = listbox(document, {
    label: "Customer priority",
    value: priority,
    choices: [["low", "Low"], ["normal", "Normal"], ["high", "High"]],
    onChange: (value) => { priority = value as Priority; },
  });
  const save = element(document, "button", { text: "Save priority" });
  save.type = "submit";
  form.append(picker, save);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    save.disabled = true;
    feedback.replaceChildren();
    void api.updatePriority(id, priority)
      .then(() => {
        feedback.replaceChildren(status(document, "Priority saved"));
        showToast(document, "Priority saved for this account.");
      })
      .catch((error: unknown) => {
        const message = error instanceof Error ? error.message : "Priority update failed.";
        feedback.replaceChildren(element(document, "p", { className: "error-panel", text: message }));
      })
      .finally(() => { save.disabled = false; });
  });
  card.append(element(document, "h2", { text: "Account details" }), form, feedback);
  page.append(card);
  return page;
}
