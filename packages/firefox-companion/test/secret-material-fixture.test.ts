import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { containsSecretMaterial } from "../src/secret-material.js";

const fixture = JSON.parse(
  await readFile(
    new URL("../../../crates/firefox-companion/tests/fixtures/secret-material.json", import.meta.url),
    "utf8",
  ),
) as { benign: string[]; secret: string[] };

for (const text of fixture.benign) {
  test(`shared fixture: not secret: ${text}`, () => {
    assert.equal(containsSecretMaterial(text), false);
  });
}

for (const text of fixture.secret) {
  test(`shared fixture: secret: ${text}`, () => {
    assert.equal(containsSecretMaterial(text), true);
  });
}
