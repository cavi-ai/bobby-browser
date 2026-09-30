---
documentedVersion: {{PRODUCT_VERSION}}
---

# Zed over ACP

Zed runs bobby as an external agent over the
[Agent Client Protocol](../surfaces/acp.md). The editor sends structured JSON
prompts; bobby executes them, remembers the page, saves checkpoints, and
recovers workflows with the same contracts as MCP and HTTP.

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

`bobby acp-stdio` loads the bootstrap credential and attaches to the local
runtime owner for the current scope, so Zed, an MCP host, and `bobby serve`
share one browser runtime. No credential belongs in `settings.json`.

Open the agent panel, start a new thread with **bobby-browser**, and send each
prompt below as one message. bobby has no planner: every message is one JSON
request.

## Walkthrough

This is the transcript of the `acp_walkthrough` release test on real
Chromium: the same frames Zed exchanges with bobby, with long results trimmed to
the fields that matter.

```text
editor -> initialize {"protocolVersion":1}
bobby  <- {"protocolVersion":1,"agentCapabilities":{"loadSession":false,"promptCapabilities":{…},"sessionCapabilities":{"close":{}},…},"authMethods":[]}
editor -> session/new ; bobby <- sessionId 9813eec3-bd93-4bcf-8553-027df4cee362
editor -> session/prompt {"url":"http://127.0.0.1:61847","workflowId":"9ad0bf05-f3ed-4f7a-a9bd-db6ffe6fbf99","intent":{"kind":"fill","input":{"purpose":"Name","hints":{"role":"textbox","accessibleName":"Name"},"value":{"kind":"setText","value":"Maya Chen"}}}}
bobby  <- session/update {"operation":"execute","workflowId":"9ad0bf05-…","status":"completed","commandId":"e36ae143-8723-44c1-9f5b-5c2d8008ed35"} ; stopReason "end_turn"
editor -> session/prompt {"operation":"contextAsk","description":"Name"}
bobby  <- session/update {"operation":"contextAsk","result":{"answer":{"target":{"role":"textbox","accessibleName":"Name"},"confidence":1.0,"observedAt":{"kind":"persisted"},"source":"observed"},"hit":true,"pageDerived":true}} ; stopReason "end_turn"
editor -> session/prompt {"operation":"checkpointSave","checkpoint":{"schemaVersion":1,"checkpointId":"758d9a66-…","workflowId":"9ad0bf05-…","attemptId":"aaf2e106-…","sessionId":"9813eec3-…","pageId":"12517b29-…","restartUrl":"http://127.0.0.1:61847","currentUrl":"http://127.0.0.1:61847","recoveryClass":"replayable",…},"evidenceRefs":["e36ae143-8723-44c1-9f5b-5c2d8008ed35"]}
bobby  <- session/update {"operation":"checkpointSave","checkpointId":"758d9a66-…","evidence":8} ; stopReason "end_turn"
editor -> session/prompt {"operation":"recoveryStatus","workflowId":"9ad0bf05-…"}
bobby  <- session/update {"operation":"recoveryStatus","checkpointId":"758d9a66-…","receipts":0} ; stopReason "end_turn"
mcp    -> context_ask, recovery_status ; same answers as ACP
editor -> session/prompt {"operation":"workflowRecover","workflowId":"9ad0bf05-…"}
bobby  <- session/update {"operation":"workflowRecover","status":"resumed","checkpointId":"758d9a66-…"} ; stopReason "end_turn"
editor -> session/close ; bobby <- {}
```

What each step shows:

1. **Act.** The first prompt carries `url`, a stable `workflowId`, and one
   intent in the shape `command_execute` takes. The reply is the full
   `CommandOutcome` with its evidence.
2. **Remember.** `contextAsk` answers from the context store: the fill was
   verified, so the field is known before any snapshot. A miss answers
   `{"answer":null,"hit":false,"reason":"notRemembered","nextStep":"a11y_snapshot"}`,
   the same shape MCP `context_ask` and `GET /v1/context/ask` return.
3. **Checkpoint.** `checkpointSave` resolves `evidenceRefs` (command ids this
   principal owns) into evidence server-side; the editor never authors
   evidence.
4. **Recover.** `recoveryStatus` and `workflowRecover` read and resume the
   workflow by id. The test asks MCP `context_ask` and `recovery_status` the
   same questions on the same runtime and asserts identical answers.
5. **Close.** `session/close` cancels in-flight work and deletes the runtime
   session.

Run it yourself:

```bash
cargo test -p interface-conformance --test acp_walkthrough -- --ignored --nocapture
```

`BOBBY_CHROME_EXECUTABLE` names the browser when it is not in the default
location.

## Next

- [ACP surface reference](../surfaces/acp.md)
- [Evidence and checkpoints](../concepts/evidence-checkpoints.md)
- [Context graph](../concepts/context-graph.md)
