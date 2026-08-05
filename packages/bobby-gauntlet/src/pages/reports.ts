import type { NorthstarApi } from "../api.js";
import { element, pageHeader, status } from "../components.js";
import type { ReportState } from "../models.js";

const REPORT_DEADLINE_MS = 10_000;

export async function reportsPage(document: Document, api: NorthstarApi): Promise<HTMLElement> {
  const page = element(document, "section", { className: "page" });
  page.append(pageHeader(document, "Operational intelligence", "Reports", "Generate a portable account report from the latest durable customer state."));
  const form = element(document, "form", { className: "workflow-card", ariaLabel: "Generate report" });
  const customer = select(document, "Customer", [["cus_atlas", "Atlas Labs"]]);
  const format = select(document, "Format", [["csv", "CSV"], ["pdf", "PDF"]]);
  const submit = element(document, "button", { text: "Generate report" });
  submit.type = "submit";
  const result = element(document, "div", { className: "report-result" });
  form.append(customer.label, format.label, submit, result);
  page.append(form);
  void api.latestReport()
    .then((report) => {
      if (report.status === "complete") result.replaceChildren(reportDownload(document, report));
    })
    .catch(() => undefined);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    submit.disabled = true;
    result.replaceChildren(status(document, "Generating report"));
    void api.createReport({ customerId: customer.select.value, format: format.select.value as "csv" | "pdf" })
      .then((report) => waitForReport(api, report))
      .then((report) => result.replaceChildren(reportDownload(document, report)))
      .catch(() => result.replaceChildren(element(document, "p", { className: "error-panel", text: "Report generation failed." })))
      .finally(() => { submit.disabled = false; });
  });
  return page;
}

async function waitForReport(api: NorthstarApi, initial: ReportState): Promise<ReportState> {
  let report = initial;
  const deadline = Date.now() + REPORT_DEADLINE_MS;
  while (report.status !== "complete") {
    if (Date.now() >= deadline) throw new Error("report generation timed out");
    report = await api.report(report.id);
  }
  return report;
}

function reportDownload(document: Document, report: ReportState): HTMLElement {
  if (report.downloadUrl === undefined || report.filename === undefined) throw new Error("complete report omitted download metadata");
  const wrapper = element(document, "div", { className: "download-card" });
  wrapper.append(status(document, "Report ready"));
  const link = element(document, "a", { text: `Download ${report.filename}` });
  link.href = report.downloadUrl;
  link.download = report.filename;
  wrapper.append(link);
  return wrapper;
}

function select(document: Document, labelText: string, options: ReadonlyArray<readonly [string, string]>): { label: HTMLLabelElement; select: HTMLSelectElement } {
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
