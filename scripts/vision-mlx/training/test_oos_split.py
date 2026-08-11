import json
import pathlib
import subprocess
import sys
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).with_name("oos_split.py")


class OutOfSampleSplitTests(unittest.TestCase):
    def test_creates_requested_output_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            corpus = root / "corpus.jsonl"
            corpus.write_text(
                "\n".join(
                    json.dumps(row)
                    for row in (
                        {"journey": "onboarding", "target_index": 1},
                        {"journey": "documents", "target_index": None},
                    )
                )
                + "\n",
                encoding="utf-8",
            )
            output = root / "nested" / "split"

            result = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--input",
                    str(corpus),
                    "--holdout",
                    "documents",
                    "--out-dir",
                    str(output),
                ],
                capture_output=True,
                text=True,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                (output / "oos_train.jsonl").read_text(encoding="utf-8"),
                '{"journey": "onboarding", "target_index": 1}\n',
            )
            self.assertEqual(
                (output / "oos_test.jsonl").read_text(encoding="utf-8"),
                '{"journey": "documents", "target_index": null}\n',
            )


if __name__ == "__main__":
    unittest.main()
