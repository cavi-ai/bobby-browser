import { ApiError, type NorthstarApi } from "../api.js";
import { element, pageHeader, status } from "../components.js";
import type { BillingCycle, OnboardingInput, Plan, RunConfig } from "../models.js";

export async function onboardingPage(document: Document, api: NorthstarApi, config: RunConfig): Promise<HTMLElement> {
  const page = element(document, "section", { className: "page" });
  page.append(pageHeader(document, "New relationship", "Onboard a customer", "Capture the essentials once, review them clearly, and create a durable customer record."));
  const form = element(document, "form", { className: "workflow-card", ariaLabel: "Customer onboarding" });
  form.noValidate = true;
  const fields = element(document, "div", { className: "form-grid" });
  const fullName = inputField(document, "Full name", "text", "name");
  const email = inputField(document, "Work email", "email", "email");
  const companyName = inputField(document, "Company name", "text", "organization");
  const postalCode = inputField(document, "Postal code", "text", "postal-code");
  const plan = selectField(document, "Plan", [["starter", "Starter"], ["growth", "Growth"], ["scale", "Scale"]]);
  const billingSlot = element(document, "div", { className: "field-slot" });
  const errors = element(document, "div", { className: "error-summary" });
  errors.setAttribute("role", "alert");
  errors.tabIndex = -1;
  const identityFields = config.level === 2 && config.traps.reversedIdentityFields
    ? [email.label, fullName.label]
    : [fullName.label, email.label];
  fields.append(...identityFields, companyName.label, postalCode.label, plan.label, billingSlot);
  let confirmEmail: HTMLInputElement | undefined;
  if (config.level === 2) {
    const delayedSlot = element(document, "div", { className: "field-slot delayed-control" });
    fields.append(delayedSlot);
    document.defaultView?.setTimeout(() => {
      const confirmation = inputField(document, "Confirm work email", "email", "email");
      confirmEmail = confirmation.input;
      delayedSlot.replaceChildren(confirmation.label);
    }, config.traps.delayedControlMs);
  }
  const submit = element(document, "button", { text: "Create customer" });
  submit.type = "submit";
  let recaptcha: HTMLElement | undefined;
  if (config.level === 2 && config.recaptchaSiteKey) {
    recaptcha = element(document, "div", { className: "g-recaptcha" });
    recaptcha.dataset.sitekey = config.recaptchaSiteKey;
    ensureRecaptchaScript(document);
  }
  form.append(errors, fields);
  if (recaptcha !== undefined) form.append(recaptcha);
  form.append(submit);
  page.append(form);

  const renderBilling = () => {
    billingSlot.replaceChildren();
    if (plan.select.value === "starter") return;
    billingSlot.append(selectField(document, "Billing cycle", [["monthly", "Monthly"], ["annual", "Annual"]]).label);
  };
  plan.select.addEventListener("change", renderBilling);
  renderBilling();

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    errors.replaceChildren();
    for (const field of form.querySelectorAll("[aria-invalid='true']")) field.removeAttribute("aria-invalid");
    const billing = form.querySelector<HTMLSelectElement>("select[aria-label='Billing cycle']")?.value ?? "monthly";
    if (confirmEmail !== undefined && confirmEmail.value !== email.input.value) {
      confirmEmail.setAttribute("aria-invalid", "true");
      errors.append(element(document, "p", { text: "The confirmation email must match the work email." }));
      confirmEmail.focus();
      return;
    }
    const recaptchaResponse = recaptcha?.querySelector<HTMLTextAreaElement>("[name='g-recaptcha-response']")?.value;
    if (config.level === 2 && !recaptchaResponse) {
      errors.append(element(document, "p", { text: "Complete the reCAPTCHA challenge before creating the customer." }));
      errors.focus();
      return;
    }
    const input: OnboardingInput = {
      fullName: fullName.input.value,
      email: email.input.value,
      companyName: companyName.input.value,
      postalCode: postalCode.input.value,
      plan: plan.select.value as Plan,
      billingCycle: billing as BillingCycle,
    };
    submit.disabled = true;
    void api.onboard(input, recaptchaResponse).then((receipt) => {
      form.replaceChildren(status(document, `Customer created · ${receipt.id}`));
    }).catch((error: unknown) => {
      if (error instanceof ApiError) {
        errors.append(element(document, "p", { text: error.message }));
        for (const [name, message] of Object.entries(error.fields)) {
          const target = name === "postalCode" ? postalCode.input : form.querySelector<HTMLElement>(`[name='${name}']`);
          target?.setAttribute("aria-invalid", "true");
          errors.append(element(document, "p", { text: message }));
        }
        const first = form.querySelector<HTMLElement>("[aria-invalid='true']");
        first?.focus();
      } else {
        errors.append(element(document, "p", { text: "Onboarding could not be completed." }));
      }
    }).finally(() => { submit.disabled = false; });
  });
  return page;
}

function ensureRecaptchaScript(document: Document): void {
  const source = "https://www.google.com/recaptcha/api.js";
  if (document.querySelector(`script[src='${source}']`) !== null) return;
  const script = document.createElement("script");
  script.src = source;
  script.async = true;
  script.defer = true;
  document.head.append(script);
}

function inputField(document: Document, labelText: string, type: string, autocomplete: string): { label: HTMLLabelElement; input: HTMLInputElement } {
  const label = element(document, "label", { text: labelText });
  const input = element(document, "input", { ariaLabel: labelText });
  input.type = type;
  input.setAttribute("autocomplete", autocomplete);
  input.required = true;
  label.append(input);
  return { label, input };
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
