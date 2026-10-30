#!/usr/bin/env python3
"""Unified E2E Verification Runner for GPTgrep Remote DevBox & XL-DocBench REEF Readiness.

Usage:
    python3 workspace/tests/e2e/runner.py --all
    python3 workspace/tests/e2e/runner.py --tier 1
    python3 workspace/tests/e2e/runner.py --tier 2
    python3 workspace/tests/e2e/runner.py --tier 3
    python3 workspace/tests/e2e/runner.py --tier 4
    python3 workspace/tests/e2e/runner.py --all --json-report workspace/handoff/e2e-receipt.json
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from test_tier1_features import TestTier1FeatureCoverage
from test_tier2_boundaries import TestTier2BoundaryCornerCases
from test_tier3_combinations import TestTier3CrossFeatureCombinations
from test_tier4_scenarios import TestTier4RealWorldScenarios

TIER_MAP = {
    1: ("tier1_features", TestTier1FeatureCoverage),
    2: ("tier2_boundaries", TestTier2BoundaryCornerCases),
    3: ("tier3_combinations", TestTier3CrossFeatureCombinations),
    4: ("tier4_scenarios", TestTier4RealWorldScenarios),
}


def build_suite(tier: int | None = None) -> unittest.TestSuite:
    """Build unittest suite for selected tier or all tiers."""
    suite = unittest.TestSuite()
    loader = unittest.defaultTestLoader

    if tier is not None:
        if tier not in TIER_MAP:
            raise ValueError(f"Invalid tier: {tier}. Choose from 1, 2, 3, 4.")
        _, test_case = TIER_MAP[tier]
        suite.addTests(loader.loadTestsFromTestCase(test_case))
    else:
        for _, test_case in TIER_MAP.values():
            suite.addTests(loader.loadTestsFromTestCase(test_case))

    return suite


def main() -> None:
    parser = argparse.ArgumentParser(description="GPTgrep Remote DevBox & XL REEF E2E Runner")
    parser.add_argument("--all", action="store_true", help="Run all 4 verification tiers (default)")
    parser.add_argument("--tier", type=int, choices=[1, 2, 3, 4], help="Run a specific tier (1-4)")
    parser.add_argument("--json-report", type=str, help="Save structured verification receipt to JSON file")
    parser.add_argument("-v", "--verbose", action="store_true", default=True, help="Verbose test runner output")
    args = parser.parse_args()

    selected_tier = args.tier if args.tier else None
    suite = build_suite(tier=selected_tier)

    started_at = time.time()
    runner = unittest.TextTestRunner(verbosity=2 if args.verbose else 1)
    result = runner.run(suite)
    elapsed = time.time() - started_at

    tier_stats = {}
    for t_num, (t_name, t_cls) in TIER_MAP.items():
        if selected_tier is None or selected_tier == t_num:
            t_suite = unittest.defaultTestLoader.loadTestsFromTestCase(t_cls)
            tier_stats[t_name] = {
                "tests_count": t_suite.countTestCases(),
            }

    receipt = {
        "schema": "gptgrep.xl-docbench.e2e-verification-receipt.v1",
        "timestamp_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "selected_tier": selected_tier if selected_tier else "all",
        "tests_run": result.testsRun,
        "failures": len(result.failures),
        "errors": len(result.errors),
        "skipped": len(result.skipped),
        "elapsed_seconds": round(elapsed, 4),
        "passed": result.wasSuccessful(),
        "tiers": tier_stats,
    }

    print("\n================================")
    print("--- E2E VERIFICATION RECEIPT ---")
    print("================================")
    print(json.dumps(receipt, indent=2))

    if args.json_report:
        report_path = Path(args.json_report)
        report_path.parent.mkdir(parents=True, exist_ok=True)
        report_path.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
        print(f"\nSaved verification receipt to: {report_path.resolve()}")

    sys.exit(0 if result.wasSuccessful() else 1)


if __name__ == "__main__":
    main()
