const AUTH_SCHEME_CREDENTIAL = /(?:^|\s)(?:bearer|basic)\s+([A-Za-z0-9._~+/=-]{8,})/gi;
const KEY_VALUE_DISCLOSURE =
  /(?:password|passwd|secret|token|api[-_ ]?key|credentials?|authorization|private[-_ ]?(?:key|token|secret)|pairing[-_ ]?code)\s*[:=]\s*\S{4}/i;
const PEM_PRIVATE_KEY = /-----BEGIN[\s\S]*?PRIVATE KEY-----/;
const JWT = /eyJ[\w-]{7,}\.[\w-]{10,}\.[\w-]{10,}/;
const PREFIXED_TOKEN =
  /(?<![A-Za-z0-9])(?:sk-|sk_live_|sk_test_|rk_live_|ghp_|gho_|ghu_|ghs_|github_pat_|xox[abprs]-)[A-Za-z0-9_-]{16,}/;
const AWS_ACCESS_KEY = /(?<![A-Z0-9])AKIA[A-Z0-9]{16}/;
const GOOGLE_API_KEY = /AIza[A-Za-z0-9_-]{35}/;
const LONG_RUN = /[A-Za-z0-9+/_=-]{40,}/g;

function isCredentialShaped(token: string): boolean {
  if (/\d/.test(token) || token.endsWith("=")) return true;
  const rest = token.slice(1);
  return /[A-Z]/.test(rest) && /[a-z]/.test(token);
}

function hasAuthSchemeCredential(value: string): boolean {
  for (const match of value.matchAll(AUTH_SCHEME_CREDENTIAL)) {
    if (isCredentialShaped(match[1] ?? "")) return true;
  }
  return false;
}

function hasLongCredentialRun(value: string): boolean {
  for (const [run] of value.matchAll(LONG_RUN)) {
    if (/\d/.test(run) && /[A-Z]/.test(run) && /[a-z]/.test(run)) return true;
  }
  return false;
}

export function containsSecretMaterial(value: string): boolean {
  return (
    hasAuthSchemeCredential(value) ||
    KEY_VALUE_DISCLOSURE.test(value) ||
    PEM_PRIVATE_KEY.test(value) ||
    JWT.test(value) ||
    PREFIXED_TOKEN.test(value) ||
    AWS_ACCESS_KEY.test(value) ||
    GOOGLE_API_KEY.test(value) ||
    (!/^https?:\/\//i.test(value.trim()) && hasLongCredentialRun(value))
  );
}
