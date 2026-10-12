"""A frozen complete order, independent of Harbor's directory sorting."""
import copy
import os
from pathlib import Path
import tempfile
import unittest

from gptgrep_harbor.common import canonical, sha
from gptgrep_harbor.trial_order import check_job_order, paired_order


def rows():
    return [{"task_name": f"ordinary-{i:03}-{arm}", "binding": {"pair_id": f"ordinary-{i}", "arm": arm}}
            for i in range(62) for arm in ("gptgrep", "raw_only")]


class TrialOrderTests(unittest.TestCase):
    def test_seeded_order_is_reproducible_complete_and_keeps_randomized_pairs_adjacent(self):
        original = rows()
        before = copy.deepcopy(original)
        order = paired_order(original, 12345)
        self.assertEqual(original, before)
        self.assertEqual(order, paired_order(original, 12345))
        self.assertNotEqual(order["task_names"], paired_order(original, 54321)["task_names"])
        names = order["task_names"]
        self.assertEqual(len(names), 124)
        self.assertEqual(set(names), {row["task_name"] for row in original})
        self.assertEqual(order["sha256"], sha(canonical(names)))
        by_name = {row["task_name"]: row["binding"] for row in original}
        first_arms = set()
        for index in range(0, 124, 2):
            first, second = by_name[names[index]], by_name[names[index + 1]]
            self.assertEqual(first["pair_id"], second["pair_id"])
            self.assertEqual({first["arm"], second["arm"]}, {"gptgrep", "raw_only"})
            first_arms.add(first["arm"])
        self.assertEqual(first_arms, {"gptgrep", "raw_only"})

    def test_partial_or_duplicate_cohort_cannot_get_an_order(self):
        for invalid in (rows()[:-1], rows()[:-1] + [rows()[0]]):
            with self.assertRaises(ValueError):
                paired_order(invalid, 12345)

    def test_changed_job_order_is_rejected_even_with_a_new_file_pin(self):
        with tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"]) as folder:
            root = Path(folder).resolve()
            dataset = root / "dataset"
            dataset.mkdir()
            tasks = rows()
            order = paired_order(tasks, 12345)
            job = {"datasets": [], "tasks": [{"path": str(dataset / name), "source": dataset.name}
                                               for name in order["task_names"]],
                   "n_attempts": 1, "n_concurrent_trials": 1, "retry": {"max_retries": 0}}
            path = root / "dataset.job.json"
            def exported():
                body = canonical(job)
                path.write_bytes(body)
                return {"tasks": tasks, "trial_order": order,
                        "job": {"path": str(path), "bytes": len(body), "sha256": sha(body)}}
            self.assertEqual(check_job_order(exported(), {"trial_order_seed": 12345}, dataset), job)
            job["tasks"][0], job["tasks"][1] = job["tasks"][1], job["tasks"][0]
            with self.assertRaisesRegex(ValueError, "trial_order_job_join"):
                check_job_order(exported(), {"trial_order_seed": 12345}, dataset)
