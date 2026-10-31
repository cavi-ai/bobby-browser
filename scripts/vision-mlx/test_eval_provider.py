import pathlib
import sys
import unittest


sys.path.insert(0, str(pathlib.Path(__file__).parent))

from eval_provider import evaluate
from providers.base import ProposeResponse, VisionProvider
from providers.v1_provider import MlxV1Provider


class NamedCandidateSelector(MlxV1Provider):
    """The v1 provider with generation replaced by a reader that picks the
    candidate named in the purpose, or -1 when none is."""

    def _generate_index(self, request):
        candidates = request.context.candidates if request.context else []
        for index, candidate in enumerate(candidates):
            if candidate.name in request.purpose:
                return str(index)
        return "-1"


class FixedResponse(VisionProvider):
    name = "fixed"

    def __init__(self, response):
        self.response = response

    def propose(self, request):
        if isinstance(self.response, Exception):
            raise self.response
        return self.response

    def extract(self, schema, content, purpose):
        raise NotImplementedError


CANDIDATES = [{"role": "link", "name": "Overview"}, {"role": "link", "name": "Reports"}]

WINDOWS = [
    {
        "purpose": "Open Reports",
        "intentKind": "locate",
        "stuck": "targetMissing",
        "imageB64": "cG5n",
        "contextUrl": "http://127.0.0.1/app",
        "contextCandidates": CANDIDATES,
        "targetIndex": 1,
        "success": True,
    },
    {
        "purpose": "Go to Overview",
        "intent_kind": "locate",
        "stuck": "targetAmbiguous",
        "image_b64": "cG5n",
        "context_candidates": CANDIDATES,
        "target_index": 0,
        "success": True,
    },
    {
        "purpose": "Delete the account",
        "intent_kind": "locate",
        "stuck": "targetMissing",
        "image_b64": "cG5n",
        "context_candidates": CANDIDATES,
        "target_index": None,
        "success": False,
        "outcome_stage": "visionRejectionFloor",
    },
]


class WindowSuiteTests(unittest.TestCase):
    def test_candidates_reach_the_provider_and_picks_are_scored_by_index(self):
        results, rows = evaluate(NamedCandidateSelector(), WINDOWS)
        window = results["window"]

        self.assertEqual([row["pick"] for row in rows], [1, 0, -1])
        self.assertEqual(window["answered"], 2)
        self.assertEqual(window["element_accuracy"], 1.0)
        self.assertEqual(window["abstain_rate_positive"], 0.0)
        self.assertEqual(window["abstain_recall_production"], 1.0)
        self.assertEqual(window["abstain_precision"], 1.0)
        self.assertEqual(window["point_answers"], 0)

    def test_a_pick_below_the_runtime_floor_is_an_abstention(self):
        below_floor = ProposeResponse(
            confidence=0.5, action={"kind": "clickCandidate", "index": 1}
        )
        results, rows = evaluate(FixedResponse(below_floor), WINDOWS[:1])

        self.assertEqual(rows[0]["pick"], -1)
        self.assertEqual(results["window"]["answered"], 0)
        self.assertEqual(results["window"]["abstain_rate_positive"], 1.0)

    def test_a_provider_error_acts_on_nothing_and_is_counted(self):
        results, rows = evaluate(FixedResponse(RuntimeError("502")), WINDOWS[2:])

        self.assertEqual(rows[0]["pick"], -1)
        self.assertEqual(results["window"]["errors"], 1)
        self.assertEqual(results["window"]["abstain_recall_production"], 1.0)

    def test_a_click_point_is_reported_not_scored(self):
        point = ProposeResponse(confidence=0.9, action={"kind": "click", "x": 10.0, "y": 20.0})
        results, rows = evaluate(FixedResponse(point), WINDOWS[:1])

        self.assertIsNone(rows[0]["pick"])
        self.assertEqual(results["window"]["point_answers"], 1)
        self.assertEqual(results["window"]["answered"], 0)


if __name__ == "__main__":
    unittest.main()
