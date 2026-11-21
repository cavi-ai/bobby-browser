import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../../", import.meta.url);

test("Homebrew publication follows binary assets and supports safe rehearsal", async () => {
  const caller = await readFile(new URL(".github/workflows/release-binaries.yml", root), "utf8");
  const workflow = await readFile(new URL(".github/workflows/publish-homebrew.yml", root), "utf8");
  assert.match(caller, /homebrew:\s+needs: release/);
  assert.match(caller, /uses: \.\/\.github\/workflows\/publish-homebrew\.yml/);
  assert.match(workflow, /workflow_call:/);
  assert.match(workflow, /workflow_dispatch:/);
  assert.doesNotMatch(workflow, /pull_request:|schedule:/);
  assert.match(workflow, /homebrew_release\.py prepare/);
  assert.match(workflow, /brew test cavi-ai\/tap\/bobby-browser/);
  assert.match(workflow, /if: \$\{\{ inputs\.dry_run != 'true' \}\}/);
  assert.match(workflow, /homebrew_release\.py publish/);
});

test("runs Node workflows on the pinned Node 24 LTS release", async () => {
  const workflows = await Promise.all([
    "ci.yml",
    "publish-docs.yml",
    // Named `publish.yml` because the npm trusted publisher matches the
    // workflow filename exactly.
    "publish.yml",
  ].map((name) => readFile(new URL(`.github/workflows/${name}`, root), "utf8")));

  for (const workflow of workflows) {
    assert.match(workflow, /node-version: (?:\")?24\.18\.1(?:\")?/);
    assert.doesNotMatch(workflow, /node-version: (?:\")?22(?:\.x)?(?:\")?/);
    assert.doesNotMatch(workflow, /npm install -g npm@latest/);
  }
});

test("npm publish is tag-guarded and idempotent under OIDC trusted publishing", async () => {
  const workflow = await readFile(
    new URL("../../.github/workflows/publish.yml", import.meta.url),
    "utf8",
  );

  assert.match(workflow, /RELEASE_TAG" != "v\$\{PKG_VERSION\}/);
  assert.match(workflow, /npm view "@cavi-ai\/bobby-browser@\$\{PKG_VERSION\}" version/);
  assert.match(workflow, /npm publish --access public --provenance/);
  assert.doesNotMatch(workflow, /NODE_AUTH_TOKEN:\s*\$\{/);
  assert.doesNotMatch(workflow, /secrets\.NPM_TOKEN/);
});

test("reusable docs workflow preserves caller dry-run through every publish guard", async () => {
  const workflow = await readFile(
    new URL("../../.github/workflows/publish-docs.yml", import.meta.url),
    "utf8",
  );

  assert.match(workflow, /DRY_RUN:\s*\$\{\{ inputs\.dry_run \|\| 'false' \}\}/);
  assert.equal(
    workflow.match(/if:\s*\$\{\{ env\.DRY_RUN != 'true' \}\}/g)?.length,
    2,
    "both release upload and consumer dispatch must be disabled by caller dry-run",
  );
});

test("binary releases carry the Firefox companion through supported installers", async () => {
  const [workflow, installer, formula] = await Promise.all([
    readFile(new URL("../../.github/workflows/release-binaries.yml", import.meta.url), "utf8"),
    readFile(new URL("../install.sh", import.meta.url), "utf8"),
    readFile(new URL("../../Formula/bobby-browser.rb", import.meta.url), "utf8"),
  ]);

  assert.match(workflow, /@cavi-ai\/bobby-firefox-companion build/);
  assert.match(workflow, /packages\/firefox-companion\/dist \"\$STAGE\/firefox-companion\"/);
  assert.match(workflow, /firefox-companion\/manifest\.json/);
  assert.match(installer, /BOBBY_SHARE_DIR.*firefox-companion/s);
  assert.match(formula, /install \"firefox-companion\"/);
});

test("release archives certify clean installs and upgrades on every release platform", async () => {
  const [releaseWorkflow, ciWorkflow, windowsInstaller, certifier] = await Promise.all([
    readFile(new URL("../../.github/workflows/release-binaries.yml", import.meta.url), "utf8"),
    readFile(new URL("../../.github/workflows/ci.yml", import.meta.url), "utf8"),
    readFile(new URL("../install.ps1", import.meta.url), "utf8"),
    readFile(new URL("../certify-release-install.py", import.meta.url), "utf8"),
  ]);

  assert.doesNotMatch(releaseWorkflow, /pull_request:/);
  assert.match(releaseWorkflow, /push:\s+tags:\s+- "v\*"/);
  assert.match(releaseWorkflow, /tag:\s+description:[^\n]+\s+required: true/);
  assert.equal(
    releaseWorkflow.match(/ref: \$\{\{ github\.event_name == 'push' && github\.sha \|\| format\('refs\/tags\/\{0\}', inputs\.tag\) \}\}/g)?.length,
    1,
    "preflight pins push SHA and resolves the selected manual tag",
  );
  assert.equal(releaseWorkflow.match(/ref: \$\{\{ needs\.preflight\.outputs\.sha \}\}/g)?.length, 4);
  assert.match(releaseWorkflow, /certify-release-install\.py/);
  assert.match(releaseWorkflow, /--asset-os "\$\{\{ matrix\.asset_os \}\}"/);
  assert.match(releaseWorkflow, /python -m zipfile -c/);
  assert.match(releaseWorkflow, /permissions:\n  contents: read/);
  assert.equal(releaseWorkflow.match(/contents: write/g)?.length, 2);
  assert.match(ciWorkflow, /scripts\/install-release\.test\.sh/);
  assert.match(windowsInstaller, /BOBBY_ARCHIVE/);
  assert.match(windowsInstaller, /File\]::Replace/);
  assert.match(certifier, /powershell\.exe/);
  assert.match(certifier, /upgrade retained stale managed files/);
});

test("release workflows bind packaged sources to the verified tag", async () => {
  const [release, docs, ...publishers] = await Promise.all([
    "release-binaries.yml", "publish-docs.yml", "publish.yml", "publish-crates.yml", "publish-python.yml",
  ].map((name) => readFile(new URL(`.github/workflows/${name}`, root), "utf8")));
  assert.match(release, /test "\$TAG" = "v\$VERSION"/);
  assert.match(release, /VERSION="\$\{TAG#v\}"/);
  assert.doesNotMatch(release, /VERSION="\$\{GITHUB_REF_NAME#v\}"/);
  assert.match(release, /--ref "\$TAG" -f tag="\$TAG" -f commit="\$RELEASE_COMMIT"/);
  assert.match(docs, /COMMIT="\$\(git rev-parse HEAD\)"/);
  assert.match(docs, /if \[ "\$DRY_RUN" != "true" \]/);
  assert.match(docs, /"\$COMMIT" != "\$TAG_COMMIT"/);
  for (const publisher of publishers) {
    assert.match(publisher, /ref: \$\{\{ inputs\.commit \|\|/);
    assert.match(publisher, /test "\$GITHUB_REF" = "refs\/tags\/\$RELEASE_TAG"/);
    assert.match(publisher, /test "\$GITHUB_SHA" = "\$RELEASE_COMMIT"/);
    assert.match(publisher, /release-gate\.py "\$RELEASE_TAG" --assets/);
  }
});
