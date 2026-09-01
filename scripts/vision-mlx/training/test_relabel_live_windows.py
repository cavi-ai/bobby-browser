import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from relabel_live_windows import STEP_BY_PURPOSE, relabel, validate_positives


class RelabelLiveWindowsTests(unittest.TestCase):
    def test_customer_update_steps_map_to_the_scripted_targets(self):
        expected = {
            "Trigger the search now": ("Search", "run_search"),
            "Open the Atlas Labs page": ("Atlas Labs", "open_customer"),
            "Save the updated customer priority": ("Save priority", "save_priority"),
        }

        for purpose, (target, step) in expected.items():
            with self.subTest(purpose=purpose):
                record = {
                    "purpose": purpose,
                    "contextCandidates": [
                        {"role": "link", "name": "Customers"},
                        {"role": "button", "name": target},
                    ],
                }
                result = relabel(record)
                self.assertEqual(result["target_index"], 1)
                self.assertEqual(result["step"], step)

    def test_customer_update_wrong_pick_is_dropped(self):
        purpose = "Trigger the search now"
        self.assertIn(purpose, STEP_BY_PURPOSE)
        records = [
            {
                "purpose": purpose,
                "success": True,
                "targetIndex": 0,
                "contextCandidates": [
                    {"role": "link", "name": "Customers"},
                    {"role": "button", "name": "Search"},
                ],
            }
        ]

        kept, dropped = validate_positives(records)

        self.assertEqual(kept, [])
        self.assertEqual(dropped, [(purpose, "Search", "Customers")])


if __name__ == "__main__":
    unittest.main()
