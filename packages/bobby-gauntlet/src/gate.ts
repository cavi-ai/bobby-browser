import type { NorthstarApi } from "./api.js";
import { element, pageHeader } from "./components.js";

export function cookieBanner(document: Document, api: NorthstarApi, onResolved: () => Promise<void>): HTMLElement {
  const backdrop = element(document, "div", { className: "consent-backdrop" });
  const dialog = element(document, "section", { className: "consent-dialog", ariaLabel: "Cookie preferences" });
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-modal", "true");
  dialog.append(
    element(document, "h2", { text: "Cookies on Northstar Ops" }),
    element(document, "p", { text: "We use required cookies to keep this operator session intact. Optional cookies measure workspace performance." }),
  );
  const actions = element(document, "div", { className: "consent-actions" });
  const accept = element(document, "button", { text: "Accept all cookies", ariaLabel: "Accept all cookies" });
  const reject = element(document, "button", { text: "Reject non-essential", ariaLabel: "Reject non-essential" });
  reject.className = "secondary";
  accept.addEventListener("click", () => { void api.setConsent("accept").then(onResolved); });
  reject.addEventListener("click", () => { void api.setConsent("reject").then(onResolved); });
  actions.append(accept, reject);
  dialog.append(actions);
  backdrop.append(dialog);
  return backdrop;
}

export function signInPage(document: Document, api: NorthstarApi, onAuthenticated: () => Promise<void>): HTMLElement {
  const page = element(document, "section", { className: "page auth-page" });
  page.append(pageHeader(document, "Operator access", "Sign in", "Use the seeded operator account, then the multi-factor code from the run snapshot."));
  const form = element(document, "form", { className: "workflow-card", ariaLabel: "Operator sign in" });
  const email = labeledInput(document, "Work email", "email");
  const password = labeledInput(document, "Password", "password");
  const errors = element(document, "div", { className: "error-summary" });
  errors.setAttribute("role", "alert");
  const submit = element(document, "button", { text: "Continue" });
  submit.type = "submit";
  form.append(errors, email.label, password.label, submit);
  page.append(form);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    errors.replaceChildren();
    submit.disabled = true;
    void api.login(email.input.value, password.input.value).then(() => {
      page.replaceChildren(mfaForm(document, api, onAuthenticated));
    }).catch((error: unknown) => {
      errors.append(element(document, "p", { text: error instanceof Error ? error.message : "Sign in failed." }));
    }).finally(() => { submit.disabled = false; });
  });
  return page;
}

function mfaForm(document: Document, api: NorthstarApi, onAuthenticated: () => Promise<void>): HTMLElement {
  const form = element(document, "form", { className: "workflow-card", ariaLabel: "Multi-factor authentication" });
  const code = labeledInput(document, "Authentication code", "text");
  code.input.inputMode = "numeric";
  const errors = element(document, "div", { className: "error-summary" });
  errors.setAttribute("role", "alert");
  const submit = element(document, "button", { text: "Verify code" });
  submit.type = "submit";
  form.append(
    element(document, "h2", { text: "Check your authenticator" }),
    errors,
    code.label,
    submit,
  );
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    errors.replaceChildren();
    submit.disabled = true;
    void api.verifyMfa(code.input.value).then(onAuthenticated).catch((error: unknown) => {
      errors.append(element(document, "p", { text: error instanceof Error ? error.message : "That code is not valid." }));
    }).finally(() => { submit.disabled = false; });
  });
  return form;
}

function labeledInput(document: Document, labelText: string, type: string): { label: HTMLLabelElement; input: HTMLInputElement } {
  const label = element(document, "label", { text: labelText });
  const input = element(document, "input", { ariaLabel: labelText });
  input.type = type;
  label.append(input);
  return { label, input };
}
