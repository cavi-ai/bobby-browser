---
documentedVersion: {{PRODUCT_VERSION}}
---

# Zed over ACP

Run bobby as an external agent in Zed. Zed sends structured JSON prompts, and bobby runs them against a browser with the same checkpoint and recovery behavior as the other surfaces. Protocol details are in [ACP](../surfaces/acp.md).

## Set up

```bash
bobby install --cli --yes
bobby doctor
```

Add bobby to Zed's `settings.json`:

```json
{
  "agent_servers": {
    "bobby-browser": {
      "type": "custom",
      "command": "bobby",
      "args": ["acp-stdio"],
      "env": {}
    }
  }
}
```

`bobby acp-stdio` loads the bootstrap credential and attaches to the shared local runtime, so Zed, MCP hosts and `bobby serve` use one browser runtime. No credential goes in `settings.json`.

## Use it

Open the agent panel, start a thread with **bobby-browser**, and send each request as one JSON message.

Fill a field:

```json
{"url": "https://example.com/signup", "workflowId": "9ad0bf05-f3ed-4f7a-a9bd-db6ffe6fbf99", "intent": {"kind": "fill", "input": {"purpose": "Name", "hints": {"role": "textbox", "accessibleName": "Name"}, "value": {"kind": "setText", "value": "Maya Chen"}}}}
```

Ask where the control is, from retained context:

```json
{"operation": "contextAsk", "description": "Name"}
```

Read the workflow's checkpoint, then recover it:

```json
{"operation": "recoveryStatus", "workflowId": "9ad0bf05-f3ed-4f7a-a9bd-db6ffe6fbf99"}
{"operation": "workflowRecover", "workflowId": "9ad0bf05-f3ed-4f7a-a9bd-db6ffe6fbf99"}
```

Reuse the same `workflowId` across messages to keep them in one workflow. `checkpointSave` takes a `checkpoint` and `evidenceRefs`: command IDs from earlier replies, which the runtime resolves to evidence itself.

See [Evidence and checkpoints](../concepts/evidence-checkpoints.md) and [Context graph](../concepts/context-graph.md).
