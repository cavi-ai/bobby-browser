#!/usr/bin/env node
import { cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  DOCUMENTED_VERSION,
  OUTPUT_REL,
  PRODUCT_ID,
  SOURCE_REL,
  computeContentSha256,
  listFilesRecursive,
} from "./lib.mjs";

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");

export async function buildBobbyBrowserDocs(root = REPO_ROOT) {
  const sourceRoot = path.join(root, SOURCE_REL);
  const pagesRoot = path.join(sourceRoot, "pages");
  const outputRoot = path.join(root, OUTPUT_REL);

  await rm(outputRoot, { recursive: true, force: true });
  await mkdir(outputRoot, { recursive: true });

  await cp(pagesRoot, outputRoot, { recursive: true });
  await cp(
    path.join(sourceRoot, "navigation.json"),
    path.join(outputRoot, "navigation.json"),
  );

  const relativePaths = await listFilesRecursive(outputRoot, outputRoot);
  const contentSha256 = await computeContentSha256(outputRoot, relativePaths);

  const navigation = JSON.parse(
    await readFile(path.join(outputRoot, "navigation.json"), "utf8"),
  );
  if (navigation.version !== DOCUMENTED_VERSION) {
    throw new Error(
      `navigation.version ${navigation.version} != documented ${DOCUMENTED_VERSION}`,
    );
  }

  /** @type {Record<string, unknown>} */
  const manifest = {
    package: PRODUCT_ID,
    product: PRODUCT_ID,
    version: DOCUMENTED_VERSION,
    contentSha256,
    publicBasePath: `/docs/${PRODUCT_ID}/v${DOCUMENTED_VERSION}`,
    stableAlias: `/docs/${PRODUCT_ID}`,
  };

  await writeFile(
    path.join(outputRoot, "manifest.json"),
    `${JSON.stringify(manifest, null, 2)}\n`,
    "utf8",
  );

  return { outputRoot, manifest };
}

const isMain =
  process.argv[1] !== undefined &&
  import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href;

if (isMain) {
  buildBobbyBrowserDocs()
    .then(({ outputRoot, manifest }) => {
      console.log(`built ${outputRoot}`);
      console.log(`contentSha256=${manifest.contentSha256}`);
    })
    .catch((error) => {
      console.error(error instanceof Error ? error.message : error);
      process.exitCode = 1;
    });
}
