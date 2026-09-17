import type { NorthstarApi } from "../api.js";
import { element, pageHeader, status } from "../components.js";
import type { BillingAddress, BillingPeriod, Plan } from "../models.js";
import { combobox } from "../widgets.js";

export async function billingPage(document: Document, api: NorthstarApi): Promise<HTMLElement> {
  const page = element(document, "section", { className: "page" });
  page.append(pageHeader(document, "Revenue", "Checkout", "Bill Atlas Labs for the selected window after a 3-D Secure challenge in a nested frame."));
  const card = element(document, "form", { className: "workflow-card", ariaLabel: "Atlas checkout" });
  const errors = element(document, "div", { className: "error-summary" });
  errors.setAttribute("role", "alert");
  const plan = selectField(document, "Plan", [["growth", "Growth"], ["scale", "Scale"]]);
  let address: BillingAddress | undefined;
  const addressFields = element(document, "div", { className: "form-grid" });
  const street = readonlyField(document, "Street");
  const city = readonlyField(document, "City");
  const postal = readonlyField(document, "Postal code");
  addressFields.append(street.label, city.label, postal.label);
  const addressBox = combobox(document, {
    label: "Billing address",
    placeholder: "Type at least three letters",
    onQuery: async (value) => (await api.addresses(value)).map((item) => ({ id: item.label, label: item.label })),
    onSelect: (_id, label) => {
      void api.addresses(label).then((matches) => {
        address = matches.find((item) => item.label === label);
        street.input.value = address?.street ?? "";
        city.input.value = address?.city ?? "";
        postal.input.value = address?.postalCode ?? "";
      });
    },
  });
  const calendar = dateRange(document);
  const run = document.defaultView?.sessionStorage.getItem("northstar.run") ?? "";
  const cardFrame = element(document, "iframe", { ariaLabel: "Card details" });
  cardFrame.id = "card-frame";
  cardFrame.setAttribute("name", "card-frame");
  cardFrame.title = "Card details";
  cardFrame.src = `/pay/card?run=${encodeURIComponent(run)}`;
  const threeDsFrame = element(document, "iframe", { ariaLabel: "3-D Secure challenge" });
  threeDsFrame.id = "three-ds-frame";
  threeDsFrame.setAttribute("name", "three-ds-frame");
  threeDsFrame.title = "3-D Secure challenge";
  threeDsFrame.hidden = true;
  const submit = element(document, "button", { text: "Charge Atlas Labs" });
  submit.type = "submit";
  submit.disabled = true;
  const result = element(document, "div");
  card.append(errors, plan.label, addressBox, addressFields, calendar.root, cardFrame, threeDsFrame, submit, result);
  page.append(card);
  let threeDsComplete = false;
  document.defaultView?.addEventListener("message", (event) => {
    if (event.origin !== document.defaultView?.location.origin) return;
    if (!isRecord(event.data)) return;
    if (event.data.type === "northstar.card.tokenized") {
      threeDsFrame.hidden = false;
      threeDsFrame.src = `/pay/3ds?run=${encodeURIComponent(run)}`;
    }
    if (event.data.type === "northstar.3ds.complete") {
      threeDsComplete = true;
      submit.disabled = false;
    }
  });
  card.addEventListener("submit", (event) => {
    event.preventDefault();
    errors.replaceChildren();
    if (address === undefined || calendar.period() === undefined || !threeDsComplete) {
      errors.append(element(document, "p", { text: "Choose an address, a billing window, and finish 3-D Secure." }));
      return;
    }
    submit.disabled = true;
    void api.charge({ plan: plan.select.value as Plan, period: calendar.period() as BillingPeriod, address }).then((charge) => {
      result.replaceChildren(status(document, `Charged ${charge.amountCents} cents`));
    }).catch((error: unknown) => {
      errors.append(element(document, "p", { text: error instanceof Error ? error.message : "Charge failed." }));
      submit.disabled = false;
    });
  });
  return page;
}

function dateRange(document: Document): { root: HTMLElement; period: () => BillingPeriod | undefined } {
  const root = element(document, "div", { className: "calendar", ariaLabel: "Billing period" });
  const heading = element(document, "h2", { text: "January 2026" });
  const grid = element(document, "div", { className: "calendar-grid" });
  grid.setAttribute("role", "grid");
  grid.setAttribute("aria-label", "January 2026");
  let start: string | undefined;
  let end: string | undefined;
  for (let day = 1; day <= 31; day += 1) {
    const iso = `2026-01-${String(day).padStart(2, "0")}`;
    const cell = element(document, "button", { text: String(day), ariaLabel: iso });
    cell.type = "button";
    cell.setAttribute("role", "gridcell");
    cell.addEventListener("click", () => {
      if (start === undefined || (end !== undefined && start !== undefined)) {
        start = iso;
        end = undefined;
      } else if (iso < start) {
        end = start;
        start = iso;
      } else {
        end = iso;
      }
      for (const button of grid.querySelectorAll("button")) {
        const value = button.getAttribute("aria-label") ?? "";
        button.dataset.selected = start !== undefined && end !== undefined && value >= start && value <= end ? "true" : value === start ? "true" : "false";
      }
    });
    grid.append(cell);
  }
  root.append(heading, grid);
  return {
    root,
    period: () => start !== undefined && end !== undefined ? { start, end } : undefined,
  };
}

function selectField(document: Document, labelText: string, options: ReadonlyArray<readonly [string, string]>): { label: HTMLLabelElement; select: HTMLSelectElement } {
  const label = element(document, "label", { text: labelText });
  const select = element(document, "select", { ariaLabel: labelText });
  for (const [value, copy] of options) {
    const option = element(document, "option", { text: copy });
    option.value = value;
    select.append(option);
  }
  label.append(select);
  return { label, select };
}

function readonlyField(document: Document, labelText: string): { label: HTMLLabelElement; input: HTMLInputElement } {
  const label = element(document, "label", { text: labelText });
  const input = element(document, "input", { ariaLabel: labelText });
  input.readOnly = true;
  label.append(input);
  return { label, input };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
