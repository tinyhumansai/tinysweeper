"""Behavioral tests for the profile-specific replay scorecard gate."""

import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("check-eval-scorecard.py")
SPEC = importlib.util.spec_from_file_location("check_eval_scorecard", SCRIPT)
CHECKER = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(CHECKER)


def scorecard() -> dict:
    return {
        "loose_replays": 0,
        "execution_limits": {"selected_case_ids": ["case-a"]},
        "cases": [
            {
                "id": "case-a",
                "error": None,
                "true_positives": 0,
                "missed": ["E1"],
            }
        ],
    }


class ScorecardGateTests(unittest.TestCase):
    def test_expected_strict_historical_group_passes_with_zero_recall(self) -> None:
        self.assertEqual(CHECKER.validate(scorecard(), {"case-a"}), (0, 1, []))

    def test_missing_or_extra_cases_fail(self) -> None:
        with self.assertRaisesRegex(ValueError, "selected case set differs"):
            CHECKER.validate(scorecard(), {"case-a", "case-b"})

    def test_case_errors_fail_the_strict_group(self) -> None:
        card = scorecard()
        card["cases"][0]["error"] = "strict cassette miss"
        with self.assertRaisesRegex(ValueError, "strict replay/review errors"):
            CHECKER.validate(card, {"case-a"})

    def test_loose_replays_fail(self) -> None:
        card = scorecard()
        card["loose_replays"] = 1
        with self.assertRaisesRegex(ValueError, "strict replay required"):
            CHECKER.validate(card, {"case-a"})

    def test_unknown_execution_limits_fail(self) -> None:
        card = scorecard()
        del card["execution_limits"]
        with self.assertRaisesRegex(ValueError, "does not persist"):
            CHECKER.validate(card, {"case-a"})

    def test_diagnostic_legacy_errors_are_reported_but_not_called_passes(self) -> None:
        card = scorecard()
        card["cases"][0]["error"] = "stale cassette"
        matched, missed, errors = CHECKER.validate(
            card, {"case-a"}, allow_review_errors=True
        )
        self.assertEqual((matched, missed, errors), (0, 1, ["case-a"]))
        summary = CHECKER.render_summary(
            "legacy-default", card["cases"], matched, missed, errors,
            card["execution_limits"], diagnostic=True
        )
        self.assertIn("diagnostic-only", summary)
        self.assertIn("Strict/review errors: case-a", summary)
        self.assertNotIn("strict replay completed with no case errors", summary)


if __name__ == "__main__":
    unittest.main()
