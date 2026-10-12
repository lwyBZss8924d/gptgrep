"""Synthetic invocation-bounded Decisions mode: no shared ledger, cost preflight, receipts."""
import copy
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
WORST = 406.3232  # 62 trials x 200 submissions x 65,536 bytes x RATE / 1,000,000


class InvocationBoundedMaterializationTests(unittest.TestCase):
    def setUp(self):
        fixtures.HarborMaterializationTests.setUp(self)
        price = write_pin(self.root / "synthetic-price-evidence.json", {"synthetic": "price evidence"})
        self.runtime.update(
            reader_mode="legacy_text", decisions_mode="invocation_bounded", decisions_budget=None,
            decisions_ledger_dir=None, decisions_preparation=None, decisions_cost_cap_usd=WORST,
            decisions_prompt_price={"model": self.runtime["jev_model"], "usd_per_million_prompt_tokens": RATE,
                                    "provenance": price})

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

    def test_treatment_mounts_no_ledger_and_passes_no_budget_file(self):
        dataset = self.root / "dataset"
        result = self.materializer().run(dataset)
        preflight = result["preflight"]["decisions"]
        self.assertEqual((preflight["mode"], preflight["treatment_trials"], preflight["max_submissions_per_trial"],
                          preflight["max_request_bytes"]), ("invocation_bounded", 62, 200, 65536))
        self.assertAlmostEqual(preflight["worst_case_usd_per_submission"], 65536 * RATE / 1e6)
        self.assertAlmostEqual(preflight["worst_case_usd_per_trial"], 200 * 65536 * RATE / 1e6)
        self.assertEqual((preflight["worst_case_usd"], preflight["cost_cap_usd"]), (WORST, WORST))
        self.assertEqual(preflight["price_provenance"], self.runtime["decisions_prompt_price"]["provenance"])
        treatments = 0
        for record in result["tasks"]:
            task = dataset / record["task_name"]
            config = json.loads((task / "environment/reader.json").read_text())
            mounts = json.loads((task / "environment/docker-compose.yaml").read_text())["services"]["main"]["volumes"]
            self.assertEqual([mount["target"] for mount in mounts], [CODEX_HOME + "/auth.json"])
            self.assertEqual(config["decisions_mode"], "invocation_bounded")
            for key in ("decisions_budget", "decisions_preparation", "decisions_cost_cap_usd", "decisions_prompt_price"):
                self.assertNotIn(key, config)
            if record["binding"]["arm"] == "gptgrep":
                treatments += 1
                argv = reader_argv(config, record["binding"], "synthetic request")
                self.assertEqual(argv[1:3], ["ask", "--native-ptc"])
                for flag in ("--decisions-budget", "--multimodal", "--max-concurrent-batches"):
                    self.assertNotIn(flag, argv)
        self.assertEqual(treatments, 62)

    def test_preflight_requires_trials_times_200_times_worst_case_within_cap(self):
        self.assertEqual(self.materializer().preflight["decisions"]["worst_case_usd"], WORST)
        self.rejected("decisions_cost_cap_preflight", decisions_cost_cap_usd=406.3231)
        price = {**self.runtime["decisions_prompt_price"], "usd_per_million_prompt_tokens": 0.5000001}
        self.rejected("decisions_cost_cap_preflight", decisions_prompt_price=price)
        self.assertFalse((self.root / "dataset").exists())

    def test_cost_cap_and_pinned_price_are_required(self):
        for cap in (None, 0, -1.0, True, "406.3232"):
            with self.subTest(cap=cap):
                self.rejected("decisions_cost_cap_usd", decisions_cost_cap_usd=cap)
        price = self.runtime["decisions_prompt_price"]
        for changed in (None, {**price, "model": "openai/another-decisions-model"},
                        {**price, "usd_per_million_prompt_tokens": 0},
                        {**price, "usd_per_million_prompt_tokens": None},
                        {key: value for key, value in price.items() if key != "provenance"},
                        {**price, "unreviewed": True}):
            with self.subTest(price=changed):
                self.rejected("decisions_prompt_price", decisions_prompt_price=changed)
        Path(price["provenance"]["path"]).write_bytes(b"changed evidence")
        self.rejected("pin_mismatch")

    def test_mode_is_exclusive_explicit_and_legacy_text_only(self):
        for key, value in (("decisions_ledger_dir", "/synthetic/unselected-ledger"),
                           ("decisions_budget", {"path": "/synthetic/unselected.json", "bytes": 1, "sha256": "d" * 64}),
                           ("decisions_preparation", {"schema": "unselected"})):
            with self.subTest(field=key):
                self.rejected("decisions_invocation_bounded_mode_conflict", **{key: value})
        self.rejected("decisions_invocation_bounded_mode_conflict", reader_mode="rich_multimodal")
        # Cost fields never select the new mode implicitly.
        self.rejected("decisions_existing_context_mode_conflict", decisions_mode=None,
                      decisions_budget={"path": "/synthetic/budget.json", "bytes": 1, "sha256": "d" * 64},
                      decisions_ledger_dir="/synthetic/ledger")
        self.rejected("decisions_mode", decisions_mode="shared_ledger")


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
                           "decisions_preparation": None, "decisions_cost_cap_usd": None}
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
        with self.assertRaisesRegex(ValueError, "decisions_existing_context_mode_conflict"):
            decisions.selected_mode({"decisions_cost_cap_usd": 1.0, "decisions_prompt_price": {"model": "x"}})
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
                                               "provenance": {"path": "/synthetic/price.json", "bytes": 1,
                                                              "sha256": "f" * 64}})
        treatment = [record for record, row in zip(records, export["tasks"]) if row["binding"]["arm"] == "gptgrep"]
        for record, receipt in zip(treatment, receipts):
            operation = parse(blobs[record["operation"]])
            if receipt is not None:
                operation["attempts"][0].update(decisions_cost=receipt,
                                                reported_retrieval_cost_subtotal=receipt["reported_cost_usd"] or 0.004,
                                                reported_retrieval_cost_receipts=1)
            blobs[record["operation"]] = canonical(operation)
        with patch("gptgrep_harbor.aggregate.pinned", side_effect=lambda pin, *args: blobs[pin]):
            return summarize(export, records, runtime)["decisions"]

    def test_cohort_total_is_known_only_when_every_treatment_trial_is_known(self):
        known = {"cost_complete": True, "reported_cost_usd": 0.01}
        summary = self.results([known] * 62)
        self.assertEqual((summary["mode"], summary["cost_complete_trials"], summary["cost_unknown_trials"]),
                         ("invocation_bounded", 62, 0))
        self.assertAlmostEqual(summary["reported_cost_usd_total"], 0.62)
        self.assertIs(summary["total_within_cap"], True)
        self.assertEqual(summary["reported_cost_receipts"], 62)
        unknown = {"cost_complete": False, "reported_cost_usd": None}
        for receipts in ([unknown] + [known] * 61, [None] + [known] * 61):
            with self.subTest(first=receipts[0]):
                summary = self.results(receipts)
                self.assertEqual((summary["cost_complete_trials"], summary["cost_unknown_trials"]), (61, 1))
                self.assertIsNone(summary["reported_cost_usd_total"])
                self.assertIsNone(summary["total_within_cap"])
                self.assertGreater(summary["reported_cost_usd_known_subtotal"], 0.6)
        self.assertFalse(summary["currency_verified"])


if __name__ == "__main__":
    unittest.main()
