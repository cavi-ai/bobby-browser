#!/usr/bin/env node
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  DOCUMENTED_VERSION,
  OUTPUT_REL,
  PRODUCT_ID,
  VERSIONED_REPO_DOCS,
  assertNavigationResolves,
  computeContentSha256,
  findStaleVersionReferences,
  listFilesRecursive,
  readRepoDoc,
  resolveReleaseIdentity,
} from "./lib.mjs";

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const COMMIT_SHA = /^[a-f0-9]{40}$/u;

export async function verifyBobbyBrowserDocs(root = REPO_ROOT, releaseInput) {
  const release = releaseInput ? resolveReleaseIdentity(releaseInput) : null;
  for (const relativePath of VERSIONED_REPO_DOCS) {
    const text = await readRepoDoc(root, relativePath);
    if (text === null) continue;
    const stale = findStaleVersionReferences(text);
    if (stale.length > 0) {
      throw new Error(
        `${relativePath} names a version other than ${DOCUMENTED_VERSION}: ${stale.join(", ")}`,
      );
    }
  }
  const artifactRoot = path.join(root, OUTPUT_REL);
  const manifestPath = path.join(artifactRoot, "manifest.json");
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));

  if (manifest.package !== PRODUCT_ID && manifest.product !== PRODUCT_ID) {
    throw new Error(`manifest identity must be ${PRODUCT_ID}`);
  }
  if (manifest.version !== DOCUMENTED_VERSION) {
    throw new Error(
      `manifest.version ${manifest.version} != ${DOCUMENTED_VERSION}`,
    );
  }
  if (typeof manifest.contentSha256 !== "string" || !manifest.contentSha256) {
    throw new Error("manifest.contentSha256 is required");
  }
  if (manifest.schemaVersion !== 1) throw new Error("manifest.schemaVersion must be 1");
  if (!manifest.release || typeof manifest.release !== "object") {
    throw new Error("manifest.release provenance is required");
  }
  if (!COMMIT_SHA.test(manifest.release.commit ?? "")) {
    throw new Error("manifest.release.commit must be a full lowercase SHA");
  }
  if (manifest.release.tag !== `v${manifest.version}`) {
    throw new Error("manifest.release.tag must match version");
  }
  if (release && (manifest.release.tag !== release.tag || manifest.release.commit !== release.commit || manifest.generatedAt !== release.generatedAt)) {
    throw new Error("manifest release provenance does not match expected release identity");
  }

  await assertNavigationResolves(artifactRoot);

  const relativePaths = await listFilesRecursive(artifactRoot, artifactRoot);
  const actual = await computeContentSha256(artifactRoot, relativePaths);
  if (actual !== manifest.contentSha256) {
    throw new Error(
      `contentSha256 mismatch: manifest=${manifest.contentSha256} actual=${actual}`,
    );
  }

  return { artifactRoot, manifest, contentSha256: actual };
}

const isMain =
  process.argv[1] !== undefined &&
  import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href;

if (isMain) {
  const values = Object.fromEntries(process.argv.slice(2).reduce((entries, option, index, args) => {
    if (option.startsWith("--") && args[index + 1] && !args[index + 1].startsWith("--")) {
      entries.push([option.slice(2), args[index + 1]]);
    }
    return entries;
  }, []));
  const releaseInput = Object.keys(values).length === 0 ? undefined : {
    ...(values.version !== undefined ? { version: values.version } : {}),
    ...(values.tag !== undefined ? { tag: values.tag } : {}),
    ...(values.commit !== undefined ? { commit: values.commit } : {}),
    ...(values["source-date-epoch"] !== undefined
      ? { sourceDateEpoch: Number(values["source-date-epoch"]) }
      : {}),
  };
  verifyBobbyBrowserDocs(REPO_ROOT, releaseInput)
    .then(({ contentSha256 }) => {
      console.log(`verified contentSha256=${contentSha256}`);
    })
    .catch((error) => {
      console.error(error instanceof Error ? error.message : error);
      process.exitCode = 1;
    });
}
