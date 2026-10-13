"""Synthetic invocation-bounded Decisions mode: no shared ledger, per-trial claims, cost preflight, receipts."""
import copy
from fractions import Fraction
import json
from pathlib import Path
import unittest
from unittest.mock import patch

from gptgrep_harbor import adapter as adapter_module, decisions
from gptgrep_harbor.aggregate import summarize
from gptgrep_harbor.common import canonical, parse, sha
from gptgrep_harbor.reader import decisions_cost, reader_argv
from gptgrep_harbor.runtime import CODEX_HOME, DECISIONS_BUDGET
import test_contracts
import test_harbor_runtime as fixtures

write_pin = fixtures.write_pin
RATE = 0.5  # synthetic USD per million prompt tokens
LIMIT = 8  # synthetic per-trial submission limit
BOUND = 16.252928  # 62 trials x LIMIT submissions x 65,536 bytes x RATE / 1,000,000


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
            decisions_claims_dir=str(self.claims), decisions_max_submissions_per_trial=LIMIT)

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

    def test_each_trial_mounts_only_its_own_claim_and_the_treatment_passes_its_trial_ledger(self):
        dataset = self.root / "dataset"
        result = self.materializer().run(dataset)
        preflight = result["preflight"]["decisions"]
        self.assertEqual((preflight["mode"], preflight["treatment_trials"], preflight["asks_per_trial"],
                          preflight["max_submissions_per_trial"], preflight["max_request_bytes"]),
                         ("invocation_bounded", 62, 1, LIMIT, 65536))
        self.assertAlmostEqual(preflight["submission_bound_usd"], 65536 * RATE / 1e6)
        self.assertAlmostEqual(preflight["trial_bound_usd"], LIMIT * 65536 * RATE / 1e6)
        self.assertEqual(preflight["bound_formula"], decisions.BOUND_FORMULA)
        self.assertEqual((preflight["cohort_bound_usd"], preflight["cost_cap_usd"]), (BOUND, BOUND))
        self.assertEqual(preflight["price"], self.runtime["decisions_prompt_price"])
        # The preflight cannot be read as an enforced spending cap.
        self.assertIs(preflight["cap_runtime_enforced"], False)
        self.assertIs(preflight["submission_limit_runtime_enforced"], True)
        self.assertEqual(preflight["limits_enforced_by"], {"asks_per_trial": "adapter_create_new_claim",
                                                           "submissions_per_trial": "product_trial_decisions_budget",
                                                           "request_bytes": "product"})
        self.assertIn("provider_prompt_tokens_beyond_one_per_request_byte", preflight["excluded"])
        # The mirrored product limits are labelled and checked in every treatment trial.
        self.assertEqual(preflight["product_limits"],
                         {"max_submissions": 200, "max_request_bytes": 65536, "source": "mirrored_from_gptgrep_jev",
                          "checked_against": "pinned_product_schema_in_every_treatment_trial_before_its_claim"})
        self.assertEqual(preflight["price_plausibility"],
                         {"usd_per_million_prompt_tokens": [0.001, 1000], "usd_per_request_at_most": 1})
        treatments, claims = 0, set()
        for record in result["tasks"]:
            task = dataset / record["task_name"]
            config = json.loads((task / "environment/reader.json").read_text())
            mounts = json.loads((task / "environment/docker-compose.yaml").read_text())["services"]["main"]["volumes"]
            self.assertEqual(config["decisions_mode"], "invocation_bounded")
            for key in ("decisions_budget", "decisions_preparation", "decisions_cost_cap_usd",
                        "decisions_prompt_price", "decisions_claims_dir"):
                self.assertNotIn(key, config)
            # Only the treatment reader needs the per-trial limit.
            self.assertEqual(config.get("decisions_max_submissions_per_trial"),
                             LIMIT if record["binding"]["arm"] == "gptgrep" else None)
            # Both arms identically: only this trial's own claim directory, read/write.
            self.assertEqual([mount["target"] for mount in mounts], [CODEX_HOME + "/auth.json", decisions.CLAIM_ROOT])
            claim = mounts[1]
            self.assertFalse(claim["read_only"])
            self.assertFalse(claim["bind"]["create_host_path"])
            self.assertEqual(Path(claim["source"]).parent, self.claims)
            self.assertEqual(list(Path(claim["source"]).iterdir()), [])
            claims.add(claim["source"])
            if record["binding"]["arm"] != "gptgrep":
                continue
            treatments += 1
            argv = reader_argv(config, record["binding"], "synthetic request")
            # The Ask joins the trial's own ledger in its claim directory.
            self.assertEqual(argv[1:5], ["--decisions-budget", decisions.CLAIM_ROOT + "/decisions-budget.json",
                                         "ask", "--native-ptc"])
            for flag in ("--multimodal", "--max-concurrent-batches"):
                self.assertNotIn(flag, argv)
        self.assertEqual((treatments, len(claims)), (62, 124))
        cohort = json.loads((self.claims / "cohort.json").read_text())
        self.assertEqual((cohort["schema"], cohort["claimed_arms"], cohort["trials"], cohort["treatment_trials"]),
                         ("gptgrep.harbor.trial-claims.v1", ["gptgrep", "raw_only"], 124, 62))
        # A consumed claims root cannot be materialized again, even into a fresh output.
        with self.assertRaisesRegex(ValueError, "decisions_claims_not_fresh"):
            self.materializer().run(self.root / "repeat-dataset")
        self.assertFalse((self.root / "repeat-dataset").exists())

    def test_frozen_profile_declares_the_trial_limit_its_enforcer_and_the_exact_commands(self):
        adapter = self.materializer()
        profile = adapter.preflight["treatment_feature_profile"]
        control = decisions.CLAIM_ROOT + "/decisions-budget.json"
        self.assertEqual(profile["command"], ["gptgrep", "--decisions-budget", control, "ask", "--native-ptc"])
        self.assertEqual(profile["decisions"], {
            "mode": "invocation_bounded", "model": self.runtime["jev_model"], "routing_and_reranking": "mandatory",
            "max_submissions_per_trial": LIMIT, "enforced_by": "product_decisions_budget_ledger",
            "trial_ledger_init": {
                "command": ["gptgrep", "decisions-budget", "init", "--plan-file", decisions.CLAIM_ROOT + "/claim.json",
                            "--max-submissions", str(LIMIT), "--ledger", decisions.CLAIM_ROOT + "/decisions-ledger.json",
                            "--output", control],
                "per_trial_options": ["--run-id", "--deadline-seconds"],
                "runs": "after_the_trial_claim_before_the_ask"}})
        # The declared command is the exact prefix a materialized treatment reader launches.
        row = adapter.rows[0]
        document = next(d for d in self.runtime["documents"] if d["document_id"] == row["document_id"])
        binding = {"pair_id": row["pair_id"], "document_id": row["document_id"], "arm": "gptgrep"}
        task = self.root / "single-treatment-task"
        adapter._task(task, Path(adapter_module.__file__).parent, row, document, binding)
        config = json.loads((task / "environment/reader.json").read_text())
        argv = reader_argv(config, binding, "synthetic request")
        self.assertEqual(argv[1:len(profile["command"])], profile["command"][1:])
        # A run at another limit has another frozen profile.
        self.runtime.update(decisions_max_submissions_per_trial=200, decisions_cost_cap_usd=406.3232)
        other = self.materializer().preflight["treatment_feature_profile"]
        self.assertNotEqual(other, profile)
        self.assertEqual(other["decisions"]["max_submissions_per_trial"], 200)
        self.assertEqual(other["decisions"]["trial_ledger_init"]["command"][5:7], ["--max-submissions", "200"])

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

    def test_claims_root_stays_fresh_when_the_output_root_cannot_be_created(self):
        adapter = self.materializer()
        with self.assertRaises(FileNotFoundError):
            adapter.run(self.root / "missing-parent" / "dataset")
        self.assertEqual(list(self.claims.iterdir()), [])
        # The untouched claims root still materializes the cohort.
        self.assertEqual(len(adapter.run(self.root / "dataset")["tasks"]), 124)
        self.assertTrue((self.claims / "cohort.json").is_file())

    def test_implausible_price_units_are_rejected(self):
        price = self.runtime["decisions_prompt_price"]
        # A per-token USD 0.10 per million price, other unit slips and an implausible fee.
        for changes in ({"usd_per_million_prompt_tokens": 0.0000001}, {"usd_per_million_prompt_tokens": 0.0009},
                        {"usd_per_million_prompt_tokens": 1000.5}, {"usd_per_request": 1.5}):
            with self.subTest(changes=changes):
                self.rejected("decisions_price_unit_implausible", decisions_prompt_price={**price, **changes},
                              decisions_cost_cap_usd=10**8)
        for changes in ({"usd_per_million_prompt_tokens": 0.001}, {"usd_per_million_prompt_tokens": 1000},
                        {"usd_per_request": 1}):
            with self.subTest(changes=changes):
                self.runtime.update(decisions_prompt_price={**price, **changes}, decisions_cost_cap_usd=10**8)
                self.assertEqual(self.materializer().preflight["decisions"]["price"], {**price, **changes})

    def test_reader_seccomp_change_after_validation_refuses_before_any_claim_or_task(self):
        adapter = self.materializer()
        profile = Path(self.runtime["reader_seccomp_profile"]["path"])
        # Same length, so only the digest catches it: the default action would allow every call.
        profile.write_bytes(profile.read_bytes().replace(b'"SCMP_ACT_ERRNO"', b'"SCMP_ACT_ALLOW"', 1))
        dataset = self.root / "dataset"
        with self.assertRaisesRegex(ValueError, "pin_mismatch"):
            adapter.run(dataset)
        self.assertFalse(dataset.exists())
        self.assertEqual(list(self.claims.iterdir()), [])
        for suffix in (".export.json", ".job.json"):
            self.assertFalse((self.root / ("dataset" + suffix)).exists())

    def test_preflight_requires_trials_times_the_trial_limit_times_the_submission_bound_within_cap(self):
        self.assertEqual(self.materializer().preflight["decisions"]["cohort_bound_usd"], BOUND)
        self.rejected("decisions_cost_cap_preflight", decisions_cost_cap_usd=16.252927)
        price = self.runtime["decisions_prompt_price"]
        self.rejected("decisions_cost_cap_preflight",
                      decisions_prompt_price={**price, "usd_per_million_prompt_tokens": 0.5000001})
        # Each submission's bound also carries the pinned per-request fee.
        fee = {**price, "usd_per_request": 0.001}
        self.rejected("decisions_cost_cap_preflight", decisions_prompt_price=fee)
        self.runtime.update(decisions_prompt_price=fee, decisions_cost_cap_usd=16.75)
        self.assertAlmostEqual(self.materializer().preflight["decisions"]["cohort_bound_usd"], BOUND + 62 * LIMIT * 0.001)
        self.assertFalse((self.root / "dataset").exists())

    def test_per_trial_submission_limit_is_explicit_bounded_and_scales_the_preflight(self):
        for value in (None, 0, 201, True, 8.0, "8"):
            with self.subTest(limit=value):
                self.rejected("decisions_max_submissions_per_trial", decisions_max_submissions_per_trial=value)
        original = copy.deepcopy(self.runtime)
        del self.runtime["decisions_max_submissions_per_trial"]
        with self.assertRaisesRegex(ValueError, "decisions_max_submissions_per_trial"):
            self.materializer()
        self.runtime = original
        # The product maximum would need 25 times this cap.
        self.rejected("decisions_cost_cap_preflight", decisions_max_submissions_per_trial=200)
        self.runtime.update(decisions_max_submissions_per_trial=200, decisions_cost_cap_usd=406.3232)
        self.assertEqual(self.materializer().preflight["decisions"]["cohort_bound_usd"], 406.3232)
        self.runtime.update(decisions_max_submissions_per_trial=1, decisions_cost_cap_usd=BOUND)
        self.assertAlmostEqual(self.materializer().preflight["decisions"]["cohort_bound_usd"], BOUND / LIMIT)
        # The field belongs to this mode only.
        self.rejected("decisions_existing_context_mode_conflict", decisions_mode=None, decisions_cost_cap_usd=None,
                      decisions_prompt_price=None, decisions_claims_dir=None,
                      decisions_budget={"path": "/synthetic/budget.json", "bytes": 1, "sha256": "d" * 64},
                      decisions_ledger_dir="/synthetic/ledger")
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
                      decisions_cost_cap_usd=None, decisions_claims_dir=None, decisions_max_submissions_per_trial=None,
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

    def test_invocation_bounded_is_explicit_and_joins_the_trial_ledger(self):
        argv = self.argv(decisions_mode="invocation_bounded", decisions_max_submissions_per_trial=8)
        self.assertEqual(argv[:5], ["/opt/gptgrep", "--decisions-budget",
                                    decisions.CLAIM_ROOT + "/decisions-budget.json", "ask", "--native-ptc"])
        self.assertEqual(argv[argv.index("--max-tool-calls") + 1], "10000")
        for limit in (None, 0, 201):
            with self.subTest(limit=limit), self.assertRaisesRegex(ValueError, "decisions_max_submissions_per_trial"):
                self.argv(decisions_mode="invocation_bounded", decisions_max_submissions_per_trial=limit)
        with self.assertRaisesRegex(ValueError, "decisions_invocation_bounded_mode"):
            self.argv(decisions_mode="invocation_bounded", reader_mode="rich_multimodal",
                      decisions_max_submissions_per_trial=8)
        for fields in ({"decisions_cost_cap_usd": 1.0, "decisions_prompt_price": {"model": "x"}},
                       {"decisions_claims_dir": "/synthetic/claims"}, {"decisions_max_submissions_per_trial": 8}):
            with self.subTest(fields=fields), \
                    self.assertRaisesRegex(ValueError, "decisions_existing_context_mode_conflict"):
                decisions.selected_mode(fields)
        with self.assertRaisesRegex(ValueError, "decisions_invocation_bounded_mode_conflict"):
            self.argv(decisions_mode="invocation_bounded", decisions_budget=self.budget,
                      decisions_max_submissions_per_trial=8)


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
                       decisions_max_submissions_per_trial=LIMIT,
                       decisions_prompt_price={"model": "synthetic", "usd_per_million_prompt_tokens": RATE,
                                               "usd_per_million_completion_tokens": 0, "usd_per_request": 0,
                                               "provenance": {"path": "/synthetic/price.json", "bytes": 1,
                                                              "sha256": "f" * 64}},
                       decisions_claims_dir="/synthetic/claims")
        treatment = [record for record, row in zip(records, export["tasks"]) if row["binding"]["arm"] == "gptgrep"]
        for index, (record, receipt) in enumerate(zip(treatment, receipts)):
            operation = parse(blobs[record["operation"]])
            if receipt is not None:
                operation["attempts"][0].update(decisions_cost=receipt,
                                                reported_retrieval_cost_subtotal=receipt["reported_cost_usd"] or 0.004,
                                                reported_retrieval_cost_receipts=receipt["priced_rows"])
            budget = getattr(self, "budgets", None)
            if budget is not None:
                operation["decisions_trial_budget"] = {"max_submissions": LIMIT, **budget[index]}
                if budget[index]["limit_bound"]:
                    # The product refused a submission: the Ask failed and was judged a known zero.
                    answer = parse(blobs[record["answer"]])
                    answer.update(status="operational_failure", answer=None)
                    blobs[record["answer"]] = canonical(answer)
                    operation.update(status="operational_failure", answer_sha256=sha(canonical(answer)),
                                     failure="reader_failed", failure_code="decisions_submission_limit_reached")
                    judgment = parse(blobs[record["judgment"]])
                    judgment.update(status="operational_failure", label=0,
                                    answer_envelope_sha256=sha(canonical(answer)))
                    blobs[record["judgment"]] = canonical(judgment)
            blobs[record["operation"]] = canonical(operation)
        with patch("gptgrep_harbor.aggregate.pinned", side_effect=lambda pin, *args: blobs[pin]):
            self.summary = summarize(export, records, runtime)
        return self.summary["decisions"]

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

    def test_summary_profile_join_tells_one_trial_limit_from_another(self):
        export, records, runtime, blobs = test_contracts.synthetic_results()
        runtime.update(decisions_mode="invocation_bounded", decisions_max_submissions_per_trial=LIMIT)
        export["preflight"] = {"treatment_feature_profile": adapter_module.treatment_feature_profile(runtime)}
        with patch("gptgrep_harbor.aggregate.pinned", side_effect=lambda pin, *args: blobs[pin]):
            profile = summarize(export, records, runtime)["profile"]["treatment_feature_profile"]
            self.assertEqual(profile["decisions"]["max_submissions_per_trial"], LIMIT)
            runtime["decisions_max_submissions_per_trial"] = 200
            with self.assertRaisesRegex(ValueError, "treatment_profile_join"):
                summarize(export, records, runtime)

    def test_limit_bound_trials_are_judged_failures_visible_in_the_summary(self):
        known = {"cost_complete": True, "reported_cost_usd": 0.01, "priced_rows": 3}
        budgets = ([{"limit_reached": True, "limit_bound": True}] * 2 + [{"limit_reached": True, "limit_bound": False}]
                   + [{"limit_reached": False, "limit_bound": False}] * 58 + [{"limit_reached": None, "limit_bound": None}])
        self.budgets = budgets
        summary = self.results([known] * 62)
        self.assertEqual(summary["submission_limit"], {"max_submissions_per_trial": LIMIT,
                                                       "enforced_by": "product_decisions_budget_ledger",
                                                       "bound_trials": 2, "reached_trials": 3, "unobserved_trials": 1})
        treatment = self.summary["search"]["gptgrep"]
        self.assertEqual((treatment["operational_failures"], treatment["correct"]), (2, 60))
        self.assertEqual(treatment["operational_failure_codes"], {"decisions_submission_limit_reached": 2})
        self.assertEqual(self.summary["search"]["raw_only"]["operational_failure_codes"], {})


if __name__ == "__main__":
    unittest.main()
