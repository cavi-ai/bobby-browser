import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

test("manifest wires browser_action icons at 16/32/48/96", () => {
  const manifest = JSON.parse(readFileSync(join(root, "manifest.json"), "utf8"));
  for (const size of ["16", "32", "48", "96"]) {
    const path = manifest.browser_action.default_icon[size];
    assert.equal(path, `icons/icon-${size}.png`);
    assert.ok(existsSync(join(root, path)), `missing ${path}`);
  }
});

test("16px SVG source has no Chinese glyphs", () => {
  const svg = readFileSync(join(root, "icons/icon-16.svg"), "utf8");
  assert.equal(svg.includes("鲍"), false);
  assert.equal(svg.includes("比"), false);
});

test("full SVG source includes 鲍比", () => {
  const svg = readFileSync(join(root, "icons/icon.svg"), "utf8");
  assert.ok(svg.includes("鲍比"));
});
