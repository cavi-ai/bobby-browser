import pathlib
import socket
import subprocess
import sys
import time
import unittest
import urllib.error
import urllib.request


SERVER = pathlib.Path(__file__).parent / "vision_server.py"


def start_server(bind: str) -> subprocess.Popen:
    return subprocess.Popen(
        [sys.executable, str(SERVER), "--provider", "v1", "--bind", bind, "--exit-with-stdin"],
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        cwd=SERVER.parent,
    )


def stop_server(server: subprocess.Popen) -> None:
    if server.poll() is None:
        server.kill()
    server.wait()


def post_status(port: int, timeout: float) -> int | None:
    """HTTP status the server at `port` answers an unknown path with, or None."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        request = urllib.request.Request(f"http://127.0.0.1:{port}/unknown", data=b"{}", method="POST")
        try:
            with urllib.request.urlopen(request, timeout=1) as response:
                return response.status
        except urllib.error.HTTPError as error:
            return error.code
        except OSError:
            time.sleep(0.2)
    return None


class VisionServerLifelineTests(unittest.TestCase):
    def test_a_managed_server_exits_when_its_stdin_closes(self):
        server = start_server("127.0.0.1:0")
        try:
            with self.assertRaises(subprocess.TimeoutExpired):
                server.wait(timeout=1)
            server.stdin.close()
            self.assertEqual(server.wait(timeout=10), 0)
        finally:
            stop_server(server)

    def test_a_server_serves_once_the_previous_holder_releases_its_address(self):
        holder = socket.socket()
        holder.bind(("127.0.0.1", 0))
        holder.listen()
        port = holder.getsockname()[1]
        server = start_server(f"127.0.0.1:{port}")
        try:
            time.sleep(1.0)
            holder.close()
            self.assertEqual(post_status(port, timeout=15), 404)
        finally:
            holder.close()
            stop_server(server)


if __name__ == "__main__":
    unittest.main()
