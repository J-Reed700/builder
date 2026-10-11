"""The coverage check itself must fail closed on partial or invalid reports."""
import importlib.util
import math
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("coverage_gate", ROOT / "scripts/check_coverage.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


def report(covered=8, total=10):
    return {"data": [{"files": [{"filename": str(ROOT / "src/lib.rs"), "summary": {"lines": {"covered": covered, "count": total}}}]}]}


class CoverageGateTests(unittest.TestCase):
    def test_exact_floor_passes(self):
        self.assertEqual(gate.check(report(), {"workspace": 80, "src": 80}), [])

    def test_a_single_uncovered_line_can_fail_the_floor(self):
        self.assertTrue(gate.check(report(7), {"workspace": 80, "src": 80}))

    def test_missing_crate_is_not_silently_skipped(self):
        self.assertTrue(gate.check(report(), {"workspace": 80, "src": 80, "crates/new": 80}))

    def test_new_crate_requires_an_explicit_floor(self):
        self.assertTrue(gate.check(report(), {"workspace": 80}))

    def test_empty_report_fails(self):
        self.assertTrue(gate.check({"data": []}, {"workspace": 80, "src": 80}))
        self.assertTrue(gate.check(report(0, 0), {"workspace": 80, "src": 80}))

    def test_invalid_counts_or_thresholds_fail(self):
        for covered, total in [(-1, 10), (11, 10)]:
            with self.assertRaises(ValueError):
                gate.check(report(covered, total), {"workspace": 80, "src": 80})
        for floor in [math.nan, -1, 101]:
            with self.assertRaises(ValueError):
                gate.check(report(), {"workspace": floor, "src": 80})


if __name__ == "__main__":
    unittest.main()
