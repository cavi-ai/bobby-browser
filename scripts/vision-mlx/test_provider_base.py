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

    def test_string_action_unknown_siblings_are_dropped_without_echo(self):
        secret = "runtime-secret-string-action-field"
        for kind in ("click", "typeText", "extractValue"):
            with self.subTest(kind=kind):
                response = VisionProvider.normalize_response(
                    {"confidence": 0.8, "action": kind, "unknown": secret}
                )
                self.assertNotIn(secret, str(response.to_dict()))
        # Candidate kinds have no default payload, so a bare string action
        # with no usable index is invalid regardless of the dropped extra.
        with self.assertRaises(ValueError):
            VisionProvider.normalize_response(
                {"confidence": 0.8, "action": "clickCandidate", "unknown": secret}
            )

    def test_request_and_action_unknown_fields_fail_closed_without_echoing_values(self):
        secret = "runtime-secret-provider-field"
        with self.assertRaisesRegex(ValueError, "invalid propose request fields") as error:
            ProposeRequest.from_dict({"purpose": "p", "intentKind": "fill", "stuck": "s", "screenshotPng": "x", "text": secret})
        self.assertNotIn(secret, str(error.exception))
        with self.assertRaises(ValueError):
            ProposeResponse(0.9, {"kind": "typeIntoCandidate", "index": 0, "text": secret}).validate()

    def test_dict_action_extra_response_fields_are_dropped_without_echoing_values(self):
        # A chatty sibling on a dict action never reaches the action or the
        # normalized output, so dropping it cannot leak a typed value — but
        # it also must not be fatal: chatty models emit one deterministically.
        secret = "runtime-secret-provider-field"
        response = VisionProvider.normalize_response(
            {"confidence": 0.9, "action": {"kind": "click", "x": 1, "y": 2}, "text": secret}
        )
        self.assertEqual(response.action, {"kind": "click", "x": 1.0, "y": 2.0})
        self.assertNotIn(secret, str(response.to_dict()))

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

    def test_challenge_solved_round_trips_without_a_payload(self):
        for kind in ("challengeSolved", "challenge_solved"):
            with self.subTest(kind=kind):
                response = VisionProvider.normalize_response(
                    {"confidence": 0.9, "action": {"kind": kind}}
                )
                self.assertEqual(response.confidence, 0.9)
                self.assertEqual(response.action, {"kind": "challengeSolved"})

    def test_list_typed_coordinates_unwrap(self):
        for given, expected in (
            ({"kind": "click", "x": [566], "y": [671]}, {"kind": "click", "x": 566.0, "y": 671.0}),
            ({"kind": "click", "x": [566, 671]}, {"kind": "click", "x": 566.0, "y": 671.0}),
            ({"kind": "click", "coordinate": [[566, 671]]}, {"kind": "click", "x": 566.0, "y": 671.0}),
        ):
            with self.subTest(given=given):
                self.assertEqual(VisionProvider.normalize_action(given), expected)

    def test_snake_case_action_kinds_canonicalize(self):
        for given, canonical, payload in (
            ("type_text", "typeText", {"text": "hello"}),
            ("extract_value", "extractValue", {"value": "hello"}),
            ("click_candidate", "clickCandidate", {"index": 0}),
            ("type_into_candidate", "typeIntoCandidate", {"index": 0}),
            ("extract_from_candidate", "extractFromCandidate", {"index": 0}),
        ):
            with self.subTest(given=given):
                action = VisionProvider.normalize_action({"kind": given, **payload})
                self.assertEqual(action["kind"], canonical)

    def test_dict_action_tolerates_non_action_response_fields(self):
        # Models often add a "reasoning" or commentary sibling; it never
        # reaches the action, so it is dropped rather than fatal.
        response = VisionProvider.normalize_response(
            {
                "confidence": 0.9,
                "action": {"kind": "click", "x": 1, "y": 2},
                "reasoning": "the checkbox is at those coordinates",
            }
        )
        self.assertEqual(response.action, {"kind": "click", "x": 1.0, "y": 2.0})

    def test_string_action_unknown_siblings_are_dropped_not_merged(self):
        # Unknown siblings of a string action are dropped rather than merged
        # into the payload, so a typed value cannot ride along.
        secret = "runtime-secret-provider-field"
        response = VisionProvider.normalize_response(
            {"confidence": 0.9, "action": "click", "x": 3, "y": 4, "secret": secret}
        )
        self.assertEqual(response.action, {"kind": "click", "x": 3.0, "y": 4.0})
        self.assertNotIn(secret, str(response.to_dict()))

    def test_challenge_solved_rejects_any_payload(self):
        with self.assertRaises(ValueError):
            VisionProvider.normalize_response(
                {"confidence": 0.9, "action": {"kind": "challengeSolved", "x": 1}}
            )

    def test_propose_prompt_covers_candidates_and_solve_challenge(self):
        for required in (
            "challengeSolved",
            "clickCandidate",
            "typeIntoCandidate",
            "extractFromCandidate",
            "solveChallenge",
            "green",
        ):
            self.assertIn(required, VisionProvider.PROPOSE_SYSTEM)


if __name__ == "__main__":
    unittest.main()
