import pathlib
import sys
import unittest


sys.path.insert(0, str(pathlib.Path(__file__).parent))

from eval_provider import evaluate, evaluate_challenge, evaluate_grounding, evaluate_ocr
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


class AnswerByPurpose(VisionProvider):
    """Answers each request by its purpose; requests for other intents get an
    off-task click."""

    name = "by-purpose"

    def __init__(self, intent_kind, answers):
        self.intent_kind = intent_kind
        self.answers = answers

    def propose(self, request):
        if request.intent_kind != self.intent_kind:
            return ProposeResponse(confidence=0.9, action={"kind": "click", "x": 0.0, "y": 0.0})
        answer = self.answers[request.purpose]
        if isinstance(answer, Exception):
            raise answer
        return answer

    def extract(self, schema, content, purpose):
        raise NotImplementedError


def click(x, y, confidence=0.9):
    return ProposeResponse(confidence=confidence, action={"kind": "click", "x": x, "y": y})


def perception_row(page, elements=(), text_runs=(), challenge="none", dpr=2):
    return {
        "schema": "bobby-perception/1",
        "page": page,
        "imageB64": "cG5n",
        "viewport": {"w": 400, "h": 300, "dpr": dpr},
        "elements": list(elements),
        "textRuns": list(text_runs),
        "challenge": {"kind": challenge, "blocking": challenge != "none"},
    }


def element(role, name, x, y, w=40, h=20, source="text"):
    return {"role": role, "name": name, "nameSource": source, "bbox": {"x": x, "y": y, "w": w, "h": h}}


class GroundingSuiteTests(unittest.TestCase):
    ROW = perception_row("/form", elements=[
        element("button", "Save", 10, 10),
        element("button", "Cancel", 100, 10),
        element("button", "Clear", 200, 10),
        element("button", "Help", 300, 10),
        element("link", "Edit", 10, 100),
        element("link", "Edit", 100, 100),
        element("button", "Close dialog", 200, 100, source="aria"),
    ])

    def test_points_are_scored_in_css_pixels_by_the_box_they_land_in(self):
        # dpr 2: screenshot pixels are twice the CSS box coordinates.
        provider = AnswerByPurpose("locate", {
            'Click the "Save" button': click(60, 40),
            'Click the "Cancel" button': click(60, 40),
            'Click the "Clear" button': click(700, 500),
            'Click the "Help" button': click(640, 40, confidence=0.5),
        })
        results, records = evaluate_grounding(provider, [self.ROW])

        self.assertEqual(
            [(r["name"], r["outcome"]) for r in records],
            [("Save", "hit"), ("Cancel", "wrongElement"), ("Clear", "offTarget"), ("Help", "abstain")],
        )
        self.assertEqual(results["grounding"]["queries"], 4)
        self.assertEqual(results["grounding"]["hit_rate"], 0.25)
        self.assertAlmostEqual(results["grounding"]["hit_rate_answered"], 1 / 3)


class ChallengeSuiteTests(unittest.TestCase):
    def test_detections_are_scored_per_class_against_the_marker_label(self):
        detected = lambda kind: ProposeResponse(
            confidence=0.4, action={"kind": "challengeDetected", "challengeType": kind, "blocking": True}
        )
        answers = iter([detected("recaptchaV2Checkbox"), detected("textCaptcha"),
                        ProposeResponse(confidence=0.9, action={"kind": "noChallengeDetected"}),
                        detected("cookieConsent")])

        class InOrder(VisionProvider):
            name = "in-order"

            def propose(self, request):
                if request.intent_kind != "detectChallenge" or request.stuck != "challengePresent":
                    return click(0, 0)
                return next(answers)

            def extract(self, schema, content, purpose):
                raise NotImplementedError

        rows = [
            perception_row("/contact", challenge="recaptchaV2Checkbox"),
            perception_row("/home"),
            perception_row("/verify", challenge="mfaCodeEntry"),
            perception_row("/consent"),
        ]
        results, records = evaluate_challenge(InOrder(), rows)
        challenge = results["challenge"]

        self.assertEqual(
            [r["predicted"] for r in records],
            ["recaptchaV2Checkbox", "textCaptcha", "none", "invalid"],
        )
        self.assertAlmostEqual(challenge["accuracy"], 1 / 4)
        self.assertEqual(challenge["per_class"]["recaptchaV2Checkbox"]["f1"], 1.0)
        self.assertEqual(challenge["per_class"]["textCaptcha"]["precision"], 0.0)
        self.assertEqual(challenge["per_class"]["mfaCodeEntry"]["recall"], 0.0)
        self.assertAlmostEqual(challenge["macro_f1"], 1 / 3)
        self.assertEqual(challenge["confusion"]["none"], {"textCaptcha": 1, "invalid": 1})


class OcrSuiteTests(unittest.TestCase):
    def test_each_box_is_cropped_in_screenshot_pixels_and_matched_exactly(self):
        row = perception_row("/overview", text_runs=[
            {"text": "Save   changes", "bbox": {"x": 10, "y": 20, "w": 80, "h": 16}, "fontSizePx": 15},
            {"text": "LAST 30 DAYS", "bbox": {"x": 10, "y": 60, "w": 70, "h": 12}, "fontSizePx": 12},
        ])

        class ByCrop:
            name = "by-crop"
            # dpr 2 with 2 CSS px of padding on each side.
            TEXT = {(16, 36, 184, 76): "Save changes", (16, 116, 164, 148): "Last 30 days"}

            def read(self, png, box):
                return self.TEXT.get(box, "")

        results, records = evaluate_ocr(ByCrop(), [row])

        self.assertEqual([(r["exact"], r["casefold"]) for r in records], [(True, True), (False, True)])
        self.assertEqual(results["ocr"]["exact_rate"], 0.5)
        self.assertEqual(results["ocr"]["casefold_rate"], 1.0)
        self.assertGreater(results["ocr"]["mean_cer"], 0.0)


if __name__ == "__main__":
    unittest.main()
