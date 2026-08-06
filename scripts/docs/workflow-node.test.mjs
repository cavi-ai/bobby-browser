import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../../", import.meta.url);

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
