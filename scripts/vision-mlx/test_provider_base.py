import pathlib
import sys
import unittest


sys.path.insert(0, str(pathlib.Path(__file__).parent))

from providers.base import ProposeResponse, VisionProvider


class VisionProviderNormalizationTests(unittest.TestCase):
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

    def test_candidate_grounded_normalization_preserves_only_the_index(self):
        for kind in ("clickCandidate", "typeIntoCandidate", "extractFromCandidate"):
            with self.subTest(kind=kind):
                self.assertEqual(
                    VisionProvider.normalize_action(
                        {"kind": kind, "index": 1, "text": "secret", "value": "secret"}
                    ),
                    {"kind": kind, "index": 1},
                )


if __name__ == "__main__":
    unittest.main()
