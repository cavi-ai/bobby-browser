const VERSION = /^(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)$/u;
const COMMIT = /^[a-f0-9]{40}$/u;
const SHA256 = /^[a-f0-9]{64}$/u;
const REPOSITORY = "cavi-ai/bobby-browser";

export function createReleaseEnvelope(input) {
  if (!VERSION.test(input.version ?? "")) throw new Error("version must be an exact stable semantic version");
  if (input.tag !== `v${input.version}`) throw new Error("release tag must match version");
  if (input.repository !== REPOSITORY) throw new Error(`release repository must be ${REPOSITORY}`);
  if (!COMMIT.test(input.commit ?? "")) throw new Error("release commit must be a lowercase 40-character SHA");
  if (!SHA256.test(input.artifactSha256 ?? "")) throw new Error("artifact SHA-256 must be 64 lowercase hexadecimal characters");
  const artifactName = `bobby-browser-docs-${input.tag}.tar.gz`;
  return {
    schemaVersion: 1,
    slug: "bobby-browser",
    kind: "product-docs",
    version: input.version,
    tag: input.tag,
    repository: input.repository,
    commit: input.commit,
    artifact: {
      url: `https://github.com/${input.repository}/releases/download/${input.tag}/${artifactName}`,
      sha256: input.artifactSha256,
      format: "tar.gz",
    },
  };
}

if (process.argv[1] && import.meta.filename === process.argv[1]) {
  try {
    const values = Object.fromEntries(process.argv.slice(2).reduce((entries, option, index, args) => {
      if (option.startsWith("--") && args[index + 1] && !args[index + 1].startsWith("--")) entries.push([option.slice(2), args[index + 1]]);
      return entries;
    }, []));
    process.stdout.write(`${JSON.stringify(createReleaseEnvelope({
      version: values.version,
      tag: values.tag,
      repository: values.repository,
      commit: values.commit,
      artifactSha256: values["artifact-sha256"],
    }))}\n`);
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  }
}
