import { createHash } from "node:crypto";
import { readdir, readFile, stat } from "node:fs/promises";
import path from "node:path";

export const PRODUCT_ID = "bobby-browser";
export const DOCUMENTED_VERSION = "0.2.1";
export const SOURCE_REL = "docs/bobby-browser/source";
export const OUTPUT_REL = `docs/bobby-browser/v${DOCUMENTED_VERSION}`;
const STABLE_VERSION = /^(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)$/u;
const COMMIT_SHA = /^[a-f0-9]{40}$/u;

export function resolveReleaseIdentity(input) {
  if (!input || typeof input !== "object") throw new Error("release identity is required");
  if (input.version !== DOCUMENTED_VERSION || !STABLE_VERSION.test(input.version)) {
    throw new Error(`release version must be ${DOCUMENTED_VERSION}`);
  }
  if (input.tag !== `v${input.version}`) throw new Error("release tag must match version");
  if (typeof input.commit !== "string" || !COMMIT_SHA.test(input.commit)) {
    throw new Error("release commit must be a full lowercase SHA");
  }
  if (!Number.isSafeInteger(input.sourceDateEpoch) || input.sourceDateEpoch < 0) {
    throw new Error("source date epoch must be a non-negative integer");
  }
  return Object.freeze({
    version: input.version,
    tag: input.tag,
    commit: input.commit,
    sourceDateEpoch: input.sourceDateEpoch,
    generatedAt: new Date(input.sourceDateEpoch * 1000).toISOString(),
  });
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
