import assert from "node:assert/strict";
import { cp, mkdtemp, mkdir, readFile, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { buildBobbyBrowserDocs } from "./build-bobby-browser.mjs";
import { verifyBobbyBrowserDocs } from "./verify-bobby-browser.mjs";
import { DOCUMENTED_VERSION, OUTPUT_REL, SOURCE_REL } from "./lib.mjs";

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");

async function withSourceFixture(run) {
  const fixtureRoot = await mkdtemp(path.join(tmpdir(), "bobby-docs-"));
  try {
    await cp(path.join(REPO_ROOT, SOURCE_REL), path.join(fixtureRoot, SOURCE_REL), {
      recursive: true,
    });
    await writeFile(
      path.join(fixtureRoot, "Cargo.toml"),
      `[workspace.package]\nversion = "${DOCUMENTED_VERSION}"\n`,
      "utf8",
    );
    await run(fixtureRoot);
  } finally {
    await rm(fixtureRoot, { recursive: true, force: true });
  }
}

const RELEASE = Object.freeze({
  version: DOCUMENTED_VERSION,
  tag: `v${DOCUMENTED_VERSION}`,
  commit: "aa5184347037c04c42064a702ce1dc7d5b16c75b",
  sourceDateEpoch: 1784953886,
});

test("build is deterministic for unchanged source", async () => {
  await withSourceFixture(async (fixtureRoot) => {
    const first = await buildBobbyBrowserDocs(fixtureRoot, RELEASE);
    const firstBytes = await readFile(path.join(first.outputRoot, "manifest.json"), "utf8");
    const second = await buildBobbyBrowserDocs(fixtureRoot, RELEASE);
    const secondBytes = await readFile(path.join(second.outputRoot, "manifest.json"), "utf8");
    assert.equal(first.manifest.contentSha256, second.manifest.contentSha256);
    assert.equal(firstBytes, secondBytes);
    assert.equal(first.manifest.schemaVersion, 1);
    assert.deepEqual(first.manifest.release, { tag: RELEASE.tag, commit: RELEASE.commit });
    assert.equal(first.manifest.generatedAt, "2026-07-25T04:31:26.000Z");
    await verifyBobbyBrowserDocs(fixtureRoot, RELEASE);
  });
});

test("build rejects incomplete or inconsistent release identity", async () => {
  await assert.rejects(
    () => buildBobbyBrowserDocs(REPO_ROOT, { ...RELEASE, tag: "v0.1.0" }),
    /tag.*version/i,
  );
  await assert.rejects(
    () => buildBobbyBrowserDocs(REPO_ROOT, { ...RELEASE, commit: "abc" }),
    /commit/i,
  );
  await assert.rejects(
    () => buildBobbyBrowserDocs(REPO_ROOT, { ...RELEASE, sourceDateEpoch: -1 }),
    /source date epoch/i,
  );
  await assert.rejects(
    () => buildBobbyBrowserDocs(REPO_ROOT, { ...RELEASE, version: "9.9.9" }),
    /release version must be/,
  );
});

test("build defaults release identity from git when args are omitted", async () => {
  await withSourceFixture(async (fixtureRoot) => {
    const built = await buildBobbyBrowserDocs(fixtureRoot);
    assert.equal(built.manifest.version, DOCUMENTED_VERSION);
    assert.equal(built.manifest.release.tag, `v${DOCUMENTED_VERSION}`);
    assert.match(built.manifest.release.commit, /^[a-f0-9]{40}$/);
  });
});

test("stamped docs substitute product and interface version tokens", async () => {
  await withSourceFixture(async (fixtureRoot) => {
    await buildBobbyBrowserDocs(fixtureRoot, RELEASE);
    const page = await readFile(
      path.join(fixtureRoot, OUTPUT_REL, "release/version-and-support.md"),
      "utf8",
    );
    assert.match(page, new RegExp(`documentedVersion: ${DOCUMENTED_VERSION}`));
    assert.doesNotMatch(page, /\{\{PRODUCT_VERSION\}\}/);
    assert.doesNotMatch(page, /\{\{INTERFACE_VERSION\}\}/);
    assert.match(page, new RegExp(DOCUMENTED_VERSION));
  });
});

test("verify fails when a page is tampered", async () => {
  await withSourceFixture(async (fixtureRoot) => {
    await buildBobbyBrowserDocs(fixtureRoot, RELEASE);
    const page = path.join(fixtureRoot, OUTPUT_REL, "introduction/overview.md");
    const original = await readFile(page, "utf8");
    await writeFile(page, `${original}\n<!-- tamper -->\n`, "utf8");
    await assert.rejects(
      () => verifyBobbyBrowserDocs(fixtureRoot, RELEASE),
      /contentSha256 mismatch/,
    );
  });
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
        version: DOCUMENTED_VERSION,
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
    await buildBobbyBrowserDocs(fixtureRoot, RELEASE);
    await assert.rejects(
      () => verifyBobbyBrowserDocs(fixtureRoot),
      /navigation path missing/,
    );
  } finally {
    await rm(fixtureRoot, { recursive: true, force: true });
  }
});

test("generated docs publish the Bobby skill and gauntlet operator guides", async () => {
  await withSourceFixture(async (fixtureRoot) => {
    await buildBobbyBrowserDocs(fixtureRoot, RELEASE);
    const navigation = JSON.parse(
      await readFile(path.join(fixtureRoot, OUTPUT_REL, "navigation.json"), "utf8"),
    );
  const guidePaths = navigation.sections
    .find((section) => section.title === "Guides")
    .pages.map((page) => page.path);
  assert.ok(guidePaths.includes("guides/skills.md"));
  assert.ok(guidePaths.includes("guides/gauntlet.md"));

    const skills = await readFile(
      path.join(fixtureRoot, OUTPUT_REL, "guides/skills.md"),
      "utf8",
    );
  assert.match(skills, /\/ghost on\|off\|status/);
  assert.match(skills, /\/zigzagzig run\|status\|stop/);
  assert.match(skills, /effectUncertain/);

    const gauntlet = await readFile(
      path.join(fixtureRoot, OUTPUT_REL, "guides/gauntlet.md"),
      "utf8",
    );
    assert.match(gauntlet, /@bobby-browser\/gauntlet/);
    assert.match(gauntlet, /BOBBY_CHAMPIONSHIP_ENGINE/);
    assert.match(gauntlet, /target\/bobby-championship/);
  });
});
