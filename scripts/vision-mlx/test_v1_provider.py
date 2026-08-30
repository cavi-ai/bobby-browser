import pathlib
import sys
import unittest


sys.path.insert(0, str(pathlib.Path(__file__).parent))

from providers.base import ProposeRequest, VisionContext, VisionContextCandidate
from providers.v1_provider import MlxV1Provider


def request(intent_kind: str) -> ProposeRequest:
    return ProposeRequest(
        purpose="select the field",
        intent_kind=intent_kind,
        stuck="targetMissing",
        screenshot_b64="",
        context=VisionContext(
            candidates=[
                VisionContextCandidate(role="textbox", name="Email"),
                VisionContextCandidate(role="button", name="Continue"),
            ]
        ),
    )


class MlxV1ProviderTests(unittest.TestCase):
    def test_valid_index_maps_to_the_action_owned_by_the_intent(self):
        for intent_kind, action_kind in (
            ("fill", "typeIntoCandidate"),
            ("type", "typeIntoCandidate"),
            ("extract", "extractFromCandidate"),
            ("locate", "clickCandidate"),
        ):
            with self.subTest(intent_kind=intent_kind):
                response = MlxV1Provider()._response_for_index(request(intent_kind), 1)

                self.assertEqual(response.confidence, 0.95)
                self.assertEqual(response.action, {"kind": action_kind, "index": 1})
                self.assertNotIn("text", response.action)
                self.assertNotIn("value", response.action)

    def test_unknown_intent_kind_and_invalid_index_abstain(self):
        # The abstain shape keeps the intent's action kind: the proxy
        # validator rejects a clickCandidate on a fill/extract intent as
        # incompatible, which would turn a clean abstain into a 502.
        for intent_kind, generated, action_kind in (
            ("unknown", "1", "clickCandidate"),
            ("fill", "2", "typeIntoCandidate"),
            ("extract", "not-an-index", "extractFromCandidate"),
        ):
            with self.subTest(intent_kind=intent_kind, generated=generated):
                parsed = MlxV1Provider._parse_index(generated)
                response = MlxV1Provider()._response_for_index(request(intent_kind), parsed)

                self.assertEqual(response.confidence, 0.0)
                self.assertEqual(response.action, {"kind": action_kind, "index": 0})

    def test_explicit_negative_index_abstains(self):
        parsed = MlxV1Provider._parse_index("-1")
        response = MlxV1Provider()._response_for_index(request("fill"), parsed)

        self.assertEqual(parsed, -1)
        self.assertEqual(response.confidence, 0.0)
        self.assertEqual(response.action, {"kind": "typeIntoCandidate", "index": 0})
