export const CURSOR_ISOLATION = "inline-mcp,no-setting-sources";

export function isGrokModelId(id: string): boolean {
  return /grok/i.test(id);
}

export function resolveGrokModel(
  requested: string | undefined,
  available: readonly { id: string }[],
): string {
  if (requested && requested !== "default") {
    if (!isGrokModelId(requested)) {
      throw new Error(
        `cursor driver requires a Grok model; refusing ${requested}`,
      );
    }
    if (available.length > 0 && !available.some((model) => model.id === requested)) {
      throw new Error(`Grok model ${requested} is not available on this account`);
    }
    return requested;
  }
  const grokIds = available
    .map((model) => model.id)
    .filter(isGrokModelId)
    .sort();
  const selected = grokIds.at(-1);
  if (!selected) {
    throw new Error("no Grok model is available; refusing to fall back");
  }
  return selected;
}

export function mcpJsonToCursorServers(
  mcpConfig: {
    mcpServers?: Record<
      string,
      { command?: string; args?: string[]; env?: Record<string, string> }
    >;
  },
): Record<
  string,
  { type: "stdio"; command: string; args?: string[]; env?: Record<string, string> }
> {
  const servers: Record<
    string,
    { type: "stdio"; command: string; args?: string[]; env?: Record<string, string> }
  > = {};
  for (const [name, server] of Object.entries(mcpConfig.mcpServers ?? {})) {
    if (typeof server.command !== "string" || server.command.length === 0) {
      continue;
    }
    servers[name] = {
      type: "stdio",
      command: server.command,
      ...(server.args ? { args: server.args } : {}),
      ...(server.env ? { env: server.env } : {}),
    };
  }
  return servers;
}

export function buildCursorAgentOptions(input: {
  workDir: string;
  model: string;
  mcpServers: ReturnType<typeof mcpJsonToCursorServers>;
  apiKey?: string;
}) {
  return {
    ...(input.apiKey ? { apiKey: input.apiKey } : {}),
    model: { id: input.model },
    local: {
      cwd: input.workDir,
      settingSources: [],
    },
    mcpServers: input.mcpServers,
  };
}

export async function probeMlx(
  url = "http://127.0.0.1:9101",
  fetchImpl: typeof fetch = fetch,
): Promise<boolean> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 2_000);
  try {
    const response = await fetchImpl(url, { signal: controller.signal });
    return true;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

export const VISION_ASSIST_PROMPT = `
This run has local MLX vision configured. When you call workflow_start or session_create, set executionPolicy.visionAssist=true. Do not skip that grant; the cap alone is not enough.`;

export function bobbyGauntletToml(vision: boolean): string {
  const base = [
    "[browser]",
    'upload_roots = ["./data/uploads"]',
    'downloads_dir = "./downloads"',
    'artifacts_dir = "./artifacts"',
    'profiles_dir = "./profiles"',
    "headless = true",
    "",
    "[http]",
    "allow_loopback = true",
    "allow_private_network = false",
    "max_redirects = 5",
    "max_header_bytes = 65536",
    "max_body_bytes = 8388608",
    "max_download_bytes = 67108864",
    "request_timeout_ms = 30000",
    "max_concurrent_requests = 8",
    "",
    "[mcp]",
    'startup_toolset = "explore"',
    "",
  ];
  if (!vision) {
    return base.join("\n");
  }
  return [
    ...base,
    "[vision]",
    'provider = "mlx"',
    'endpoint_url = "http://127.0.0.1:9100/vision"',
    'token_env = "BOBBY_VISION_TOKEN"',
    "timeout_ms = 120000",
    "",
    "[vision.providers.mlx]",
    'base_url = "http://127.0.0.1:9101"',
    'model = "mlx-community/Qwen3.5-27B-4bit"',
    "",
    "[nodes.vision]",
    'kind = "vision"',
    'endpoint_url = "http://127.0.0.1:9100/vision"',
    'token_env = "BOBBY_VISION_TOKEN"',
    "",
  ].join("\n");
}
