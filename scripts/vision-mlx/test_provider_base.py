import pathlib
import sys
import unittest


sys.path.insert(0, str(pathlib.Path(__file__).parent))

from providers.base import ProposeRequest, ProposeResponse, VisionProvider


class VisionProviderNormalizationTests(unittest.TestCase):
    def test_string_actions_preserve_all_supported_sibling_payload_aliases(self):
        cases = []
        for action in ("click", "left_click", "leftClick", "mouse_click", "press"):
            for payload in (
                {"x": 4, "y": 5},
                {"coordinate": [4, 5]},
                {"coordinates": [4, 5]},
                {"position": {"x": 4, "y": 5}},
                {"clickX": 4, "clickY": 5},
            ):
                cases.append(({"action": action, **payload}, {"kind": "click", "x": 4.0, "y": 5.0}))
        for action in ("typeText", "type", "inputText", "enterText", "text"):
            for payload in ({"text": "hello"}, {"value": "hello"}):
                cases.append(({"action": action, **payload}, {"kind": "typeText", "text": "hello"}))
        for action in ("extractValue", "extract", "read", "getValue"):
            for payload in ({"value": "title"}, {"text": "title"}):
                cases.append(({"action": action, **payload}, {"kind": "extractValue", "value": "title"}))
        for index, action in enumerate(("clickCandidate", "typeIntoCandidate", "extractFromCandidate")):
            cases.append(({"action": action, "index": index}, {"kind": action, "index": index}))
        for payload, expected in cases:
            raw = {"confidence": 0.8, **payload}
            with self.subTest(action=raw["action"], fields=tuple(payload)):
                response = VisionProvider.normalize_response(raw)
                self.assertEqual(response.action, expected)
                response.validate()

    def test_string_action_sibling_whitelist_rejects_unknown_fields_without_echo(self):
        secret = "runtime-secret-string-action-field"
        for kind in ("click", "typeText", "extractValue", "clickCandidate"):
            with self.subTest(kind=kind):
                with self.assertRaisesRegex(ValueError, "unknown vision response fields") as error:
                    VisionProvider.normalize_response(
                        {"confidence": 0.8, "action": kind, "unknown": secret}
                    )
                self.assertNotIn(secret, str(error.exception))

    def test_request_and_action_unknown_fields_fail_closed_without_echoing_values(self):
        secret = "runtime-secret-provider-field"
        with self.assertRaisesRegex(ValueError, "invalid propose request fields") as error:
            ProposeRequest.from_dict({"purpose": "p", "intentKind": "fill", "stuck": "s", "screenshotPng": "x", "text": secret})
        self.assertNotIn(secret, str(error.exception))
        with self.assertRaises(ValueError):
            ProposeResponse(0.9, {"kind": "typeIntoCandidate", "index": 0, "text": secret}).validate()
        with self.assertRaisesRegex(ValueError, "unknown vision response fields") as error:
            VisionProvider.normalize_response({"confidence": 0.9, "action": {"kind": "clickCandidate", "index": 0}, "text": secret})
        self.assertNotIn(secret, str(error.exception))

    def test_abstention_actions_force_zero_confidence(self):
        for kind in ("terminate", "abort", "refuse", "none", "noop"):
            with self.subTest(kind=kind):
                response = VisionProvider.normalize_response(
                    {"confidence": 0.99, "action": {"kind": kind}}
                )

                self.assertEqual(response.confidence, 0.0)
                self.assertEqual(
                    response.action,
                    {"kind": "click", "x": 0.0, "y": 0.0},
                )

    def test_candidate_grounded_actions_require_only_a_non_negative_integer_index(self):
        for kind in ("clickCandidate", "typeIntoCandidate", "extractFromCandidate"):
            with self.subTest(kind=kind):
                for index in (0, 1):
                    ProposeResponse(0.9, {"kind": kind, "index": index}).validate()

                for index in (True, 1.5, -1, None):
                    with self.subTest(index=index):
                        with self.assertRaises(ValueError):
                            ProposeResponse(0.9, {"kind": kind, "index": index}).validate()

                with self.assertRaises(ValueError):
                    ProposeResponse(0.9, {"kind": kind, "index": 1, "text": "secret"}).validate()

                with self.assertRaises(ValueError):
                    ProposeResponse(0.9, {"kind": kind}).validate()

                with self.assertRaises(ValueError):
                    ProposeResponse(
                        0.9,
                        {"kind": kind, "index": 0, "clear_first": True},
                    ).validate()

    def test_candidate_grounded_normalization_rejects_unknown_payload_fields(self):
        for kind in ("clickCandidate", "typeIntoCandidate", "extractFromCandidate"):
            with self.subTest(kind=kind):
                with self.assertRaises(ValueError):
                    VisionProvider.normalize_action(
                        {"kind": kind, "index": 1, "text": "secret", "value": "secret"}
                    )


if __name__ == "__main__":
    unittest.main()
