import copy
import json
import pathlib
import sys
import tempfile
import unittest


sys.path.insert(0, str(pathlib.Path(__file__).parent))

from model_promotion import (
    PromotionError,
    activate_release,
    adapter_digest,
    build_assessment,
    promote_adapter,
    sha256_file,
)
from adapter_freshness import write_promotion_assessment
from mlx_finetune import (
    MLXFineTuneConfig,
    build_training_metadata,
    deduplicate_examples,
)


def record(index=0, *, success=True, purpose="Choose Search", step="step"):
    row = {
        "purpose": purpose,
        "intent_kind": "locate",
        "stuck": "targetMissing",
        "context_url": "https://example.com/search",
        "context_candidates": [
            {"role": "button", "name": "Search"},
            {"role": "link", "name": "Cancel"},
        ],
        "model_response": {
            "confidence": 1.0,
            "action": {"kind": "clickCandidate", "index": index},
        },
        "success": success,
        "journey": "authenticated",
        "step": step,
        "privacy_version": 1,
    }
    if index is not None:
        row["target_index"] = index
    return row


class ModelPromotionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temp.name)
        self.adapter = self.root / "adapter"
        self.adapter.mkdir()
        (self.adapter / "adapters.safetensors").write_bytes(b"adapter-v1")
        (self.adapter / "adapter_config.json").write_text(
            json.dumps({"fine_tune_type": "lora", "num_layers": 16})
        )
        (self.adapter / "metadata.json").write_text(
            json.dumps({"base_model": "local/base", "schema": "v1"})
        )

        self.corpus = self.root / "vision-corpus.jsonl"
        rows = [record(step=f"positive-{i}") for i in range(4)]
        rows.append(record(None, success=False, purpose="Unknown control", step="negative"))
        self.corpus.write_text("".join(json.dumps(row) + "\n" for row in rows))

        self.suites = {
            "corpus": {
                "metrics": {
                    "element_accuracy": 1.0,
                    "abstain_recall_production": 1.0,
                    "abstain_recall_singletons": 1.0,
                    "abstain_precision": 1.0,
                },
                "floors": {
                    "element_accuracy": 0.98,
                    "abstain_recall_production": 1.0,
                    "abstain_recall_singletons": 0.5,
                    "abstain_precision": 0.95,
                },
            },
            "paraphrase": {
                "metrics": {"element_accuracy": 0.9},
                "floors": {"element_accuracy": 0.85},
            },
            "contrastive": {
                "metrics": {
                    "element_accuracy": 1.0,
                    "abstain_recall": 1.0,
                    "abstain_precision": 1.0,
                },
                "floors": {
                    "element_accuracy": 1.0,
                    "abstain_recall": 0.66,
                    "abstain_precision": 1.0,
                },
            },
        }

    def tearDown(self):
        self.temp.cleanup()

    def assessment(self):
        return build_assessment(
            adapter_dir=self.adapter,
            corpus_path=self.corpus,
            base_model="local/base",
            schema="v1",
            suites=self.suites,
        )

    def test_promotes_verified_adapter_with_content_bound_manifest(self):
        assessment = self.assessment()
        report = self.root / "assessment.json"
        report.write_text(json.dumps(assessment))

        release = promote_adapter(
            adapter_dir=self.adapter,
            corpus_path=self.corpus,
            assessment_path=report,
            store_dir=self.root / "promoted",
            version="v1.0.0",
        )

        manifest = json.loads((release / "promotion.json").read_text())
        self.assertEqual(manifest["version"], "v1.0.0")
        self.assertEqual(manifest["adapter"]["sha256"], adapter_digest(self.adapter))
        self.assertEqual(manifest["corpus"]["sha256"], sha256_file(self.corpus))
        self.assertEqual(manifest["assessmentSha256"], sha256_file(report))
        self.assertFalse((self.root / "promoted" / "current").exists())
        self.assertFalse((self.root / "promoted" / "current.json").exists())
        self.assertEqual((release.stat().st_mode & 0o777), 0o700)
        self.assertEqual(((release / "adapters.safetensors").stat().st_mode & 0o777), 0o600)

    def test_rejects_report_when_corpus_or_adapter_changed(self):
        report = self.root / "assessment.json"
        report.write_text(json.dumps(self.assessment()))
        self.corpus.write_text(self.corpus.read_text() + json.dumps(record(step="later")) + "\n")

        with self.assertRaisesRegex(PromotionError, "corpus digest"):
            promote_adapter(
                self.adapter,
                self.corpus,
                report,
                self.root / "promoted",
                "v1",
            )

        self.corpus.write_text(self.corpus.read_text().rsplit("\n", 2)[0] + "\n")
        report.write_text(json.dumps(self.assessment()))
        (self.adapter / "adapters.safetensors").write_bytes(b"changed")
        with self.assertRaisesRegex(PromotionError, "adapter digest"):
            promote_adapter(
                self.adapter,
                self.corpus,
                report,
                self.root / "promoted",
                "v1",
            )

    def test_rejects_failed_floor_unsafe_corpus_and_version_collision(self):
        failed = copy.deepcopy(self.suites)
        failed["corpus"]["metrics"]["element_accuracy"] = 0.5
        with self.assertRaisesRegex(PromotionError, "element_accuracy"):
            build_assessment(
                self.adapter,
                self.corpus,
                "local/base",
                "v1",
                failed,
            )
        with self.assertRaisesRegex(PromotionError, "missing required suites"):
            build_assessment(
                self.adapter,
                self.corpus,
                "local/base",
                "v1",
                {"corpus": self.suites["corpus"]},
            )

        report = self.root / "assessment.json"
        report.write_text(json.dumps(self.assessment()))
        promote_adapter(
            self.adapter,
            self.corpus,
            report,
            self.root / "promoted",
            "v1",
        )
        (self.root / "promoted" / "releases" / "v2").mkdir()
        with self.assertRaisesRegex(PromotionError, "already exists"):
            promote_adapter(
                self.adapter,
                self.corpus,
                report,
                self.root / "promoted",
                "v2",
            )

        unsafe = json.loads(self.corpus.read_text().splitlines()[0])
        unsafe["context_url"] = "https://user:secret@example.com/?token=secret"
        self.corpus.write_text(json.dumps(unsafe) + "\n")
        with self.assertRaisesRegex(PromotionError, "unsafe corpus"):
            build_assessment(
                self.adapter,
                self.corpus,
                "local/base",
                "v1",
                self.suites,
            )

    def test_rejects_private_paths_and_invalid_versions(self):
        with self.assertRaisesRegex(PromotionError, "base model"):
            build_assessment(
                self.adapter,
                self.corpus,
                str(self.root / "private-model"),
                "v1",
                self.suites,
            )

        report = self.root / "assessment.json"
        report.write_text(json.dumps(self.assessment()))
        with self.assertRaisesRegex(PromotionError, "version"):
            promote_adapter(
                self.adapter,
                self.corpus,
                report,
                self.root / "promoted",
                "../escape",
            )

        forged = self.assessment()
        forged["suites"]["corpus"]["metrics"]["element_accuracy"] = 0.0
        forged["suites"]["corpus"]["floors"]["element_accuracy"] = 0.0
        report.write_text(json.dumps(forged))
        with self.assertRaisesRegex(PromotionError, "floor"):
            promote_adapter(
                self.adapter,
                self.corpus,
                report,
                self.root / "promoted",
                "v1",
            )

    def test_freshness_results_write_a_promotion_assessment(self):
        report = self.root / "assessment.json"
        metrics = {
            "corpus": {
                "element_accuracy": 1.0,
                "abstain_recall_production": 1.0,
                "abstain_recall_singletons": 1.0,
                "abstain_precision": 1.0,
            },
            "paraphrase": {"element_accuracy": 0.9},
            "contrastive": {
                "element_accuracy": 1.0,
                "abstain_recall": 1.0,
                "abstain_precision": 1.0,
            },
        }

        assessment = write_promotion_assessment(
            adapter_dir=self.adapter,
            corpus_path=self.corpus,
            base_model="local/base",
            schema="v1",
            metrics_by_suite=metrics,
            output_path=report,
        )

        self.assertEqual(json.loads(report.read_text()), assessment)
        self.assertEqual(assessment["verdict"], "passed")
        self.assertEqual(
            assessment["suites"]["contrastive"]["floors"]["abstain_recall"],
            0.66,
        )
        self.assertEqual(report.stat().st_mode & 0o777, 0o600)

    def test_training_deduplicates_replayed_records_without_merging_labels(self):
        first = record(step="same")
        first.update({"timestamp": "one", "run_id": "run-one"})
        replay = dict(first)
        replay.update({"timestamp": "two", "run_id": "run-two"})
        different_label = dict(replay)
        different_label["target_index"] = 1
        different_label["model_response"] = {
            "confidence": 1.0,
            "action": {"kind": "clickCandidate", "index": 1},
        }

        unique = deduplicate_examples([first, replay, different_label])

        self.assertEqual(unique, [first, different_label])

    def test_training_metadata_binds_corpus_without_recording_private_path(self):
        config = MLXFineTuneConfig(
            model_name="local/base",
            input_path=str(self.corpus),
            schema="v1",
        )

        metadata = build_training_metadata(
            config,
            {"train": 4, "valid": 1},
            corpus_records=5,
            supervised_examples=5,
        )

        self.assertNotIn("training_data", metadata)
        self.assertNotIn(str(self.root), json.dumps(metadata))
        self.assertEqual(metadata["training_corpus"]["sha256"], sha256_file(self.corpus))
        self.assertEqual(metadata["training_corpus"]["records"], 5)
        self.assertEqual(metadata["training_corpus"]["supervised_examples"], 5)

    def test_activation_supports_verified_rollback_and_rejects_tampering(self):
        report = self.root / "assessment.json"
        report.write_text(json.dumps(self.assessment()))
        store = self.root / "promoted"
        first = promote_adapter(self.adapter, self.corpus, report, store, "v1")
        activate_release(store, "v1")

        (self.adapter / "adapters.safetensors").write_bytes(b"adapter-v2")
        report.write_text(json.dumps(self.assessment()))
        promote_adapter(self.adapter, self.corpus, report, store, "v2")
        activate_release(store, "v2")

        current = activate_release(store, "v1")
        self.assertEqual(current["version"], "v1")
        self.assertEqual((store / "current").resolve(), first.resolve())

        (first / "adapters.safetensors").write_bytes(b"tampered")
        with self.assertRaisesRegex(PromotionError, "adapter digest"):
            activate_release(store, "v1")


if __name__ == "__main__":
    unittest.main()
