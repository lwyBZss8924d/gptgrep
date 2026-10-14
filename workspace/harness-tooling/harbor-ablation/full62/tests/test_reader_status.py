"""Current documentation consistency only; no runtime-qualification claim."""
import json
from pathlib import Path
import re
import unittest


class ReaderStatusTests(unittest.TestCase):
    def test_current_report_keeps_unverified_receipts_and_pending_probes_explicit(self):
        text = (Path(__file__).resolve().parents[1] / "README.md").read_text(encoding="utf-8")
        matches = re.findall(r"<!-- BEGIN full62-current-status -->\s*```json\s*(.*?)\s*```\s*"
                             r"<!-- END full62-current-status -->", text, re.DOTALL)
        self.assertEqual(len(matches), 1)
        status = json.loads(matches[0])
        self.assertEqual(status["as_of_utc"], "2026-10-13")
        self.assertEqual(status["reported_source_revision"], "964bf3777658222469f6be18e89477e997ee44dd")
        self.assertEqual({key: status[key] for key in (
            "adapter_integration", "probes_1_3", "probe_1_original_proc", "probe_4", "probe_5",
            "supervised_host_workload", "harbor_nested_chain", "receipt_provenance")}, {
                "adapter_integration": "reported_merged", "probes_1_3": "reported_accepted",
                "probe_1_original_proc": "failed_retained", "probe_4": "pending", "probe_5": "pending",
                "supervised_host_workload": "reported", "harbor_nested_chain": "unverified",
                "receipt_provenance": "not_available_in_selected_inputs"})
        current = text.split("**Reported status, 2026-10-13 UTC.**", 1)[1].split("<!-- BEGIN", 1)[0]
        self.assertIn("No exact acceptance receipt was supplied", current)
        self.assertIn("Probe 4 (actual Judge/verifier default profile) and probe 5 (paired native-tool smoke) remain pending", current)
        self.assertIn("preserves the original `--proc` failure", current)
        self.assertIn("do not establish the Harbor trial-container qualification", current)


if __name__ == "__main__":
    unittest.main()
