import { readFileSync } from "node:fs";
import assert from "node:assert/strict";
import test from "node:test";

test("production shell uses origin-absolute assets so nested routes can boot", () => {
  const html = readFileSync(new URL("../index.html", import.meta.url), "utf8");

  assert.match(html, /href="\/app\.css"/);
  assert.match(html, /src="\/app\.js"/);
  assert.doesNotMatch(html, /src="\.\/app\.js"/);
});
