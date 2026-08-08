import unittest

from evaluate_adapter import element_accuracy, parse_prediction


class MixedActionEvaluationTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
