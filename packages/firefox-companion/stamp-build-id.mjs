// Stamp the built extension with an id derived from its contents: the
// background script reports it when it connects, and `build-id.json` carries
// it on disk, so the runtime can tell whether the running extension is the
// one installed in the profile.
import { createHash } from "node:crypto";
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

const PLACEHOLDER = "@@BOBBY_EXTENSION_BUILD_ID@@";
// Written at install or release time, not part of the bundle contents.
const EXCLUDED = new Set(["bobby-scope.json", "build-id.json", "bobby-firefox-companion.xpi"]);

const dist = process.argv[2] ?? join(dirname(fileURLToPath(import.meta.url)), "dist");

function files(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    return entry.isDirectory() ? files(path) : [path];
  });
}

const background = join(dist, "background.js");
const source = readFileSync(background, "utf8");
if (source.split(PLACEHOLDER).length !== 2) {
  throw new Error("background.js must contain the build id placeholder exactly once");
}

const hash = createHash("sha256");
const bundled = files(dist)
  .map((path) => relative(dist, path).split(sep).join("/"))
  .filter((path) => !EXCLUDED.has(path))
  .sort();
for (const path of bundled) {
  hash.update(path);
  hash.update("\0");
  hash.update(readFileSync(join(dist, path)));
  hash.update("\0");
}
const buildId = hash.digest("hex").slice(0, 32);

writeFileSync(background, source.replace(PLACEHOLDER, buildId));
writeFileSync(join(dist, "build-id.json"), `${JSON.stringify({ buildId })}\n`);
