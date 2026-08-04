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
