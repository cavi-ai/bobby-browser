---
documentedVersion: {{PRODUCT_VERSION}}
---

# Workflow files

A workflow file is a named list of MCP tool calls that `workflow_run` runs in one call. Each step is a normal call of its tool, with that tool's capability checks, guards and evidence, on the page the run names and under its `workflowId`.

## Where files live

bobby reads `<name>.json` files from `<config dir>/workflows` and then from each directory in `[mcp].workflow_dirs`. A relative directory is resolved against the directory that holds `config.toml`. When two directories hold the same name, the first one wins.

```toml
[mcp]
workflow_dirs = ["workflows"]
```

Files are read on every call, so an edited file takes effect on the next run. `bobby://workflows` lists the built-in workflows and every file with its inputs; a file that does not load is listed with its error. `bobby://workflow-schema` is the JSON Schema of a file.

## Example

`contact.json` opens a form, fills it, sends it only when `send` is true, and returns the help link and the fill status.

```json
{
  "$schema": "bobby://workflow-schema",
  "name": "contact",
  "description": "Fill the contact form; send it when send is true.",
  "inputs": {
    "formUrl": {"type": "string"},
    "name": {"type": "string"},
    "email": {"type": "string"},
    "send": {"type": "boolean", "default": false}
  },
  "steps": [
    {"id": "open", "tool": "navigate", "args": {"url": {"$input": "formUrl"}}},
    {"id": "help", "tool": "intent_extract", "args": {"purpose": "read the help link", "fields": [
      {"name": "help", "purpose": "help link", "hints": {"role": "link", "accessibleName": "Help"},
       "value": {"kind": "href"}}]}},
    {"id": "fill", "tool": "intent_complete_form", "args": {"purpose": "fill the contact form", "fields": [
      {"name": "Name", "purpose": "name", "hints": {"role": "textbox", "accessibleName": "Name"},
       "value": {"kind": "setText", "value": {"$input": "name"}}},
      {"name": "Email", "purpose": "email", "hints": {"role": "textbox", "accessibleName": "Email"},
       "value": {"kind": "setText", "value": {"$input": "email"}}}]}},
    {"id": "send", "tool": "intent_submit_and_verify", "when": {"$input": "send"},
     "args": {"purpose": "send the form", "hints": {"role": "button", "accessibleName": "Send"}}}
  ],
  "outputs": {
    "help": {"$step": "help", "field": "help"},
    "filled": {"$step": "fill", "pointer": "/status"}
  }
}
```

```json
{"name": "workflow_run", "arguments": {"workflowHandle": "<handle>", "workflow": "contact",
  "inputs": {"formUrl": "https://example.com/contact", "name": "Ada", "email": "ada@example.com"}}}
```

## Fields

| Field | Meaning |
|---|---|
| `name` | Lowercase letters, digits and `-`; equals the file name without `.json` |
| `description` | Shown in `bobby://workflows` |
| `inputs` | Up to 32 named inputs, each `{type, description, default}`; `type` is `string`, `number`, `boolean`, `array` or `object`. An input without `default` is required |
| `steps` | 1 to 32 steps, run in order |
| `outputs` | Named values read from step results after the run |

A step has an `id` (starting with a lowercase letter, unique in the file) and exactly one of:

- `tool`: an MCP tool. `args` are its arguments; the run adds `sessionId`, `pageId` and `workflowId` where the tool takes them, so a step never names them.
- `workflow`: a built-in or file workflow. `args` are its inputs.

| Step field | Meaning |
|---|---|
| `when` | A ref; the step runs only when it is neither null nor false, else its result is `{"status":"skipped"}` |
| `forEach` | An array input; the step runs once per item, at most 50, and its result is the array of item results |
| `continueOn` | Error codes that do not stop the run, such as `targetNotFound` |

Lifecycle and raw tools are not steps: `session_create`, `session_close`, `page_open`, `page_close`, `page_activate`, `toolset_select`, `command_execute`, `evaluate_javascript`, `cookie_set`, `cookie_delete`, `click_and_wait_for_popup`, `job_submit`, `job_cancel`, `workflow_start`, `workflow_recover` and `workflow_run`.

## Refs

Any value in `args`, `when` or `outputs` may be a ref object. A ref that names nothing resolves to null.

| Ref | Value |
|---|---|
| `{"$input": "name"}` | The input |
| `{"$step": "id", "pointer": "/status"}` | A JSON Pointer into an earlier step's result |
| `{"$step": "id", "field": "help"}` | The value an earlier step's extraction evidence holds for that field |
| `{"$item": ""}` | Inside `forEach`, a JSON Pointer into the current item |

Steps read only earlier steps; `outputs` may read any step.

## Results and reruns

The result has `status`, `workflow`, `workflowId`, `runId`, `steps` (each `{id, tool or workflow, result}`), `outputs` and `evidence`, which lists every screenshot the steps took. The run stops at the first step that fails with a code outside its `continueOn`: `stoppedAt` names that step, and `status` and `error` are its.

A failed run can be rerun from a step without repeating the ones before it:

```json
{"workflow": "contact", "inputs": {...}, "runId": "<runId>", "from": "send"}
```

The steps before `from` keep their recorded results, marked `"reused": true`, for refs and outputs. Runs are kept per connection, the last 16; `from` must be a step the earlier run reached.

## Limits

| Limit | Value |
|---|---|
| Nested workflows | 2 levels |
| Tool calls per run | 256 |
| `forEach` items | 50 |
| File size | 256 KiB |
| Files listed | 256 |

## Errors

`workflow_run` answers invalid params with `error.data.reason`:

| Reason | Cause |
|---|---|
| `workflowNotFound` | No built-in or file has the name |
| `workflowInvalid` | The file does not load; `data.workflowError` has `code`, `message` and `pointer`, and `data.path` names the file |
| `workflowInputs` | An unknown, missing or mistyped input |
| `workflowRun` | `runId` without `from` or the reverse, an unknown run, or a rerun of a built-in |

## Checking files

```bash
bobby workflow validate
bobby workflow validate workflows/contact.json
bobby workflow schema > workflow.schema.json
```

`validate` with no file checks every file in the workflow directories, prints `ok <path>` or `invalid <path>: <code> at <pointer>: <message>`, and exits non-zero when any file is invalid.

## Next

- [MCP tools](../surfaces/mcp-tools.md#named-workflows)
- [Intent commands](intents.md)
