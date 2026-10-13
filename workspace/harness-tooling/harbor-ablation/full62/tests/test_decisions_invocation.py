"""Synthetic invocation-bounded Decisions mode: no shared ledger, per-trial claims, cost preflight, receipts."""
import copy
from fractions import Fraction
import json
from pathlib import Path
import unittest
from unittest.mock import patch

from gptgrep_harbor import decisions
from gptgrep_harbor.aggregate import summarize
from gptgrep_harbor.common import canonical, parse
from gptgrep_harbor.reader import decisions_cost, reader_argv
from gptgrep_harbor.runtime import CODEX_HOME, DECISIONS_BUDGET
import test_contracts
import test_harbor_runtime as fixtures

write_pin = fixtures.write_pin
RATE = 0.5  # synthetic USD per million prompt tokens
BOUND = 406.3232  # 62 trials x 200 submissions x 65,536 bytes x RATE / 1,000,000


class InvocationBoundedMaterializationTests(unittest.TestCase):
    def setUp(self):
        fixtures.HarborMaterializationTests.setUp(self)
        price = write_pin(self.root / "synthetic-price-evidence.json", {"synthetic": "price evidence"})
        self.claims = self.root / "claims"
        self.claims.mkdir()
        self.runtime.update(
            reader_mode="legacy_text", decisions_mode="invocation_bounded", decisions_budget=None,
            decisions_ledger_dir=None, decisions_preparation=None, decisions_cost_cap_usd=BOUND,
            decisions_prompt_price={"model": self.runtime["jev_model"], "usd_per_million_prompt_tokens": RATE,
                                    "usd_per_million_completion_tokens": 0, "usd_per_request": 0,
                                    "provenance": price},
            decisions_claims_dir=str(self.claims))

    def materializer(self):
        return fixtures.HarborMaterializationTests.materializer(self)

    def rejected(self, code, **changes):
        original = copy.deepcopy(self.runtime)
        self.runtime.update(changes)
        try:
            with self.assertRaisesRegex(ValueError, code):
                self.materializer()
        finally:
            self.runtime = original

    def test_treatment_mounts_only_its_own_claim_and_passes_no_budget_file(self):
        dataset = self.root / "dataset"
        result = self.materializer().run(dataset)
        preflight = result["preflight"]["decisions"]
        self.assertEqual((preflight["mode"], preflight["treatment_trials"], preflight["asks_per_trial"],
                          preflight["max_submissions_per_trial"], preflight["max_request_bytes"]),
                         ("invocation_bounded", 62, 1, 200, 65536))
        self.assertAlmostEqual(preflight["submission_bound_usd"], 65536 * RATE / 1e6)
        self.assertAlmostEqual(preflight["trial_bound_usd"], 200 * 65536 * RATE / 1e6)
        self.assertEqual((preflight["cohort_bound_usd"], preflight["cost_cap_usd"]), (BOUND, BOUND))
        self.assertEqual(preflight["price"], self.runtime["decisions_prompt_price"])
        # The preflight cannot be read as an enforced spending cap.
        self.assertIs(preflight["cap_runtime_enforced"], False)
        self.assertEqual(preflight["limits_enforced_by"]["asks_per_trial"], "adapter_create_new_claim")
        self.assertIn("provider_prompt_tokens_beyond_one_per_request_byte", preflight["excluded"])
        treatments, claims = 0, set()
        for record in result["tasks"]:
            task = dataset / record["task_name"]
            config = json.loads((task / "environment/reader.json").read_text())
            mounts = json.loads((task / "environment/docker-compose.yaml").read_text())["services"]["main"]["volumes"]
            self.assertEqual(config["decisions_mode"], "invocation_bounded")
            for key in ("decisions_budget", "decisions_preparation", "decisions_cost_cap_usd",
                        "decisions_prompt_price", "decisions_claims_dir"):
                self.assertNotIn(key, config)
            if record["binding"]["arm"] != "gptgrep":
                self.assertEqual([mount["target"] for mount in mounts], [CODEX_HOME + "/auth.json"])
                continue
            treatments += 1
            self.assertEqual([mount["target"] for mount in mounts], [CODEX_HOME + "/auth.json", decisions.CLAIM_ROOT])
            claim = mounts[1]
            self.assertFalse(claim["read_only"])
            self.assertFalse(claim["bind"]["create_host_path"])
            self.assertEqual(Path(claim["source"]).parent, self.claims)
            self.assertEqual(list(Path(claim["source"]).iterdir()), [])
            claims.add(claim["source"])
            argv = reader_argv(config, record["binding"], "synthetic request")
            self.assertEqual(argv[1:3], ["ask", "--native-ptc"])
            for flag in ("--decisions-budget", "--multimodal", "--max-concurrent-batches"):
                self.assertNotIn(flag, argv)
        self.assertEqual((treatments, len(claims)), (62, 62))
        self.assertEqual(json.loads((self.claims / "cohort.json").read_text())["treatment_trials"], 62)
        # A consumed claims root cannot be materialized again, even into a fresh output.
        with self.assertRaisesRegex(ValueError, "decisions_claims_not_fresh"):
            self.materializer().run(self.root / "repeat-dataset")
        self.assertFalse((self.root / "repeat-dataset").exists())

    def test_claims_directory_is_physical_and_outside_the_jobs_and_task_output(self):
        self.rejected("decisions_directory", decisions_claims_dir=None)
        self.rejected("decisions_physical_directory", decisions_claims_dir=str(self.root / "missing-claims"))
        # Harbor deletes unfinished trial directories on resume: never under jobs_dir.
        inside = Path(self.runtime["jobs_dir"]) / "claims"
        inside.mkdir(parents=True)
        self.rejected("decisions_claims_inside_jobs_dir", decisions_claims_dir=str(inside))
        self.rejected("decisions_claims_inside_jobs_dir", jobs_dir=str(self.claims / "jobs"))
        with self.assertRaisesRegex(ValueError, "decisions_inside_task_output"):
            self.materializer().run(self.claims / "dataset")
        self.assertEqual(list(self.claims.iterdir()), [])

    def test_preflight_requires_trials_times_200_times_the_submission_bound_within_cap(self):
        self.assertEqual(self.materializer().preflight["decisions"]["cohort_bound_usd"], BOUND)
        self.rejected("decisions_cost_cap_preflight", decisions_cost_cap_usd=406.3231)
        price = self.runtime["decisions_prompt_price"]
        self.rejected("decisions_cost_cap_preflight",
                      decisions_prompt_price={**price, "usd_per_million_prompt_tokens": 0.5000001})
        # Each submission's bound also carries the pinned per-request fee.
        fee = {**price, "usd_per_request": 0.001}
        self.rejected("decisions_cost_cap_preflight", decisions_prompt_price=fee)
        self.runtime.update(decisions_prompt_price=fee, decisions_cost_cap_usd=419)
        self.assertAlmostEqual(self.materializer().preflight["decisions"]["cohort_bound_usd"], BOUND + 12.4)
        self.assertFalse((self.root / "dataset").exists())

    def test_completion_price_must_be_zero_because_output_tokens_are_unbounded(self):
        price = self.runtime["decisions_prompt_price"]
        for rate in (0.0000001, 1):
            with self.subTest(rate=rate):
                self.rejected("decisions_completion_price_unbounded",
                              decisions_prompt_price={**price, "usd_per_million_completion_tokens": rate})

    def test_cost_cap_and_pinned_price_are_required(self):
        for cap in (None, 0, -1.0, True, "406.3232", 10**400):
            with self.subTest(cap=cap):
                self.rejected("decisions_cost_cap_usd", decisions_cost_cap_usd=cap)
        price = self.runtime["decisions_prompt_price"]
        for changed in (None, {**price, "model": "openai/another-decisions-model"}, {**price, "model": None},
                        {**price, "usd_per_million_prompt_tokens": 0},
                        {**price, "usd_per_million_prompt_tokens": None},
                        {**price, "usd_per_million_prompt_tokens": 10**400},
                        {**price, "usd_per_request": -0.001}, {**price, "usd_per_request": None},
                        {key: value for key, value in price.items() if key != "usd_per_request"},
                        {key: value for key, value in price.items() if key != "usd_per_million_completion_tokens"},
                        {key: value for key, value in price.items() if key != "provenance"},
                        {**price, "unreviewed": True}):
            with self.subTest(price=changed):
                self.rejected("decisions_prompt_price", decisions_prompt_price=changed)
        Path(price["provenance"]["path"]).write_bytes(b"changed evidence")
        self.rejected("pin_mismatch")

    def test_legacy_decisions_model_must_be_one_bounded_argv_value(self):
        price = self.runtime["decisions_prompt_price"]
        for model in (None, "", " openai/padded", "-flag", "a" * 257, "bad\x00model", 7):
            with self.subTest(model=model):
                self.rejected("decisions_model", jev_model=model, decisions_prompt_price={**price, "model": model})
        # The same check applies to every legacy_text Decisions mode.
        self.rejected("decisions_model", jev_model=None, decisions_mode=None, decisions_prompt_price=None,
                      decisions_cost_cap_usd=None, decisions_claims_dir=None,
                      decisions_budget={"path": "/synthetic/budget.json", "bytes": 1, "sha256": "d" * 64},
                      decisions_ledger_dir="/synthetic/ledger")
        for model in ("~typesafe/jev-latest", "openai/gpt-6-luna-decisions-20261006"):
            with self.subTest(model=model):
                self.runtime.update(jev_model=model, decisions_prompt_price={**price, "model": model})
                self.assertEqual(self.materializer().preflight["decisions"]["price"]["model"], model)

    def test_mode_is_exclusive_explicit_and_legacy_text_only(self):
        for key, value in (("decisions_ledger_dir", "/synthetic/unselected-ledger"),
                           ("decisions_budget", {"path": "/synthetic/unselected.json", "bytes": 1, "sha256": "d" * 64}),
                           ("decisions_preparation", {"schema": "unselected"})):
            with self.subTest(field=key):
                self.rejected("decisions_invocation_bounded_mode_conflict", **{key: value})
        self.rejected("decisions_invocation_bounded_mode_conflict", reader_mode="rich_multimodal",
                      jev_model="openai/gpt-6-luna-decisions")
        # Cost and claim fields never select the new mode implicitly.
        self.rejected("decisions_existing_context_mode_conflict", decisions_mode=None,
                      decisions_budget={"path": "/synthetic/budget.json", "bytes": 1, "sha256": "d" * 64},
                      decisions_ledger_dir="/synthetic/ledger")
        self.rejected("decisions_mode", decisions_mode="shared_ledger")


class ExactDecimalTests(unittest.TestCase):
    def test_prices_and_caps_are_exact_bounded_decimals_with_coded_errors(self):
        self.assertEqual(decisions.decimal(0.1, "x"), Fraction(1, 10))
        self.assertEqual(decisions.decimal(7, "x"), 7)
        self.assertEqual(decisions.decimal(0, "x", zero=True), 0)
        for value in (10**400, decisions.MAX_USD + 1, float("inf"), float("nan"), 0, -1, True, None, "1"):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "^x$"):
                decisions.decimal(value, "x")
        with self.assertRaisesRegex(ValueError, "^x$"):
            decisions.decimal(-0.5, "x", zero=True)


class ModeSelectionTests(unittest.TestCase):
    config = {"gptgrep_binary": "/opt/gptgrep", "codex_binary": "/opt/codex", "codex_home": CODEX_HOME,
              "jev_model": "synthetic", "deadline_seconds": 20, "max_tool_calls": 10000, "max_input_bytes": 8192,
              "reader_mode": "legacy_text", "native_ptc": True, "codex_permissions": "full62_reader",
              "source_relative_path": "generic-source.txt"}
    budget = {"path": DECISIONS_BUDGET, "bytes": 1, "sha256": "e" * 64}

    def argv(self, **changes):
        return reader_argv({**self.config, **changes}, {"arm": "gptgrep"}, "generic question")

    def test_original_modes_keep_their_selection_and_flags(self):
        for selector in ({}, {"decisions_mode": "existing_context"}):
            with self.subTest(selector=selector):
                runtime = {"decisions_budget": self.budget, "decisions_ledger_dir": "/x", **selector,
                           "decisions_preparation": None, "decisions_cost_cap_usd": None,
                           "decisions_claims_dir": None}
                self.assertEqual(decisions.selected_mode(runtime), "existing_context")
                self.assertEqual(self.argv(decisions_budget=self.budget, **selector)[1:4],
                                 ["--decisions-budget", DECISIONS_BUDGET, "ask"])
        self.assertEqual(decisions.selected_mode({"decisions_preparation": {"schema": "x"},
                                                  "decisions_budget": None}), "preparation")
        with self.assertRaisesRegex(ValueError, "decisions_mode"):
            self.argv(decisions_mode="preparation")

    def test_invocation_bounded_is_explicit_and_drops_the_budget_flag(self):
        argv = self.argv(decisions_mode="invocation_bounded")
        self.assertEqual(argv[:3], ["/opt/gptgrep", "ask", "--native-ptc"])
        self.assertNotIn("--decisions-budget", argv)
        self.assertEqual(argv[argv.index("--max-tool-calls") + 1], "10000")
        with self.assertRaisesRegex(ValueError, "decisions_invocation_bounded_mode"):
            self.argv(decisions_mode="invocation_bounded", reader_mode="rich_multimodal")
        for fields in ({"decisions_cost_cap_usd": 1.0, "decisions_prompt_price": {"model": "x"}},
                       {"decisions_claims_dir": "/synthetic/claims"}):
            with self.subTest(fields=fields), \
                    self.assertRaisesRegex(ValueError, "decisions_existing_context_mode_conflict"):
                decisions.selected_mode(fields)
        with self.assertRaisesRegex(ValueError, "decisions_invocation_bounded_mode_conflict"):
            self.argv(decisions_mode="invocation_bounded", decisions_budget=self.budget)


class CostReceiptTests(unittest.TestCase):
    complete = {"requests": 3, "attempted_calls": 3, "unobserved_attempts": 0, "accounting_complete": True,
                "usage": [{"cost": 0.25}, {"cost": 0.125}, {"cost": 0.0}]}

    def test_complete_receipt_sums_reported_costs(self):
        receipt = decisions_cost(self.complete)
        self.assertEqual((receipt["cost_complete"], receipt["reported_cost_usd"]), (True, 0.375))
        self.assertEqual((receipt["usage_rows"], receipt["priced_rows"]), (3, 3))
        self.assertFalse(receipt["currency_verified"])

    def test_any_unobserved_or_unpriced_response_keeps_the_trial_unknown(self):
        cases = {"unobserved_attempt": {**self.complete, "attempted_calls": 4, "unobserved_attempts": 1},
                 "null_usage": {**self.complete, "usage": [{"cost": 0.25}, None, {"cost": 0.0}]},
                 "missing_cost": {**self.complete, "usage": [{"cost": 0.25}, {"prompt_tokens": 9}, {"cost": 0.0}]},
                 "accounting_incomplete": {**self.complete, "accounting_complete": False},
                 "count_mismatch": {**self.complete, "requests": 2, "attempted_calls": 2},
                 "boolean_count": {**self.complete, "unobserved_attempts": False},
                 "missing": None}
        for name, jev in cases.items():
            with self.subTest(case=name):
                receipt = decisions_cost(jev)
                self.assertFalse(receipt["cost_complete"])
                self.assertIsNone(receipt["reported_cost_usd"])
        self.assertFalse(decisions_cost(None)["observed"])


class CostAggregationTests(unittest.TestCase):
    def results(self, receipts):
        export, records, runtime, blobs = test_contracts.synthetic_results()
        runtime.update(decisions_mode="invocation_bounded", decisions_cost_cap_usd=1.0,
                       decisions_prompt_price={"model": "synthetic", "usd_per_million_prompt_tokens": RATE,
                                               "usd_per_million_completion_tokens": 0, "usd_per_request": 0,
                                               "provenance": {"path": "/synthetic/price.json", "bytes": 1,
                                                              "sha256": "f" * 64}},
                       decisions_claims_dir="/synthetic/claims")
        treatment = [record for record, row in zip(records, export["tasks"]) if row["binding"]["arm"] == "gptgrep"]
        for record, receipt in zip(treatment, receipts):
            operation = parse(blobs[record["operation"]])
            if receipt is not None:
                operation["attempts"][0].update(decisions_cost=receipt,
                                                reported_retrieval_cost_subtotal=receipt["reported_cost_usd"] or 0.004,
                                                reported_retrieval_cost_receipts=receipt["priced_rows"])
            blobs[record["operation"]] = canonical(operation)
        with patch("gptgrep_harbor.aggregate.pinned", side_effect=lambda pin, *args: blobs[pin]):
            return summarize(export, records, runtime)["decisions"]

    def test_cohort_total_is_known_only_when_every_treatment_trial_is_known(self):
        # One Ask reports several priced Jev usage rows; they are not receipts.
        known = {"cost_complete": True, "reported_cost_usd": 0.01, "priced_rows": 3}
        summary = self.results([known] * 62)
        self.assertEqual((summary["mode"], summary["cost_complete_trials"], summary["cost_unknown_trials"]),
                         ("invocation_bounded", 62, 0))
        self.assertAlmostEqual(summary["reported_cost_usd_total"], 0.62)
        self.assertIs(summary["total_within_cap"], True)
        self.assertEqual((summary["trials_with_cost_receipt"], summary["priced_usage_rows"]), (62, 186))
        self.assertNotIn("reported_cost_receipts", summary)
        unknown = {"cost_complete": False, "reported_cost_usd": None, "priced_rows": 2}
        for receipts, with_receipt, rows in (([unknown] + [known] * 61, 62, 185), ([None] + [known] * 61, 61, 183)):
            with self.subTest(first=receipts[0]):
                summary = self.results(receipts)
                self.assertEqual((summary["cost_complete_trials"], summary["cost_unknown_trials"]), (61, 1))
                self.assertEqual((summary["trials_with_cost_receipt"], summary["priced_usage_rows"]),
                                 (with_receipt, rows))
                self.assertIsNone(summary["reported_cost_usd_total"])
                self.assertIsNone(summary["total_within_cap"])
                self.assertGreater(summary["reported_cost_usd_known_subtotal"], 0.6)
        self.assertFalse(summary["currency_verified"])


if __name__ == "__main__":
    unittest.main()
