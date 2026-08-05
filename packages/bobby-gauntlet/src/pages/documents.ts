import type { NorthstarApi } from "../api.js";
import { element, pageHeader, status } from "../components.js";

export async function documentsPage(document: Document, customerId: string, api: NorthstarApi): Promise<HTMLElement> {
  const page = element(document, "section", { className: "page" });
  page.append(pageHeader(document, "Customer records", "Documents", "Upload source material and verify it in the embedded preview before confirming the record."));
  const card = element(document, "article", { className: "workflow-card" });
  const form = element(document, "form", { ariaLabel: "Upload customer document" });
  const label = element(document, "label", { text: "Customer document" });
  const input = element(document, "input", { ariaLabel: "Customer document" });
  input.type = "file";
  input.accept = "text/plain,application/pdf";
  label.append(input);
  const submit = element(document, "button", { text: "Upload document" });
  submit.type = "submit";
  const result = element(document, "div", { className: "document-result" });
  form.append(label, submit);
  card.append(form, result);
  page.append(card);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const file = input.files?.item(0);
    if (file === null || file === undefined) {
      result.replaceChildren(element(document, "p", { className: "error-panel", text: "Choose a document to upload." }));
      return;
    }
    submit.disabled = true;
    result.replaceChildren(status(document, "Uploading document"));
    void api.uploadDocument(customerId, file).then((receipt) => {
      const frame = element(document, "iframe");
      frame.id = "document-preview";
      frame.title = `Preview of ${receipt.filename}`;
      frame.src = receipt.previewUrl;
      result.replaceChildren(status(document, "Upload complete"), frame);
    }).catch(() => result.replaceChildren(element(document, "p", { className: "error-panel", text: "Document upload failed." }))).finally(() => { submit.disabled = false; });
  });
  return page;
}
