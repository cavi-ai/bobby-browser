"""Intent envelope builders match the TypeScript wire shape."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from bobby_browser.intents import locate_envelope  # noqa: E402


class IntentEnvelopeTests(unittest.TestCase):
    def test_locate_envelope_uses_the_intent_command_shape(self) -> None:
        envelope = locate_envelope(
            {
                "commandId": "c",
                "workflowId": "w",
                "attemptId": "a",
                "sessionId": "s",
                "deadline": "2026-11-23T00:00:00Z",
            },
            "the submit button",
        )
        self.assertEqual(envelope["schemaVersion"], 2)
        self.assertEqual(envelope["command"]["kind"], "intent")
        self.assertEqual(envelope["command"]["input"]["kind"], "locate")
        self.assertEqual(envelope["command"]["input"]["input"]["purpose"], "the submit button")
        self.assertFalse(envelope["command"]["input"]["input"]["hints"]["allowBestMatch"])

    def test_locate_envelope_rejects_an_empty_purpose(self) -> None:
        with self.assertRaises(ValueError):
            locate_envelope(
                {"commandId": "c", "workflowId": "w", "attemptId": "a", "sessionId": "s", "deadline": "2026-11-23T00:00:00Z"},
                "",
            )


if __name__ == "__main__":
    unittest.main()
