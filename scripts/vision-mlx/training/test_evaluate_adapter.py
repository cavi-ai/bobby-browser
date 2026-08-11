import json
import pathlib
import sys
import unittest


sys.path.insert(0, str(pathlib.Path(__file__).parent))

from evaluate_adapter import (
    element_accuracy,
    parse_prediction,
    parse_v1_response,
    v1_metrics,
)
from mlx_finetune import build_completion, build_prompt


class MixedActionEvaluationTests(unittest.TestCase):
    def test_candidate_typing_scores_the_selected_target_without_content(self):
        predictions = [
            {"prediction": {"action": {"kind": "typeIntoCandidate", "index": 1}}}
        ]
        examples = [
            {
                "target_index": 1,
                "context_candidates": [{}, {}],
                "model_response": {
                    "action": {"kind": "typeText", "text": "runtime secret"}
                },
            }
        ]

        result = element_accuracy(predictions, examples)

        self.assertEqual(result["correct"], 1)
        self.assertEqual(result["content_scored"], 0)
        self.assertEqual(result["content_correct"], 0)

    def test_candidate_completion_omits_runtime_owned_text(self):
        completion = json.loads(
            build_completion(
                {
                    "target_index": 1,
                    "model_response": {
                        "confidence": 0.9,
                        "action": {"kind": "typeText", "text": "runtime secret"},
                    },
                },
                schema="candidate",
            )
        )

        self.assertEqual(
            completion["action"], {"kind": "typeIntoCandidate", "index": 1}
        )
        self.assertNotIn("runtime secret", json.dumps(completion))

    def test_production_camel_case_record_flows_to_completion_and_evaluator(self):
        record = {
            "purpose": "Fill email",
            "intent_kind": "fill",
            "stuck": "targetAmbiguous",
            "targetIndex": 1,
            "contextCandidates": [
                {"role": "textbox", "name": "First"},
                {"role": "textbox", "name": "Email"},
            ],
            "modelResponse": {"confidence": 0.9, "action": {"kind": "typeIntoCandidate", "index": 1}},
        }
        completion = json.loads(build_completion(record, schema="candidate"))
        prompt = build_prompt(record, schema="candidate")
        result = element_accuracy([{"prediction": completion}], [record])
        self.assertIn("candidates", prompt)
        self.assertEqual(completion["action"], {"kind": "typeIntoCandidate", "index": 1})
        self.assertEqual(result["correct"], 1)

    def test_snake_case_candidate_kind_is_canonicalized_symmetrically(self):
        example = {"target_index": 0, "model_response": {"action": {"kind": "extract_from_candidate", "index": 0}}}
        completion = json.loads(build_completion(example, schema="candidate"))
        result = element_accuracy([{"prediction": completion}], [example])
        self.assertEqual(completion["action"], {"kind": "extractFromCandidate", "index": 0})
        self.assertEqual(result["correct"], 1)

    def test_wrong_action_kind_cannot_receive_element_credit(self):
        predictions = [{"prediction": {"action": {"kind": "typeText", "text": "Save"}}}]
        examples = [
            {
                "target_index": 0,
                "context_candidates": [
                    {"bbox": {"x": 10, "y": 20, "w": 100, "h": 40}}
                ],
                "model_response": {
                    "action": {"kind": "click", "x": 30.0, "y": 30.0}
                },
            }
        ]

        result = element_accuracy(predictions, examples)

        self.assertEqual(result["scored"], 1)
        self.assertEqual(result["correct"], 0)
        self.assertEqual(result["content_scored"], 0)

    def test_candidate_extraction_scores_target_but_not_unemitted_content(self):
        predictions = [
            {"prediction": {"action": {"kind": "extractFromCandidate", "index": 1}}}
        ]
        examples = [
            {
                "target_index": 1,
                "context_candidates": [{}, {}],
                "model_response": {
                    "action": {"kind": "extractValue", "value": "Paid Invoice"}
                },
            }
        ]

        result = element_accuracy(predictions, examples)

        self.assertEqual(result["correct"], 1)
        self.assertEqual(result["content_scored"], 0)
        self.assertEqual(result["content_correct"], 0)

    def test_non_numeric_click_coordinates_are_counted_incorrect_instead_of_crashing(self):
        predictions = [
            {"prediction": {"action": {"kind": "click", "x": "30", "y": None}}}
        ]
        examples = [
            {
                "target_index": 0,
                "context_candidates": [
                    {"bbox": {"x": 10, "y": 20, "w": 100, "h": 40}}
                ],
                "model_response": {
                    "action": {"kind": "click", "x": 30.0, "y": 30.0}
                },
            }
        ]

        result = element_accuracy(predictions, examples)

        self.assertEqual(result["scored"], 1)
        self.assertEqual(result["correct"], 0)

    def test_payload_error_does_not_erase_correct_target_but_fails_full_action(self):
        predictions = [
            {"prediction": {"action": {"kind": "typeText", "text": "wrong"}}}
        ]
        examples = [
            {
                "target_index": 0,
                "context_candidates": [{"role": "textbox"}],
                "model_response": {
                    "action": {"kind": "typeText", "text": "alice@example.com"}
                },
            }
        ]

        result = element_accuracy(predictions, examples)

        self.assertEqual(result["correct"], 1)
        self.assertEqual(result["content_correct"], 0)
        self.assertEqual(result["fully_correct"], 0)
        self.assertEqual(result["fully_correct_accuracy"], 0)

    def test_parser_rejects_non_numeric_click_coordinates(self):
        prediction = parse_prediction(
            '{"confidence":0.5,"action":{"kind":"click","x":"30","y":null}}'
        )

        self.assertIsNone(prediction)

    def test_parser_rejects_unknown_action_kind(self):
        prediction = parse_prediction(
            '{"confidence":0.5,"action":{"kind":"hover","index":0}}'
        )

        self.assertIsNone(prediction)

    def test_v1_parser_rejects_explanatory_text(self):
        self.assertIsNone(parse_v1_response("I choose 2 because it is the button", 3))

    def test_v1_parser_rejects_out_of_range_indexes(self):
        self.assertIsNone(parse_v1_response("9", 3))

    def test_v1_parser_accepts_explicit_abstention(self):
        self.assertEqual(parse_v1_response("-1", 3), -1)

    def test_v1_parse_failure_does_not_count_as_a_correct_abstention(self):
        result = v1_metrics(
            [{"prediction": None}],
            [{"negative": True, "target_index": None}],
        )

        self.assertEqual(result["abstain_recall"], 0.0)


if __name__ == "__main__":
    unittest.main()
