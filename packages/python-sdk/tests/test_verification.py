import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bobby_browser import command_verification_status


class VerificationTests(unittest.TestCase):
    def test_shared_sdk_contract(self):
        fixture = Path(__file__).resolve().parents[3] / "tests/fixtures/command-verification.json"
        for case in json.loads(fixture.read_text()):
            with self.subTest(case=case["name"]):
                self.assertEqual(command_verification_status(case["outcome"]), case["expected"])

    def test_unknown_outcome_is_not_treated_as_success(self):
        with self.assertRaises(ValueError):
            command_verification_status({"status": "futureStatus", "evidence": []})
