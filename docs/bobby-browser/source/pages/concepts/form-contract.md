---
documentedVersion: {{PRODUCT_VERSION}}
---

# Canonical form contract

`FormSnapshot` is the versioned, engine-neutral contract for observing forms
before an agent plans edits. Version 1 is additive: it does not change the
existing accessibility snapshot. Agents can request the contract through the
read-only MCP `form_snapshot` tool with `{ sessionId, pageId, maxControls? }`; it requires
`page:read`, not browser mutation or caller-enabled JavaScript evaluation.

The contract represents owned forms, unowned controls, labeled groups,
constraints, validity, options, supported operations, and safe semantic
targets. It intentionally excludes CSS selectors, DOM or backend node IDs, raw
HTML, arbitrary attributes, and secret values.

## Root shape

```ts
interface FormSnapshot {
  schemaVersion: 1;
  pageId: string;
  forms: FormDescriptor[];
  unownedControls: FormControl[];
  truncated: boolean;
}
```

Every control has a stable snapshot-local ID, its form and group membership,
a normalized `controlKind`, current typed state, constraints, validity,
options, and `supportedOperations`. References must resolve inside the same
snapshot; duplicate IDs, inconsistent group membership, and submit/reset
references to the wrong control kind are invalid.

Targets use only role, accessible name, optional ordinal, and bounded semantic
frame/shadow paths. A missing target means the control was observable but a
safe command-ready identity could not be produced.

## Sensitive state

Password controls never contain `{ kind: "text", value: ... }`. Their state is
represented without content:

```json
{ "kind": "redacted", "present": true }
```

`present` answers only whether a value exists. Consumers must not infer or log
its contents. Rust serialization and both Rust and TypeScript validators reject
password controls that expose text.

## Bounds and compatibility

Version 1 is fail-closed: unknown fields and unsupported schema versions are
rejected. A snapshot contains at most 64 forms and 512 total controls; nested
collections and strings are also bounded. `truncated: true` tells consumers
that discovery reached a budget and the snapshot is incomplete.

Rust consumers use the types exported by the `types` crate and can generate a
JSON Schema with its `schema` feature. TypeScript consumers use
`FormSnapshot`, `FORM_SNAPSHOT_SCHEMA_VERSION`, and `isFormSnapshot` from
`@cavi-ai/bobby-browser`.

Future engine, MCP, and agent-skill adapters should produce or consume this
contract instead of defining engine-specific form shapes.

Live Chromium and Firefox reads use the same bounded raw DOM projection and
the same Rust normalizer. Canonical IDs, control kinds, typed state, validity,
supported operations, redaction, and truncation are not decided by
engine-specific scripts.

## Typed control actions

Each control advertises the exact operations accepted by the reconciliable
`controlAction` primitive. Chromium and Firefox preflight the operation against
the reread control, execute it once through their native transport, then return
`ControlActionEvidence` with the operation, semantic target, typed state,
validity, and node-replacement status. Unsupported or ambiguous targets fail
before mutation; uncertainty after dispatch is never blindly replayed.
