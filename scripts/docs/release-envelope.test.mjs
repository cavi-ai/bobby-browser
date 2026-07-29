import assert from "node:assert/strict";
import test from "node:test";

import { createReleaseEnvelope } from "./create-release-envelope.mjs";

test("creates the exact cavi-home product-docs envelope", () => {
  assert.deepEqual(createReleaseEnvelope({
    version: "0.2.0",
    tag: "v0.2.0",
    repository: "cavi-ai/bobby-browser",
    commit: "a".repeat(40),
    artifactSha256: "b".repeat(64),
  }), {
    schemaVersion: 1,
    slug: "bobby-browser",
    kind: "product-docs",
    version: "0.2.0",
    tag: "v0.2.0",
    repository: "cavi-ai/bobby-browser",
    commit: "a".repeat(40),
    artifact: {
      url: "https://github.com/cavi-ai/bobby-browser/releases/download/v0.2.0/bobby-browser-docs-v0.2.0.tar.gz",
      sha256: "b".repeat(64),
      format: "tar.gz",
    },
  });
});

test("rejects mismatched tags, repositories, commits, and digests", () => {
  const valid = {
    version: "0.2.0",
    tag: "v0.2.0",
    repository: "cavi-ai/bobby-browser",
    commit: "a".repeat(40),
    artifactSha256: "b".repeat(64),
  };
  assert.throws(() => createReleaseEnvelope({ ...valid, tag: "v0.1.0" }), /tag/i);
  assert.throws(() => createReleaseEnvelope({ ...valid, repository: "other/repo" }), /repository/i);
  assert.throws(() => createReleaseEnvelope({ ...valid, commit: "abc" }), /commit/i);
  assert.throws(() => createReleaseEnvelope({ ...valid, artifactSha256: "abc" }), /SHA-256/i);
});
