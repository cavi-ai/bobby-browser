---
documentedVersion: 0.2.0
---

# Intent commands

Semantic automation is available through the authenticated SDK and MCP surfaces with `intent:execute`. Supported intents include **Locate**, **Fill**, **SubmitAndVerify**, **WaitForState**, and **Follow**.

`Follow` activates a described link/control and verifies the resulting destination against an `expected_destination` wait condition. It carries a caller-supplied `boundary: bool` flag: set `boundary: true` when activation may perform a mutating action (requires a matching `WorkflowCheckpoint`); leave it `false` for ordinary same-tab navigation.

Vision-assisted resolution is **deny-by-default**: the bearer must hold `vision:assist`, and the session must have `executionPolicy.visionAssist = true`.
