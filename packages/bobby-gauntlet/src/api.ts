import type {
  CustomerDetail,
  CustomerSummary,
  DashboardSummary,
  DocumentReceipt,
  IntegrationState,
  OnboardingInput,
  OnboardingReceipt,
  Priority,
  ReportInput,
  ReportState,
} from "./models.js";

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly fields: Record<string, string> = {},
  ) {
    super(message);
    this.name = "ApiError";
  }
}

interface ErrorPayload {
  code?: unknown;
  message?: unknown;
  fields?: unknown;
}

export class NorthstarApi {
  constructor(
    readonly runId: string,
    private readonly fetcher: typeof fetch = fetch,
  ) {}

  dashboard(): Promise<DashboardSummary> {
    return this.request("/api/dashboard");
  }

  customers(query = ""): Promise<CustomerSummary[]> {
    return this.request(`/api/customers?q=${encodeURIComponent(query)}`);
  }

  customer(id: string): Promise<CustomerDetail> {
    return this.request(`/api/customers/${encodeURIComponent(id)}`);
  }

  updatePriority(id: string, priority: Priority): Promise<CustomerDetail> {
    return this.request(`/api/customers/${encodeURIComponent(id)}/priority`, {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ priority }),
    });
  }

  onboard(input: OnboardingInput): Promise<OnboardingReceipt> {
    return this.request("/api/onboarding", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(input),
    });
  }

  uploadDocument(customerId: string, file: File): Promise<DocumentReceipt> {
    const body = new FormData();
    body.set("customerId", customerId);
    body.set("document", file);
    return this.request("/api/documents", { method: "POST", body });
  }

  integrationState(): Promise<IntegrationState> {
    return this.request("/api/integrations/ledger-cloud");
  }

  completeAuthorization(code: string): Promise<IntegrationState> {
    return this.request("/api/integrations/ledger-cloud/complete", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ code }),
    });
  }

  createReport(input: ReportInput): Promise<ReportState> {
    return this.request("/api/reports", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(input),
    });
  }

  report(id: string): Promise<ReportState> {
    return this.request(`/api/reports/${encodeURIComponent(id)}`);
  }

  private async request<T>(path: string, init: RequestInit = {}): Promise<T> {
    const headers = new Headers(init.headers);
    headers.set("x-northstar-run", this.runId);
    const response = await this.fetcher(new URL(path, "https://northstar.test"), { ...init, headers });
    const isJson = response.headers.get("content-type")?.includes("application/json") === true;
    const payload: unknown = isJson ? await response.json() : undefined;
    if (!response.ok) throw toApiError(response.status, payload);
    if (!isJson) throw new ApiError(response.status, "invalid_response", "The server returned a non-JSON success response.");
    return payload as T;
  }
}

function toApiError(status: number, payload: unknown): ApiError {
  const error = isRecord(payload) ? payload as ErrorPayload : {};
  const code = typeof error.code === "string" ? error.code : "http_error";
  const message = typeof error.message === "string" ? error.message : `Request failed with status ${status}.`;
  const fields = isStringRecord(error.fields) ? error.fields : {};
  return new ApiError(status, code, message, fields);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isStringRecord(value: unknown): value is Record<string, string> {
  return isRecord(value) && Object.values(value).every((entry) => typeof entry === "string");
}
