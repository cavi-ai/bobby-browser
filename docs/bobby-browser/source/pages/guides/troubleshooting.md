---
documentedVersion: {{PRODUCT_VERSION}}
---

# Troubleshooting

Start with `bobby doctor`. It checks configuration, credential, storage, browser and host entries without changing anything. `bobby doctor --fix` repairs bobby-owned state. Pass `--config` and `--bootstrap-env` to match how you launch the server.

## Authentication errors (401)

- Missing or wrong `Authorization: Bearer ...`, or an expired or revoked principal.
- SDK and curl clients read `AUTOMATION_RUNTIME_TOKEN`. Set it with `export AUTOMATION_RUNTIME_TOKEN="$(bobby token)"`.
- A direct `mcp-gateway` launch needs all four `AUTOMATION_RUNTIME_BOOTSTRAP_*` variables.
- A non-loopback `bobby serve` needs a credential from `bobby init` first.

See [Authentication](auth.md).

## `missingCapability` (403)

The caller lacks the capability for the operation or for a nested command. Compare against [Capabilities](../concepts/capabilities.md). `bobby init` creates the agent preset, which has no `authority:admin`. Use `--preset unrestricted` for operator work.

## Not found (404)

Authenticated routes are under `/v1`, for example `/v1/runtime`. Delete a session with `DELETE /v1/sessions/{id}`.

## Event gap (409 on events)

Retention moved past your cursor. Re-read durable state and resume from the earliest cursor. See [Events and recovery](events-recovery.md).

## MCP

- Send `initialize` (protocol `2025-11-25`) before `tools/list` or `tools/call`.
- After a token rotation over HTTP, send `initialize` again.
- MCP over HTTP takes only a bearer token. It ignores `x-interface-version`, `x-correlation-id` and `x-deadline`.
- `workflowBindingConflict` means a call mixed `workflowHandle` with explicit IDs. `unknownWorkflowHandle` means the handle is gone; use explicit IDs.

## Browser and engine

- The default engine is Firefox. If doctor warns about BiDi, run `bobby firefox-start` and pair again. See [Firefox companion](firefox-companion.md).
- Chromium needs an installed browser. Set `BOBBY_CHROME_EXECUTABLE` when it is not in a standard location.
- `engineUnreachable` means the configured engine did not answer. Run `bobby doctor`, fix what it names, and resend the request unchanged.

## Configuration and credential paths

- A malformed `config.toml` stops startup and names the path. Fix the TOML. Keep secrets out of it.
- `--config` and `BOBBY_BROWSER_CONFIG` select the config file. `--bootstrap-env` and `BOBBY_BROWSER_BOOTSTRAP_ENV` select the credential file.
- `unsupportedInterfaceVersion` means `x-interface-version` is missing or wrong. Send `{{INTERFACE_VERSION}}`.

## Form fills fail

- Prefer `role` plus exact `nearText` when you know the label, and keep `purpose` as the task description.
- A fill without postcondition evidence fails. Do not treat a click as success. Locate again and retry with a new attempt ID or idempotency key.
- `setChecked` is for checkboxes and radios. A radio cannot be unchecked.
- File fields need `file:upload`.
- `intent_complete_form` stops at the first failed field. Fix that field and resend the form. Duplicate or empty field names are rejected.
- If evidence shows `formControlValid: "false"`, read `formControlValidationMessage` and correct the value.

## Accessibility snapshot

- Large pages return `truncated: true`. Raise `maxNodes` or scope the snapshot with `target`.
- Password and masked values read `"[redacted]"`.
- Pass a node's `target` straight to `click`, `type_text` or `upload_files`. See [Accessibility snapshot](accessibility-snapshot.md).

## Vision assist

Vision assist needs the `vision:assist` capability, `executionPolicy.visionAssist = true` on the session, and a reachable provider. A capability and an opt-in alone are not enough.

- Set up the provider with `bobby vision connect`, export the variables it prints, and start with `bobby serve --vision`. See [Configuration](configuration.md#vision).
- Doctor checks: `vision-service` (provider reachable), `vision-provider` (selected profile exists), `vision-upstream-key` (the profile's key variable is set), `vision-routing`, `vision-acp-reachability` and `vision-auth-path` for ACP profiles. The ACP checks make no model call.
- For an ACP harness, log in through the harness first. bobby calls the harness's `authenticate` at run time and fails closed when no advertised method matches. It does not read IDE keychains or store provider tokens.
- The token lives in the variable named by `token_env`, never in `config.toml`. Endpoints must be `https`, or `http` on loopback.
- For LM Studio or MLX, use the server URL the app shows. Port 1234 is common but not fixed.

## Error codes

| Code | Meaning |
|---|---|
| `invalidRequest` | Malformed headers, body or query |
| `unsupportedInterfaceVersion` | Interface version missing or wrong |
| `invalidIdempotencyKey` | Key is not 1 to 128 printable ASCII characters |
| `idempotencyConflict` | Same key, different payload |
| `deadlineExceeded` | Past `x-deadline`, or the command's `timeoutMs` ran out (retryable) |
| `authenticationFailed` | Bad or missing bearer |
| `tokenExpired` | Principal expired |
| `missingCapability` | Capability check failed |
| `malformedScope` | Scope or authority malformed |
| `artifactDenied` | Artifact access denied |
| `unsupportedOperation` | Operation not supported |
| `notFound` | Resource missing |
| `resourceExhausted` | Capacity or in-flight limit reached |
| `internal` | Unexpected server failure |
