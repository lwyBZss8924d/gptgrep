"""E2E Verification Test Runner for XL-DocBench OCR Pipeline.

Usage:
    python3 workspace/tests/run_e2e_verification.py --all
    python3 workspace/tests/run_e2e_verification.py --tier 1
"""
import argparse
import json
from pathlib import Path
import sys
import time
import unittest

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))


def build_suite(tier: int | None = None, include_clean_shutdown: bool = True) -> unittest.TestSuite:
    suite = unittest.TestSuite()
    loader = unittest.TestLoader()

    tier_modules = {
        1: 'test_tier1_cloud_io',
        2: 'test_tier2_pre_model_probe',
        3: 'test_tier3_cloud_durability_and_local_sync',
        4: 'test_tier4_output_structure_and_reader',
    }

    if tier is not None:
        if tier in tier_modules:
            mod = __import__(tier_modules[tier])
            suite.addTests(loader.loadTestsFromModule(mod))
        else:
            raise ValueError(f"Unknown tier: {tier}")
    else:
        for mod_name in tier_modules.values():
            mod = __import__(mod_name)
            suite.addTests(loader.loadTestsFromModule(mod))

    if include_clean_shutdown and tier is None:
        mod = __import__('test_clean_shutdown')
        suite.addTests(loader.loadTestsFromModule(mod))

    return suite


def main():
    parser = argparse.ArgumentParser(description="XL-DocBench E2E Verification Runner")
    parser.add_argument('--all', action='store_true', help="Run all verification tiers and clean shutdown")
    parser.add_argument('--tier', type=int, choices=[1, 2, 3, 4], help="Run specific tier (1-4)")
    parser.add_argument('--clean-shutdown', action='store_true', help="Run clean shutdown tests only")
    parser.add_argument('--empirical', action='store_true', help="Run empirical Google Drive physical directory verification")
    parser.add_argument('--json-report', type=str, help="Save verification report to JSON file")
    args = parser.parse_args()

    started_at = time.time()
    if args.clean_shutdown:
        suite = unittest.TestSuite()
        mod = __import__('test_clean_shutdown')
        suite.addTests(unittest.TestLoader().loadTestsFromModule(mod))
    elif args.empirical:
        suite = unittest.TestSuite()
        mod = __import__('verify_m2_empirical_artifacts')
        suite.addTests(unittest.TestLoader().loadTestsFromTestCase(mod.TestM2EmpiricalArtifacts))
    elif args.tier:
        suite = build_suite(tier=args.tier, include_clean_shutdown=False)
    else:
        # Default or --all
        suite = build_suite(tier=None, include_clean_shutdown=True)

    runner = unittest.TextTestRunner(verbosity=2)
    result = runner.run(suite)
    elapsed = time.time() - started_at

    report = {
        'schema': 'gptgrep.xl-docbench.e2e-verification-receipt.v1',
        'tests_run': result.testsRun,
        'failures': len(result.failures),
        'errors': len(result.errors),
        'skipped': len(result.skipped),
        'elapsed_seconds': round(elapsed, 4),
        'passed': result.wasSuccessful()
    }

    print("\n--- E2E VERIFICATION RECEIPT ---")
    print(json.dumps(report, indent=2))

    if args.json_report:
        Path(args.json_report).write_text(json.dumps(report, indent=2))

    sys.exit(0 if result.wasSuccessful() else 1)


if __name__ == '__main__':
    main()
