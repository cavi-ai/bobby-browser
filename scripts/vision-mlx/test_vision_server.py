import pathlib
import subprocess
import sys
import unittest


SERVER = pathlib.Path(__file__).parent / "vision_server.py"


class VisionServerLifelineTests(unittest.TestCase):
    def test_a_managed_server_exits_when_its_stdin_closes(self):
        server = subprocess.Popen(
            [sys.executable, str(SERVER), "--provider", "v1", "--bind", "127.0.0.1:0", "--exit-with-stdin"],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            cwd=SERVER.parent,
        )
        try:
            with self.assertRaises(subprocess.TimeoutExpired):
                server.wait(timeout=1)
            server.stdin.close()
            self.assertEqual(server.wait(timeout=10), 0)
        finally:
            if server.poll() is None:
                server.kill()
                server.wait()


if __name__ == "__main__":
    unittest.main()
