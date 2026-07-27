---
documentedVersion: 0.2.0
---

# Configuration

`serve` loads `./config.toml` at startup, overridable with the `BOBBY_BROWSER_CONFIG` environment variable. A missing file uses built-in defaults; a malformed or invalid file fails startup loudly with the offending path named.

The committed [`config.toml`](https://github.com/cavi-ai/bobby-browser/blob/main/config.toml) documents every field and mirrors the `AppConfig` schema (`server`, `browser`, `storage`, `http`, `interface`). The bootstrap credential is supplied separately through the `AUTOMATION_RUNTIME_BOOTSTRAP_*` environment variables, never the config file.
