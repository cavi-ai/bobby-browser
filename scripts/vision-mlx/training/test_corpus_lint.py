import json
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from corpus_lint import lint


def record(**overrides):
    base = {
        "purpose": "Run the customer search",
        "intent_kind": "locate",
        "stuck": "targetMissing",
        "context_url": "http://127.0.0.1/",
        "context_candidates": [
            {"role": "button", "name": "Search"},
            {"role": "link", "name": "Cancel"},
        ],
        "target_index": 0,
        "model_response": {"confidence": 1.0, "action": {"kind": "click"}},
        "success": True,
        "journey": "unit",
        "step": "s",
    }
    base.update(overrides)
    return base


def negative(purpose, **overrides):
    base = {
        "purpose": purpose,
        "target_index": None,
        "success": False,
        "outcome_stage": "visionRejectionFloor",
    }
    base.update(overrides)
    return record(**base)


class CorpusLintTests(unittest.TestCase):
    def test_clean_corpus_passes(self):
        rows = [record(step=f"s{i}") for i in range(4)] + [negative("the widget in the corner")]
        errors, _ = lint(rows)
        self.assertEqual(errors, [])

    def test_landmark_role_in_window_is_an_error(self):
        rows = [
            record(
                context_candidates=[
                    {"role": "main", "name": "Name Continue Resume"},
                    {"role": "button", "name": "Continue"},
                ],
                target_index=1,
            ),
            negative("vague"),
        ]
        errors, _ = lint(rows)
        self.assertTrue(any("non-actionable role 'main'" in e for e in errors))

    def test_empty_window_is_an_error(self):
        rows = [record(context_candidates=[], target_index=None, success=False)]
        errors, _ = lint(rows)
        self.assertTrue(any("empty candidate window" in e for e in errors))

    def test_out_of_range_target_is_an_error(self):
        rows = [record(target_index=5), negative("vague")]
        errors, _ = lint(rows)
        self.assertTrue(any("out of range" in e for e in errors))

    def test_negative_without_success_false_is_an_error(self):
        rows = [record(), negative("vague", success=True)]
        errors, _ = lint(rows)
        self.assertTrue(any("success is not False" in e for e in errors))

    def test_starved_negative_class_is_an_error(self):
        rows = [record(step=f"s{i}") for i in range(45)] + [
            negative(f"vague {i}") for i in range(5)
        ]
        errors, _ = lint(rows)
        self.assertTrue(any("outside the healthy band" in e for e in errors))

    def test_small_corpus_skips_the_balance_band(self):
        # CI smoke corpora (one grow run per journey) have few negatives;
        # presence is asserted, proportions are not.
        rows = [record(step=f"s{i}") for i in range(12)] + [negative("vague")]
        errors, _ = lint(rows)
        self.assertEqual(errors, [])

    def test_no_negatives_is_an_error(self):
        rows = [record(step=f"s{i}") for i in range(10)]
        errors, _ = lint(rows)
        self.assertTrue(any("no abstain-labeled negatives" in e for e in errors))

    def test_repeated_negative_same_window_warns_not_errors(self):
        rows = [record(step=f"s{i}") for i in range(8)] + [
            negative("the widget in the corner"),
            negative("the widget in the corner"),
        ]
        errors, warnings = lint(rows)
        self.assertEqual(errors, [])
        self.assertTrue(any("identical window" in w for w in warnings))

    def test_unknown_intent_kind_is_an_error(self):
        errors, _ = lint([record(intent_kind="hover"), negative("vague")])
        self.assertTrue(any("unknown intent_kind" in e for e in errors))

    def test_missing_required_field_is_an_error(self):
        row = record()
        del row["model_response"]
        errors, _ = lint([row, negative("vague")])
        self.assertTrue(any("missing required field" in e for e in errors))


if __name__ == "__main__":
    unittest.main()
