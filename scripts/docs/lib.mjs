import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { readdir, readFile, stat } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");

function workspacePackageVersion() {
  const manifest = readFileSync(path.join(REPO_ROOT, "Cargo.toml"), "utf8");
  const match = manifest.match(/\[workspace\.package\][^[]*?\bversion\s*=\s*"([^"]+)"/s);
  if (!match) throw new Error("workspace package version not found in Cargo.toml");
  return match[1];
}

function rootPackageJsonVersion() {
  const manifest = JSON.parse(readFileSync(path.join(REPO_ROOT, "package.json"), "utf8"));
  if (typeof manifest.version !== "string" || !manifest.version) {
    throw new Error("package.json version is required");
  }
  return manifest.version;
}

function readInterfaceVersion() {
  const ts = readFileSync(
    path.join(REPO_ROOT, "packages/typescript-sdk/src/contracts.ts"),
    "utf8",
  );
  const rust = readFileSync(path.join(REPO_ROOT, "crates/types/src/interface.rs"), "utf8");
  const tsMatch = ts.match(/export const INTERFACE_VERSION = "([^"]+)"\s+as const/);
  const rustMatch = rust.match(
    /pub const CURRENT_INTERFACE_VERSION:\s*&str\s*=\s*"([^"]+)"/,
  );
  if (!tsMatch) throw new Error("INTERFACE_VERSION not found in typescript-sdk contracts");
  if (!rustMatch) throw new Error("CURRENT_INTERFACE_VERSION not found in types::interface");
  if (tsMatch[1] !== rustMatch[1]) {
    throw new Error(
      `interface version drift: typescript=${tsMatch[1]} rust=${rustMatch[1]}`,
    );
  }
  return tsMatch[1];
}

const cargoVersion = workspacePackageVersion();
const npmVersion = rootPackageJsonVersion();
if (cargoVersion !== npmVersion) {
  throw new Error(
    `package version drift: Cargo.toml=${cargoVersion} package.json=${npmVersion}`,
  );
}

export const PRODUCT_ID = "bobby-browser";
export const DOCUMENTED_VERSION = cargoVersion;
export const INTERFACE_VERSION = readInterfaceVersion();
export const SOURCE_REL = "docs/bobby-browser/source";
export const OUTPUT_REL = `docs/bobby-browser/v${DOCUMENTED_VERSION}`;
export const PRODUCT_VERSION_TOKEN = "{{PRODUCT_VERSION}}";
export const INTERFACE_VERSION_TOKEN = "{{INTERFACE_VERSION}}";

const STABLE_VERSION = /^(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)$/u;
const COMMIT_SHA = /^[a-f0-9]{40}$/u;

function git(command, args) {
  return execFileSync("git", [command, ...args], {
    cwd: REPO_ROOT,
    encoding: "utf8",
  }).trim();
}

/** Release identity derived from workspace version + current git HEAD. */
export function defaultReleaseIdentity() {
  const commit = git("rev-parse", ["HEAD"]);
  if (!COMMIT_SHA.test(commit)) {
    throw new Error("git HEAD must resolve to a full lowercase SHA");
  }
  const sourceDateEpoch = Number(git("show", ["-s", "--format=%ct", commit]));
  if (!Number.isSafeInteger(sourceDateEpoch) || sourceDateEpoch < 0) {
    throw new Error("git commit timestamp must be a non-negative integer");
  }
  return Object.freeze({
    version: DOCUMENTED_VERSION,
    tag: `v${DOCUMENTED_VERSION}`,
    commit,
    sourceDateEpoch,
    generatedAt: new Date(sourceDateEpoch * 1000).toISOString(),
  });
}

/**
 * @param {Partial<{version:string,tag:string,commit:string,sourceDateEpoch:number}>|undefined|null} input
 */
export function resolveReleaseIdentity(input) {
  const defaults = defaultReleaseIdentity();
  const version = input?.version ?? defaults.version;
  const tag = input?.tag ?? `v${version}`;
  const commit = input?.commit ?? defaults.commit;
  const sourceDateEpoch = input?.sourceDateEpoch ?? defaults.sourceDateEpoch;

  if (version !== DOCUMENTED_VERSION || !STABLE_VERSION.test(version)) {
    throw new Error(`release version must be ${DOCUMENTED_VERSION}`);
  }
  if (tag !== `v${version}`) throw new Error("release tag must match version");
  if (typeof commit !== "string" || !COMMIT_SHA.test(commit)) {
    throw new Error("release commit must be a full lowercase SHA");
  }
  if (!Number.isSafeInteger(sourceDateEpoch) || sourceDateEpoch < 0) {
    throw new Error("source date epoch must be a non-negative integer");
  }
  return Object.freeze({
    version,
    tag,
    commit,
    sourceDateEpoch,
    generatedAt: new Date(sourceDateEpoch * 1000).toISOString(),
  });
}

/** Replace product/interface version tokens with live constants. */
export function stampVersionTokens(text) {
  return text
    .replaceAll(PRODUCT_VERSION_TOKEN, DOCUMENTED_VERSION)
    .replaceAll(INTERFACE_VERSION_TOKEN, INTERFACE_VERSION);
}

/** @param {string} root @param {string} directory */
export async function listFilesRecursive(root, directory) {
  /** @type {string[]} */
  const files = [];
  async function walk(current) {
    const entries = await readdir(current, { withFileTypes: true });
    for (const entry of entries) {
      const absolute = path.join(current, entry.name);
      if (entry.isDirectory()) {
        await walk(absolute);
        continue;
      }
      if (entry.isFile()) {
        files.push(path.relative(root, absolute).split(path.sep).join("/"));
      }
    }
  }
  await walk(directory);
  return files.sort();
}

/**
 * contentSha256: hash every artifact file except manifest.json,
 * in lexical path order, as path + NUL + bytes + NUL.
 * @param {string} artifactRoot absolute path to versioned directory
 * @param {string[]} relativePaths paths relative to artifactRoot
 */
export async function computeContentSha256(artifactRoot, relativePaths) {
  const hash = createHash("sha256");
  const paths = relativePaths
    .filter((relativePath) => relativePath !== "manifest.json")
    .sort();
  for (const relativePath of paths) {
    const bytes = await readFile(path.join(artifactRoot, relativePath));
    hash.update(relativePath);
    hash.update("\0");
    hash.update(bytes);
    hash.update("\0");
  }
  return hash.digest("hex");
}

/** @param {unknown} navigation */
export function collectNavigationPagePaths(navigation) {
  if (!navigation || typeof navigation !== "object") {
    throw new Error("navigation.json must be an object");
  }
  const sections = /** @type {{ sections?: unknown }} */ (navigation).sections;
  if (!Array.isArray(sections)) {
    throw new Error("navigation.json missing sections array");
  }
  /** @type {string[]} */
  const paths = [];
  for (const section of sections) {
    if (!section || typeof section !== "object") continue;
    const record = /** @type {{ path?: unknown, pages?: unknown }} */ (section);
    if (typeof record.path === "string") paths.push(record.path);
    if (Array.isArray(record.pages)) {
      for (const page of record.pages) {
        if (page && typeof page === "object" && typeof page.path === "string") {
          paths.push(page.path);
        }
      }
    }
  }
  return paths;
}

/** @param {string} artifactRoot */
export async function assertNavigationResolves(artifactRoot) {
  const navigation = JSON.parse(
    await readFile(path.join(artifactRoot, "navigation.json"), "utf8"),
  );
  const pagePaths = collectNavigationPagePaths(navigation);
  for (const pagePath of pagePaths) {
    const absolute = path.join(artifactRoot, pagePath);
    try {
      const info = await stat(absolute);
      if (!info.isFile()) {
        throw new Error(`navigation path is not a file: ${pagePath}`);
      }
    } catch (error) {
      if (error && typeof error === "object" && "code" in error && error.code === "ENOENT") {
        throw new Error(`navigation path missing: ${pagePath}`);
      }
      throw error;
    }
  }
  return pagePaths;
}
