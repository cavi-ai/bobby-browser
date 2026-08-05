export type Priority = "low" | "normal" | "high";
export type Plan = "starter" | "growth" | "scale";
export type BillingCycle = "monthly" | "annual";

export interface DashboardSummary {
  activeCustomers: number;
  pendingOnboarding: number;
  documentsProcessed: number;
  reportsReady: number;
}

export interface CustomerSummary {
  id: string;
  name: string;
  email: string;
  priority: Priority;
  status: "active" | "onboarding" | "paused";
}

export interface CustomerDetail extends CustomerSummary {
  company: string;
  joinedAt: string;
}

export interface OnboardingInput {
  fullName: string;
  email: string;
  companyName: string;
  postalCode: string;
  plan: Plan;
  billingCycle: BillingCycle;
}

export interface OnboardingReceipt {
  id: string;
  status: "complete";
}

export interface DocumentReceipt {
  id: string;
  customerId: string;
  filename: string;
  mediaType: string;
  sha256: string;
  previewUrl: string;
}

export interface IntegrationState {
  connected: boolean;
  identity?: string;
  authorizationUrl?: string;
}

export interface ReportInput {
  customerId: string;
  format: "csv" | "pdf";
}

export interface ReportState {
  id: string;
  status: "pending" | "complete";
  filename?: string;
  mediaType?: string;
  downloadUrl?: string;
  sha256?: string;
}
