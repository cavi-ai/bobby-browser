/**
 * `@cavi-ai/bobby-browser` — typed HTTP client for a Bobby Browser runtime
 * (`bobby serve`) speaking the authenticated `/v1` interface.
 *
 * Pair with the Rust crate `bobby-browser-client` for the same surface from
 * native callers. Auth headers on every request: `Authorization: Bearer …`,
 * `x-interface-version`, `x-correlation-id`, and `x-deadline`.
 *
 * @packageDocumentation
 */
export * from "./client.js";
export * from "./contracts.js";
export * from "./controls.js";
export * from "./errors.js";
export * from "./intents.js";
export * from "./validators.js";
