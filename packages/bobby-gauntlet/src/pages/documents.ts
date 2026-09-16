import type { NorthstarApi } from "../api.js";
import { element, pageHeader, status } from "../components.js";

export async function documentsPage(document: Document, customerId: string, api: NorthstarApi): Promise<HTMLElement> {
  registerPreview(document);
  const page = element(document, "section", { className: "page" });
  page.append(pageHeader(document, "Customer records", "Documents", "Drop a file or use the picker, then confirm inside the preview widget."));
  const card = element(document, "article", { className: "workflow-card" });
  const form = element(document, "form", { ariaLabel: "Upload customer document" });
  const drop = element(document, "div", { className: "dropzone", text: "Drop a customer document here", ariaLabel: "Document dropzone" });
  drop.setAttribute("role", "group");
  const label = element(document, "label", { text: "Customer document" });
  const input = element(document, "input", { ariaLabel: "Customer document" });
  input.type = "file";
  input.accept = "text/plain,application/pdf";
  label.append(input);
  const submit = element(document, "button", { text: "Upload document" });
  submit.type = "submit";
  const result = element(document, "div", { className: "document-result" });
  form.append(drop, label, submit);
  card.append(form, result);
  page.append(card);
  let chosen: File | undefined;
  const takeFile = (file: File) => {
    chosen = file;
    drop.textContent = `Ready: ${file.name}`;
  };
  drop.addEventListener("dragover", (event) => { event.preventDefault(); });
  drop.addEventListener("drop", (event) => {
    event.preventDefault();
    const file = event.dataTransfer?.files.item(0);
    if (file) takeFile(file);
  });
  input.addEventListener("change", () => {
    const file = input.files?.item(0);
    if (file) takeFile(file);
  });
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const file = chosen ?? input.files?.item(0) ?? undefined;
    if (file === undefined) {
      result.replaceChildren(element(document, "p", { className: "error-panel", text: "Choose a document to upload." }));
      return;
    }
    submit.disabled = true;
    const notice = status(document, "Uploading document");
    result.replaceChildren(notice);
    void api.uploadDocument(customerId, file).then((receipt) => {
      const preview = document.createElement("northstar-preview");
      preview.id = "document-preview-widget";
      preview.setAttribute("role", "group");
      preview.setAttribute("aria-label", "Document preview widget");
      preview.setAttribute("title", `Preview of ${receipt.filename}`);
      preview.dataset.previewUrl = receipt.previewUrl;
      preview.dataset.confirmId = receipt.id;
      preview.addEventListener("northstar-confirmed", () => {
        result.append(status(document, "Document confirmed"));
      });
      notice.textContent = "Upload complete";
      result.append(preview);
    }).catch(() => result.replaceChildren(element(document, "p", { className: "error-panel", text: "Document upload failed." }))).finally(() => { submit.disabled = false; });
  });
  return page;
}

function registerPreview(document: Document): void {
  const hostWindow = document.defaultView;
  if (hostWindow === null || hostWindow.customElements.get("northstar-preview") !== undefined) return;
  definePreview(document, hostWindow as Window & typeof globalThis);
}

function definePreview(document: Document, hostWindow: Window & typeof globalThis): void {
  class NorthstarPreview extends hostWindow.HTMLElement {
    connectedCallback(): void {
      const root = this.attachShadow({ mode: "open" });
      const frame = document.createElement("iframe");
      frame.id = "document-preview";
      frame.title = this.getAttribute("title") ?? "Document preview";
      frame.src = this.dataset.previewUrl ?? "";
      const confirm = document.createElement("button");
      confirm.id = "confirm-preview";
      confirm.type = "button";
      confirm.setAttribute("aria-label", "Confirm document preview");
      confirm.textContent = "Confirm document";
      confirm.addEventListener("click", () => {
        const id = this.dataset.confirmId;
        if (id === undefined) return;
        const run = hostWindow.sessionStorage.getItem("northstar.run") ?? "";
        void hostWindow.fetch(`/api/documents/${id}/confirm`, {
          method: "POST",
          credentials: "include",
          headers: { "x-northstar-run": run },
        }).then((response) => {
          if (!response.ok) return;
          confirm.disabled = true;
          this.dispatchEvent(new hostWindow.CustomEvent("northstar-confirmed", { bubbles: true }));
        });
      });
      root.append(frame, confirm);
    }
  }
  hostWindow.customElements.define("northstar-preview", NorthstarPreview);
}
