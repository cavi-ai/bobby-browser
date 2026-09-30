# bobby-browser (Python SDK)

Typed HTTP client for the authenticated Bobby Browser `/v1` runtime
interface. Stdlib only (`urllib`, `json`, `dataclasses`, `typing`) -- no
third-party HTTP client dependency. Python >= 3.10.

Mirrors `@cavi-ai/bobby-browser` (TypeScript,
`packages/typescript-sdk`) and `bobby-browser-client` (Rust,
`crates/bobby-browser-client`): same auth header contract, idempotency-key
passthrough, and `CommandOutcome` status discriminator.

## Install

```bash
pip install bobby-browser
```

From a bobby-browser checkout: `pip install -e packages/python-sdk`.

`bobby install --skill-hermes` copies `skill/hermes/SKILL.md` into
`$HERMES_HOME/skills/bobby-browser/` (else `~/.hermes/skills/`).

## Use

```python
import os
from bobby_browser import BrowserRuntimeClient

client = BrowserRuntimeClient("http://127.0.0.1:7777", os.environ["AUTOMATION_RUNTIME_TOKEN"])
```

Full method catalog, headers, and error shape:
[docs/bobby-browser/source/pages/surfaces/python-sdk.md](../../docs/bobby-browser/source/pages/surfaces/python-sdk.md).

## Test

```bash
python3 -m unittest discover -s tests
```

`tests/test_live_runtime.py` additionally starts the real `bobby` runtime
from this worktree's release build and skips cleanly when
`BOBBY_CHROME_EXECUTABLE` is unset.
