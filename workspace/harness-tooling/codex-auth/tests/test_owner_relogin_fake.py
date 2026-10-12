"""Owner refresh and re-login logic against a FAKE codex binary.

Covers the owner's stop sequence (stdin close, SIGTERM grace, SIGKILL anomaly),
refresh policy, failure classification (permanent -> quarantine, transient ->
REFRESH_UNAVAILABLE), signals and a killed supervisor during a refresh, binary
pinning, pre-run refresh, the consumer auth-failure contract (exit 77) and the
gated staging re-login. The real CLI is never used here.
"""
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


class FakeCodexMixin:
    def install_fake_codex(self, **settings):
        fake = support.script(self.env.root / "codex", (FIXTURES / "fake_codex.py").read_text())
        Path(str(fake) + ".json").write_text(json.dumps(settings))
        self.env.reconfigure(codex_bin=str(fake), codex_bin_sha256=support.sha256_file(fake),
                             owner={"exit_wait_seconds": 1, "term_grace_seconds": 3, "rpc_timeout_seconds": 20})
        return fake

    def records(self, run_dir):
        path = Path(run_dir) / "state" / "fake-codex-record.jsonl"
        if not path.exists():
            return []
        return [json.loads(line) for line in path.read_text().splitlines()]


class OwnerFakeTests(FakeCodexMixin, unittest.TestCase):
    def setUp(self):
        self.env = Env("owner")
        self.env.credential.write(1, time.time() + 3600, in_place=False)

    def test_policy_refresh_rewrites_in_place_and_records_ledger(self):
        self.install_fake_codex(app_server="normal")
        self.env.credential.age(13 * 3600)
        before = self.env.credential.meta()
        code, receipt, proc = self.env.supervisor("refresh", "--run-id", "own1")
        self.assertEqual((code, receipt["state"], receipt["refresh"]["decision"]), (0, "PASS", "REFRESHED"),
                         proc.stderr)
        after = self.env.credential.meta()
        self.assertEqual(after["ino"], before["ino"])
        self.assertNotEqual((after["mtime_ns"], after["size"]), (before["mtime_ns"], before["size"]))
        self.assertEqual(after["mode"], 0o600)
        self.assertEqual(receipt["owner"]["stop"], "stdin_closed")
        self.assertEqual(receipt["auth_link_after"], {"present": True, "symlink": True, "target_matches": True,
                                                      "home_physical": True})
        self.assertTrue(receipt["auth_link_final"]["released"], "the owner's link is removed after the refresh")
        record = self.records(self.env.run_dir("_owner", "own1"))[0]
        self.assertIsNone(record["refresh_override"], "owner must not carry the consumer blocker")
        self.assertEqual(record["revoke_override"], sup.DEFAULT_BLOCKER_URL.replace("/oauth/token", "/oauth/revoke"))
        self.assertEqual(record["rust_log"], sup.OWNER_RUST_LOG)
        for name in sup.STRIPPED_ALWAYS:
            self.assertNotIn(name, record["env_names"])
        self.assertEqual(record["argv"], ["app-server", "--listen", "stdio://", "--strict-config"])
        entry = self.env.ledger()[-1]
        self.assertEqual((entry["decision"], entry["canonical_changed"], entry["inode_preserved"]),
                         ("REFRESHED", True, True))
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "own2")
        self.assertEqual((code, receipt["refresh"]["decision"]), (0, "SKIPPED_FRESH"))
        self.assertEqual(self.records(self.env.run_dir("_owner", "own2")), [], "fresh: no app-server launch")
        self.assertEqual(support.run_auth_links(self.env), [])

    def test_unchanged_canonical_is_auth_refresh_failed_and_blocks_consumers(self):
        self.install_fake_codex(app_server="no_write")
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "nw1", "--force")
        self.assertEqual((code, receipt["state"], receipt["refresh"]["decision"]),
                         (sup.EXIT_BLOCKED_AUTH, "BLOCKED_AUTH", "AUTH_REFRESH_FAILED"))
        self.assertIsNone(receipt["refresh"]["owner_log_failure_kind"], "unclassified, never read as transient")
        self.assertTrue(self.env.quarantine_marker().exists())
        ready = self.env.root / "ready-nw"
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "nw-run",
                                               *probe("hold", 0, ready))
        self.assertEqual((code, receipt["state"]), (sup.EXIT_QUARANTINED, "QUARANTINED"))
        self.assertFalse(ready.exists())
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "nw2", "--force")
        self.assertEqual(code, sup.EXIT_QUARANTINED, "no retry loop while quarantined")
        code, _, _ = self.env.supervisor("clear-quarantine", "--run-id", "nwc", "--confirm", "--reason", "fake drill")
        self.assertEqual(code, 0)

    def test_permanent_owner_failure_is_classified_and_quarantined(self):
        self.install_fake_codex(app_server="permanent_fail")
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "pf1", "--force")
        self.assertEqual((code, receipt["state"]), (sup.EXIT_BLOCKED_AUTH, "BLOCKED_AUTH"))
        self.assertEqual((receipt["refresh"]["owner_log_failure_class"], receipt["refresh"]["owner_log_failure_kind"]),
                         ("REFRESH_TOKEN_REUSED", "permanent"))
        marker = json.loads(self.env.quarantine_marker().read_text())
        self.assertEqual((marker["reason"], marker["details"]["failure_kind"]), ("AUTH_REFRESH_FAILED", "permanent"))
        support.observe("owner_permanent_failure_fake", failure_class="REFRESH_TOKEN_REUSED", quarantined=True)

    def test_transient_owner_failure_is_not_quarantined(self):
        self.install_fake_codex(app_server="transient_fail")
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "tf1", "--force")
        self.assertEqual((code, receipt["state"], receipt["refresh"]["decision"]),
                         (sup.EXIT_UNAVAILABLE, "REFRESH_UNAVAILABLE", "REFRESH_DEFERRED_TRANSIENT"))
        self.assertEqual(receipt["refresh"]["owner_log_failure_class"], "TRANSPORT")
        self.assertFalse(self.env.quarantine_marker().exists(), "a network failure is not an auth failure")
        self.assertEqual(self.env.ledger()[-1]["decision"], "REFRESH_DEFERRED_TRANSIENT")
        code, _, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "tf-run", *probe("exit", 0))
        self.assertEqual(code, 0, "consumers keep working with the unchanged credential")
        support.observe("owner_transient_failure_fake", state="REFRESH_UNAVAILABLE", quarantined=False)

    def test_owner_gets_long_sigterm_grace_without_kill(self):
        self.install_fake_codex(app_server="slow_term")
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "st1", "--force")
        self.assertEqual((code, receipt["state"]), (0, "PASS"))
        self.assertEqual(receipt["owner"]["stop"], "sigterm")
        self.assertEqual(receipt["anomalies"], [])

    def test_owner_sigkill_after_grace_is_anomaly_and_quarantine(self):
        self.install_fake_codex(app_server="stuck")
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "sk1", "--force")
        self.assertEqual((code, receipt["state"]), (sup.EXIT_QUARANTINED, "QUARANTINED"))
        self.assertEqual(receipt["owner"]["stop"], "sigkill_after_grace")
        self.assertIn("OWNER_SIGKILLED_AFTER_GRACE", [a["code"] for a in receipt["anomalies"]])
        self.assertEqual(json.loads(self.env.quarantine_marker().read_text())["reason"],
                         "OWNER_SIGKILLED_AFTER_GRACE")

    def test_owner_requires_matching_binary_pin(self):
        fake = self.install_fake_codex(app_server="normal")
        self.env.reconfigure(codex_bin_sha256=None)
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "pin1", "--force")
        self.assertEqual((code, receipt["refusal"]["reason"]), (sup.EXIT_USAGE,
                                                                "codex_bin_pin_required_for_owner_and_relogin"))
        self.env.reconfigure(codex_bin_sha256="0" * 64)
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "pin2", "--force")
        self.assertEqual((code, receipt["refusal"]["reason"]), (sup.EXIT_USAGE, "binary_pin_mismatch"))
        self.assertEqual(self.records(self.env.run_dir("_owner", "pin2")), [])
        self.assertTrue(fake.exists())

    def test_owner_waits_for_exclusive_lease(self):
        self.install_fake_codex(app_server="normal")
        ready = self.env.root / "ready-ow"
        holder = self.env.popen("run", "--role", "dev", "--run-id", "ow-hold",
                                *probe("hold", 8, ready, self.env.lease_path))
        self.assertTrue(support.wait_for(ready.exists, 60))
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "ow-busy", "--force", "--wait-seconds", "0")
        self.assertEqual((code, receipt["state"]), (sup.EXIT_AUTH_BUSY, "AUTH_BUSY"))
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "ow-wait", "--force", "--wait-seconds", "30")
        self.assertEqual((code, receipt["state"]), (0, "PASS"))
        holder_receipt = support.last_json(holder.communicate(timeout=30)[0])
        self.assertGreaterEqual(receipt["timings"]["lease_acquired_at"], holder_receipt["timings"]["finished_at"])

    def test_pre_run_refresh_then_consumer(self):
        self.install_fake_codex(app_server="normal")
        out = self.env.root / "env-pre.json"
        code, receipt, proc = self.env.supervisor("run", "--role", "dev", "--run-id", "pre1", "--pre-refresh",
                                                  "always", *probe("env-report", out))
        self.assertEqual((code, receipt["state"]), (0, "PASS"), proc.stderr)
        self.assertEqual(receipt["pre_refresh_result"]["decision"], "REFRESHED")
        owner_receipt = json.loads((self.env.run_dir("_owner", "pre1-pre") / "receipt.json").read_text())
        self.assertEqual(owner_receipt["state"], "PASS")
        self.assertFalse(receipt["canonical_changed"], "the refresh happened before the consumer baseline")

    # -- interrupted owner (LC: signals and a killed supervisor during a refresh) -------------

    def _slow_owner(self, delay):
        started, finished = self.env.root / "refresh-started", self.env.root / "refresh-finished"
        self.install_fake_codex(app_server="slow_refresh", refresh_delay=delay, started=str(started),
                                finished=str(finished))
        self.env.credential.age(13 * 3600)
        return started, finished

    def _signal_during_owner_refresh(self, sig, run_id):
        started, finished = self._slow_owner(4)
        before = self.env.credential.meta()
        proc = self.env.popen("refresh", "--run-id", run_id, "--force")
        self.assertTrue(support.wait_for(started.exists, 60))
        proc.send_signal(sig)
        out, _ = proc.communicate(timeout=120)
        receipt = support.last_json(out)
        self.assertTrue(finished.exists(), "the in-flight refresh was allowed to finish its in-place save")
        self.assertEqual((receipt["state"], receipt["refresh"]["decision"]), ("PASS", "REFRESHED"))
        self.assertEqual(receipt["cancel_requested"], sig.name)
        self.assertEqual([e["decision"] for e in self.env.ledger() if e.get("run_id") == run_id], ["REFRESHED"])
        self.assertNotEqual(self.env.credential.meta()["mtime_ns"], before["mtime_ns"])
        self.assertFalse(self.env.quarantine_marker().exists())
        self.assertEqual(self.env.lease_probe(), "free")
        support.observe("signal_during_owner_refresh", signal=sig.name, recorded=True)

    def test_sigint_during_owner_refresh_is_recorded_not_lost(self):
        self._signal_during_owner_refresh(signal.SIGINT, "oi-int")

    def test_sigterm_during_owner_refresh_is_recorded_not_lost(self):
        self._signal_during_owner_refresh(signal.SIGTERM, "oi-term")

    def test_supervisor_killed_during_owner_refresh_guardian_records_and_quarantines(self):
        started, finished = self._slow_owner(4)
        proc = self.env.popen("refresh", "--run-id", "oi-kill", "--force")
        self.assertTrue(support.wait_for(started.exists, 60))
        proc.send_signal(signal.SIGKILL)
        proc.communicate(timeout=30)
        self.assertNotEqual(self.env.lease_probe(), "free", "the guardian and the owner still hold the lease")
        self.assertTrue(support.wait_for(lambda: self.env.lease_probe() == "free", 90, 0.2))
        self.assertTrue(finished.exists())
        marker = json.loads(self.env.quarantine_marker().read_text())
        self.assertEqual(marker["reason"], "OWNER_REFRESH_UNRECORDED")
        entries = [e for e in self.env.ledger() if e.get("run_id") == "oi-kill"]
        self.assertEqual([e["decision"] for e in entries], ["OWNER_INTERRUPTED"])
        self.assertTrue(entries[0]["canonical_changed"])
        receipt = self.env.receipt("_owner", "oi-kill")
        self.assertTrue(receipt["supervisor_lost"])
        self.assertEqual(support.run_auth_links(self.env), [])
        support.observe("supervisor_killed_during_owner_refresh", quarantine=marker["reason"], ledger="OWNER_INTERRUPTED")

    # -- consumer auth failure contract (F3) ---------------------------------------------------

    def test_consumer_auth_failure_blocks_new_consumers_until_owner_refresh(self):
        self.install_fake_codex(app_server="normal")
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "af1",
                                               *probe("auth-fail", "AUTH_UNAUTHORIZED"))
        self.assertEqual((code, receipt["state"]), (sup.EXIT_BLOCKED_AUTH, "BLOCKED_AUTH"))
        self.assertTrue(receipt["auth_block"]["created"])
        self.assertEqual(receipt["auth_failure_record"],
                         {"present": True, "valid": True, "failure_class": "AUTH_UNAUTHORIZED"})
        marker = json.loads(self.env.auth_block_marker().read_text())
        self.assertEqual(marker["reason"], "CONSUMER_REPORTED_AUTH_FAILURE")
        self.assertEqual(marker["details"]["auth_failure_class"], "AUTH_UNAUTHORIZED")
        ready = self.env.root / "ready-af2"
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "af2", *probe("hold", 0, ready))
        self.assertEqual((code, receipt["refusal"]["reason"]), (sup.EXIT_BLOCKED_AUTH,
                                                                "auth_blocked_pending_owner_refresh"))
        self.assertFalse(ready.exists())
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "af-fail", *probe("exit", 1))
        self.assertEqual(receipt["refusal"]["reason"], "auth_blocked_pending_owner_refresh")
        # The canonical file is fresh, but the auth block makes the owner refresh due.
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "af-own")
        self.assertEqual((code, receipt["refresh"]["decision"]), (0, "REFRESHED"))
        self.assertTrue(receipt["auth_block_cleared"])
        self.assertFalse(self.env.auth_block_marker().exists())
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "af3", *probe("exit", 0))
        self.assertEqual(code, 0)
        support.observe("consumer_auth_failure_contract", first_state="BLOCKED_AUTH", next_refused=True,
                        cleared_by="owner_refresh")

    def test_non_auth_failures_do_not_block(self):
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "na1", *probe("exit", 69))
        self.assertEqual((code, receipt["state"]), (1, "FAILED"))
        self.assertFalse(self.env.auth_block_marker().exists())

    def test_exit_77_alone_is_not_an_auth_failure(self):
        """L5: exit 77 is also sysexits EX_NOPERM. Without the auth-failure record it is FAILED, records
        an anomaly and blocks nobody."""
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "np1", *probe("exit", 77))
        self.assertEqual((code, receipt["state"], receipt["child_exit_code"]), (1, "FAILED", 77))
        self.assertIn("CONSUMER_EXIT_77_WITHOUT_AUTH_FAILURE_RECORD", [a["code"] for a in receipt["anomalies"]])
        self.assertEqual(receipt["auth_failure_record"], {"present": False, "valid": False})
        self.assertNotIn("auth_block", receipt)
        self.assertFalse(self.env.auth_block_marker().exists())
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "np2", *probe("exit", 0))
        self.assertEqual((code, receipt["state"]), (0, "PASS"), "the next consumer is not blocked")
        support.observe("exit_77_without_record", state="FAILED", blocked=False)

    def test_unrecognised_auth_failure_records_do_not_block(self):
        """L5: only this uid's private, single-link regular file with the schema and a known class counts,
        and only together with exit 77."""
        valid = json.dumps({"schema": sup.AUTH_FAILURE_SCHEMA, "class": "AUTH_UNAUTHORIZED"})
        elsewhere = self.env.root / "record-elsewhere.json"
        elsewhere.write_text(valid)
        cases = [("ar1", probe("auth-record", json.dumps({"schema": sup.AUTH_FAILURE_SCHEMA, "class": "EX_NOPERM"}),
                               77), "schema_or_class"),
                 ("ar2", probe("auth-record", json.dumps({"schema": "other", "class": "AUTH_UNAUTHORIZED"}), 77),
                  "schema_or_class"),
                 ("ar3", probe("auth-record", "not json", 77), "invalid_json"),
                 ("ar4", probe("auth-record", valid + " " * sup.AUTH_FAILURE_RECORD_MAX_BYTES, 77),
                  "unsafe_file"),
                 ("ar5", probe("auth-record-symlink", elsewhere, 77), "unsafe_file")]
        for run_id, args, rejected in cases:
            with self.subTest(run_id=run_id):
                code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", run_id, *args)
                self.assertEqual((code, receipt["state"]), (1, "FAILED"))
                self.assertEqual(receipt["auth_failure_record"], {"present": True, "valid": False,
                                                                  "rejected": rejected})
                self.assertFalse(self.env.auth_block_marker().exists())
        # A valid record without exit 77 is not read at all.
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "ar6", *probe("auth-record", valid, 0))
        self.assertEqual((code, receipt["state"]), (0, "PASS"))
        self.assertNotIn("auth_failure_record", receipt)
        self.assertFalse(self.env.auth_block_marker().exists())
        support.observe("auth_failure_records_rejected", cases=len(cases) + 1)

    def test_auth_block_can_be_cleared_by_a_human(self):
        self.env.supervisor("run", "--role", "dev", "--run-id", "ab1", *probe("auth-fail", "AUTH_NOT_LOGGED_IN"))
        self.assertTrue(self.env.auth_block_marker().exists())
        code, receipt, _ = self.env.supervisor("clear-quarantine", "--auth-block", "--run-id", "ab-clear", "--confirm",
                                               "--reason", "fake drill: consumer misreported")
        self.assertEqual(code, 0)
        self.assertFalse(self.env.auth_block_marker().exists())
        self.assertTrue(Path(receipt["archived_marker"]).exists())
        self.assertIn("AUTH_BLOCK_CLEARED", [e["decision"] for e in self.env.ledger()])


class ReloginFakeTests(FakeCodexMixin, unittest.TestCase):
    def setUp(self):
        self.env = Env("relogin")
        self.env.credential.write(1, time.time() + 3600, in_place=False)

    def permanent_quarantine(self):
        sup.write_quarantine(self.env.cfg(), "AUTH_REFRESH_FAILED", "fake-owner", {
            "operation": "refresh", "owner_log_failure_class": "REFRESH_TOKEN_REUSED", "failure_kind": "permanent"})

    def test_relogin_requires_explicit_confirmation(self):
        self.install_fake_codex()
        code, receipt, _ = self.env.supervisor("relogin", "--run-id", "rl0")
        self.assertEqual((code, receipt["refusal"]["reason"]), (sup.EXIT_USAGE, "relogin_requires_confirm_flag"))
        self.assertFalse(self.env.run_dir("_relogin", "rl0").exists())

    def test_relogin_is_refused_while_the_credential_is_usable(self):
        self.install_fake_codex()
        before = self.env.credential.meta()
        code, receipt, _ = self.env.supervisor("relogin", "--run-id", "rl-usable", "--confirm-relogin")
        self.assertEqual((code, receipt["state"]), (sup.EXIT_USAGE, "RELOGIN_NOT_REQUIRED"))
        self.assertEqual(receipt["refusal"]["reason"], "relogin_not_required_credential_usable")
        self.assertEqual(receipt["gate"]["login_status_exit_code"], 0)
        self.assertEqual(self.env.credential.meta(), before)
        records = self.records(self.env.run_dir("_relogin", "rl-usable"))
        self.assertEqual([r["kind"] for r in records], ["status"], "no device login was started")
        self.assertFalse(os.path.lexists(self.env.run_dir("_relogin", "rl-usable") / "verify-home" / "auth.json"))
        support.observe("relogin_refused_usable_credential", state=receipt["state"])

    def test_relogin_after_permanent_failure_stages_and_installs_atomically(self):
        self.install_fake_codex(login_sleep=8.0)
        self.permanent_quarantine()
        sup.write_auth_block(self.env.cfg(), "CONSUMER_REPORTED_AUTH_FAILURE", "fake-consumer", {})
        before = self.env.credential.meta()
        proc = self.env.popen("relogin", "--run-id", "rl1", "--confirm-relogin",
                              env=self.env.parent_env(CODEX_API_KEY="FAKE-CANARY-relogin"))
        self.assertTrue(support.wait_for(lambda: self.env.sidecar("_relogin", "rl1").exists(), 60))
        self.assertEqual(self.env.lease_probe(), "held_exclusive", "re-login holds the exclusive maintenance lease")
        code, busy, _ = self.env.supervisor("clear-quarantine", "--run-id", "rl-busy", "--confirm", "--reason",
                                            "fake drill", "--wait-seconds", "0")
        self.assertEqual(code, sup.EXIT_AUTH_BUSY)
        self.assertEqual(self.env.credential.meta(), before, "the existing login is untouched while the code is shown")
        out, err = proc.communicate(timeout=60)
        receipt = support.last_json(out)
        self.assertEqual((proc.returncode, receipt["state"]), (0, "PASS"), err)
        self.assertIn("FAKE-DEVICE-CODE", out, "device prompt reaches the terminal")
        self.assertIn("empty staging home", err, "the operator is told how the official login behaves")
        self.assertEqual((receipt["login_exit_code"], receipt["status_exit_code"], receipt["installed"]), (0, 0, True))
        self.assertTrue(receipt["canonical_changed"])
        after = self.env.credential.meta()
        self.assertNotEqual(after["ino"], before["ino"], "installed by rename")
        self.assertEqual(after["mode"], 0o600)
        self.assertFalse(os.path.lexists(self.env.run_dir("_relogin", "rl1") / "codex-home" / "auth.json"))
        records = self.records(self.env.run_dir("_relogin", "rl1"))
        login = [r for r in records if r["kind"] == "login"]
        self.assertEqual(len(login), 1)
        login = login[0]
        self.assertFalse(login["cleared_existing_auth"], "the staging home had nothing to revoke or delete")
        self.assertEqual(login["codex_home"], str(self.env.run_dir("_relogin", "rl1") / "codex-home"))
        self.assertNotEqual(login["codex_home"], str(self.env.credential.home))
        pinned = ["-c", 'cli_auth_credentials_store="file"', "-c", 'forced_login_method="chatgpt"']
        self.assertEqual(login["argv"][:4], pinned)
        self.assertEqual(login["argv"][-2:], ["login", "--device-auth"])
        self.assertEqual(login["refresh_override"], sup.DEFAULT_BLOCKER_URL)
        self.assertTrue(login["revoke_override"].endswith(":0/oauth/revoke"))
        self.assertNotIn("CODEX_API_KEY", login["env_names"])
        self.assertTrue(receipt["auth_block_cleared"])
        self.assertTrue(self.env.quarantine_marker().exists(), "the quarantine is cleared by a human after review")
        hits = support.scan_for_leaks([self.env.runs_root, *support.lease_side_files(self.env)],
                                      ["FAKE-DEVICE-CODE", "FAKE-RELOGIN-RT", "FAKE-CANARY"],
                                      skip=[self.env.credential.home])
        self.assertEqual(hits, [])
        self.assertEqual(self.env.ledger()[-1]["decision"], "RELOGIN_PASS")
        support.observe("relogin_staged_install_fake", inode_changed=True, existing_login_cleared_by_cli=False)

    def test_declined_relogin_leaves_the_existing_login_untouched(self):
        self.install_fake_codex(login_exit=1)
        self.permanent_quarantine()
        before = self.env.credential.meta()
        code, receipt, _ = self.env.supervisor("relogin", "--run-id", "rl2", "--confirm-relogin")
        self.assertEqual((code, receipt["state"], receipt["login_exit_code"]), (sup.EXIT_BLOCKED_AUTH,
                                                                                 "BLOCKED_AUTH", 1))
        self.assertEqual(self.env.credential.meta(), before, "decline/expiry never revokes or deletes the login")
        self.assertFalse(receipt["installed"])
        login = [r for r in self.records(self.env.run_dir("_relogin", "rl2")) if r["kind"] == "login"][0]
        self.assertFalse(login["cleared_existing_auth"])
        support.observe("relogin_declined_fake", canonical_unchanged=True)

    def test_relogin_for_an_empty_credential_and_unverified_login_is_discarded(self):
        self.install_fake_codex(status_exit=1)
        os.truncate(self.env.credential.path, 0)
        before = self.env.credential.meta()
        code, receipt, _ = self.env.supervisor("relogin", "--run-id", "rl3", "--confirm-relogin")
        self.assertEqual(receipt["gate"]["basis"], ["canonical_auth_empty"])
        self.assertEqual((code, receipt["state"]), (sup.EXIT_BLOCKED_AUTH, "BLOCKED_AUTH"))
        self.assertTrue(receipt["staging_credential_discarded"], "an unverified new login is not left behind")
        self.assertEqual(self.env.credential.meta(), before)
        self.assertFalse(os.path.lexists(self.env.run_dir("_relogin", "rl3") / "codex-home" / "auth.json"))


if __name__ == "__main__":
    unittest.main()
