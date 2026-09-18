import base64
import json
import pathlib
import sys
import tempfile
import unittest


sys.path.insert(0, str(pathlib.Path(__file__).parent))

from collect_training_data import VisionDataCollector
from mlx_finetune import build_completion, corpus_privacy_errors


class SecureCollectionTests(unittest.TestCase):
    def test_collected_record_is_private_and_directly_trainable(self):
        with tempfile.TemporaryDirectory() as output_dir:
            collector = VisionDataCollector(output_dir)
            example = collector.collect_vision_proposal(
                screenshot_b64=base64.b64encode(b"sanitized").decode(),
                purpose="Enter maya@atlas.example",
                intent_kind="fill",
                stuck="targetMissing",
                context={
                    "url": "https://alice:secret@example.com/reset/code?token=secret",
                    "candidates": [
                        {
                            "role": "textbox",
                            "name": "Email maya@atlas.example",
                            "ordinal": 1,
                            "bbox": {"x": 10, "y": 20, "w": 100, "h": 30},
                        }
                    ],
                    "recentCommandKinds": ["navigate"],
                },
                model_response={
                    "confidence": 0.9,
                    "action": {
                        "kind": "typeText",
                        "index": 0,
                        "text": "must-not-survive",
                    },
                },
                success=True,
                journey="authenticated",
                step="fill-email",
                error_message="Authorization: Bearer must-not-survive",
                screenshot_sanitized=True,
            )

        record = example.to_dict()
        encoded = json.dumps(record)
        completion = json.loads(build_completion(record))

        self.assertEqual(corpus_privacy_errors(record), [])
        self.assertEqual(record["target_index"], 0)
        self.assertEqual(
            record["model_response"]["action"],
            {"kind": "typeIntoCandidate", "index": 0},
        )
        self.assertEqual(
            completion["action"], {"kind": "typeIntoCandidate", "index": 0}
        )
        self.assertEqual(record["context_url"], "https://example.com/reset/:id")
        self.assertEqual(record["context_candidates"][0]["bbox"]["x"], 10)
        self.assertNotIn("must-not-survive", encoded)
        self.assertNotIn("maya@atlas.example", encoded)

    def test_raw_or_coordinate_only_success_is_rejected(self):
        with tempfile.TemporaryDirectory() as output_dir:
            collector = VisionDataCollector(output_dir)
            common = dict(
                screenshot_b64=base64.b64encode(b"frame").decode(),
                purpose="select the target",
                intent_kind="locate",
                stuck="targetMissing",
                context={"candidates": [{"role": "button", "name": "Save"}]},
                success=True,
                journey="authenticated",
                step="save",
            )

            with self.assertRaisesRegex(ValueError, "sanitized"):
                collector.collect_vision_proposal(
                    **common,
                    model_response={"action": {"kind": "clickCandidate", "index": 0}},
                )
            with self.assertRaisesRegex(ValueError, "candidate index"):
                collector.collect_vision_proposal(
                    **common,
                    model_response={"action": {"kind": "click", "x": 1, "y": 2}},
                    screenshot_sanitized=True,
                )


if __name__ == "__main__":
    unittest.main()
