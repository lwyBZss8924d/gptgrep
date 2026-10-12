"""(b) Layout, generated config, environment allowlist and canonical preflight."""
import json
import os
from pathlib import Path
import stat
import time
import tomllib
import unittest
from unittest import mock

import support
from support import PY, FIXTURES, Env, sup

PROBE = str(FIXTURES / "consumer_probe.py")
CANARY_ENV = {
    "OPENAI_API_KEY": "FAKE-CANARY-openai", "CODEX_API_KEY": "FAKE-CANARY-codex",
    "CODEX_ACCESS_TOKEN": "FAKE-CANARY-access", "OP_SERVICE_ACCOUNT_TOKEN": "FAKE-CANARY-op",
    "OP_SESSION_fake": "FAKE-CANARY-opsession", "OPENROUTER_API_KEY": "FAKE-CANARY-openrouter",
    "AWS_SECRET_ACCESS_KEY": "FAKE-CANARY-aws", "ODDLY_NAMED_VALUE": "FAKE-CANARY-odd",
    "RUST_LOG": "trace", "SSLKEYLOGFILE": "/tmp/FAKE-CANARY-keylog", "HTTPS_PROXY": "http://FAKE-CANARY-proxy:1",
    "CODEX_HOME": "/FAKE-CANARY-wrong-home", "CODEX_REFRESH_TOKEN_URL_OVERRIDE": "http://FAKE-CANARY-elsewhere/",
    "DOCKER_HOST": "unix:///fake/docker.sock", "BASH_ENV": "/FAKE-CANARY-startup",
}
BASE_NAMES = {"CODEX_HOME", "CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED", "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
              "CODEX_REVOKE_TOKEN_URL_OVERRIDE", "CODEX_SQLITE_HOME", "GPTGREP_AUTH_LEASE_FD", "GPTGREP_AUTH_RUN_ID", "GPTGREP_AUTH_SOURCE_ID",
              "HOME", "LANG", "LC_ALL", "NO_PROXY", "no_proxy", "PATH", "TMPDIR", "XDG_CACHE_HOME",
              "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"}
PLATFORM_NAMES = {"__CF_USER_TEXT_ENCODING"}  # injected by macOS at process start, not by the supervisor


class LayoutEnvTests(unittest.TestCase):
    def setUp(self):
        self.env = Env("layout")
        self.env.credential.write(1, time.time() + 3600, in_place=False)
        # A canonical config.toml with a marker key must never reach run homes.
        (self.env.credential.home / "config.toml").write_text('model = "FAKE-CANARY-canonical-config"\n')

    def report(self, role, run_id, extra_env=None):
        out = self.env.root / ("env-%s.json" % run_id)
        code, receipt, proc = self.env.supervisor(
            "run", "--role", role, "--run-id", run_id, "--", PY, "-I", "-B", PROBE, "env-report", str(out),
            env=self.env.parent_env(**(extra_env or {})))
        self.assertEqual(code, 0, proc.stderr)
        return json.loads(out.read_text()), receipt

    def test_fake_world_is_outside_real_credential_roots(self):
        for path in (self.env.credential.home, self.env.lease_path, self.env.runs_root):
            support.assert_fake_path(path)
            for root in support.real_credential_roots():
                self.assertNotIn(root, Path(os.path.realpath(path)).parents)

    def test_layout_symlink_modes_and_generated_config(self):
        report, receipt = self.report("dev", "layout1")
        run = self.env.run_dir("dev", "layout1")
        for name in ("codex-home", "state", "state/logs", "home", "tmp", "work", "results"):
            info = os.lstat(run / name)
            self.assertTrue(stat.S_ISDIR(info.st_mode))
            self.assertEqual(stat.S_IMODE(info.st_mode), 0o700, name)
        link = run / "codex-home" / "auth.json"
        # The link exists only while the run holds the lease (CS-3): it was an absolute
        # link to the canonical file during the run and is gone afterwards.
        self.assertEqual(report["auth_link"], {"is_symlink": True, "target": str(self.env.credential.path)})
        self.assertTrue(os.path.isabs(report["auth_link"]["target"]))
        self.assertFalse(os.path.lexists(link))
        self.assertTrue(receipt["auth_link_final"]["released"])
        config = tomllib.loads((run / "codex-home" / "config.toml").read_text())
        self.assertEqual(config["cli_auth_credentials_store"], "file")
        self.assertEqual(config["forced_login_method"], "chatgpt")
        self.assertIs(config["features"]["use_agent_identity"], False)
        for feature in ("memories", "hooks", "plugins", "apps", "secret_auth_storage"):
            self.assertIs(config["features"][feature], False, feature)
        self.assertEqual(config["notify"], [])
        self.assertIs(config["cloud"]["skills"]["enabled"], False)
        self.assertEqual(config["memories"], {"generate_memories": False, "use_memories": False})
        self.assertEqual(config["log_dir"], str(run / "state" / "logs"))
        self.assertNotIn("FAKE-CANARY", (run / "codex-home" / "config.toml").read_text())
        self.assertFalse((run / "codex-home" / "skills").exists())
        # CODEX_HOME must not be under the child's TMPDIR (arg0 refuses that).
        values = report["values"]
        self.assertFalse(Path(values["CODEX_HOME"]).is_relative_to(values["TMPDIR"]))
        self.assertEqual(values["CODEX_HOME"], str(run / "codex-home"))
        self.assertEqual(values["CODEX_SQLITE_HOME"], str(run / "state"))
        self.assertEqual(values["HOME"], str(run / "home"))
        self.assertEqual(values["TMPDIR"], str(run / "tmp"))
        self.assertEqual(report["cwd"], str(run / "work"))
        for key in ("XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"):
            self.assertTrue(Path(values[key]).is_relative_to(run / "home"), key)
        self.assertEqual(receipt["state"], "PASS")
        self.assertEqual(receipt["auth_backend"], "file")
        self.assertFalse(receipt["credential_contents_read_by_supervisor"])

    def test_allowlist_strips_secrets_and_pins_blocker(self):
        report, receipt = self.report("dev", "env1", CANARY_ENV)
        names = set(report["names"]) - PLATFORM_NAMES
        self.assertEqual(names, BASE_NAMES)
        self.assertFalse(report["canary_value_present"])
        self.assertEqual(report["values"]["CODEX_REFRESH_TOKEN_URL_OVERRIDE"], sup.DEFAULT_BLOCKER_URL)
        self.assertEqual(report["values"]["CODEX_REVOKE_TOKEN_URL_OVERRIDE"], "http://127.0.0.1:0/oauth/revoke")
        self.assertEqual(sup.DEFAULT_BLOCKER_URL, "http://127.0.0.1:0/oauth/token")
        for key in ("NO_PROXY", "no_proxy"):
            self.assertEqual(set(report["values"][key].split(",")), {"127.0.0.1", "localhost", "::1"})
        self.assertEqual(report["values"]["CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED"], "1")
        self.assertTrue(report["lease_fd_open"])
        self.assertEqual(set(receipt["env_keys"]), BASE_NAMES)
        for name in sup.STRIPPED_ALWAYS:
            self.assertNotIn(name, receipt["env_keys"])
        receipt_text = (self.env.run_dir("dev", "env1") / "receipt.json").read_text()
        self.assertNotIn("FAKE-CANARY", receipt_text)

    def test_role_passthrough_is_explicit_and_openrouter_is_role_scoped(self):
        report, _ = self.report("harbor", "env2", CANARY_ENV)
        self.assertEqual(set(report["names"]) - PLATFORM_NAMES, BASE_NAMES | {"DOCKER_HOST"})
        self.assertEqual(report["values"]["DOCKER_HOST"], "unix:///fake/docker.sock")
        self.assertFalse(report["openrouter_present"])
        report, _ = self.report("judge", "env3", CANARY_ENV)
        self.assertTrue(report["openrouter_present"])
        self.assertNotIn("CODEX_API_KEY", report["names"])

    def test_config_rejects_forbidden_passthrough_and_unknown_codex_keys(self):
        cases = [
            {"roles": {"dev": {"passthrough_env": ["CODEX_API_KEY"], "tool_permissions": "unrestricted"}}},
            {"roles": {"dev": {"passthrough_env": ["OPENAI_API_KEY"], "tool_permissions": "unrestricted"}}},
            {"roles": {"dev": {"passthrough_env": ["OPENROUTER_API_KEY"], "tool_permissions": "unrestricted"}}},
            {"roles": {"dev": {"passthrough_env": ["CODEX_HOME"], "tool_permissions": "unrestricted"}}},
            {"roles": {"dev": {"passthrough_env": ["RUST_LOG"], "tool_permissions": "unrestricted"}}},
            {"roles": {"dev": {"passthrough_env": ["OP_SESSION_abc"], "tool_permissions": "unrestricted"}}},
            {"codex_config": {"cli_auth_credentials_store": "keyring"}},
            {"codex_config": {"model_providers": "x"}},
            {"blocker_url": "http://192.0.2.10:9/oauth/token"},
            {"owner": {"refresh_token_url_override": "https://example.invalid/oauth/token"}},
            {"runs_root": str(self.env.credential.home / "runs")},
            {"expected_user": "nobody-fake-user-name"},
            {"roles": {"dev": {"path_env": "relative/bin", "tool_permissions": "unrestricted"}}},
            {"roles": {"dev": {}}},
            {"roles": {"dev": {"tool_permissions": "none"}}},
            {"roles": {"dev": {"tool_permissions": {"extends": ":danger-full-access"}}}},
            {"roles": {"dev": {"tool_permissions": {"extends": ":workspace", "filesystem": {}}}}},
            {"roles": {"dev": {"tool_permissions": "unrestricted", "stop_scope": "group"}}},
            {"relogin": {"experimental_issuer": "https://auth.example.invalid"}},
            {"blocker_url": "http://127.0.0.1:99999/oauth/token"},
            {"runs_root": str(self.env.lease_path.parent / "runs")},
            {"codex_config": {"default_permissions": "x"}},
        ]
        reasons = []
        for change in cases:
            values = dict(self.env.values, **change)
            with self.subTest(change=sorted(change)):
                with self.assertRaises(sup.SupervisorError) as ctx:
                    sup.Config(dict(values, schema=sup.CONFIG_SCHEMA), "inline")
                reasons.append(ctx.exception.reason)
        self.assertEqual(reasons.count("config_role_tool_permissions_required"), 1, reasons)
        self.assertIn("config_lease_directory_contains_runs_root", reasons)

    def test_expected_effective_user_and_portable_example_config(self):
        import pwd
        user = pwd.getpwuid(os.geteuid()).pw_name
        sup.Config(dict(self.env.values, schema=sup.CONFIG_SCHEMA, expected_user=user), "inline")
        example = json.loads((support.TOOL_DIR / "config.example.json").read_text())
        cfg = sup.Config(example, "example")
        self.assertFalse(cfg.allow_shared_consumers)
        self.assertEqual(cfg.blocker_url, sup.DEFAULT_BLOCKER_URL)
        harbor = cfg.role("harbor")
        self.assertEqual((harbor["stop_scope"], harbor["tool_permissions"]), ("root-first", "consumer-managed"))
        self.assertGreaterEqual(harbor["term_grace_seconds"], 120)
        self.assertIn("/path/to/docker-bin", harbor["path_env"].split(":"))
        text = (support.TOOL_DIR / "config.example.json").read_text()
        self.assertNotIn(str(Path.home()), text, "portable example carries no machine paths")

    def test_account_probe_refuses_outside_a_supervised_consumer(self):
        import subprocess
        proc = subprocess.run([PY, "-I", "-B", str(support.SUPERVISOR), "account-probe", "--codex-bin", "/usr/bin/false",
                               "--out", str(self.env.root / "never.json")], capture_output=True, text=True,
                              env=self.env.parent_env(), timeout=60)
        self.assertEqual(proc.returncode, sup.EXIT_USAGE)
        self.assertEqual(support.last_json(proc.stdout)["refusal"]["reason"],
                         "account_probe_requires_supervised_consumer_env")
        self.assertFalse((self.env.root / "never.json").exists())

    def test_invalid_or_unsafe_config_is_refused(self):
        self.env.config_path.write_text("{not json")
        code, receipt, _ = self.env.supervisor("status")
        self.assertEqual((code, receipt["refusal"]["reason"]), (sup.EXIT_USAGE, "config_invalid_json"))
        support.write_config(self.env.config_path, self.env.values)
        os.chmod(self.env.config_path, 0o666)
        code, receipt, _ = self.env.supervisor("status")
        self.assertEqual((code, receipt["refusal"]["reason"]), (sup.EXIT_USAGE, "config_file_unsafe"))
        os.chmod(self.env.config_path, 0o600)
        code, receipt, _ = self.env.supervisor("status")
        self.assertEqual((code, receipt["lease"]["probe"]), (0, "absent"))
        self.assertEqual(receipt["canonical"]["preflight"], "ok")
        self.assertFalse(receipt["credential_contents_read_by_supervisor"])

    def test_shared_lease_refused_unless_enabled(self):
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "sh1", "--lease", "shared",
                                               "--", PY, "-c", "pass")
        self.assertEqual(code, sup.EXIT_USAGE)
        self.assertEqual(receipt["refusal"]["reason"], "shared_lease_not_enabled")
        self.assertFalse(self.env.run_dir("dev", "sh1").exists())

    def test_placeholders_and_dry_run(self):
        code, plan, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "dry1", "--dry-run", "--",
                                            "/bin/echo", "{codex_home}", "{results}", "{log_dir}", "{run_dir}")
        self.assertEqual(code, 0)
        run = self.env.run_dir("dev", "dry1")
        self.assertEqual(plan["argv"][1:], [str(run / "codex-home"), str(run / "results"),
                                            str(run / "state" / "logs"), str(run)])
        self.assertFalse(run.exists())
        self.assertFalse(self.env.lease_path.exists())
        self.assertIn('cli_auth_credentials_store = "file"', plan["codex_config_toml"])

    def test_toml_renderer_round_trips(self):
        cfg = self.env.cfg()
        layout = sup.Layout(cfg, "dev", "toml1")
        tree = sup.codex_config_tree(cfg, layout, {"model": "x\"y", "project_doc_max_bytes": 0})
        self.assertEqual(tomllib.loads(sup.render_toml(tree)), tree)

    def test_canonical_preflight_rejections(self):
        cred = self.env.credential
        cases = []

        def mode_0644():
            os.chmod(cred.path, 0o644)

        def dir_0755():
            os.chmod(cred.home, 0o755)

        def symlinked_file():
            real = cred.home / "real-auth.json"
            os.rename(cred.path, real)
            os.symlink(real, cred.path)

        def missing():
            os.unlink(cred.path)

        def empty():
            os.truncate(cred.path, 0)

        cases = [("canonical_auth_not_0600", mode_0644), ("canonical_home_not_0700", dir_0755),
                 ("canonical_auth_not_regular_file", symlinked_file), ("canonical_auth_missing", missing),
                 ("canonical_auth_empty", empty)]
        for index, (reason, breaker) in enumerate(cases):
            with self.subTest(reason=reason):
                for leftover in (cred.home / "real-auth.json",):
                    if leftover.exists():
                        os.unlink(leftover)
                if os.path.lexists(cred.path):
                    os.unlink(cred.path)
                os.chmod(cred.home, 0o700)
                cred.write(1, time.time() + 3600, in_place=False)
                breaker()
                ready = self.env.root / ("never-%d" % index)
                code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "pre%d" % index, "--",
                                                       PY, "-I", "-B", PROBE, "hold", "0", str(ready))
                self.assertEqual(code, sup.EXIT_BLOCKED_AUTH)
                self.assertEqual(receipt["state"], "BLOCKED_AUTH")
                self.assertEqual(receipt["refusal"]["reason"], reason)
                self.assertFalse(ready.exists(), "consumer must not start")
        os.chmod(cred.home, 0o700)

    def test_foreign_owner_refused_for_canonical_and_lease(self):
        cfg = self.env.cfg()
        real_uid = os.getuid()
        with mock.patch.object(sup.os, "getuid", return_value=real_uid + 1):
            with self.assertRaises(sup.SupervisorError) as ctx:
                sup.canonical_preflight(cfg)
            self.assertEqual(ctx.exception.reason, "canonical_home_foreign_owner")
            with self.assertRaises(sup.SupervisorError) as ctx:
                sup.Lease(cfg).open()
            self.assertEqual(ctx.exception.reason, "directory_foreign_owner")

    def test_blocker_port_with_listener_is_refused(self):
        listener = support.Trap()
        try:
            self.env.reconfigure(blocker_url=listener.url, blocker_must_be_closed=True)
            ready = self.env.root / "never-blocker"
            code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "blk1", "--",
                                                   PY, "-I", "-B", PROBE, "hold", "0", str(ready))
            self.assertEqual(code, sup.EXIT_BLOCKED_AUTH)
            self.assertEqual(receipt["refusal"]["reason"], "blocker_port_has_listener")
            self.assertFalse(ready.exists())
        finally:
            listener.stop()

    def test_supervisor_permission_profile_denies_credential_paths_to_tool_commands(self):
        roles = dict(self.env.values["roles"])
        roles["tools"] = {"tool_permissions": {"extends": ":workspace", "network_enabled": False}}
        self.env.reconfigure(roles=roles)
        code, plan, _ = self.env.supervisor("run", "--role", "tools", "--run-id", "perm1", "--dry-run", "--",
                                            "/bin/echo")
        self.assertEqual(code, 0)
        config = tomllib.loads(plan["codex_config_toml"])
        run = self.env.run_dir("tools", "perm1")
        self.assertEqual(config["default_permissions"], sup.SUPERVISOR_PERMISSION_PROFILE)
        profile = config["permissions"][sup.SUPERVISOR_PERMISSION_PROFILE]
        self.assertEqual(profile["extends"], ":workspace")
        self.assertIs(profile["network"]["enabled"], False)
        self.assertEqual(profile["filesystem"], {str(self.env.credential.home): "deny",
                                                 str(run / "codex-home" / "auth.json"): "deny",
                                                 str(self.env.lease_path.parent): "deny"})
        self.assertEqual(plan["tool_credential_access"]["policy"], "supervisor_profile")
        code, plan, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "perm2", "--dry-run", "--",
                                            "/bin/echo")
        self.assertNotIn("default_permissions", tomllib.loads(plan["codex_config_toml"]))
        self.assertEqual(plan["tool_credential_access"],
                         {"policy": "unrestricted", "tool_commands_can_read_credential": True})
        code, plan, _ = self.env.supervisor("run", "--role", "harbor", "--run-id", "perm3", "--dry-run", "--",
                                            "/bin/echo")
        self.assertEqual(plan["tool_credential_access"], {"policy": "consumer_managed",
                                                          "verified_by_supervisor": False})

    def test_refusals_before_launch_leave_no_auth_link(self):
        listener = support.Trap()
        try:
            self.env.reconfigure(blocker_url=listener.url, blocker_must_be_closed=True)
            self.env.supervisor("run", "--role", "dev", "--run-id", "nolink1", "--", PY, "-c", "pass")
        finally:
            listener.stop()
        self.env.reconfigure(blocker_url=sup.DEFAULT_BLOCKER_URL)
        os.chmod(self.env.credential.path, 0o644)
        self.env.supervisor("run", "--role", "dev", "--run-id", "nolink2", "--", PY, "-c", "pass")
        os.chmod(self.env.credential.path, 0o600)
        self.assertEqual(support.run_auth_links(self.env), [])
        self.assertTrue(self.env.run_dir("dev", "nolink1").exists() and self.env.run_dir("dev", "nolink2").exists())


if __name__ == "__main__":
    unittest.main()
