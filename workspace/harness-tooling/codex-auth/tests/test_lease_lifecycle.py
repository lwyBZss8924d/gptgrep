"""(a) lease semantics and (h) lifecycle: busy bound, shared/exclusive, descendants,
sweep before release, the lease guardian (signals, exceptions and a killed
supervisor after the child exits), sidecar metadata, deadline, cancel, quarantine,
Harbor label gate, run auth-link lifetime, blocker re-probe and lease binding."""
import json
import os
from pathlib import Path
import signal
import socket
import time
import unittest

import support
from support import PY, FIXTURES, Env, sup

PROBE = str(FIXTURES / "consumer_probe.py")


def probe(*args):
    return ["--", PY, "-I", "-B", PROBE, *[str(a) for a in args]]


def kill_quietly(pid):
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass


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

    def next_ok(self, prefix, timeout=90):
        return support.wait_for(lambda: self.env.supervisor(
            "run", "--role", "test", "--run-id", "%s-%d" % (prefix, time.monotonic_ns()),
            *probe("exit", 0))[0] == 0, timeout=timeout, interval=1)

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

    def test_shared_consumer_with_if_stale_policy_overlaps_while_credential_is_fresh(self):
        """A fresh credential needs no owner, so if-stale must not upgrade a shared consumer to exclusive."""
        self.env.reconfigure(allow_shared_consumers=True, codex_bin="/usr/bin/false",
                             codex_bin_sha256=support.sha256_file("/usr/bin/false"))
        holder = self.start_holder("sh-fresh-a", 8, "shared")
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "sh-fresh-b", "--lease", "shared",
                                               "--pre-refresh", "if-stale", "--wait-seconds", "2", *probe("exit", 0))
        holder.communicate(timeout=60)
        self.assertEqual((code, receipt["state"]), (0, "PASS"))
        self.assertEqual(receipt["pre_refresh_result"]["decision"], "SKIPPED_FRESH")
        self.assertFalse(receipt["pre_refresh_result"]["lease_upgraded"])
        support.observe("shared_if_stale_fresh_overlaps", state=receipt["state"])

    def test_sidecar_records_only_allowed_metadata(self):
        holder = self.start_holder("side1", 6)
        sidecar = json.loads(self.env.sidecar("dev", "side1").read_text())
        self.assertEqual(set(sidecar), {"pid", "host_id", "process_start_time", "run_id", "role", "operation", "mode",
                                        "acquired_at", "released_at", "guardian_pid"})
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
        self.assertIn("retained_by_guardian_pid", receipt["lease"])
        self.assertTrue(support.run_auth_links(self.env), "the guardian keeps the auth link while survivors live")
        # The guardian works from a config snapshot: breaking the config file must not release the lease.
        good = self.env.config_path.read_text()
        self.env.config_path.write_text("{not json")
        time.sleep(2)
        self.env.config_path.write_text(good)
        code, busy, _ = self.env.supervisor("run", "--role", "test", "--run-id", "orph-busy", *probe("exit", 0))
        self.assertEqual(code, sup.EXIT_AUTH_BUSY)
        self.assertTrue(self.next_ok("orph-after"))
        sidecar = json.loads(self.env.sidecar("dev", "orph1").read_text())
        self.assertEqual(sidecar.get("released_by"), "lease_guardian_keep")
        self.assertEqual(support.run_auth_links(self.env), [], "the guardian removed the link at release")

    def test_escaped_descendant_with_closed_fds_is_detected_and_lease_kept(self):
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "esc1", *probe("survivor", 15, "close"))
        self.assertEqual(code, sup.EXIT_UNKNOWN)
        self.assertTrue(receipt["sweep"]["run_dir_open_file_holders"] or receipt["sweep"]["tracked_alive"])
        code, _, _ = self.env.supervisor("run", "--role", "test", "--run-id", "esc-busy", *probe("exit", 0))
        self.assertEqual(code, sup.EXIT_AUTH_BUSY)
        self.assertTrue(self.next_ok("esc-after"))

    def test_supervisor_killed_child_keeps_lease(self):
        ready = self.env.root / "ready-kill1"
        proc = self.env.popen("run", "--role", "dev", "--run-id", "kill1", *probe("hold", 12, ready, self.env.lease_path))
        self.assertTrue(support.wait_for(ready.exists, 60))
        proc.send_signal(signal.SIGKILL)
        proc.communicate(timeout=10)
        code, _, _ = self.env.supervisor("run", "--role", "test", "--run-id", "kill-busy", *probe("exit", 0))
        self.assertEqual(code, sup.EXIT_AUTH_BUSY, "orphaned child and guardian still hold the lease")
        self.assertTrue(self.next_ok("kill-after"))
        receipt = self.env.receipt("dev", "kill1")
        self.assertTrue(receipt["supervisor_lost"], "the guardian wrote the receipt of the lost supervisor")
        self.assertEqual(receipt["state"], "UNKNOWN")
        self.assertEqual(support.run_auth_links(self.env), [])

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


class BindingTests(unittest.TestCase):
    """One lease per auth source, bound by a non-secret marker in the canonical home (LOCK-08)."""

    def setUp(self):
        self.env = Env("bind")
        self.env.credential.write(1, time.time() + 3600, in_place=False)

    def test_second_config_with_another_lease_path_is_refused(self):
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "b1", *probe("exit", 0))
        self.assertEqual(code, 0)
        self.assertTrue(receipt["lease"]["binding"]["created"])
        binding = json.loads(self.env.binding().read_text())
        self.assertEqual(set(binding), {"schema", "auth_source_id", "lease_path", "lease_dev", "lease_ino", "bound_at"})
        other = self.env.root / "other-locks"
        other.mkdir(mode=0o700)
        self.env.reconfigure(lease_path=str(other / "fake.lease"))
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "b2", *probe("exit", 0))
        self.assertEqual((code, receipt["refusal"]["reason"]), (sup.EXIT_USAGE, "lease_binding_mismatch"))
        self.assertNotIn("child_started_at", receipt["timings"])
        support.observe("lease_binding_mismatch_refused", reason=receipt["refusal"]["reason"])

    def test_deleted_lease_file_cannot_split_the_lock(self):
        ready = self.env.root / "ready-split"
        holder = self.env.popen("run", "--role", "dev", "--run-id", "split-hold",
                                *probe("hold", 10, ready, self.env.lease_path))
        self.assertTrue(support.wait_for(ready.exists, 60))
        os.unlink(self.env.lease_path)
        code, receipt, _ = self.env.supervisor("run", "--role", "test", "--run-id", "split-2", "--wait-seconds", "0",
                                               *probe("exit", 0))
        self.assertEqual((code, receipt["refusal"]["reason"]), (sup.EXIT_USAGE, "lease_file_missing"))
        self.assertFalse(self.env.lease_path.exists(), "a refused command does not recreate the lock file")
        code, receipt, _ = self.env.supervisor("rebind-lease", "--run-id", "rb-live", "--confirm", "--reason",
                                               "fake drill: lock file removed")
        self.assertEqual((code, receipt["refusal"]["reason"]), (sup.EXIT_AUTH_BUSY, "rebind_refused_live_holders"))
        out, _ = holder.communicate(timeout=60)
        self.assertEqual(support.last_json(out)["state"], "PASS")
        self.assertTrue(support.wait_for(lambda: self.env.supervisor(
            "rebind-lease", "--run-id", "rb-%d" % time.monotonic_ns(), "--confirm", "--reason",
            "fake drill: lock file removed")[0] == 0, timeout=60, interval=1))
        code, receipt, _ = self.env.supervisor("run", "--role", "test", "--run-id", "split-3", *probe("exit", 0))
        self.assertEqual(code, 0)
        self.assertIn("LEASE_REBOUND", [e["decision"] for e in self.env.ledger()])
        support.observe("lease_file_removed_refused_then_rebound", refused="lease_file_missing")


class LifecycleTests(unittest.TestCase):
    def setUp(self):
        self.env = Env("life")
        self.env.credential.write(1, time.time() + 3600, in_place=False)

    def next_consumer(self, run_id, role="test"):
        code, receipt, _ = self.env.supervisor("run", "--role", role, "--run-id", run_id, "--wait-seconds", "0",
                                               *probe("exit", 0))
        return code, (receipt or {}).get("state")

    def next_ok(self, prefix, timeout=90):
        return support.wait_for(lambda: self.env.supervisor(
            "run", "--role", "test", "--run-id", "%s-%d" % (prefix, time.monotonic_ns()),
            *probe("exit", 0))[0] == 0, timeout=timeout, interval=1)

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

    def test_root_first_stop_gives_the_root_its_own_cleanup_grace(self):
        """Harbor-style roles: the root alone gets SIGTERM and a long grace before the tree is signalled."""
        roles = dict(self.env.values["roles"])
        roles["harbor"] = dict(roles["harbor"], stop_scope="root-first", term_grace_seconds=20)
        self.env.reconfigure(roles=roles)
        workdir = self.env.root / "root-first"
        workdir.mkdir(mode=0o700)
        proc = self.env.popen("run", "--role", "harbor", "--run-id", "rf1", *probe("root-first-cleanup", workdir))
        self.assertTrue(support.wait_for((workdir / "root-ready").exists, 30))
        proc.send_signal(signal.SIGTERM)
        out, _ = proc.communicate(timeout=60)
        receipt = support.last_json(out)
        self.assertEqual((receipt["state"], receipt["stop"]), ("CANCELLED", "terminated"))
        cleanup = json.loads((workdir / "root-cleanup.json").read_text())
        self.assertTrue(cleanup["grandchild_alive_during_root_cleanup"],
                        "the tree is not signalled while the root runs its own cancel path")
        support.observe("root_first_stop", grandchild_untouched_during_root_cleanup=True)

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
        self.assertEqual(support.run_auth_links(self.env), [], "no run keeps a link after it ends or is refused")

    # -- auth link lifetime (CS-3) ----------------------------------------------------------

    def test_auth_link_exists_only_while_the_run_holds_the_lease(self):
        out = self.env.root / "link-report.json"
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "lk1", *probe("env-report", out))
        self.assertEqual(code, 0)
        during = json.loads(out.read_text())["auth_link"]
        self.assertEqual(during, {"is_symlink": True, "target": str(self.env.credential.path)})
        self.assertTrue(receipt["auth_link_final"]["released"])
        self.assertFalse(os.path.lexists(self.env.run_dir("dev", "lk1") / "codex-home" / "auth.json"))
        for run_id, args in (("lk-fail", probe("exit", 3)),
                             ("lk-dl", ["--deadline-seconds", "1", *probe("ignore-term", 30, self.env.root / "r")])):
            self.env.supervisor("run", "--role", "dev", "--run-id", run_id, *args)
        holder_ready = self.env.root / "lk-ready"
        holder = self.env.popen("run", "--role", "dev", "--run-id", "lk-hold",
                                *probe("hold", 6, holder_ready, self.env.lease_path))
        self.assertTrue(support.wait_for(holder_ready.exists, 60))
        code, receipt, _ = self.env.supervisor("run", "--role", "test", "--run-id", "lk-busy", "--wait-seconds", "0",
                                               *probe("exit", 0))
        self.assertEqual(code, sup.EXIT_AUTH_BUSY)
        holder.communicate(timeout=60)
        self.env.supervisor("clear-quarantine", "--run-id", "lk-maint", "--confirm", "--reason", "no marker")
        self.assertEqual(support.run_auth_links(self.env), [])
        self.assertFalse(os.path.lexists(self.env.run_dir("_maintenance", "lk-maint") / "codex-home" / "auth.json"))
        support.observe("auth_link_lifetime", live_links_after_runs=0)

    def test_replaced_link_is_quarantined_and_left_for_review(self):
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "rl1", *probe("replace-link"))
        self.assertEqual((code, receipt["state"]), (sup.EXIT_QUARANTINED, "QUARANTINED"))
        self.assertIn("RUN_AUTH_LINK_REPLACED", [a["code"] for a in receipt["anomalies"]])
        self.assertEqual(json.loads(self.env.quarantine_marker().read_text())["reason"], "RUN_AUTH_LINK_REPLACED")
        replaced = self.env.run_dir("dev", "rl1") / "codex-home" / "auth.json"
        self.assertTrue(replaced.is_file() and not replaced.is_symlink(), "evidence is not erased automatically")
        self.assertFalse(receipt["auth_link_final"]["released"])
        replaced.unlink()  # the test removes its own synthetic per-run login after checking

    # -- post-launch robustness (F2 / LC-01) --------------------------------------------------

    def _fake_docker(self, ids, wait_seconds=3):
        state = self.env.root / "docker-state"
        state.write_text("\n".join(ids))
        fake = support.script(self.env.root / "docker", (FIXTURES / "fake_docker.py").read_text())
        Path(str(fake) + ".json").write_text(json.dumps({"state": str(state)}))
        self.env.reconfigure(docker={"bin": str(fake), "env": {"DOCKER_HOST": "unix:///fake/orbstack.sock"},
                                     "wait_seconds": wait_seconds, "poll_seconds": 0.2})
        return state

    def _signal_during_docker_wait(self, sig, run_id):
        state = self._fake_docker(["c0ffee99"], wait_seconds=60)
        proc = self.env.popen("run", "--role", "harbor", "--run-id", run_id, "--harbor-job-label",
                              "gptgrep.auth-job=" + run_id, *probe("exit", 0))
        self.assertTrue(support.wait_for(lambda: self.env.has_event("harbor", run_id, "child_exited"), 60))
        time.sleep(1.5)  # the supervisor is now polling docker while the container is still listed
        proc.send_signal(sig)
        out, _ = proc.communicate(timeout=60)
        receipt = support.last_json(out)
        self.assertEqual((proc.returncode, receipt["state"]), (sup.EXIT_UNKNOWN, "UNKNOWN"))
        self.assertTrue(receipt["docker"]["interrupted_by_signal"])
        self.assertEqual(receipt["signals_after_child_exit"], 1)
        self.assertTrue(receipt["docker"]["label_matched"])
        self.assertIsNotNone(self.env.receipt("harbor", run_id))
        self.assertEqual(self.env.lease_probe(), "held_exclusive", "the guardian keeps the lease")
        self.assertEqual(self.next_consumer(run_id + "-busy")[0], sup.EXIT_AUTH_BUSY)
        state.write_text("")
        self.assertTrue(self.next_ok(run_id + "-after"))
        support.observe("signal_during_docker_wait", signal=sig.name, supervisor_state=receipt["state"],
                        lease_kept_while_container_listed=True)

    def test_sigint_during_harbor_container_wait_keeps_lease(self):
        self._signal_during_docker_wait(signal.SIGINT, "dw-int")

    def test_sigterm_during_harbor_container_wait_keeps_lease(self):
        self._signal_during_docker_wait(signal.SIGTERM, "dw-term")

    def test_sigint_during_survivor_sweep_keeps_lease(self):
        self.env.reconfigure(sweep_settle_seconds=30)
        pidfile = self.env.root / "escaper-sw.pid"
        proc = self.env.popen("run", "--role", "dev", "--run-id", "sw1", *probe("escaped", pidfile, 20))
        self.assertTrue(support.wait_for(lambda: self.env.has_event("dev", "sw1", "child_exited"), 60))
        gpid = int(pidfile.read_text())
        try:
            time.sleep(1.5)  # the supervisor is re-sweeping a live survivor
            proc.send_signal(signal.SIGINT)
            out, _ = proc.communicate(timeout=60)
            receipt = support.last_json(out)
            self.assertEqual(receipt["state"], "UNKNOWN")
            self.assertTrue(receipt["sweep"]["interrupted_by_signal"])
            self.assertTrue(support.alive(gpid))
            self.assertEqual(self.next_consumer("sw1-busy")[0], sup.EXIT_AUTH_BUSY)
        finally:
            kill_quietly(gpid)
        self.assertTrue(self.next_ok("sw1-after"))

    def test_supervisor_sigkill_after_child_exit_leaves_guardian_holding_lease(self):
        self.env.reconfigure(sweep_settle_seconds=30)
        pidfile = self.env.root / "escaper-k.pid"
        proc = self.env.popen("run", "--role", "dev", "--run-id", "sk1", *probe("escaped", pidfile, 12))
        self.assertTrue(support.wait_for(lambda: self.env.has_event("dev", "sk1", "child_exited"), 60))
        gpid = int(pidfile.read_text())
        time.sleep(1.0)
        proc.send_signal(signal.SIGKILL)
        proc.communicate(timeout=30)
        try:
            self.assertTrue(support.alive(gpid))
            self.assertEqual(self.next_consumer("sk1-busy")[0], sup.EXIT_AUTH_BUSY,
                             "the guardian took over from the killed supervisor")
        finally:
            pass
        self.assertTrue(self.next_ok("sk1-after", timeout=120))
        self.assertFalse(support.alive(gpid), "released only after the survivor exited")
        receipt = self.env.receipt("dev", "sk1")
        self.assertTrue(receipt["supervisor_lost"])
        support.observe("supervisor_sigkill_guardian_takeover", survivor_outlived_supervisor=True,
                        receipt_written_by_guardian=True)

    def test_post_launch_exception_path_keeps_lease(self):
        """A consumer chmods its CODEX_HOME to 000 and leaves an escaped survivor (no INTERNAL_ERROR, no release)."""
        pidfile = self.env.root / "escaper-chmod.pid"
        code, receipt, proc = self.env.supervisor("run", "--role", "dev", "--run-id", "ch1",
                                                  *probe("chmod-home", pidfile, 12))
        gpid = int(pidfile.read_text())
        try:
            self.assertEqual((code, receipt["state"]), (sup.EXIT_UNKNOWN, "UNKNOWN"), proc.stderr[-1000:])
            self.assertIn("RUN_CODEX_HOME_MODE_RESTORED", [a["code"] for a in receipt["anomalies"]])
            self.assertEqual(self.next_consumer("ch1-busy")[0], sup.EXIT_AUTH_BUSY)
        finally:
            kill_quietly(gpid)
        self.assertTrue(self.next_ok("ch1-after"))

    def test_guardian_rechecks_canonical_before_release(self):
        pidfile = self.env.root / "late-writer.pid"
        before = self.env.credential.meta()
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "kw1",
                                               *probe("escaped", pidfile, 10, "canonical-append"))
        self.assertEqual((code, receipt["state"]), (sup.EXIT_UNKNOWN, "UNKNOWN"))
        self.assertFalse(receipt["canonical_changed"])
        self.assertTrue(support.wait_for(lambda: self.env.lease_probe() == "free", 60, 0.2))
        self.assertNotEqual(self.env.credential.meta()["size"], before["size"], "the survivor wrote the credential")
        self.assertEqual(json.loads(self.env.quarantine_marker().read_text())["reason"],
                         "CANONICAL_CHANGED_AFTER_SUPERVISOR")
        self.assertEqual(self.next_consumer("kw1-next", role="dev"), (sup.EXIT_QUARANTINED, "QUARANTINED"))
        support.observe("guardian_canonical_recheck", quarantined=True)

    def test_guardian_does_not_adopt_a_reused_root_pid(self):
        """A takeover tracker must not treat an unrelated process that reused the root pid as the run."""
        cfg = self.env.cfg()
        tracker = sup.Tracker(cfg, os.getpid(), {}, adopt_root=False)
        tracker.snapshot()
        self.assertEqual(tracker.alive(tracker.snapshot()), {})
        launcher = sup.Tracker(cfg, os.getpid(), {})
        launcher.snapshot()
        self.assertIn(os.getpid(), launcher.alive(launcher.snapshot()))

    # -- Harbor label gate (F1 / LC-02) ------------------------------------------------------

    def test_harbor_label_seen_during_run_then_cleared(self):
        state = self._fake_docker(["c0ffee01", "c0ffee02"])
        ready = self.env.root / "ready-h1"
        proc = self.env.popen("run", "--role", "harbor", "--run-id", "h1", "--harbor-job-label",
                              "gptgrep.auth-job=h1", *probe("hold", 2, ready, self.env.lease_path))
        self.assertTrue(support.wait_for(ready.exists, 60))
        time.sleep(1.0)
        state.write_text("")
        out, _ = proc.communicate(timeout=60)
        receipt = support.last_json(out)
        self.assertEqual(receipt["state"], "PASS")
        self.assertTrue(receipt["docker"]["cleared"])
        self.assertTrue(receipt["docker"]["label_matched"])
        self.assertEqual(receipt["docker"]["max_seen_during_run"], 2)
        calls = [json.loads(line) for line in Path(str(state) + ".calls").read_text().splitlines()]
        self.assertTrue(all(c == {"filter": "label=gptgrep.auth-job=h1", "docker_host": "unix:///fake/orbstack.sock"}
                            for c in calls))

    def test_harbor_label_that_never_matched_is_unknown(self):
        self._fake_docker([])
        code, receipt, _ = self.env.supervisor("run", "--role", "harbor", "--run-id", "h0", "--harbor-job-label",
                                               "label.never.applied=job-1", *probe("exit", 0))
        self.assertEqual((code, receipt["state"]), (sup.EXIT_UNKNOWN, "UNKNOWN"))
        self.assertFalse(receipt["docker"]["label_matched"])
        self.assertFalse(receipt["docker"]["cleared"])
        self.assertIn("HARBOR_LABEL_NEVER_MATCHED", [a["code"] for a in receipt["anomalies"]])
        self.assertTrue(self.next_ok("h0-after"))
        support.observe("harbor_label_never_matched", state=receipt["state"])

    def test_harbor_label_with_remaining_containers_is_unknown_and_keeps_lease(self):
        state = self._fake_docker(["deadbeef01"])
        code, receipt, _ = self.env.supervisor("run", "--role", "harbor", "--run-id", "h2", "--harbor-job-label",
                                               "gptgrep.auth-job=h2", *probe("exit", 0))
        self.assertEqual((code, receipt["state"]), (sup.EXIT_UNKNOWN, "UNKNOWN"))
        self.assertFalse(receipt["docker"]["cleared"])
        self.assertEqual(receipt["docker"]["remaining"], 1)
        code, _, _ = self.env.supervisor("run", "--role", "test", "--run-id", "h2-busy", *probe("exit", 0))
        self.assertEqual(code, sup.EXIT_AUTH_BUSY, "guardian retains the lease while labelled containers remain")
        state.write_text("")
        self.assertTrue(self.next_ok("h2-after"))

    def test_guardian_keeps_watching_the_label_after_a_clear_check(self):
        """Survivors kept the run UNKNOWN; a container they start later still holds the lease."""
        state = self._fake_docker(["c0ffee03"])
        pidfile = self.env.root / "late-container.pid"
        proc = self.env.popen("run", "--role", "harbor", "--run-id", "lc1", "--harbor-job-label",
                              "gptgrep.auth-job=lc1",
                              *probe("escaped", pidfile, 5, "docker-add", state, "late0001"))
        self.assertTrue(support.wait_for(lambda: self.env.has_event("harbor", "lc1", "child_exited"), 60))
        state.write_text("")
        out, _ = proc.communicate(timeout=60)
        receipt = support.last_json(out)
        self.assertEqual(receipt["state"], "UNKNOWN")
        self.assertTrue(receipt["docker"]["cleared"])
        keeper = json.loads((self.env.run_dir("harbor", "lc1") / "state" / "lease-guardian-input.json").read_text())
        self.assertEqual(keeper["docker_label"], "gptgrep.auth-job=lc1")
        self.assertTrue(support.wait_for(lambda: "late0001" in state.read_text(), 30))
        time.sleep(3)
        self.assertEqual(self.next_consumer("lc1-busy")[0], sup.EXIT_AUTH_BUSY,
                         "the late container keeps the lease")
        state.write_text("")
        self.assertTrue(self.next_ok("lc1-after"))

    # -- blocker re-probe (F6) -----------------------------------------------------------------

    def test_listener_appearing_on_blocker_port_stops_run_and_quarantines(self):
        with socket.socket() as reserve:
            reserve.bind(("127.0.0.1", support.next_port()))
            port = reserve.getsockname()[1]
        self.env.reconfigure(blocker_url="http://127.0.0.1:%d/oauth/token" % port, blocker_must_be_closed=True)
        ready = self.env.root / "ready-bind"
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "bp1",
                                               *probe("bind-port", port, 60, ready))
        self.assertTrue(ready.exists(), "the listener appeared after the launch-time probe")
        self.assertEqual((code, receipt["state"]), (sup.EXIT_QUARANTINED, "QUARANTINED"))
        self.assertIn("BLOCKER_LISTENER_APPEARED", [a["code"] for a in receipt["anomalies"]])
        self.assertEqual(receipt["stop"], "terminated")
        self.assertEqual(json.loads(self.env.quarantine_marker().read_text())["reason"],
                         "BLOCKER_LISTENER_APPEARED")
        support.observe("blocker_listener_reprobe", state=receipt["state"])

    def test_default_blocker_is_port_zero_and_unbindable(self):
        cfg = self.env.cfg()
        self.assertEqual(cfg.blocker_url, "http://127.0.0.1:0/oauth/token")
        self.assertEqual(cfg.revoke_blocker_url, "http://127.0.0.1:0/oauth/revoke")
        # "closed" (EADDRNOTAVAIL) unguarded; "blocked" (EPERM) under the qualification's seatbelt guard,
        # which cannot express port 0. Either way nothing can receive the request.
        self.assertIn(sup.blocker_probe(cfg), ("closed", "blocked"))
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            self.assertNotEqual(listener.getsockname()[1], 0, "binding port 0 never yields port 0")


if __name__ == "__main__":
    unittest.main()
