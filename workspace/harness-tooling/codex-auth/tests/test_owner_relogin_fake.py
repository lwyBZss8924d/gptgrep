"""Owner refresh and re-login logic against a FAKE codex binary.

Covers the owner's stop sequence (stdin close, SIGTERM grace, SIGKILL anomaly),
refresh policy, AUTH_REFRESH_FAILED quarantine, binary pinning, pre-run refresh
and the maintenance re-login path. The real CLI is never used here.
"""
import json
import os
from pathlib import Path
import time
import unittest

import support
from support import PY, FIXTURES, Env, sup

PROBE = str(FIXTURES / "consumer_probe.py")


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
        self.assertEqual(receipt["auth_link_after"], {"present": True, "symlink": True, "target_matches": True})
        record = self.records(self.env.run_dir("_owner", "own1"))[0]
        self.assertIsNone(record["refresh_override"], "owner must not carry the consumer blocker")
        for name in sup.STRIPPED_ALWAYS:
            self.assertNotIn(name, record["env_names"])
        self.assertEqual(record["argv"], ["app-server", "--listen", "stdio://", "--strict-config"])
        entry = self.env.ledger()[-1]
        self.assertEqual((entry["decision"], entry["canonical_changed"], entry["inode_preserved"]),
                         ("REFRESHED", True, True))
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "own2")
        self.assertEqual((code, receipt["refresh"]["decision"]), (0, "SKIPPED_FRESH"))
        self.assertEqual(self.records(self.env.run_dir("_owner", "own2")), [], "fresh: no app-server launch")

    def test_unchanged_canonical_is_auth_refresh_failed_and_blocks_consumers(self):
        self.install_fake_codex(app_server="no_write")
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "nw1", "--force")
        self.assertEqual((code, receipt["state"], receipt["refresh"]["decision"]),
                         (sup.EXIT_BLOCKED_AUTH, "BLOCKED_AUTH", "AUTH_REFRESH_FAILED"))
        self.assertTrue(self.env.quarantine_marker().exists())
        ready = self.env.root / "ready-nw"
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "nw-run", "--",
                                               PY, "-I", "-B", PROBE, "hold", "0", str(ready))
        self.assertEqual((code, receipt["state"]), (sup.EXIT_QUARANTINED, "QUARANTINED"))
        self.assertFalse(ready.exists())
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "nw2", "--force")
        self.assertEqual(code, sup.EXIT_QUARANTINED, "no retry loop while quarantined")
        code, _, _ = self.env.supervisor("clear-quarantine", "--run-id", "nwc", "--confirm", "--reason", "fake drill")
        self.assertEqual(code, 0)

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
        holder = self.env.popen("run", "--role", "dev", "--run-id", "ow-hold", "--",
                                PY, "-I", "-B", PROBE, "hold", "8", str(ready), str(self.env.lease_path))
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
                                                  "always", "--", PY, "-I", "-B", PROBE, "env-report", str(out))
        self.assertEqual((code, receipt["state"]), (0, "PASS"), proc.stderr)
        self.assertEqual(receipt["pre_refresh_result"]["decision"], "REFRESHED")
        owner_receipt = json.loads((self.env.run_dir("_owner", "pre1-pre") / "receipt.json").read_text())
        self.assertEqual(owner_receipt["state"], "PASS")
        self.assertFalse(receipt["canonical_changed"], "the refresh happened before the consumer baseline")


class ReloginFakeTests(FakeCodexMixin, unittest.TestCase):
    def setUp(self):
        self.env = Env("relogin")
        self.env.credential.write(1, time.time() + 3600, in_place=False)

    def test_relogin_requires_explicit_confirmation(self):
        self.install_fake_codex()
        code, receipt, _ = self.env.supervisor("relogin", "--run-id", "rl0")
        self.assertEqual((code, receipt["refusal"]["reason"]), (sup.EXIT_USAGE, "relogin_requires_confirm_flag"))
        self.assertFalse(self.env.run_dir("_relogin", "rl0").exists())

    def test_relogin_runs_official_shape_under_exclusive_lease_and_records_exit_codes_only(self):
        self.install_fake_codex(login_sleep=8.0)
        before = self.env.credential.meta()
        proc = self.env.popen("relogin", "--run-id", "rl1", "--confirm-relogin",
                              env=self.env.parent_env(CODEX_API_KEY="FAKE-CANARY-relogin"))
        self.assertTrue(support.wait_for(lambda: self.env.sidecar("_relogin", "rl1").exists(), 60))
        code, busy, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "rl-busy", "--",
                                            PY, "-I", "-B", PROBE, "exit", "0")
        self.assertEqual(code, sup.EXIT_AUTH_BUSY, "re-login holds the exclusive maintenance lease")
        out, err = proc.communicate(timeout=60)
        receipt = support.last_json(out)
        self.assertEqual((proc.returncode, receipt["state"]), (0, "PASS"), err)
        self.assertIn("FAKE-DEVICE-CODE", out, "device prompt reaches the terminal")
        self.assertEqual((receipt["login_exit_code"], receipt["status_exit_code"]), (0, 0))
        self.assertTrue(receipt["canonical_changed"])
        self.assertNotEqual(self.env.credential.meta()["ino"], before["ino"])
        records = self.records(self.env.run_dir("_relogin", "rl1"))
        self.assertEqual([r["kind"] for r in records], ["login", "status"])
        login, status = records
        pinned = ["-c", 'cli_auth_credentials_store="file"', "-c", 'forced_login_method="chatgpt"']
        self.assertEqual(login["argv"][:4], pinned)
        self.assertEqual(login["argv"][-2:], ["login", "--device-auth"])
        self.assertEqual(status["argv"][-2:], ["login", "status"])
        self.assertEqual(login["codex_home"], str(self.env.credential.home))
        self.assertIsNone(login["refresh_override"])
        self.assertNotIn("CODEX_API_KEY", login["env_names"])
        hits = support.scan_for_leaks([self.env.runs_root, *support.lease_side_files(self.env)],
                                      ["FAKE-DEVICE-CODE", "FAKE-RELOGIN-RT", "FAKE-CANARY"],
                                      skip=[self.env.credential.home])
        self.assertEqual(hits, [])
        self.assertEqual(self.env.ledger()[-1]["decision"], "RELOGIN_PASS")

    def test_failed_relogin_is_blocked_auth(self):
        self.install_fake_codex(login_exit=1)
        code, receipt, _ = self.env.supervisor("relogin", "--run-id", "rl2", "--confirm-relogin")
        self.assertEqual((code, receipt["state"], receipt["login_exit_code"]), (sup.EXIT_BLOCKED_AUTH,
                                                                                 "BLOCKED_AUTH", 1))


if __name__ == "__main__":
    unittest.main()
