"""Tool-command credential denial under the official Codex sandbox (LEAK-03 / F5).

Codex applies its own macOS seatbelt sandbox to tool commands, and macOS refuses
to apply a sandbox inside one that is already applied. This module therefore
cannot run under the qualification's seatbelt guard; run_qualification.py runs
it in a separate pass only with --unguarded-tool-sandbox. Without the guard:

* every endpoint is a loopback fake (backend, model provider) and the refresh
  and revoke endpoints are the unbindable port-0 blocker;
* HTTPS_PROXY / HTTP_PROXY / ALL_PROXY point at a loopback proxy trap, so any
  request to a non-loopback host is captured and closed, never forwarded; the
  test fails if the trap saw a single connection;
* all credentials are unsigned fake JWTs under the run's own temporary root
  (asserted to be outside the real credential roots).

A tool command is driven through app-server `command/exec`, which applies the
configured permission profile without any model request.
"""
import json
import os
import time
import unittest

import support
from support import PY, FIXTURES, TOOL_DIR, Env, sup

RPC = str(FIXTURES / "rpc_client.py")
PROXY_NAMES = ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "https_proxy", "http_proxy", "all_proxy"]


def cat(path, label):
    return {"op": "request", "method": "command/exec", "label": label, "timeout": 60,
            "params": {"command": ["/bin/cat", str(path)]}}


@unittest.skipUnless(support.SANDBOX == "proxy-trap" and support.CODEX_BIN,
                     "runs only in run_qualification.py's explicit --unguarded-tool-sandbox pass")
class ToolSandboxTests(unittest.TestCase):
    def setUp(self):
        self.env = Env("toolsbx")
        for path in (self.env.credential.home, self.env.lease_path, self.env.runs_root):
            support.assert_fake_path(path)
        self.backend = support.FakeBackend()
        self.proxy = support.ProxyTrap()
        roles = dict(self.env.values["roles"])
        roles["tools"] = {"tool_permissions": {"extends": ":workspace", "network_enabled": False},
                          "passthrough_env": PROXY_NAMES}
        roles["open"] = {"tool_permissions": "unrestricted", "passthrough_env": PROXY_NAMES}
        self.env.reconfigure(codex_bin=support.CODEX_BIN,
                             codex_bin_sha256=support.CODEX_SHA256 or support.sha256_file(support.CODEX_BIN),
                             codex_config={"chatgpt_base_url": self.backend.chatgpt_base_url,
                                           "openai_base_url": self.backend.openai_base_url},
                             blocker_url=sup.DEFAULT_BLOCKER_URL, blocker_must_be_closed=True, roles=roles)
        self.env.credential.write(1, time.time() + 3600, in_place=False)

    def tearDown(self):
        try:
            self.assertEqual(self.proxy.count, 0, "nothing tried to reach a non-loopback host")
            hits = support.scan_for_leaks([self.env.runs_root, *support.lease_side_files(self.env)],
                                          sorted(self.env.credential.canaries()), skip=[self.env.credential.home])
            self.assertEqual(hits, [])
        finally:
            self.backend.stop()
            self.proxy.stop()

    def run_steps(self, role, run_id, steps):
        steps_path = self.env.root / ("steps-%s.json" % run_id)
        steps_path.write_text(json.dumps(steps))
        env = self.env.parent_env(**{name: self.proxy.proxy_url for name in PROXY_NAMES})
        code, receipt, proc = self.env.supervisor(
            "run", "--role", role, "--run-id", run_id, "--", PY, "-I", "-B", RPC, str(TOOL_DIR), support.CODEX_BIN,
            str(steps_path), "{results}/rpc.json", "{log_dir}/app-server.stderr.log", env=env)
        rpc = json.loads((self.env.run_dir(role, run_id) / "results" / "rpc.json").read_text())
        return code, receipt, {r.get("label"): r for r in rpc["results"] if r.get("label")}, rpc, proc

    def steps(self, role, run_id):
        run = self.env.run_dir(role, run_id)
        init = [{"op": "request", "method": "initialize",
                 "params": {"clientInfo": {"name": "tool_sandbox_probe", "title": None, "version": "0"}}},
                {"op": "notify", "method": "initialized"}]
        return init + [
            {"op": "request", "method": "account/read", "params": {}, "timeout": 60},
            cat(run / "codex-home" / "auth.json", "run_link"),
            cat(self.env.credential.path, "canonical"),
            cat(self.env.lease_path, "lease_file"),
            {"op": "request", "method": "command/exec", "label": "control_write", "timeout": 60,
             "params": {"command": ["/bin/sh", "-c", "echo MARKER-OK > control.txt && cat control.txt"],
                        "cwd": str(run / "work")}},
            {"op": "request", "method": "thread/start", "params": {"cwd": str(run / "work"), "ephemeral": True},
             "timeout": 60}]

    def test_supervisor_profile_denies_the_credential_to_tool_commands(self):
        code, receipt, by_label, rpc, proc = self.run_steps("tools", "deny1", self.steps("tools", "deny1"))
        self.assertEqual((code, receipt["state"]), (0, "PASS"), proc.stderr[-1500:])
        for label in ("run_link", "canonical", "lease_file"):
            with self.subTest(label=label):
                self.assertNotEqual(by_label[label]["exit_code"], 0, by_label[label])
                self.assertFalse(by_label[label]["stdout_has_fake_token"])
        self.assertEqual(by_label["control_write"]["exit_code"], 0, by_label["control_write"])
        self.assertTrue(by_label["control_write"]["stdout_has_marker"])
        starts = [r for r in rpc["results"] if r.get("method") == "thread/start"]
        self.assertTrue(starts and starts[0]["ok"], starts)
        self.assertEqual(receipt["tool_credential_access"]["policy"], "supervisor_profile")
        support.observe("tool_sandbox_denies_credential", run_link_exit=by_label["run_link"]["exit_code"],
                        canonical_exit=by_label["canonical"]["exit_code"],
                        lease_exit=by_label["lease_file"]["exit_code"],
                        workspace_write_exit=by_label["control_write"]["exit_code"], proxy_trap_connections=self.proxy.count)

    def test_port_zero_blocker_without_the_guard(self):
        """The real port-0 semantics (EADDRNOTAVAIL), not the guard's EPERM: the refresh fails, the token stays."""
        self.env.credential.write(2, time.time() + 120, in_place=True)
        before = self.env.credential.meta()
        self.assertEqual(sup.blocker_probe(self.env.cfg()), "closed")
        steps = [{"op": "request", "method": "initialize",
                  "params": {"clientInfo": {"name": "port0_probe", "title": None, "version": "0"}}},
                 {"op": "notify", "method": "initialized"},
                 {"op": "request", "method": "account/read", "params": {"refreshToken": True}, "timeout": 60},
                 {"op": "request", "method": "account/read", "params": {}, "timeout": 60}]
        code, receipt, _by_label, rpc, proc = self.run_steps("open", "port0", steps)
        self.assertEqual((code, receipt["state"], receipt["blocker_probe"]), (0, "PASS", "closed"),
                         proc.stderr[-1500:])
        reads = [r for r in rpc["results"] if r.get("method") == "account/read"]
        self.assertTrue(all(r["ok"] and r["account_type"] == "chatgpt" for r in reads), reads)
        self.assertEqual(self.env.credential.meta(), before, "nothing was refreshed or written")
        support.observe("port_zero_blocker_unguarded", blocker_probe=receipt["blocker_probe"],
                        canonical_unchanged=True, proxy_trap_connections=self.proxy.count)

    def test_negative_control_unrestricted_role_can_read_through_the_link(self):
        code, receipt, by_label, _rpc, proc = self.run_steps("open", "open1", self.steps("open", "open1"))
        self.assertEqual((code, receipt["state"]), (0, "PASS"), proc.stderr[-1500:])
        self.assertEqual(by_label["run_link"]["exit_code"], 0, "without the profile the tool command reads it")
        self.assertTrue(by_label["run_link"]["stdout_has_fake_token"])
        self.assertEqual(receipt["tool_credential_access"]["policy"], "unrestricted")
        support.observe("tool_sandbox_negative_control", run_link_exit=by_label["run_link"]["exit_code"])


if __name__ == "__main__":
    unittest.main()
