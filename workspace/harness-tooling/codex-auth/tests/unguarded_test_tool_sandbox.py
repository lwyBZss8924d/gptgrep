"""Tool-command credential denial under the official Codex sandbox (LEAK-03 / F5).

Codex applies its own macOS seatbelt sandbox to tool commands, and macOS refuses
to apply a sandbox inside one that is already applied. This module therefore
cannot run under the qualification's seatbelt guard; run_qualification.py runs
it in a separate pass only with --unguarded-tool-sandbox. Without the guard:

* every endpoint is a loopback fake (backend, model provider) and the refresh
  and revoke endpoints are the unbindable port-0 blocker;
* HTTPS_PROXY / HTTP_PROXY / ALL_PROXY point at a loopback proxy trap, so any
  request to a non-loopback host is captured and closed, never forwarded; the
  test fails if the trap saw a single connection, and reports each connection's
  request line (method and host only);
* all credentials are unsigned fake JWTs under the run's own temporary root
  (asserted to be outside the real credential roots).

A tool command is driven through app-server `command/exec`, which applies the
configured permission profile without any model request. The configurable
denies and read carve-outs are driven through `codex sandbox --permission-profile
gptgrep_codex_auth`, which applies the same profile without a model or network.
"""
import json
import os
import subprocess
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
            if self.proxy.count:
                support.observe("proxy_trap_connections", test=self.id().split(".")[-1], count=self.proxy.count,
                                connections=self.proxy.destinations)
            self.assertEqual(self.proxy.count, 0, "nothing tried to reach a non-loopback host: %s"
                             % self.proxy.destinations)
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

    def paths_world(self):
        """Fake files, an extra deny, and a `paths` role with a role deny and read carve-outs
        (one of them the parent of the fake canonical auth home)."""
        root = self.env.root
        files = {"role_deny": root / "role-denied" / "secret.txt",
                 "carve_out": root / "role-denied" / "public" / "readme.txt",
                 "extra_deny": root / "extra-denied" / "secret.txt",
                 "control": root / "open" / "control.txt"}
        for label, path in files.items():
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("MARKER-%s\n" % label.upper())
        reads = [str(root / "role-denied" / "public"), str(self.env.credential.home.parent)]
        roles = dict(self.env.values["roles"])
        roles["paths"] = {"tool_permissions": {"extends": ":workspace", "network_enabled": False,
                                               "deny_paths": [str(root / "role-denied")], "read_paths": reads},
                          "passthrough_env": PROXY_NAMES}
        self.env.reconfigure(roles=roles, extra_denied_paths=[str(root / "extra-denied")])
        return files, reads

    def test_codex_sandbox_enforces_extra_and_role_denies_and_read_carve_outs(self):
        """(l) Model-free: the official `codex sandbox --permission-profile gptgrep_codex_auth` runs
        /bin/cat as a supervised consumer (so the argv guard sees and allows it). A role deny and an
        extra_denied_paths entry are enforced; a read carve-out below a denied parent is readable;
        the fake canonical credential stays denied below a read carve-out; a sibling file outside
        every deny is readable (control). Under a role without that deny the role-denied file is
        readable, while the global extra deny still applies there."""
        root = self.env.root
        files, reads = self.paths_world()
        env = self.env.parent_env(**{name: self.proxy.proxy_url for name in PROXY_NAMES})
        # (label, role, file, readable)
        probes = [("role_deny", "paths", "role_deny", False), ("extra_deny", "paths", "extra_deny", False),
                  ("canonical", "paths", None, False), ("carve_out", "paths", "carve_out", True),
                  ("control", "paths", "control", True), ("other_role_role_deny", "tools", "role_deny", True),
                  ("other_role_extra_deny", "tools", "extra_deny", False)]
        results = {}
        for label, role, name, readable in probes:
            run_id = "sbx-" + label.replace("_", "-")
            path = files[name] if name else self.env.credential.path
            code, receipt, proc = self.env.supervisor(
                "run", "--role", role, "--run-id", run_id, "--", support.CODEX_BIN, "sandbox",
                "--permission-profile", sup.SUPERVISOR_PERMISSION_PROFILE, "-C", "{work_dir}", "--",
                "/bin/cat", str(path), env=env)
            logs = self.env.run_dir(role, run_id) / "state" / "logs"
            out = (logs / "child.stdout.log").read_text()
            err = (logs / "child.stderr.log").read_text()
            item = {"exit": code, "state": receipt["state"], "child_exit": receipt.get("child_exit_code"),
                    "not_permitted": "Operation not permitted" in err, "stdout_bytes": len(out),
                    "marker": bool(name) and ("MARKER-%s" % name.upper()) in out}
            results[label] = item
            with self.subTest(label=label):
                self.assertEqual(receipt["tool_credential_access"]["consumer_argv_guard"], "checked")
                self.assertEqual(receipt["auth_link_final"]["released"], True)
                if readable:
                    self.assertEqual((code, item["state"], item["child_exit"], item["marker"]), (0, "PASS", 0, True),
                                     item)
                else:
                    self.assertEqual((item["state"], item["not_permitted"], item["stdout_bytes"]),
                                     ("FAILED", True, 0), item)
                    self.assertNotEqual(item["child_exit"], 0, item)
            if role == "paths":
                access = receipt["tool_credential_access"]
        self.assertEqual(access["denied_paths"][-2:], [str(root / "extra-denied"), str(root / "role-denied")])
        self.assertEqual(access["read_paths"], reads)
        self.assertEqual(self.env.credential.meta()["mode"], 0o600)
        support.observe("codex_sandbox_profile_paths", proxy_trap_connections=self.proxy.count,
                        **{label: {k: v for k, v in item.items() if k != "stdout_bytes"}
                           for label, item in results.items()})

    def test_project_root_below_a_deny_reopens_it_so_the_argv_guard_refuses_it(self):
        """(m) Why -C/--add-dir inside a deny is refused. Outside the supervisor, on a run home with the
        generated profile and no auth link, `codex sandbox -C <dir below the role deny>` lets the
        sandboxed command read and write that directory; with -C at the work directory it is denied.
        Under the supervisor the same command line is refused before anything is created."""
        self.paths_world()
        below = self.env.root / "role-denied" / "below"
        below.mkdir()
        (below / "file.txt").write_text("MARKER-BELOW\n")
        cfg = self.env.cfg()
        spec = cfg.role("paths")
        layout = sup.Layout(cfg, "paths", "premise")
        layout.create(cfg, spec["codex_config"], spec["tool_permissions"])
        env, _ = sup.build_consumer_env(cfg, "paths", layout,
                                        self.env.parent_env(**{name: self.proxy.proxy_url for name in PROXY_NAMES}))

        def sandboxed(cwd, *command):
            return subprocess.run([support.CODEX_BIN, "sandbox", "--permission-profile",
                                   sup.SUPERVISOR_PERMISSION_PROFILE, "-C", str(cwd), "--", *command], env=env,
                                  cwd=str(layout.work_dir), capture_output=True, text=True, timeout=120)

        read_below = sandboxed(below, "/bin/cat", str(below / "file.txt"))
        write_below = sandboxed(below, "/bin/sh", "-c", "echo WRITTEN > '%s'" % (below / "new.txt"))
        read_from_work = sandboxed(layout.work_dir, "/bin/cat", str(below / "file.txt"))
        self.assertEqual((read_below.returncode, "MARKER-BELOW" in read_below.stdout), (0, True), read_below.stderr)
        self.assertEqual((write_below.returncode, (below / "new.txt").exists()), (0, True), write_below.stderr)
        self.assertNotEqual(read_from_work.returncode, 0)
        self.assertIn("Operation not permitted", read_from_work.stderr)
        self.assertFalse(os.path.lexists(layout.auth_link), "the unsupervised home never had an auth link")
        code, result, _ = self.env.supervisor(
            "run", "--role", "paths", "--run-id", "premise-supervised", "--", support.CODEX_BIN, "sandbox",
            "--permission-profile", sup.SUPERVISOR_PERMISSION_PROFILE, "-C", str(below), "--", "/bin/cat",
            str(below / "file.txt"))
        self.assertEqual((code, result["refusal"]), (sup.EXIT_USAGE, {"reason": "consumer_argv_reopens_denied_path",
                                                                       "detail": "-C " + str(below)}))
        self.assertFalse(self.env.run_dir("paths", "premise-supervised").exists())
        support.observe("project_root_below_deny_premise", read_below_exit=read_below.returncode,
                        write_below_exit=write_below.returncode, read_from_work_exit=read_from_work.returncode,
                        supervised_refusal=result["refusal"]["reason"])

    def test_app_server_starts_a_thread_and_enforces_role_paths_through_command_exec(self):
        """(l) The same profile through the official app-server (--strict-config): a thread starts
        (AGENTS.md is loaded through Codex's fs sandbox helper under the profile), and command/exec
        denies the role deny and the extra deny but reads the carve-out."""
        files, _reads = self.paths_world()
        run = self.env.run_dir("paths", "rpc-paths")
        steps = [{"op": "request", "method": "initialize",
                  "params": {"clientInfo": {"name": "profile_paths_probe", "title": None, "version": "0"}}},
                 {"op": "notify", "method": "initialized"},
                 {"op": "request", "method": "thread/start", "params": {"cwd": str(run / "work"), "ephemeral": True},
                  "timeout": 60},
                 cat(files["role_deny"], "role_deny"), cat(files["extra_deny"], "extra_deny"),
                 cat(files["carve_out"], "carve_out"), cat(self.env.credential.path, "canonical")]
        code, receipt, by_label, rpc, proc = self.run_steps("paths", "rpc-paths", steps)
        self.assertEqual((code, receipt["state"]), (0, "PASS"), proc.stderr[-1500:])
        starts = [r for r in rpc["results"] if r.get("method") == "thread/start"]
        self.assertTrue(starts and starts[0]["ok"] and starts[0]["thread_started"], starts)
        for label in ("role_deny", "extra_deny", "canonical"):
            with self.subTest(label=label):
                self.assertNotEqual(by_label[label]["exit_code"], 0, by_label[label])
                self.assertTrue(by_label[label]["stderr_denied"], by_label[label])
                self.assertEqual(by_label[label]["stdout_bytes"], 0, by_label[label])
        self.assertEqual(by_label["carve_out"]["exit_code"], 0, by_label["carve_out"])
        self.assertEqual(by_label["carve_out"]["stdout_bytes"], len("MARKER-CARVE_OUT\n"), by_label["carve_out"])
        self.assertEqual(receipt["tool_credential_access"]["consumer_argv_guard"], "not_a_codex_command")
        support.observe("app_server_profile_paths", thread_started=True, proxy_trap_connections=self.proxy.count,
                        **{label: by_label[label]["exit_code"]
                           for label in ("role_deny", "extra_deny", "carve_out", "canonical")})


if __name__ == "__main__":
    unittest.main()
