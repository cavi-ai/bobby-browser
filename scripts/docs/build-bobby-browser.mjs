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
  resolveReleaseIdentity,
  stampVersionTokens,
} from "./lib.mjs";

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");

async function stampTree(root) {
  const relativePaths = await listFilesRecursive(root, root);
  for (const relativePath of relativePaths) {
    if (!/\.(md|json)$/u.test(relativePath)) continue;
    const absolute = path.join(root, relativePath);
    const original = await readFile(absolute, "utf8");
    const stamped = stampVersionTokens(original);
    if (stamped !== original) {
      await writeFile(absolute, stamped, "utf8");
    }
  }
}

export async function buildBobbyBrowserDocs(root = REPO_ROOT, releaseInput) {
  const release = resolveReleaseIdentity(releaseInput);
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
  await stampTree(outputRoot);

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
    schemaVersion: 1,
    package: PRODUCT_ID,
    product: PRODUCT_ID,
    version: DOCUMENTED_VERSION,
    contentSha256,
    publicBasePath: `/docs/${PRODUCT_ID}/v${DOCUMENTED_VERSION}`,
    stableAlias: `/docs/${PRODUCT_ID}`,
    release: { tag: release.tag, commit: release.commit },
    generatedAt: release.generatedAt,
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
  const values = Object.fromEntries(process.argv.slice(2).reduce((entries, option, index, args) => {
    if (option.startsWith("--") && args[index + 1] && !args[index + 1].startsWith("--")) {
      entries.push([option.slice(2), args[index + 1]]);
    }
    return entries;
  }, []));
  const releaseInput = {};
  if (values.version !== undefined) releaseInput.version = values.version;
  if (values.tag !== undefined) releaseInput.tag = values.tag;
  if (values.commit !== undefined) releaseInput.commit = values.commit;
  if (values["source-date-epoch"] !== undefined) {
    releaseInput.sourceDateEpoch = Number(values["source-date-epoch"]);
  }
  buildBobbyBrowserDocs(REPO_ROOT, releaseInput)
    .then(({ outputRoot, manifest }) => {
      console.log(`built ${outputRoot}`);
      console.log(`contentSha256=${manifest.contentSha256}`);
    })
    .catch((error) => {
      console.error(error instanceof Error ? error.message : error);
      process.exitCode = 1;
    });
}
