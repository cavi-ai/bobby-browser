"""Live tests: start the real ``bobby`` runtime from this checkout's build,
mint a bootstrap credential with ``bobby init``, and drive it through the
Python SDK over real HTTP, including the documented quick start example.

The runtime binary is ``$BOBBY_BIN``, else ``target/release/bobby``, else
``target/debug/bobby``. Skipped cleanly (not failed) when
``BOBBY_CHROME_EXECUTABLE`` is unset, same convention the Rust workspace tests
use (crates/worker-pool/tests/chromium_worker.rs). Never prints the bootstrap
bearer: it is read from ``bobby init``'s stdout and handed straight on.
"""

from __future__ import annotations

import contextlib
import http.server
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
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
PACKAGE_ROOT = Path(__file__).resolve().parents[1]


def _bobby_bin() -> Path:
    if os.environ.get("BOBBY_BIN"):
        return Path(os.environ["BOBBY_BIN"])
    for profile in ("release", "debug"):
        candidate = REPO_ROOT / "target" / profile / "bobby"
        if candidate.is_file():
            return candidate
    return REPO_ROOT / "target" / "release" / "bobby"


BOBBY_BIN = _bobby_bin()

SIGNUP_PAGE = b"""<!doctype html><title>Sign up</title>
<form id="signup">
  <label>Full name <input name="name"></label>
  <label>Work email <input name="email" type="email"></label>
  <button type="submit">Submit</button>
</form>
<p role="status" aria-label="Confirmation" id="status"></p>
<script>
  document.getElementById("signup").addEventListener("submit", (event) => {
    event.preventDefault();
    document.getElementById("status").textContent = "Thanks, " + event.target.name.value;
  });
</script>
"""


@contextlib.contextmanager
def _signup_site():
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:  # noqa: N802 (http.server naming)
            self.send_response(200)
            self.send_header("content-type", "text/html; charset=utf-8")
            self.send_header("content-length", str(len(SIGNUP_PAGE)))
            self.end_headers()
            self.wfile.write(SIGNUP_PAGE)

        def log_message(self, *args) -> None:
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}/signup"
    finally:
        server.shutdown()
        server.server_close()

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


@contextlib.contextmanager
def _live_runtime(test: unittest.TestCase):
    """Yields (base_url, bearer, tmp_path) for a real `bobby serve`."""
    if not BOBBY_BIN.is_file():
        test.skipTest(f"{BOBBY_BIN} not built; run: cargo build -p bobby-browser")
    with tempfile.TemporaryDirectory(prefix="bobby-python-sdk-live-") as tmp:
        tmp_path = Path(tmp)
        bootstrap_path = tmp_path / "bootstrap.env"
        init = subprocess.run(
            [str(BOBBY_BIN), "init", "--path", str(bootstrap_path), "--force"],
            cwd=tmp_path,
            capture_output=True,
            text=True,
            timeout=30,
        )
        test.assertEqual(init.returncode, 0, msg="bobby init failed (see stderr on the CI runner)")
        bearer = init.stdout.strip()
        test.assertTrue(bearer, "bobby init printed no bearer on stdout")

        port = _free_tcp_port()
        config_path = tmp_path / "config.toml"
        # Storage and memory resolve next to this file; loopback navigation
        # reaches the local fixture page.
        config_path.write_text(
            f'[server]\nhost = "127.0.0.1"\nport = {port}\n'
            f"[http]\nallow_loopback = true\n"
            f"[context]\ndir = {json.dumps(str(tmp_path / 'context'))}\n",
            encoding="utf-8",
        )
        for relative in (
            "data/profiles",
            "data/uploads",
            "data/downloads",
            "data/artifacts",
            "data/storage/checkpoints",
        ):
            (tmp_path / relative).mkdir(parents=True, exist_ok=True)

        env = dict(os.environ)
        env.setdefault(
            "AUTOMATION_RUNTIME_BROWSER_SELECTION",
            json.dumps({"preference": {"mode": "managedChromium"}}),
        )
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
            yield base_url, bearer, tmp_path
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


@unittest.skipUnless(os.environ.get("BOBBY_CHROME_EXECUTABLE"), _SKIP_REASON)
class LiveRuntimeTests(unittest.TestCase):
    def test_runtime_info_and_create_session_over_real_http(self) -> None:
        with _live_runtime(self) as (base_url, bearer, _):
            client = BrowserRuntimeClient(base_url, bearer)
            info = client.runtime_info()
            self.assertIn("version", info)
            self.assertIsInstance(info["capabilities"], list)

            session = client.create_session({"profile": "default", "proxy": None})
            self.assertTrue(session.get("id"))
            self.assertEqual(session.get("profile"), "default")
            client.delete_session(session["id"])

    def test_the_documented_quick_start_runs_against_a_real_runtime(self) -> None:
        with _live_runtime(self) as (base_url, bearer, tmp_path), _signup_site() as form_url:
            env = dict(os.environ)
            env.update(
                BOBBY_RUNTIME_URL=base_url,
                AUTOMATION_RUNTIME_TOKEN=bearer,
                BOBBY_EXAMPLE_URL=form_url,
                PYTHONPATH=str(PACKAGE_ROOT),
            )
            run = subprocess.run(
                [sys.executable, str(PACKAGE_ROOT / "examples" / "quickstart.py")],
                cwd=tmp_path,
                env=env,
                capture_output=True,
                text=True,
                timeout=180,
            )
            self.assertEqual(run.returncode, 0, msg=f"stdout: {run.stdout}\nstderr: {run.stderr}")
            self.assertIn("message: Thanks, Maya Chen", run.stdout)
            self.assertIn("screenshot: ", run.stdout)
            self.assertTrue((tmp_path / "confirmation.png").stat().st_size > 0)


if __name__ == "__main__":
    unittest.main()
