import type { NorthstarApi } from "../api.js";
import { element, pageHeader, status } from "../components.js";
import type { AppRouter } from "../router.js";
import type { CustomerSummary, Priority } from "../models.js";

export async function customersPage(document: Document, api: NorthstarApi, router: AppRouter): Promise<HTMLElement> {
  const page = element(document, "section", { className: "page" });
  page.append(pageHeader(document, "Customer operations", "Customers", "Find an account, understand its state, and move work forward."));
  const search = element(document, "form", { className: "search-panel", ariaLabel: "Customer search" });
  const label = element(document, "label", { text: "Search customers" });
  const input = element(document, "input", { ariaLabel: "Search customers" });
  input.type = "search";
  input.placeholder = "Name, company, or email";
  label.append(input);
  const submit = element(document, "button", { text: "Search" });
  submit.type = "submit";
  const results = element(document, "div", { className: "customer-results" });
  let searchGeneration = 0;
  search.append(label, submit);
  page.append(search, results);
  search.addEventListener("submit", (event) => {
    event.preventDefault();
    const generation = ++searchGeneration;
    results.replaceChildren(loading(document, "Searching customer records"));
    void api.customers(input.value).then((customers) => {
      if (generation === searchGeneration) results.replaceChildren(customerTable(document, customers, router));
    }).catch((error: unknown) => {
      if (generation !== searchGeneration) return;
      const detail = error instanceof Error ? ` ${error.message}` : "";
      results.replaceChildren(element(document, "p", { className: "error-panel", text: `Customer search failed.${detail}` }));
    });
  });
  return page;
}

function loading(document: Document, label: string): HTMLElement {
  const node = element(document, "div", { className: "skeleton-panel", text: label });
  node.setAttribute("aria-busy", "true");
  return node;
}

function customerTable(document: Document, customers: CustomerSummary[], router: AppRouter): HTMLElement {
  const table = element(document, "table", { className: "data-table" });
  const caption = element(document, "caption", { text: `${customers.length} customer result${customers.length === 1 ? "" : "s"}` });
  const body = element(document, "tbody");
  for (const customer of customers) {
    const row = element(document, "tr");
    const name = element(document, "td");
    const link = element(document, "a", { text: customer.name });
    link.href = `/customers/${customer.id}`;
    link.addEventListener("click", (event) => { event.preventDefault(); void router.navigate(link.pathname); });
    name.append(link);
    row.append(name, element(document, "td", { text: customer.email }), element(document, "td", { text: customer.priority }));
    body.append(row);
  }
  table.append(caption, body);
  return table;
}

export async function customerDetailPage(document: Document, id: string, api: NorthstarApi): Promise<HTMLElement> {
  const customer = await api.customer(id);
  const page = element(document, "section", { className: "page" });
  page.append(pageHeader(document, "Customer profile", customer.name, `${customer.email} · Joined ${customer.joinedAt}`));
  const card = element(document, "article", { className: "detail-card" });
  const form = element(document, "form", { className: "priority-form", ariaLabel: "Update customer priority" });
  const feedback = element(document, "div", { className: "priority-feedback" });
  const label = element(document, "label", { text: "Customer priority" });
  const select = element(document, "select", { ariaLabel: "Customer priority" });
  for (const priority of ["low", "normal", "high"] as const) {
    const option = element(document, "option", { text: priority[0]?.toUpperCase() + priority.slice(1) });
    option.value = priority;
    option.selected = customer.priority === priority;
    select.append(option);
  }
  label.append(select);
  const save = element(document, "button", { text: "Save priority" });
  save.type = "submit";
  form.append(label, save);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    save.disabled = true;
    feedback.replaceChildren();
    void api.updatePriority(id, select.value as Priority)
      .then(() => feedback.replaceChildren(status(document, "Priority saved")))
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
