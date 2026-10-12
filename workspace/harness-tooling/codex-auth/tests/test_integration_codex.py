"""Qualification against the official Codex binary with FAKE credentials only.

Skipped unless GPTGREP_AUTH_TEST_CODEX_BIN names the official 0.162.1 CLI.
Every endpoint is a loopback fake: CODEX_REFRESH_TOKEN_URL_OVERRIDE (owner ->
fake authority, consumers -> blocker/trap), chatgpt_base_url and
openai_base_url (-> fake backend), the device-code issuer (-> fake issuer). The
runner additionally wraps the whole suite in a seatbelt profile allowing only
its reserved loopback ports.

(c) owner refresh via symlink and owner failure classes; (d) consumer cannot
refresh (port-0 blocker); (e) consumer adopts an owner write; (f) 401 fails
closed and the account-probe auth contract; (g) concurrency; (i) CODEX_API_KEY
never reaches consumers; (j) leak canaries (checked after every test);
(k) staged re-login with the official device flow.
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


LINK_OK = {"present": True, "symlink": True, "target_matches": True, "home_physical": True}


@unittest.skipUnless(support.CODEX_BIN, "set GPTGREP_AUTH_TEST_CODEX_BIN to the official codex 0.162.1 binary")
class CodexIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.extra_stops = []
        self.make_world("codex")

    def make_world(self, name, authority_mode="ok", owner_url=None, **config):
        self.env = Env(name)
        self.authority = support.FakeAuthority(self.env.credential, mode=authority_mode)
        self.backend = support.FakeBackend()
        self.trap = support.Trap()
        values = dict(codex_bin=support.CODEX_BIN, codex_bin_sha256=codex_pin(),
                      codex_config={"chatgpt_base_url": self.backend.chatgpt_base_url,
                                    "openai_base_url": self.backend.openai_base_url},
                      owner={"refresh_token_url_override": owner_url or self.authority.url,
                             "rpc_timeout_seconds": 60, "exit_wait_seconds": 30, "term_grace_seconds": 60},
                      blocker_url=self.trap.url, blocker_must_be_closed=False)
        values.update(config)
        self.env.reconfigure(**values)
        support.assert_fake_path(self.env.credential.home)

    def tearDown(self):
        try:
            scanned = []
            needles = sorted(self.env.credential.canaries()) + ["FAKE-CANARY", "FAKE-CODE-1234"]
            hits = support.scan_for_leaks([self.env.runs_root, *support.lease_side_files(self.env)], needles,
                                          skip=[self.env.credential.home], counter=scanned)
            support.observe("leak_scan", test=self.id().split(".")[-1], files_scanned=len(scanned),
                            canaries=len(needles), hits=len(hits))
            self.assertTrue(scanned, "leak scan must not be vacuous")
            self.assertEqual(hits, [], "fake credential canaries leaked into supervisor/run files")
            self.assertEqual(support.run_auth_links(self.env), [], "no run keeps a link to the credential")
        finally:
            for stop in [self.authority.stop, self.backend.stop, self.trap.stop] + self.extra_stops:
                stop()

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

    def account_probe(self, run_id, role="dev"):
        return self.env.supervisor(
            "run", "--role", role, "--run-id", run_id, "--live-request-attempted", "no", "--deadline-seconds", "120",
            "--", PY, "-I", "-B", str(support.SUPERVISOR), "account-probe", "--codex-bin", support.CODEX_BIN,
            "--out", "{results}/account-probe.json", "--stderr-log", "{log_dir}/account-probe.stderr.log")

    def probe_result(self, run_id, role="dev"):
        return json.loads((self.env.run_dir(role, run_id) / "results" / "account-probe.json").read_text())

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
        self.assertEqual(receipt["auth_link_after"], LINK_OK, "the write went through the run's symlink")
        self.assertTrue(receipt["auth_link_final"]["released"])
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
        tokens = [r["refresh_token"] for r in self.authority.requests()]
        self.assertEqual(len(set(tokens)), len(tokens), "the owner never presents a refresh token twice")
        self.assertEqual(self.env.credential.meta()["ino"], before["ino"])
        support.observe("c_owner_expiring_token", authority_requests=count)

    def test_c_owner_failure_classes_from_the_official_log(self):
        """Permanent authority answers quarantine; transport, 429 and 5xx do not (F4)."""
        self.authority.stop()
        self.backend.stop()
        self.trap.stop()
        cases = [("reused", "REFRESH_TOKEN_REUSED", "permanent"), ("expired", "REFRESH_TOKEN_EXPIRED", "permanent"),
                 ("invalidated", "REFRESH_TOKEN_REVOKED", "permanent"), ("invalid_grant", None, "permanent"),
                 ("unknown_401", "REFRESH_REJECTED", "permanent"),
                 ("server_error", "AUTHORITY_SERVER_ERROR", "transient"),
                 ("rate_limited", "RATE_LIMITED", "transient"), ("closed", "TRANSPORT", "transient")]
        observed = {}
        first = True
        for mode, expected_class, kind in cases:
            for lifetime in (3600, 120):
                with self.subTest(mode=mode, lifetime=lifetime):
                    if not first:
                        self.tearDown_world()  # the last world stays for tearDown's leak scan
                    first = False
                    owner_url = None
                    if mode == "closed":
                        port = support.next_port()
                        owner_url = "http://127.0.0.1:%d/oauth/token" % port
                    self.make_world("cls-%s-%d" % (mode, lifetime), authority_mode=mode if mode != "closed" else "ok",
                                    owner_url=owner_url)
                    self.env.credential.write(1, time.time() + lifetime, in_place=False)
                    before = self.env.credential.meta()
                    code, receipt, proc = self.env.supervisor("refresh", "--run-id", "own-cls", "--force")
                    refresh = receipt["refresh"]
                    observed["%s/%d" % (mode, lifetime)] = [receipt["state"], refresh.get("owner_log_failure_class"),
                                                            refresh.get("owner_log_failure_kind")]
                    self.assertEqual(self.env.credential.meta(), before, "a failed refresh never writes")
                    self.assertEqual(refresh.get("owner_log_failure_kind"), kind, proc.stderr[-1500:])
                    if expected_class:
                        self.assertEqual(refresh.get("owner_log_failure_class"), expected_class)
                    else:
                        self.assertIn(refresh.get("owner_log_failure_class"), ("INVALID_GRANT", "REFRESH_REJECTED"))
                    if kind == "permanent":
                        self.assertEqual((code, receipt["state"]), (sup.EXIT_BLOCKED_AUTH, "BLOCKED_AUTH"))
                        marker = json.loads(self.env.quarantine_marker().read_text())
                        self.assertEqual(marker["details"]["failure_kind"], "permanent")
                    else:
                        self.assertEqual((code, receipt["state"]), (sup.EXIT_UNAVAILABLE, "REFRESH_UNAVAILABLE"))
                        self.assertFalse(self.env.quarantine_marker().exists())
                        self.assertTrue(refresh["account_chatgpt"])
                    if lifetime == 3600 and mode != "closed":
                        self.assertGreaterEqual(len(self.authority.requests()), 1)
        support.observe("c_owner_failure_classes", cases=observed)

    def tearDown_world(self):
        scanned = []
        hits = support.scan_for_leaks([self.env.runs_root, *support.lease_side_files(self.env)],
                                      sorted(self.env.credential.canaries()), skip=[self.env.credential.home],
                                      counter=scanned)
        self.authority.stop()
        self.backend.stop()
        self.trap.stop()
        self.assertTrue(scanned, "leak scan must not be vacuous")
        self.assertEqual(hits, [])

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
        self.assertEqual(receipt["auth_link_after"], LINK_OK)
        self.assertIn("CODEX_REFRESH_TOKEN_URL_OVERRIDE", receipt["env_keys"])
        self.assertIn("CODEX_REVOKE_TOKEN_URL_OVERRIDE", receipt["env_keys"])
        support.observe("d_consumer_blocked", authority_requests=0, blocked_attempts_at_trap=self.trap.count,
                        canonical_unchanged=True, thread_start_ok=True)

    def test_d_production_port_zero_blocker(self):
        """The default blocker is port 0: no process can listen there, and the refresh fails transiently."""
        self.env.reconfigure(blocker_url=sup.DEFAULT_BLOCKER_URL, blocker_must_be_closed=True)
        self.env.credential.write(1, time.time() + 120, in_place=False)
        before = self.env.credential.meta()
        code, receipt, rpc, proc = self.consume("blk2", initialize() + [read(True), read()])
        self.assertEqual((code, receipt["state"]), (0, "PASS"), proc.stderr[-2000:])
        # EPERM under the seatbelt guard (it cannot express port 0); EADDRNOTAVAIL without it.
        self.assertIn(receipt["blocker_probe"], ("closed", "blocked"))
        self.assertEqual(self.authority.requests(), [])
        self.assertEqual(self.env.credential.meta(), before)
        self.assertTrue(all(r["ok"] and r["account_type"] == "chatgpt" for r in self.reads(rpc)))
        log = (self.env.run_dir("dev", "blk2") / "state" / "logs" / "app-server.stderr.log").read_text()
        self.assertNotIn("panicked", log)
        support.observe("d_port_zero_blocker", authority_requests=0, blocker_probe=receipt["blocker_probe"],
                        canonical_unchanged=True)

    def test_d_account_probe_rehearses_first_live_step(self):
        """The runbook's first live step, against fakes: consumer, blocker, no refresh, no model call."""
        self.env.credential.write(1, time.time() + 120, in_place=False)
        before = self.env.credential.meta()
        code, receipt, proc = self.account_probe("probe1")
        self.assertEqual((code, receipt["state"]), (0, "PASS"), proc.stderr[-2000:])
        result = self.probe_result("probe1")
        self.assertTrue(result["account_ok"])
        self.assertEqual(result["outcome"], "OK")
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

    # -- (f) 401 fails closed; the consumer auth-failure contract --------------------------------

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

    def test_f_account_probe_auth_failure_is_blocked_auth_and_blocks_next_consumer(self):
        self.backend.mode = "unauthorized"
        self.env.credential.write(1, time.time() + 3600, in_place=False)
        code, receipt, proc = self.account_probe("p401")
        self.assertEqual((code, receipt["state"], receipt["child_exit_code"]),
                         (sup.EXIT_BLOCKED_AUTH, "BLOCKED_AUTH", sup.EXIT_BLOCKED_AUTH), proc.stderr[-1500:])
        self.assertEqual(self.probe_result("p401")["outcome"], "AUTH_UNAUTHORIZED")
        self.assertTrue(self.env.auth_block_marker().exists())
        self.assertFalse(self.env.quarantine_marker().exists())
        self.backend.mode = "ok"
        code, receipt, _ = self.account_probe("p401-next")
        self.assertEqual((code, receipt["refusal"]["reason"]), (sup.EXIT_BLOCKED_AUTH,
                                                                "auth_blocked_pending_owner_refresh"))
        code, owner, _ = self.env.supervisor("refresh", "--run-id", "p401-own")
        self.assertEqual((code, owner["refresh"]["decision"]), (0, "REFRESHED"))
        self.assertFalse(self.env.auth_block_marker().exists())
        code, receipt, _ = self.account_probe("p401-after")
        self.assertEqual((code, receipt["state"]), (0, "PASS"))
        support.observe("f_probe_auth_contract", first="BLOCKED_AUTH", next_refused=True, after_owner_refresh="PASS")

    def test_f_account_probe_service_failures_are_not_auth_failures(self):
        self.env.credential.write(1, time.time() + 3600, in_place=False)
        observed = {}
        for mode in ("server_error", "rate_limited"):
            self.backend.mode = mode
            code, receipt, proc = self.account_probe("psvc-" + mode)
            observed[mode] = [receipt["state"], receipt["child_exit_code"]]
            self.assertEqual((receipt["state"], receipt["child_exit_code"]), ("FAILED", sup.EXIT_UNAVAILABLE),
                             proc.stderr[-1500:])
            self.assertEqual(self.probe_result("psvc-" + mode)["outcome"], "SERVICE_UNAVAILABLE")
            self.assertFalse(self.env.auth_block_marker().exists(), "rate limits and 5xx never block auth")
        support.observe("f_probe_service_failures", cases=observed)

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
        self.assertEqual(receipt["auth_link_after"], LINK_OK)
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
        layout.link_auth(cfg)
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

    # -- (k) staged re-login with the official device flow (CS-1) -------------------------------

    def _relogin_world(self, mode):
        issuer = support.FakeIssuer(self.env.credential, mode)
        self.extra_stops.append(issuer.stop)
        self.env.reconfigure(relogin={"experimental_issuer": issuer.url})
        self.env.credential.write(1, time.time() + 3600, in_place=False)
        sup.write_quarantine(self.env.cfg(), "AUTH_REFRESH_FAILED", "fake-owner", {
            "operation": "refresh", "owner_log_failure_class": "REFRESH_TOKEN_REUSED", "failure_kind": "permanent"})
        return issuer

    def test_k_declined_official_device_login_never_revokes_or_deletes(self):
        issuer = self._relogin_world("decline")
        before = self.env.credential.meta()
        code, receipt, proc = self.env.supervisor("relogin", "--run-id", "rl-decline", "--confirm-relogin")
        self.assertEqual((code, receipt["state"], receipt["login_exit_code"]),
                         (sup.EXIT_BLOCKED_AUTH, "BLOCKED_AUTH", 1), proc.stderr[-1500:])
        self.assertEqual(self.env.credential.meta(), before, "the existing login was neither deleted nor replaced")
        self.assertNotIn("/oauth/revoke", issuer.paths())
        self.assertIn("/api/accounts/deviceauth/usercode", issuer.paths())
        self.assertFalse(receipt["installed"])
        self.assertFalse(os.path.lexists(self.env.run_dir("_relogin", "rl-decline") / "codex-home" / "auth.json"))
        self.assertIn("FAKE-CODE-1234", proc.stdout, "the code reached the terminal, not a file")
        support.observe("k_relogin_declined_official", canonical_unchanged=True, revoke_requests=0,
                        issuer_paths=issuer.paths())

    def test_k_approved_official_device_login_is_installed_atomically_and_adopted(self):
        issuer = self._relogin_world("approve")
        before = self.env.credential.meta()
        code, receipt, proc = self.env.supervisor("relogin", "--run-id", "rl-approve", "--confirm-relogin")
        self.assertEqual((code, receipt["state"], receipt["installed"]), (0, "PASS", True), proc.stderr[-1500:])
        after = self.env.credential.meta()
        self.assertNotEqual(after["ino"], before["ino"])
        self.assertEqual((after["mode"], after["regular"]), (0o600, True))
        self.assertNotIn("/oauth/revoke", issuer.paths())
        self.assertEqual(self.authority.requests(), [])
        code, _, _ = self.env.supervisor("clear-quarantine", "--run-id", "rl-clear", "--confirm", "--reason",
                                         "fake drill: re-login completed")
        self.assertEqual(code, 0)
        since = time.time()
        code, receipt, rpc, _ = self.consume("after-relogin", initialize() + [read()])
        self.assertEqual(code, 0)
        bearers = self.bearers_since(since)
        self.assertTrue(bearers and all(b == self.label(900) for b in bearers), bearers)
        support.observe("k_relogin_approved_official", inode_changed=True, revoke_requests=0,
                        consumer_uses_new_login=True)


if __name__ == "__main__":
    unittest.main()
