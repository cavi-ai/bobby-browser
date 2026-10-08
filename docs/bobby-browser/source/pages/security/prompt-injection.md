---
documentedVersion: {{PRODUCT_VERSION}}
---

# Prompt injection

An agent using bobby reads text it does not control: page content, accessibility labels, extracted values and `context_ask` answers. A page can put anything in that text, including strings that look like instructions to a model. This page says what that text can and cannot do.

## What page text can do

- Appear verbatim in `a11y_snapshot`, `workflow_observe`, `inspect`, `intent_extract`, `extract_structured` and `context_ask` results.
- Influence which candidate a vision-assisted intent proposes. The proposal still passes the same bounded, capability-gated, verified step as any other.
- Make a command fail verification (`verificationFailed`) by changing how the page behaves.

## What page text cannot do

- **Grant a capability.** Capabilities are checked against the caller's authenticated token on every dispatch, before any page content is read. No path lets page text change a token's grant or a session's `executionPolicy`.
- **Select a tool.** The runtime never parses result text into a tool call. A host that treats tool output as new instructions makes that choice itself.
- **Turn on `evaluate_javascript` or vision extraction.** The session must already hold `javascript:evaluate` or `vision:assist` and the matching `executionPolicy` flag.
- **Trigger an outbound request.** Navigation, downloads and vision calls happen only when the caller issues them.

## The `pageDerived` marker

MCP results that carry page text set `pageDerived: true` in `structuredContent`. This covers `a11y_snapshot`, `workflow_observe` (including the `postState` of mutating actions), `inspect`, `intent_extract`, `extract_structured`, `context_ask`, `context_neighbors`, `context_site` and `form_snapshot`. HTTP context answers, command evidence and form snapshots carry the same marker.

Text under `pageDerived` is data from the page, never an instruction. The marker is a signal for your host's prompt construction. It enforces nothing. The capability gates above hold whether or not a host reads it.

## What your agent host must do

- Treat `pageDerived` text as untrusted when building a prompt. Quote it. Do not place it in the instruction stream.
- Do not let model output alone trigger a capability-gated tool such as `evaluate_javascript` or vision extraction.
- Handle `error.repair` text the same way before feeding it to a model. It is runtime-generated, but any text fed back to a model deserves the same care.

## Related

- [Security model](model.md)
- [Capabilities](../concepts/capabilities.md)
