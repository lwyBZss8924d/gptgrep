"""Qualification against the official Codex binary with FAKE credentials only.

Skipped unless GPTGREP_AUTH_TEST_CODEX_BIN names the official 0.162.1 CLI.
Every endpoint is a loopback fake: CODEX_REFRESH_TOKEN_URL_OVERRIDE (owner ->
fake authority, consumers -> blocker/trap), chatgpt_base_url and
openai_base_url (-> fake backend). The runner additionally wraps the whole
suite in a seatbelt profile allowing only its reserved loopback ports.

(c) owner refresh via symlink; (d) consumer cannot refresh; (e) consumer adopts
an owner write; (f) 401 fails closed; (g) concurrency; (i) CODEX_API_KEY never
reaches consumers; (j) leak canaries (checked after every test).
"""
import json
import os
from pathlib import Path
import subprocess
import time
import unittest

import support
from support import PY, FIXTURES, TOOL_DIR, Env, sup

RPC = str(FIXTURES / "rpc_client.py")
_PIN = {}


def codex_pin():
    if support.CODEX_SHA256:
        return support.CODEX_SHA256
    if "sha" not in _PIN:
        _PIN["sha"] = support.sha256_file(support.CODEX_BIN)
    return _PIN["sha"]


def initialize():
    return [{"op": "request", "method": "initialize",
             "params": {"clientInfo": {"name": "fake_consumer", "title": None, "version": "0"}}},
            {"op": "notify", "method": "initialized"}]


def read(refresh=False, timeout=60):
    return {"op": "request", "method": "account/read", "params": {"refreshToken": True} if refresh else {},
            "timeout": timeout}


def thread_start(cwd):
    return {"op": "request", "method": "thread/start", "params": {"cwd": str(cwd), "ephemeral": True},
            "timeout": 60}


@unittest.skipUnless(support.CODEX_BIN, "set GPTGREP_AUTH_TEST_CODEX_BIN to the official codex 0.162.1 binary")
class CodexIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.env = Env("codex")
        self.authority = support.FakeAuthority(self.env.credential)
        self.backend = support.FakeBackend()
        self.trap = support.Trap()
        self.env.reconfigure(
            codex_bin=support.CODEX_BIN, codex_bin_sha256=codex_pin(),
            codex_config={"chatgpt_base_url": self.backend.chatgpt_base_url,
                          "openai_base_url": self.backend.openai_base_url},
            owner={"refresh_token_url_override": self.authority.url, "rpc_timeout_seconds": 60,
                   "exit_wait_seconds": 30, "term_grace_seconds": 60},
            blocker_url=self.trap.url, blocker_must_be_closed=False)

    def tearDown(self):
        try:
            scanned = []
            needles = sorted(self.env.credential.canaries()) + ["FAKE-CANARY"]
            hits = support.scan_for_leaks([self.env.runs_root, *support.lease_side_files(self.env)], needles,
                                          skip=[self.env.credential.home], counter=scanned)
            support.observe("leak_scan", test=self.id().split(".")[-1], files_scanned=len(scanned),
                            canaries=len(needles), hits=len(hits))
            self.assertTrue(scanned, "leak scan must not be vacuous")
            self.assertEqual(hits, [], "fake credential canaries leaked into supervisor/run files")
        finally:
            self.authority.stop()
            self.backend.stop()
            self.trap.stop()

    # -- helpers -----------------------------------------------------------------------------

    def consume(self, run_id, steps, role="dev", extra=(), env=None, popen=False):
        steps_path = self.env.root / ("steps-%s.json" % run_id)
        steps_path.write_text(json.dumps(steps))
        args = ["run", "--role", role, "--run-id", run_id, *extra, "--", PY, "-I", "-B", RPC, str(TOOL_DIR),
                support.CODEX_BIN, str(steps_path), "{results}/rpc.json", "{log_dir}/app-server.stderr.log"]
        if popen:
            return self.env.popen(*args, env=env)
        code, receipt, proc = self.env.supervisor(*args, env=env)
        return code, receipt, self.rpc(role, run_id), proc

    def rpc(self, role, run_id):
        path = self.env.run_dir(role, run_id) / "results" / "rpc.json"
        return json.loads(path.read_text()) if path.exists() else None

    def reads(self, rpc):
        return [r for r in rpc["results"] if r.get("method") == "account/read"]

    def bearers_since(self, since, path_suffix="/accounts/check"):
        return [e["bearer"] for e in self.backend.requests() if e["t"] >= since and e["path"].endswith(path_suffix)]

    def label(self, generation):
        return "FAKE-AT-%s-%d" % (self.env.credential.nonce, generation)

    # -- (c) owner ----------------------------------------------------------------------------

    def test_c_owner_refresh_rewrites_canonical_in_place_through_symlink(self):
        self.env.credential.write(1, time.time() + 3600, in_place=False)
        self.env.credential.age(13 * 3600)
        before = self.env.credential.meta()
        code, receipt, proc = self.env.supervisor("refresh", "--run-id", "own1")
        self.assertEqual((code, receipt["state"], receipt["refresh"]["decision"]), (0, "PASS", "REFRESHED"),
                         proc.stderr[-2000:])
        requests = self.authority.requests()
        self.assertEqual(len(requests), 1, "exactly one token request")
        self.assertEqual(requests[0]["grant_type"], "refresh_token")
        self.assertEqual(requests[0]["refresh_token"], self.env.credential.issued[0]["refresh_token"])
        after = self.env.credential.meta()
        self.assertEqual(after["ino"], before["ino"], "in-place rewrite preserves the inode")
        self.assertTrue(after["regular"])
        self.assertEqual(after["mode"], 0o600)
        self.assertNotEqual((after["mtime_ns"], after["size"]), (before["mtime_ns"], before["size"]))
        link = self.env.run_dir("_owner", "own1") / "codex-home" / "auth.json"
        self.assertTrue(link.is_symlink())
        self.assertEqual(os.readlink(link), str(self.env.credential.path))
        self.assertEqual(receipt["owner"]["rpc"][2]["account_type"], "chatgpt")
        self.assertEqual(receipt["owner"]["stop"], "stdin_closed")
        entry = self.env.ledger()[-1]
        self.assertEqual((entry["decision"], entry["canonical_changed"], entry["inode_preserved"]),
                         ("REFRESHED", True, True))
        self.assertIn(self.label(101), self.bearers_since(0), "owner's follow-up read used the new token")
        support.observe("c_owner_refresh", authority_requests=len(requests), inode_preserved=True,
                        mode=oct(after["mode"]), size_changed=after["size"] != before["size"],
                        owner_stop=receipt["owner"]["stop"], link_intact=True)
        code, receipt, _ = self.env.supervisor("refresh", "--run-id", "own2")
        self.assertEqual((code, receipt["refresh"]["decision"]), (0, "SKIPPED_FRESH"))
        self.assertEqual(len(self.authority.requests()), 1)
        support.observe("c_owner_policy_skip", decision="SKIPPED_FRESH", authority_requests_total=1)

    def test_c_owner_with_expiring_token_refreshes_and_rotation_is_recorded(self):
        self.env.credential.write(1, time.time() + 120, in_place=False)
        before = self.env.credential.meta()
        code, receipt, proc = self.env.supervisor("refresh", "--run-id", "own3", "--force")
        self.assertEqual((code, receipt["state"]), (0, "PASS"), proc.stderr[-2000:])
        count = len(self.authority.requests())
        # Startup proactive refresh (exp <= now+5min) plus the explicit refreshToken=true.
        self.assertIn(count, (1, 2))
        self.assertEqual(self.env.credential.meta()["ino"], before["ino"])
        support.observe("c_owner_expiring_token", authority_requests=count)

    # -- (d) consumers cannot refresh -----------------------------------------------------------

    def test_d_consumer_with_expiring_token_never_reaches_authority(self):
        self.env.credential.write(1, time.time() + 120, in_place=False)
        before = self.env.credential.meta()
        work = self.env.run_dir("dev", "blk1") / "work"
        code, receipt, rpc, proc = self.consume("blk1", initialize() + [read(True), thread_start(work), read()])
        self.assertEqual((code, receipt["state"]), (0, "PASS"), proc.stderr[-2000:])
        self.assertEqual(self.authority.requests(), [], "consumers must not contact the token authority")
        self.assertGreaterEqual(self.trap.count, 1, "the consumer did try to refresh and was blocked")
        self.assertEqual(self.env.credential.meta(), before)
        self.assertFalse(receipt["canonical_changed"])
        reads = self.reads(rpc)
        self.assertTrue(all(r["ok"] and r["account_type"] == "chatgpt" for r in reads), reads)
        starts = [r for r in rpc["results"] if r.get("method") == "thread/start"]
        self.assertTrue(starts and starts[0]["ok"] and starts[0]["thread_started"], starts)
        self.assertEqual(receipt["auth_link_after"], {"present": True, "symlink": True, "target_matches": True})
        self.assertIn("CODEX_REFRESH_TOKEN_URL_OVERRIDE", receipt["env_keys"])
        support.observe("d_consumer_blocked", authority_requests=0, blocked_attempts_at_trap=self.trap.count,
                        canonical_unchanged=True, thread_start_ok=True)

    def test_d_production_closed_port_blocker(self):
        self.env.reconfigure(blocker_url=sup.DEFAULT_BLOCKER_URL, blocker_must_be_closed=True)
        self.env.credential.write(1, time.time() + 120, in_place=False)
        before = self.env.credential.meta()
        code, receipt, rpc, proc = self.consume("blk2", initialize() + [read(True), read()])
        self.assertEqual((code, receipt["state"], receipt["blocker_probe"]), (0, "PASS", "closed"),
                         proc.stderr[-2000:])
        self.assertEqual(self.authority.requests(), [])
        self.assertEqual(self.env.credential.meta(), before)
        self.assertTrue(all(r["ok"] for r in self.reads(rpc)))
        support.observe("d_closed_port_blocker", authority_requests=0, blocker_probe=receipt["blocker_probe"],
                        canonical_unchanged=True)

    def test_d_account_probe_rehearses_first_live_step(self):
        """The runbook's first live step, against fakes: consumer, blocker, no refresh, no model call."""
        self.env.credential.write(1, time.time() + 120, in_place=False)
        before = self.env.credential.meta()
        code, receipt, proc = self.env.supervisor(
            "run", "--role", "dev", "--run-id", "probe1", "--live-request-attempted", "no", "--deadline-seconds", "120",
            "--", PY, "-I", "-B", str(support.SUPERVISOR), "account-probe", "--codex-bin", support.CODEX_BIN,
            "--out", "{results}/account-probe.json", "--stderr-log", "{log_dir}/account-probe.stderr.log")
        self.assertEqual((code, receipt["state"]), (0, "PASS"), proc.stderr[-2000:])
        result = json.loads((self.env.run_dir("dev", "probe1") / "results" / "account-probe.json").read_text())
        self.assertTrue(result["account_ok"])
        self.assertFalse(result["refresh_requested"])
        self.assertEqual(self.authority.requests(), [])
        self.assertEqual(self.env.credential.meta(), before)
        self.assertFalse(receipt["live_request_attempted"])
        self.assertEqual([e["method"] for e in self.backend.requests() if e["method"] == "POST"], [],
                         "no model request was sent")
        support.observe("d_account_probe_rehearsal", account_ok=True, authority_requests=0,
                        blocked_attempts=self.trap.count, model_posts=0)

    # -- (e) consumers adopt the owner's write ------------------------------------------------

    def test_e_new_consumer_adopts_owner_refresh_without_authority(self):
        self.env.credential.write(1, time.time() + 120, in_place=False)
        code, receipt, rpc, _ = self.consume("adopt-a", initialize() + [read(True)])
        self.assertEqual((code, len(self.authority.requests())), (0, 0))
        code, owner, _ = self.env.supervisor("refresh", "--run-id", "adopt-own", "--force")
        self.assertEqual((code, owner["refresh"]["decision"]), (0, "REFRESHED"))
        owner_requests = len(self.authority.requests())
        self.assertIn(owner_requests, (1, 2))
        newest = self.env.credential.issued[-1]["access_token"]
        since = time.time()
        code, receipt, rpc, _ = self.consume("adopt-b", initialize() + [read(True), read()])
        self.assertEqual(code, 0)
        self.assertEqual(len(self.authority.requests()), owner_requests, "consumer B made no token request")
        bearers = self.bearers_since(since)
        self.assertTrue(bearers)
        self.assertTrue(all(b == support.jwt_label(newest) for b in bearers), bearers)
        support.observe("e_adopt_after_owner", owner_authority_requests=owner_requests,
                        consumer_b_authority_requests=0, consumer_b_backend_requests_with_new_token=len(bearers))

    def test_e_running_consumer_guarded_reload_picks_up_in_place_write(self):
        self.env.credential.write(1, time.time() + 3600, in_place=False)
        ready, rewritten = self.env.root / "e-ready", self.env.root / "e-rewritten"
        proc = self.consume("adopt-live", initialize() + [
            read(), {"op": "touch", "path": str(ready)},
            {"op": "wait_file", "path": str(rewritten), "timeout": 60}, read(True), read()], popen=True)
        self.assertTrue(support.wait_for(ready.exists, 60))
        ino = self.env.credential.meta()["ino"]
        since = time.time()
        # Simulate the owner's official in-place save (same inode, truncate + write).
        self.env.credential.write(2, time.time() + 3600, in_place=True)
        self.assertEqual(self.env.credential.meta()["ino"], ino)
        rewritten.touch()
        out, err = proc.communicate(timeout=180)
        receipt = support.last_json(out)
        self.assertEqual(self.authority.requests(), [], "guarded reload adopted the file; no authority call")
        self.assertEqual(self.trap.count, 0, "nothing even attempted a refresh")
        bearers = self.bearers_since(since)
        self.assertTrue(bearers and all(b == self.label(2) for b in bearers), bearers)
        self.assertTrue(all(r["ok"] for r in self.reads(self.rpc("dev", "adopt-live"))))
        # The supervisor cannot attribute a write made during a consumer run: it must quarantine.
        self.assertEqual(receipt["state"], "QUARANTINED")
        self.assertIn("CANONICAL_CHANGED_BY_CONSUMER", [a["code"] for a in receipt["anomalies"]])
        support.observe("e_adopt_live_in_place_write", authority_requests=0, blocked_attempts=self.trap.count,
                        backend_requests_with_new_token=len(bearers), supervisor_state=receipt["state"])
        code, _, _ = self.env.supervisor("clear-quarantine", "--run-id", "e-clear", "--confirm", "--reason",
                                         "fake drill: simulated owner write")
        self.assertEqual(code, 0)

    # -- (f) 401 fails closed -----------------------------------------------------------------

    def test_f_unauthorized_without_owner_refresh_fails_closed(self):
        self.backend.mode = "unauthorized"
        self.env.credential.write(1, time.time() + 3600, in_place=False)
        before = self.env.credential.meta()
        code, receipt, rpc, proc = self.consume("u401", initialize() + [read()])
        self.assertEqual(code, 0, proc.stderr[-2000:])
        self.assertEqual(self.authority.requests(), [], "401 recovery must not reach the authority")
        self.assertGreaterEqual(self.trap.count, 1, "UnauthorizedRecovery step 2 was blocked")
        result = self.reads(rpc)[0]
        self.assertFalse(result["ok"])
        self.assertIn("unauthorized", (result.get("error_message") or "").lower())
        self.assertEqual(self.env.credential.meta(), before)
        support.observe("f_401_fail_closed", authority_requests=0, blocked_attempts=self.trap.count,
                        error_message=result.get("error_message"), canonical_unchanged=True)

    # -- (g) concurrency ----------------------------------------------------------------------

    def test_g_four_shared_consumers_then_owner_waits_for_exclusive(self):
        self.env.reconfigure(allow_shared_consumers=True)
        self.env.credential.write(1, time.time() + 120, in_place=False)
        procs = [self.consume("g%d" % i, initialize() + [read(True), {"op": "sleep", "seconds": 10}, read()],
                              extra=("--lease", "shared"), popen=True) for i in range(4)]
        self.assertTrue(support.wait_for(lambda: all(self.env.sidecar("dev", "g%d" % i).exists() for i in range(4)),
                                         120))
        owner = self.env.popen("refresh", "--run-id", "g-own", "--force", "--wait-seconds", "300")
        receipts = [support.last_json(p.communicate(timeout=300)[0]) for p in procs]
        owner_receipt = support.last_json(owner.communicate(timeout=300)[0])
        self.assertEqual([r["state"] for r in receipts], ["PASS"] * 4)
        self.assertLess(max(r["timings"]["lease_acquired_at"] for r in receipts),
                        min(r["timings"]["child_exited_at"] for r in receipts), "shared holders overlapped")
        self.assertEqual(owner_receipt["state"], "PASS")
        self.assertGreaterEqual(owner_receipt["timings"]["lease_acquired_at"],
                                max(r["timings"]["finished_at"] for r in receipts))
        acquired = sup._dt.datetime.fromisoformat(owner_receipt["timings"]["lease_acquired_at"]).timestamp()
        requests = self.authority.requests()
        self.assertIn(len(requests), (1, 2))
        self.assertTrue(all(r["t"] >= acquired for r in requests), "no token request before the owner held EX")
        self.assertGreaterEqual(self.trap.count, 4, "each shared consumer tried and was blocked")
        support.observe("g_shared_consumers_then_owner", consumers=4, consumer_states=[r["state"] for r in receipts],
                        authority_requests=len(requests), authority_requests_before_owner_lease=0,
                        blocked_attempts=self.trap.count)

    def test_g_exclusive_consumers_serialize(self):
        self.env.credential.write(1, time.time() + 3600, in_place=False)
        first = self.consume("x1", initialize() + [read(), {"op": "sleep", "seconds": 10}], popen=True)
        self.assertTrue(support.wait_for(lambda: self.env.sidecar("dev", "x1").exists(), 60))
        code, receipt, _, _ = self.consume("x2", initialize() + [read()])
        self.assertEqual((code, receipt["state"]), (sup.EXIT_AUTH_BUSY, "AUTH_BUSY"))
        self.assertEqual(support.last_json(first.communicate(timeout=120)[0])["state"], "PASS")

    # -- (i) CODEX_API_KEY ---------------------------------------------------------------------

    def test_i_codex_api_key_never_reaches_consumer_exec(self):
        self.env.credential.write(1, time.time() + 3600, in_place=False)
        before = self.env.credential.meta()
        env = self.env.parent_env(CODEX_API_KEY="FAKE-CANARY-codex-api-key", OPENAI_API_KEY="FAKE-CANARY-openai")
        # The login check runs at exec startup (the negative control shows it acts within about a
        # second). Afterwards exec keeps retrying an unreachable fake model origin, so the
        # supervisor's deadline ends it; TIMEOUT is an accepted outcome here.
        code, receipt, proc = self.env.supervisor(
            "run", "--role", "dev", "--run-id", "api1", "--deadline-seconds", "20", "--",
            support.CODEX_BIN, "exec", "--skip-git-repo-check", "--ephemeral", "synthetic prompt for a fake backend",
            env=env)
        self.assertIn(receipt["state"], ("PASS", "FAILED", "TIMEOUT"), proc.stderr[-2000:])
        self.assertEqual(receipt["sweep"]["survivor_pids"], [])
        self.assertNotIn("CODEX_API_KEY", receipt["env_keys"])
        self.assertNotIn("OPENAI_API_KEY", receipt["env_keys"])
        self.assertEqual(receipt["auth_link_after"], {"present": True, "symlink": True, "target_matches": True})
        self.assertEqual(self.env.credential.meta(), before)
        stderr = (self.env.run_dir("dev", "api1") / "state" / "logs" / "child.stderr.log").read_text()
        self.assertNotIn("Logging out", stderr)
        self.assertEqual(self.authority.requests(), [])
        support.observe("i_api_key_stripped", state=receipt["state"], link_intact=True, logout_message=False)

    def test_i_negative_control_api_key_logout_only_unlinks_run_symlink(self):
        """Outside the supervisor: why the key must be stripped, and what the symlink limits."""
        self.env.credential.write(1, time.time() + 3600, in_place=False)
        before = self.env.credential.meta()
        cfg = self.env.cfg()
        layout = sup.Layout(cfg, "dev", "api-neg")
        layout.create(cfg, {})
        env, _ = sup.build_consumer_env(cfg, "dev", layout, {})
        env["CODEX_API_KEY"] = "FAKE-CANARY-negative-control"
        proc = subprocess.run([support.CODEX_BIN, "exec", "--skip-git-repo-check", "--ephemeral", "synthetic"],
                              env=env, cwd=str(layout.work_dir), capture_output=True, text=True, timeout=120)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("Logging out", proc.stderr)
        self.assertFalse(os.path.lexists(layout.auth_link), "forced logout removed the run's auth.json link")
        self.assertEqual(self.env.credential.meta(), before, "the canonical credential survived")
        support.observe("i_negative_control_unsupervised", exit_code=proc.returncode, run_link_removed=True,
                        canonical_unchanged=True)
        self.assertEqual(self.authority.requests(), [])


if __name__ == "__main__":
    unittest.main()
