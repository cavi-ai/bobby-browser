import { element } from "./components.js";
import type { RunConfig } from "./models.js";

export function levelTwoInterruption(document: Document, config: RunConfig): HTMLElement | null {
  if (config.level !== 2 || !config.traps.extraModal) return null;
  const backdrop = element(document, "div", { className: "interruption-backdrop" });
  const dialog = element(document, "section", {
    className: "interruption-dialog",
    ariaLabel: "Workflow interruption",
  });
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-modal", "true");
  dialog.append(
    element(document, "h2", { text: "Before you continue" }),
    element(document, "p", { text: "Review the account checkpoint, then return to this workflow." }),
  );
  const proceed = element(document, "button", { text: "Open checkpoint" });
  proceed.type = "button";
  proceed.addEventListener("click", () => {
    if (config.traps.extraPopup) {
      document.defaultView?.open(
        `/level-two-checkpoint?seed=${encodeURIComponent(config.seed)}`,
        "northstar-level-two-checkpoint",
        "popup,width=520,height=480",
      );
    }
    backdrop.remove();
  });
  dialog.append(proceed);
  backdrop.append(dialog);
  return backdrop;
}
