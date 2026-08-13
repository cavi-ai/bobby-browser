// Stage the extension's non-bundled files next to the esbuild output.
//
// This ran as `cp … && mkdir -p … && cp -R …` inside the build script until the
// v0.9.0 tag, where the Windows release build failed with "The syntax of the
// command is incorrect": pnpm runs package scripts through cmd.exe on Windows,
// which has neither `cp` nor `mkdir -p`. The release workflow only started
// building this bundle after v0.8.0, so no earlier tag exercised it there.
import { cpSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, "dist");

mkdirSync(dist, { recursive: true });
for (const file of ["manifest.json", "popup.html"]) {
  cpSync(join(here, file), join(dist, file));
}
cpSync(join(here, "icons"), join(dist, "icons"), { recursive: true });
