import { element } from "./components.js";

export function showToast(document: Document, message: string): void {
  document.querySelector(".toast-stack")?.remove();
  const stack = element(document, "div", { className: "toast-stack" });
  stack.setAttribute("role", "status");
  stack.append(element(document, "p", { text: message }));
  const dismiss = element(document, "button", { text: "Dismiss", ariaLabel: "Dismiss notification" });
  dismiss.addEventListener("click", () => stack.remove());
  stack.append(dismiss);
  document.querySelector(".app-shell")?.append(stack);
}

export function chatOverlay(document: Document): HTMLElement {
  const panel = element(document, "aside", { className: "chat-overlay", ariaLabel: "Workspace assistant" });
  panel.setAttribute("role", "dialog");
  panel.append(
    element(document, "p", { text: "Need help reconciling Ledger Cloud? Ask the assistant after you dismiss this." }),
  );
  const dismiss = element(document, "button", { text: "Not now", ariaLabel: "Dismiss workspace assistant" });
  dismiss.addEventListener("click", () => panel.remove());
  panel.append(dismiss);
  return panel;
}
