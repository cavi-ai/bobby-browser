#!/usr/bin/env node
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  DOCUMENTED_VERSION,
  OUTPUT_REL,
  PRODUCT_ID,
  assertNavigationResolves,
  computeContentSha256,
  listFilesRecursive,
} from "./lib.mjs";

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");

export async function verifyBobbyBrowserDocs(root = REPO_ROOT) {
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
  verifyBobbyBrowserDocs()
    .then(({ contentSha256 }) => {
      console.log(`verified contentSha256=${contentSha256}`);
    })
    .catch((error) => {
      console.error(error instanceof Error ? error.message : error);
      process.exitCode = 1;
    });
}
