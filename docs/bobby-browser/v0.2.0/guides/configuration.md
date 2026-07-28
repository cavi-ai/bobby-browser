---
documentedVersion: 0.2.0
---

# Configuration

`bobby serve` loads `./config.toml` at startup, overridable with the `BOBBY_BROWSER_CONFIG` environment variable. A missing file uses built-in defaults; a malformed or invalid file fails startup loudly with the offending path named.

The committed [`config.toml`](https://github.com/cavi-ai/bobby-browser/blob/main/config.toml) documents every field and mirrors the `AppConfig` schema (`server`, `browser`, `storage`, `http`, `interface`). The bootstrap credential is never stored in the config file.

Bootstrap resolution uses process env first, then a local secret file. The secret
path defaults to the OS config directory (`…/bobby-browser/bootstrap.env`) and
can be overridden with `BOBBY_BROWSER_BOOTSTRAP_ENV`. Generate that file with
`bobby init`, or let loopback-only `bobby serve` auto-generate it on first run.
