"""(a) lease semantics and (h) lifecycle: busy bound, shared/exclusive, descendants,
sweep before release, sidecar metadata, deadline, cancel, quarantine, docker label."""
import json
import os
from pathlib import Path
import signal
import time
import unittest

import support
from support import PY, FIXTURES, Env, sup

PROBE = str(FIXTURES / "consumer_probe.py")


def probe(*args):
    return ["--", PY, "-I", "-B", PROBE, *[str(a) for a in args]]


class LeaseTests(unittest.TestCase):
    def setUp(self):
        self.env = Env("lease")
        self.env.credential.write(1, time.time() + 3600, in_place=False)

    def start_holder(self, run_id, seconds, mode="exclusive", role="dev"):
        ready = self.env.root / ("ready-" + run_id)
        proc = self.env.popen("run", "--role", role, "--run-id", run_id, "--lease", mode,
                              *probe("hold", seconds, ready, self.env.lease_path))
        self.assertTrue(support.wait_for(ready.exists, 60), "holder did not start")
        return proc

    def test_exclusive_blocks_second_consumer_with_bounded_auth_busy(self):
        holder = self.start_holder("hold1", 15)
        start = time.monotonic()
        code, receipt, _ = self.env.supervisor("run", "--role", "test", "--run-id", "busy1", "--wait-seconds", "1",
                                               *probe("exit", 0))
        elapsed = time.monotonic() - start
        self.assertEqual(code, sup.EXIT_AUTH_BUSY)
        self.assertEqual(receipt["state"], "AUTH_BUSY")
        self.assertGreaterEqual(receipt["lease"]["waited_seconds"], 1.0)
        self.assertLess(elapsed, 12, "the wait is bounded")
        self.assertNotIn("child_started_at", receipt["timings"])
        out, _ = holder.communicate(timeout=30)
        self.assertEqual(support.last_json(out)["state"], "PASS")
        code, receipt, _ = self.env.supervisor("run", "--role", "test", "--run-id", "after1", *probe("exit", 0))
        self.assertEqual((code, receipt["state"]), (0, "PASS"))

    def test_shared_consumers_overlap_and_exclusive_waits(self):
        self.env.reconfigure(allow_shared_consumers=True)
        a = self.start_holder("sh-a", 8, "shared")
        b = self.start_holder("sh-b", 8, "shared")
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "ex-busy", "--wait-seconds", "0.5",
                                               *probe("exit", 0))
        self.assertEqual(code, sup.EXIT_AUTH_BUSY)
        for proc in (a, b):
            out, _ = proc.communicate(timeout=30)
            self.assertEqual(support.last_json(out)["state"], "PASS")
        receipts = [json.loads((self.env.run_dir("dev", r) / "receipt.json").read_text()) for r in ("sh-a", "sh-b")]
        self.assertLess(max(r["timings"]["lease_acquired_at"] for r in receipts),
                        min(r["timings"]["child_exited_at"] for r in receipts))

    def test_sidecar_records_only_allowed_metadata(self):
        holder = self.start_holder("side1", 6)
        sidecar = json.loads(self.env.sidecar("dev", "side1").read_text())
        self.assertEqual(set(sidecar), {"pid", "host_id", "process_start_time", "run_id", "role", "operation", "mode",
                                        "acquired_at", "released_at"})
        self.assertEqual((sidecar["run_id"], sidecar["mode"], sidecar["released_at"]), ("side1", "exclusive", None))
        self.assertTrue(sidecar["process_start_time"])
        holder.communicate(timeout=30)
        sidecar = json.loads(self.env.sidecar("dev", "side1").read_text())
        self.assertIsNotNone(sidecar["released_at"])
        self.assertTrue(self.env.lease_path.exists(), "lock file is never deleted")

    def test_descendant_keeping_lease_fd_retains_lease_until_it_exits(self):
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "orph1", *probe("survivor", 15, "keep"))
        self.assertEqual(code, sup.EXIT_UNKNOWN)
        self.assertEqual(receipt["state"], "UNKNOWN")
        self.assertTrue(receipt["sweep"]["survivor_pids"])
        self.assertTrue(receipt["sweep"]["inherited_fd_holders"])
        self.assertIn("retained_by_keeper_pid", receipt["lease"])
        # The keeper works from a config snapshot: breaking the config file must not release the lease.
        good = self.env.config_path.read_text()
        self.env.config_path.write_text("{not json")
        time.sleep(2)
        self.env.config_path.write_text(good)
        code, busy, _ = self.env.supervisor("run", "--role", "test", "--run-id", "orph-busy", *probe("exit", 0))
        self.assertEqual(code, sup.EXIT_AUTH_BUSY)
        self.assertTrue(support.wait_for(lambda: self.env.supervisor(
            "run", "--role", "test", "--run-id", "orph-after-%d" % time.monotonic_ns(),
            *probe("exit", 0))[0] == 0, timeout=90, interval=1))
        sidecar = json.loads(self.env.sidecar("dev", "orph1").read_text())
        self.assertEqual(sidecar.get("released_by"), "lease_keeper_after_survivors_exited")

    def test_escaped_descendant_with_closed_fds_is_detected_and_lease_kept(self):
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "esc1", *probe("survivor", 15, "close"))
        self.assertEqual(code, sup.EXIT_UNKNOWN)
        self.assertTrue(receipt["sweep"]["run_dir_open_file_holders"] or receipt["sweep"]["tracked_alive"])
        code, _, _ = self.env.supervisor("run", "--role", "test", "--run-id", "esc-busy", *probe("exit", 0))
        self.assertEqual(code, sup.EXIT_AUTH_BUSY)
        self.assertTrue(support.wait_for(lambda: self.env.supervisor(
            "run", "--role", "test", "--run-id", "esc-after-%d" % time.monotonic_ns(),
            *probe("exit", 0))[0] == 0, timeout=90, interval=1))

    def test_supervisor_killed_child_keeps_lease(self):
        ready = self.env.root / "ready-kill1"
        proc = self.env.popen("run", "--role", "dev", "--run-id", "kill1", *probe("hold", 12, ready, self.env.lease_path))
        self.assertTrue(support.wait_for(ready.exists, 60))
        proc.send_signal(signal.SIGKILL)
        proc.communicate(timeout=10)
        code, _, _ = self.env.supervisor("run", "--role", "test", "--run-id", "kill-busy", *probe("exit", 0))
        self.assertEqual(code, sup.EXIT_AUTH_BUSY, "orphaned child still holds the inherited lease")
        self.assertTrue(support.wait_for(lambda: self.env.supervisor(
            "run", "--role", "test", "--run-id", "kill-after-%d" % time.monotonic_ns(),
            *probe("exit", 0))[0] == 0, timeout=90, interval=1))

    def test_lease_symlink_refused(self):
        target = self.env.root / "elsewhere.lock"
        target.write_text("")
        os.chmod(target, 0o600)
        os.symlink(target, self.env.lease_path)
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "sym1", *probe("exit", 0))
        self.assertEqual(code, sup.EXIT_USAGE)
        self.assertEqual(receipt["refusal"]["reason"], "lease_path_is_symlink")

    def test_lease_file_mode_and_directory_mode_refused(self):
        self.env.lease_path.write_text("")
        os.chmod(self.env.lease_path, 0o644)
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "mode1", *probe("exit", 0))
        self.assertEqual(receipt["refusal"]["reason"], "lease_not_0600")
        os.chmod(self.env.lease_path, 0o600)
        os.chmod(self.env.lease_path.parent, 0o755)
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "mode2", *probe("exit", 0))
        self.assertEqual(receipt["refusal"]["reason"], "directory_not_private_0700")
        os.chmod(self.env.lease_path.parent, 0o700)


class LifecycleTests(unittest.TestCase):
    def setUp(self):
        self.env = Env("life")
        self.env.credential.write(1, time.time() + 3600, in_place=False)

    def test_nonzero_exit_is_failed_and_run_ids_are_not_reused(self):
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "fail1", *probe("exit", 7))
        self.assertEqual((code, receipt["state"], receipt["child_exit_code"]), (7, "FAILED", 7))
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "fail1", *probe("exit", 0))
        self.assertEqual((code, receipt["state"]), (sup.EXIT_RUN_EXISTS, "RUN_ALREADY_EXISTS"))

    def test_deadline_terminates_group_and_releases_lease(self):
        start = time.monotonic()
        ready = self.env.root / "ready-dl1"
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "dl1", "--deadline-seconds", "1",
                                               *probe("hold", 60, ready, self.env.lease_path))
        self.assertEqual((code, receipt["state"]), (sup.EXIT_TIMEOUT, "TIMEOUT"))
        self.assertTrue(receipt["deadline_hit"])
        self.assertLess(time.monotonic() - start, 30)
        self.assertEqual(receipt["sweep"]["survivor_pids"], [])
        code, _, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "dl1-after", *probe("exit", 0))
        self.assertEqual(code, 0)

    def test_deadline_kills_term_ignoring_child_after_grace(self):
        ready = self.env.root / "ready-dl2"
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "dl2", "--deadline-seconds", "1",
                                               *probe("ignore-term", 60, ready))
        self.assertEqual((code, receipt["state"], receipt["stop"]), (sup.EXIT_TIMEOUT, "TIMEOUT", "killed"))

    def test_cancel_signal_is_forwarded(self):
        ready = self.env.root / "ready-c1"
        proc = self.env.popen("run", "--role", "dev", "--run-id", "c1", *probe("hold", 60, ready, self.env.lease_path))
        self.assertTrue(support.wait_for(ready.exists, 20))
        proc.send_signal(signal.SIGTERM)
        out, _ = proc.communicate(timeout=60)
        receipt = support.last_json(out)
        self.assertEqual((proc.returncode, receipt["state"], receipt["cancelled"]),
                         (sup.EXIT_CANCELLED, "CANCELLED", "SIGTERM"))
        code, _, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "c1-after", *probe("exit", 0))
        self.assertEqual(code, 0)

    def test_consumer_changing_canonical_is_quarantined_until_cleared(self):
        before = self.env.credential.meta()
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "q1", *probe("write-canonical"))
        self.assertEqual((code, receipt["state"]), (sup.EXIT_QUARANTINED, "QUARANTINED"))
        self.assertIn("CANONICAL_CHANGED_BY_CONSUMER", [a["code"] for a in receipt["anomalies"]])
        self.assertEqual(self.env.credential.meta()["ino"], before["ino"])
        self.assertTrue(self.env.quarantine_marker().exists())
        ready = self.env.root / "ready-q2"
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "q2",
                                               *probe("hold", 0, ready, self.env.lease_path))
        self.assertEqual((code, receipt["state"]), (sup.EXIT_QUARANTINED, "QUARANTINED"))
        self.assertFalse(ready.exists())
        code, receipt, _ = self.env.supervisor("clear-quarantine", "--run-id", "qc0", "--reason", "x")
        self.assertEqual(code, sup.EXIT_USAGE)
        code, receipt, _ = self.env.supervisor("clear-quarantine", "--run-id", "qc1", "--confirm", "--reason",
                                               "fake drill reviewed")
        self.assertEqual((code, receipt["state"]), (0, "PASS"))
        self.assertFalse(self.env.quarantine_marker().exists())
        self.assertTrue(Path(receipt["archived_marker"]).exists(), "audit evidence is archived, not erased")
        code, _, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "q3", *probe("exit", 0))
        self.assertEqual(code, 0)
        decisions = [e["decision"] for e in self.env.ledger()]
        self.assertIn("QUARANTINE_CLEARED", decisions)

    def _fake_docker(self, ids):
        state = self.env.root / "docker-state"
        state.write_text("\n".join(ids))
        fake = support.script(self.env.root / "docker", (FIXTURES / "fake_docker.py").read_text())
        Path(str(fake) + ".json").write_text(json.dumps({"state": str(state)}))
        self.env.reconfigure(docker={"bin": str(fake), "env": {"DOCKER_HOST": "unix:///fake/orbstack.sock"},
                                     "wait_seconds": 3, "poll_seconds": 0.2})
        return state

    def test_harbor_label_waits_for_containers_to_clear(self):
        state = self._fake_docker(["c0ffee01", "c0ffee02"])
        proc = self.env.popen("run", "--role", "harbor", "--run-id", "h1", "--harbor-job-label",
                              "gptgrep.job=h1", *probe("exit", 0))
        time.sleep(1.0)
        state.write_text("")
        out, _ = proc.communicate(timeout=60)
        receipt = support.last_json(out)
        self.assertEqual(receipt["state"], "PASS")
        self.assertTrue(receipt["docker"]["cleared"])
        calls = [json.loads(line) for line in Path(str(state) + ".calls").read_text().splitlines()]
        self.assertTrue(all(c == {"filter": "label=gptgrep.job=h1", "docker_host": "unix:///fake/orbstack.sock"}
                            for c in calls))

    def test_harbor_label_with_remaining_containers_is_unknown_and_keeps_lease(self):
        state = self._fake_docker(["deadbeef01"])
        code, receipt, _ = self.env.supervisor("run", "--role", "harbor", "--run-id", "h2", "--harbor-job-label",
                                               "gptgrep.job=h2", *probe("exit", 0))
        self.assertEqual((code, receipt["state"]), (sup.EXIT_UNKNOWN, "UNKNOWN"))
        self.assertFalse(receipt["docker"]["cleared"])
        self.assertEqual(receipt["docker"]["remaining"], 1)
        code, _, _ = self.env.supervisor("run", "--role", "test", "--run-id", "h2-busy", *probe("exit", 0))
        self.assertEqual(code, sup.EXIT_AUTH_BUSY, "keeper retains the lease while labelled containers remain")
        state.write_text("")
        self.assertTrue(support.wait_for(lambda: self.env.supervisor(
            "run", "--role", "test", "--run-id", "h2-after-%d" % time.monotonic_ns(),
            *probe("exit", 0))[0] == 0, timeout=90, interval=1))


if __name__ == "__main__":
    unittest.main()
