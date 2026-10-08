---
documentedVersion: {{PRODUCT_VERSION}}
---

# Workflow replay

`bobby audit replay` turns a signed [audit bundle](../concepts/evidence-checkpoints.md#audit-bundles) into one HTML page. The page lists every command in order with its phases, timestamps, outcome, evidence and screenshots. It has no scripts and makes no network requests, so it opens from disk or as an email attachment.

```bash
bobby audit export --workflow <workflowId> --out workflow.tar
bobby audit replay workflow.tar --public-key <hex>
```

This writes `workflow.html` next to the bundle; pass `--out <path>` to choose another path. Replay verifies the bundle first and refuses one whose digests or signature do not match. With `--public-key` it also refuses a bundle signed by anyone else. Print your signing key with `bobby audit key`.

The page header shows the signer and whether the key was pinned. Journal and page text is escaped, and screenshots are embedded from the verified bytes.

See the [sample replay](replay-sample.html) of a short session that fills a field and clicks a button.

## Next

- [Evidence and checkpoints](../concepts/evidence-checkpoints.md)
- [CLI reference](cli.md)
