import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { buildBobbyBrowserDocs } from "./build-bobby-browser.mjs";
import { verifyBobbyBrowserDocs } from "./verify-bobby-browser.mjs";
import { OUTPUT_REL } from "./lib.mjs";

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");

test("build is deterministic for unchanged source", async () => {
  const first = await buildBobbyBrowserDocs(REPO_ROOT);
  const second = await buildBobbyBrowserDocs(REPO_ROOT);
  assert.equal(first.manifest.contentSha256, second.manifest.contentSha256);
  await verifyBobbyBrowserDocs(REPO_ROOT);
});

test("verify fails when a page is tampered", async () => {
  await buildBobbyBrowserDocs(REPO_ROOT);
  const page = path.join(
    REPO_ROOT,
    OUTPUT_REL,
    "introduction/overview.md",
  );
  const original = await readFile(page, "utf8");
  await writeFile(page, `${original}\n<!-- tamper -->\n`, "utf8");
  await assert.rejects(() => verifyBobbyBrowserDocs(REPO_ROOT), /contentSha256 mismatch/);
  await buildBobbyBrowserDocs(REPO_ROOT);
});

test("verify fails when navigation points at a missing page", async () => {
  const fixtureRoot = await mkdtemp(path.join(tmpdir(), "bobby-docs-"));
  try {
    const sourcePages = path.join(fixtureRoot, "docs/bobby-browser/source/pages/introduction");
    await mkdir(sourcePages, { recursive: true });
    await writeFile(
      path.join(sourcePages, "overview.md"),
      "# Overview\n",
      "utf8",
    );
    await writeFile(
      path.join(fixtureRoot, "docs/bobby-browser/source/navigation.json"),
      JSON.stringify({
        title: "bobby-browser",
        version: "0.2.0",
        sections: [
          {
            title: "Introduction",
            pages: [
              { title: "Overview", path: "introduction/overview.md" },
              { title: "Missing", path: "introduction/missing.md" },
            ],
          },
        ],
      }),
      "utf8",
    );
    await buildBobbyBrowserDocs(fixtureRoot);
    await assert.rejects(
      () => verifyBobbyBrowserDocs(fixtureRoot),
      /navigation path missing/,
    );
  } finally {
    await rm(fixtureRoot, { recursive: true, force: true });
  }
});

test("generated docs publish the Bobby skill and gauntlet operator guides", async () => {
  await buildBobbyBrowserDocs(REPO_ROOT);
  const navigation = JSON.parse(
    await readFile(path.join(REPO_ROOT, OUTPUT_REL, "navigation.json"), "utf8"),
  );
  const guidePaths = navigation.sections
    .find((section) => section.title === "Guides")
    .pages.map((page) => page.path);
  assert.ok(guidePaths.includes("guides/skills.md"));
  assert.ok(guidePaths.includes("guides/gauntlet.md"));

  const skills = await readFile(
    path.join(REPO_ROOT, OUTPUT_REL, "guides/skills.md"),
    "utf8",
  );
  assert.match(skills, /\/ghost on\|off\|status/);
  assert.match(skills, /\/zigzagzig run\|status\|stop/);
  assert.match(skills, /effectUncertain/);

  const gauntlet = await readFile(
    path.join(REPO_ROOT, OUTPUT_REL, "guides/gauntlet.md"),
    "utf8",
  );
  assert.match(gauntlet, /@bobby-browser\/gauntlet/);
  assert.match(gauntlet, /BOBBY_CHAMPIONSHIP_ENGINE/);
  assert.match(gauntlet, /target\/bobby-championship/);
});
