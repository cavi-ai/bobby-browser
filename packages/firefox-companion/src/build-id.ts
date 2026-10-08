// The id of this extension build. `stamp-build-id.mjs` replaces the
// placeholder in the built background.js with an id derived from the bundle
// contents and writes the same id to `build-id.json` beside it. An unstamped
// build (tests, a bundle from before the stamp) reports no id.
export const EXTENSION_BUILD_ID: string = "@@BOBBY_EXTENSION_BUILD_ID@@";

const BUILD_ID = /^[0-9a-f]{32}$/;

export function isExtensionBuildId(value: unknown): value is string {
  return typeof value === "string" && BUILD_ID.test(value);
}

export function stampedExtensionBuildId(): string | undefined {
  return isExtensionBuildId(EXTENSION_BUILD_ID) ? EXTENSION_BUILD_ID : undefined;
}
