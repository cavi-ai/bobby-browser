---
documentedVersion: {{PRODUCT_VERSION}}
---

# Workflow replay

`bobby audit replay` turns an [audit bundle](../concepts/evidence-checkpoints.md#audit-bundles)
into one self-contained HTML page: every command in journal order, its phases
with timestamps, its outcome and error, its evidence, and the screenshots the
bundle carries. The page has no script and no external requests, so it opens
from disk or as an attachment.

```bash
bobby audit export --workflow <workflowId> --out workflow.tar
bobby audit replay workflow.tar --public-key <hex>
# writes workflow.html
```

Replay verifies the bundle first: it refuses a bundle whose digests or
signature do not match, and with `--public-key` one signed by anyone else. The
header states the signer and whether it was pinned. Page and journal text is
escaped; screenshots are embedded as `data:` images from the verified bytes.

[Sample replay](replay-sample.html): a real Chromium run against the test
fixture (navigate, fill **Name**, screenshot, click **Continue**, screenshot),
exported and replayed by the `audit_replay_live` test. Regenerate it with:

```bash
BOBBY_WRITE_REPLAY_SAMPLE="$PWD/docs/bobby-browser/source/pages/guides/replay-sample.html" \
  cargo test -p bobby-browser --test audit_replay_live -- --ignored
```

## Next

- [Evidence and checkpoints](../concepts/evidence-checkpoints.md)
- [CLI reference](cli.md)
