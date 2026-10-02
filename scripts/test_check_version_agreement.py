"""The Homebrew formula may trail the release it is about to be updated for."""
import importlib.util
import pathlib
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parent / "check-version-agreement.py"
SPEC = importlib.util.spec_from_file_location("check_version_agreement", SCRIPT)
agreement = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(agreement)


class FormulaMayTrail(unittest.TestCase):
    def test_a_patch_release_accepts_the_formula_of_the_release_before_it(self):
        self.assertTrue(agreement.formula_may_trail("0.19.0", "0.19.1"))

    def test_a_minor_release_accepts_any_release_of_the_previous_minor(self):
        self.assertTrue(agreement.formula_may_trail("0.18.0", "0.19.0"))
        self.assertTrue(agreement.formula_may_trail("0.19.1", "0.20.0"))

    def test_the_formula_may_already_match(self):
        self.assertTrue(agreement.formula_may_trail("0.19.1", "0.19.1"))

    def test_a_formula_two_minors_behind_or_ahead_is_refused(self):
        self.assertFalse(agreement.formula_may_trail("0.17.0", "0.19.1"))
        self.assertFalse(agreement.formula_may_trail("0.19.2", "0.19.1"))
        self.assertFalse(agreement.formula_may_trail("0.20.0", "0.19.1"))

    def test_another_major_is_refused(self):
        self.assertFalse(agreement.formula_may_trail("0.19.0", "1.0.0"))

    def test_a_malformed_version_is_refused(self):
        self.assertFalse(agreement.formula_may_trail("0.19", "0.19.1"))
        self.assertFalse(agreement.formula_may_trail("0.19.0-rc1", "0.19.1"))


if __name__ == "__main__":
    unittest.main()
