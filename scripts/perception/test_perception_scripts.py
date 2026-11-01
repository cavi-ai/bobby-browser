import base64
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import detector_data  # noqa: E402
import generate_pages  # noqa: E402


class GeneratePagesTest(unittest.TestCase):
    def test_the_same_seed_writes_the_same_pages(self):
        self.assertEqual(generate_pages.generate(5, 11), generate_pages.generate(5, 11))
        self.assertNotEqual(generate_pages.generate(5, 11), generate_pages.generate(5, 12))

    def test_pages_carry_controls_and_non_interactive_look_alikes(self):
        pages = generate_pages.generate(200, 3)
        for page in pages:
            self.assertTrue(page.startswith("<!doctype html>"))
            self.assertTrue(any(tag in page for tag in ("<button", "<a href", "<input", "<select", "<textarea")))
        self.assertTrue(any("border-radius:999px;padding:2px 8px" in page for page in pages))

    def test_the_command_writes_one_file_per_page(self):
        with tempfile.TemporaryDirectory() as out:
            subprocess.run(
                [sys.executable, str(HERE / "generate_pages.py"), "--out", out, "--count", "3", "--seed", "1"],
                check=True,
                capture_output=True,
            )
            self.assertEqual(sorted(path.name for path in pathlib.Path(out).iterdir()),
                             ["page-00000.html", "page-00001.html", "page-00002.html"])


class DetectorDataTest(unittest.TestCase):
    def row(self, elements, dpr=1):
        return {"viewport": {"dpr": dpr}, "elements": elements, "imageB64": base64.b64encode(b"png").decode()}

    def test_boxes_scale_to_screenshot_pixels_and_drop_specks(self):
        row = self.row(
            [
                {"bbox": {"x": 10, "y": 20, "w": 30, "h": 12}},
                {"bbox": {"x": 0, "y": 0, "w": 1.5, "h": 40}},
            ],
            dpr=2,
        )
        self.assertEqual(detector_data.boxes(row), [(20.0, 40.0, 60.0, 24.0)])
        self.assertEqual(detector_data.png(row), b"png")

    def test_rows_skip_blank_lines(self):
        with tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False) as handle:
            handle.write(json.dumps(self.row([])) + "\n\n")
        self.assertEqual(len(list(detector_data.rows(handle.name))), 1)

    def test_iou_of_identical_disjoint_and_half_overlapping_boxes(self):
        self.assertEqual(detector_data.iou((0, 0, 10, 10), (0, 0, 10, 10)), 1.0)
        self.assertEqual(detector_data.iou((0, 0, 10, 10), (20, 20, 5, 5)), 0.0)
        self.assertAlmostEqual(detector_data.iou((0, 0, 10, 10), (5, 0, 10, 10)), 50 / 150)

    def test_each_truth_box_matches_at_most_one_prediction(self):
        truth = [(0, 0, 10, 10), (100, 0, 10, 10)]
        predicted = [(0, 0, 10, 10), (1, 0, 10, 10), (300, 0, 10, 10)]
        self.assertEqual(detector_data.match(predicted, truth), (1, 3, 2))


if __name__ == "__main__":
    unittest.main()
