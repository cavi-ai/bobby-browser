import { createApiClient, createRuntimeProviderRegistry, type CapabilityClient, type RuntimeProviderModule } from "@cavi-ai/api-client";
import { createClaudeProviderModule } from "@cavi-ai/api-client/providers/claude";
import { createCodexProviderModule } from "@cavi-ai/api-client/providers/codex";
import { createAgyProviderModule } from "@cavi-ai/api-client/providers/agy";
import { createOpenCodeProviderModule } from "@cavi-ai/api-client/providers/opencode";

export function required(env: NodeJS.ProcessEnv, name: string): string {
  const value = env[name];
  if (!value?.trim()) throw new Error(`Set ${name}`);
  return value;
}

export function createModelClient(env: NodeJS.ProcessEnv): CapabilityClient {
  const provider = required(env, "CAVI_PROVIDER");
  const common = { baseUrl: env.CAVI_BASE_URL, defaultModel: env.CAVI_MODEL, defaultTimeoutMs: 10_000 };
  let module: RuntimeProviderModule;
  switch (provider) {
    case "claude":
      module = createClaudeProviderModule({ ...common, apiKey: required(env, "CAVI_API_KEY"),
        defaultModel: required(env, "CAVI_MODEL"), defaultMaxTokens: 512 });
      break;
    case "codex":
      module = createCodexProviderModule({ ...common, apiKey: required(env, "CAVI_API_KEY"),
        defaultModel: required(env, "CAVI_MODEL") });
      break;
    case "opencode":
      module = createOpenCodeProviderModule({ ...common, baseUrl: required(env, "CAVI_BASE_URL"),
        scope: { directory: required(env, "CAVI_DIRECTORY") },
        username: env.CAVI_USERNAME, password: env.CAVI_PASSWORD });
      break;
    case "agy":
      module = createAgyProviderModule({ ...common, baseUrl: required(env, "CAVI_BASE_URL"), apiKey: env.CAVI_API_KEY });
      break;
    case "hermes":
    case "openclaw":
      return createApiClient(provider, { baseUrl: required(env, "CAVI_BASE_URL"),
        token: required(env, "CAVI_API_KEY"), defaultTimeoutMs: 10_000,
        ...(provider === "openclaw" ? { clientId: "cli", clientMode: "cli",
          requestedScopes: ["operator.read", "operator.write"] } : {}) });
    default:
      throw new Error("CAVI_PROVIDER must be claude, codex, openclaw, hermes, opencode or agy");
  }
  return createApiClient(provider, { registry: createRuntimeProviderRegistry({ modules: [module] }) });
}
