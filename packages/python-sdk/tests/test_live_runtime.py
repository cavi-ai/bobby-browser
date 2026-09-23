"""One live test: start the real ``bobby`` runtime from this worktree's
release build, mint a bootstrap credential with ``bobby init``, and drive
``runtime_info`` / ``create_session`` through the Python SDK over real HTTP.

Skipped cleanly (not failed) when ``BOBBY_CHROME_EXECUTABLE`` is unset, same
convention the Rust workspace tests use (crates/worker-pool/tests/chromium_worker.rs).
Never prints the bootstrap bearer: it is read from ``bobby init``'s stdout
and handed straight to the client.
"""

from __future__ import annotations

import contextlib
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path

# See test_client.py: makes `bobby_browser` importable for
# `python3 -m unittest discover -s packages/python-sdk/tests` run straight
# from a checkout, with no `pip install` step first.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from bobby_browser import BrowserRuntimeClient  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[3]
BOBBY_BIN = REPO_ROOT / "target" / "release" / "bobby"

_SKIP_REASON = "BOBBY_CHROME_EXECUTABLE is unset; skipping the live runtime test"


def _free_tcp_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def _wait_for_healthz(base_url: str, deadline: float) -> None:
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"{base_url}/healthz", timeout=2) as response:
                if response.status == 200:
                    return
        except (urllib.error.URLError, OSError) as error:
            last_error = error
        time.sleep(0.25)
    raise AssertionError(f"bobby serve never became healthy: {last_error}")


@unittest.skipUnless(os.environ.get("BOBBY_CHROME_EXECUTABLE"), _SKIP_REASON)
class LiveRuntimeTests(unittest.TestCase):
    def test_runtime_info_and_create_session_over_real_http(self) -> None:
        if not BOBBY_BIN.is_file():
            self.skipTest(f"{BOBBY_BIN} not built; run: cargo build -p bobby-browser --release")

        with tempfile.TemporaryDirectory(prefix="bobby-python-sdk-live-") as tmp:
            tmp_path = Path(tmp)
            # `bobby serve` does not auto-create every storage/browser
            # directory (deploy/docker/entrypoint.sh pre-creates them for
            # the same reason) -- the defaults are relative to the process
            # cwd, which we pin to tmp_path below.
            for relative in (
                "data/profiles",
                "data/uploads",
                "data/downloads",
                "data/artifacts",
                "data/storage/checkpoints",
            ):
                (tmp_path / relative).mkdir(parents=True, exist_ok=True)

            bootstrap_path = tmp_path / "bootstrap.env"
            init = subprocess.run(
                [str(BOBBY_BIN), "init", "--path", str(bootstrap_path), "--force"],
                cwd=tmp_path,
                capture_output=True,
                text=True,
                timeout=30,
            )
            self.assertEqual(init.returncode, 0, msg="bobby init failed (see stderr on the CI runner)")
            bearer = init.stdout.strip()
            self.assertTrue(bearer, "bobby init printed no bearer on stdout")

            port = _free_tcp_port()
            config_path = tmp_path / "config.toml"
            config_path.write_text(
                f'[server]\nhost = "127.0.0.1"\nport = {port}\n', encoding="utf-8"
            )

            env = dict(os.environ)
            env.setdefault("AUTOMATION_RUNTIME_BROWSER_SELECTION", json.dumps({"preference": {"mode": "managedChromium"}}))

            serve_stdout = open(tmp_path / "serve.stdout.log", "wb")
            serve_stderr = open(tmp_path / "serve.stderr.log", "wb")
            process = subprocess.Popen(
                [str(BOBBY_BIN), "serve", "--config", str(config_path), "--bootstrap-env", str(bootstrap_path)],
                cwd=tmp_path,
                env=env,
                stdout=serve_stdout,
                stderr=serve_stderr,
            )
            try:
                base_url = f"http://127.0.0.1:{port}"
                _wait_for_healthz(base_url, deadline=time.monotonic() + 30)

                client = BrowserRuntimeClient(base_url, bearer)
                info = client.runtime_info()
                self.assertIn("version", info)
                self.assertIsInstance(info["capabilities"], list)

                session = client.create_session({"profile": "default", "proxy": None})
                self.assertTrue(session.get("id"))
                self.assertEqual(session.get("profile"), "default")
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)
                with contextlib.suppress(Exception):
                    serve_stdout.close()
                with contextlib.suppress(Exception):
                    serve_stderr.close()


if __name__ == "__main__":
    unittest.main()
