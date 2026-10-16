"""(b) Layout, generated config, environment allowlist and canonical preflight."""
import json
import os
from pathlib import Path
import stat
import subprocess
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
              "CODEX_REVOKE_TOKEN_URL_OVERRIDE", "CODEX_SQLITE_HOME", "GPTGREP_AUTH_FAILURE_RECORD",
              "GPTGREP_AUTH_RUN_ID", "GPTGREP_AUTH_SOURCE_ID",
              "HOME", "LANG", "LC_ALL", "NO_PROXY", "no_proxy", "PATH", "TMPDIR", "XDG_CACHE_HOME",
              "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"}
PLATFORM_NAMES = {"__CF_USER_TEXT_ENCODING"}  # injected by macOS at process start, not by the supervisor
PROFILE = sup.SUPERVISOR_PERMISSION_PROFILE
# Codex command lines (after the executable) that replace the generated profile, with the refusal detail.
ARGV_REFUSED = [
    (["exec", "--sandbox", "workspace-write", "say hi"], "--sandbox"),
    (["exec", "--sandbox=danger-full-access", "say hi"], "--sandbox"),
    (["-s", "read-only", "exec", "say hi"], "-s"),
    (["exec", "-sdanger-full-access", "say hi"], "-s"),
    (["-hs", "read-only"], "-s"),
    (["exec", "say hi", "--sandbox", "read-only"], "--sandbox"),
    (["exec", "--dangerously-bypass-approvals-and-sandbox", "say hi"], "--dangerously-bypass-approvals-and-sandbox"),
    (["exec", "--yolo", "say hi"], "--yolo"),
    (["--full-auto", "exec", "say hi"], "--full-auto"),
    (["exec", "--approve-for-me", "say hi"], "--approve-for-me"),
    (["exec", "--not-so-yolo", "say hi"], "--not-so-yolo"),
    (["exec", "-c", 'sandbox_mode="danger-full-access"', "say hi"], "-c sandbox_mode"),
    (["exec", '-csandbox_mode="danger-full-access"', "say hi"], "-c sandbox_mode"),
    (["exec", "-c", ' sandbox_mode = "read-only"', "say hi"], "-c sandbox_mode"),
    (["exec", "-C", "-c", "sandbox_mode=read-only", "say hi"], "-c sandbox_mode"),
    (['--config=default_permissions=":danger-full-access"', "exec", "say hi"], "--config default_permissions"),
    (["exec", "--config", "permissions.%s.filesystem={}" % PROFILE, "say hi"], "--config permissions"),
    (["app-server", "-c", "permissions={}"], "-c permissions"),
    (["exec", "-c", "sandbox_workspace_write.network_access=true", "say hi"], "-c sandbox_workspace_write"),
    (["exec", "--profile", "other", "say hi"], "--profile"),
    (["exec", "-p", "other", "say hi"], "-p"),
    (["-pother", "exec", "say hi"], "-p"),
    (["exec", "--ignore-user-config", "say hi"], "--ignore-user-config"),
    (["sandbox", "--permission-profile", ":danger-full-access", "--", "/bin/cat", "x"], "--permission-profile"),
    (["sandbox", "--permissions-profile=other", "--", "/bin/true"], "--permissions-profile"),
    (["sandbox", "-P", "other", "--", "/bin/true"], "-P"),
    (["sandbox", "--sandbox-state-json", "{}", "--", "/bin/true"], "--sandbox-state-json"),
    (["sandbox", "--sandbox-state-readable-root", "/", "--", "/bin/true"], "--sandbox-state-readable-root"),
    (["sandbox", "--allow-unix-socket", "/var/run/docker.sock", "--", "/bin/true"], "--allow-unix-socket"),
    (["--remote", "ws://127.0.0.1:1", "say hi"], "--remote"),
    (["exec", "-c", 'projects={"/w"={trust_level="trusted"}}', "say hi"], "-c projects"),
    (["exec", "-c", 'projects."/w".trust_level="trusted"', "say hi"], "-c projects"),
    (["exec", "-c", 'mcp_servers.fake.command="/bin/sh"', "say hi"], "-c mcp_servers"),
    (["exec", "-c", 'approvals_reviewer="auto_review"', "say hi"], "-c approvals_reviewer"),
    (["exec", "-c", 'cli_auth_credentials_store="keyring"', "say hi"], "-c cli_auth_credentials_store"),
    (["exec", "-c", 'notify=["/bin/sh", "-c", "true"]', "say hi"], "-c notify"),
    (["exec", "-c", 'log_dir="/w/elsewhere"', "say hi"], "-c log_dir"),
    (["exec", "-c", "hooks.fake={}", "say hi"], "-c hooks"),
    (["exec", "-c", "marketplaces.fake={}", "say hi"], "-c marketplaces"),
    (["exec", "-c", "features.plugins=true", "say hi"], "-c features.plugins"),
    (["exec", "-c", "features={plugins=true}", "say hi"], "-c features"),
    (["exec", "--enable", "hooks", "say hi"], "--enable hooks"),
    (["--enable=secret_auth_storage", "exec", "say hi"], "--enable secret_auth_storage"),
]
def judge_args(schema, root_args=("-C", "{work_dir}")):
    """The owner's production Judge command line after the executable (H1)."""
    return ["exec", "--skip-git-repo-check", "--ephemeral", "--json", *root_args, "-m", "gpt-6-luna",
            "-c", 'model_reasoning_effort="max"', "-c", 'service_tier="fast"', "-c", 'web_search="disabled"',
            "-c", "features.shell_tool=false", "-c", "features.view_image=false",
            "-c", "features.code_mode.enabled=false", "-c", "features.code_mode_host.enabled=false",
            "--output-schema", str(schema), "-o", "{results}/verdict.json", "-"]


ARGV_ALLOWED = [
    ["exec", "-c", 'model="fake-model"', "-m", "fake-model", "-C", "/w", "-a", "never", "say hi"],
    ["exec", "-c", "features.fake_feature=true", "--skip-git-repo-check", "--", "--sandbox is prompt text"],
    ["app-server", "--listen", "stdio://", "--strict-config"],
    ["sandbox", "--permission-profile", PROFILE, "-C", "/w", "--", "/bin/cat", "-s"],
    ["sandbox", "--permission-profile=" + PROFILE, "--", "/bin/true"],
    ["sandbox", "-P" + PROFILE, "--", "/bin/true"],
    ["-C/w/sandbox-dir", "exec", "say hi"],
    ["exec", "--enable", "fake_feature", "--disable", "memories", "-c", "features.fake_feature=true", "say hi"],
    ["--version"],
]


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
            str(self.env.lease_path), env=self.env.parent_env(**(extra_env or {})))
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
        self.assertEqual(values["GPTGREP_AUTH_FAILURE_RECORD"], str(run / "results" / "auth-failure.json"))
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
        # L1: the consumer gets no lease descriptor and no variable naming one.
        self.assertIsNone(report["lease_fd_env"])
        self.assertEqual(report["lease_descriptors"], 0)
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

    def probe_run(self, run_id, *probe_args, before=None):
        """account-probe as the supervised consumer; /usr/bin/false stands in for Codex and never
        answers. `before` is a consumer_probe mode run first in the same consumer."""
        probe = [PY, "-I", "-B", str(support.SUPERVISOR), "account-probe", "--codex-bin", "/usr/bin/false",
                 *probe_args]
        command = probe
        if before:
            script = " ".join("'%s'" % part for part in [PY, "-I", "-B", PROBE, *before]) + " && exec " + \
                " ".join("'%s'" % part for part in probe)
            command = ["/bin/sh", "-c", script]
        code, receipt, proc = self.env.supervisor("run", "--role", "dev", "--run-id", run_id, "--", *command)
        stdout = (self.env.run_dir("dev", run_id) / "state" / "logs" / "child.stdout.log").read_text()
        return code, receipt, support.last_json(stdout) or {}

    def test_account_probe_outputs_never_reach_the_credential_or_lease_paths(self):
        """L3 (CS-5): --stderr-log and --out equal to or inside the canonical auth home, any run's
        codex-home or the lease directory are refused before the app-server starts, in any spelling and
        through symlinks. The log is opened O_NOFOLLOW|O_APPEND|O_CREAT (0600) and refused when the file
        has another link (a hard link to the credential)."""
        before = self.env.credential.meta()
        home, lease_dir = self.env.credential.home, self.env.lease_path.parent
        out = ["--out", "{results}/probe.json"]
        cases = [("cs5-link", out + ["--stderr-log", "{codex_home}/auth.json"], "--stderr-log"),
                 ("cs5-canonical", out + ["--stderr-log", str(self.env.credential.path)], "--stderr-log"),
                 ("cs5-home-spelling", out + ["--stderr-log", str(home.parent / home.name.upper() / "x.log")],
                  "--stderr-log"),
                 ("cs5-lease-dir", out + ["--stderr-log", str(lease_dir / "probe.log")], "--stderr-log"),
                 ("cs5-other-run", out + ["--stderr-log",
                                          str(self.env.run_dir("dev", "other") / "codex-home" / "x.log")],
                  "--stderr-log"),
                 ("cs5-own-home", out + ["--stderr-log", "{codex_home}/probe.log"], "--stderr-log"),
                 ("cs5-out-home", ["--out", str(home / "probe.json")], "--out"),
                 ("cs5-out-codex-home", ["--out", "{codex_home}/probe.json"], "--out")]
        for run_id, args, flag in cases:
            with self.subTest(run_id=run_id):
                code, receipt, result = self.probe_run(run_id, *args)
                self.assertEqual(result.get("refusal"), {"reason": "account_probe_output_in_protected_path",
                                                         "detail": flag})
                self.assertEqual((code, receipt["state"], receipt["child_exit_code"]), (1, "FAILED", sup.EXIT_USAGE))
                self.assertFalse((self.env.run_dir("dev", run_id) / "results" / "probe.json").exists())
        self.assertEqual(self.env.credential.meta(), before)
        # A symlink into the credential, created by the consumer in its results: refused (it resolves into
        # the auth home), then removed by the post-run scan, which quarantines.
        code, receipt, result = self.probe_run("cs5-symlink", *out, "--stderr-log", "{results}/s.log",
                                               before=["link-credential", "sym-canonical", "s.log"])
        self.assertEqual(result.get("refusal"), {"reason": "account_probe_output_in_protected_path",
                                                 "detail": "--stderr-log"})
        self.assertEqual((code, receipt["state"]), (sup.EXIT_QUARANTINED, "QUARANTINED"))
        self.assertEqual(self.env.credential.meta(), before)
        self.assertFalse(os.path.lexists(self.env.run_dir("dev", "cs5-symlink") / "results" / "s.log"))
        self.env.supervisor("clear-quarantine", "--run-id", "cs5-clear1", "--confirm", "--reason", "fake drill")
        # A hard link passes the path check but is refused after O_APPEND opens it (two links).
        code, receipt, result = self.probe_run("cs5-hardlink", *out, "--stderr-log", "{results}/h.log",
                                               before=["link-credential", "hard", "h.log"])
        self.assertEqual(result.get("refusal"), {"reason": "account_probe_stderr_log_refused",
                                                 "detail": "not_a_private_single_link_file"})
        self.assertEqual((code, receipt["state"]), (sup.EXIT_QUARANTINED, "QUARANTINED"))
        self.assertEqual(self.env.credential.meta()["size"], before["size"], "nothing was appended")
        self.assertEqual(os.lstat(self.env.credential.path).st_nlink, 1, "the post-run scan removed the link")
        self.env.supervisor("clear-quarantine", "--run-id", "cs5-clear2", "--confirm", "--reason", "fake drill")
        # A symlink as the last component is never followed (O_NOFOLLOW), even to a harmless file.
        elsewhere = self.env.root / "elsewhere.log"
        elsewhere.write_text("")
        code, receipt, result = self.probe_run(
            "cs5-final-symlink", *out, "--stderr-log", "{log_dir}/s.log",
            before=["symlink", str(elsewhere), "{log_dir}/s.log"])
        self.assertEqual(result.get("refusal"), {"reason": "account_probe_stderr_log_refused", "detail": "ELOOP"})
        self.assertEqual(elsewhere.read_text(), "")
        # Allowed: a new log below the run's state directory, created 0600.
        code, receipt, result = self.probe_run("cs5-ok", *out, "--stderr-log", "{log_dir}/probe.stderr.log")
        self.assertEqual((receipt["state"], receipt["child_exit_code"], result.get("outcome")),
                         ("FAILED", sup.EXIT_UNAVAILABLE, "SERVICE_UNAVAILABLE"))
        log = self.env.run_dir("dev", "cs5-ok") / "state" / "logs" / "probe.stderr.log"
        self.assertEqual(stat.S_IMODE(os.lstat(log).st_mode), 0o600)
        self.assertEqual(self.env.credential.meta(), before)
        support.observe("account_probe_protected_outputs", refused=len(cases) + 3, allowed=1)

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
                         {"policy": "unrestricted", "tool_commands_can_read_credential": True,
                          "consumer_argv_guard": "not_applicable"})
        code, plan, _ = self.env.supervisor("run", "--role", "harbor", "--run-id", "perm3", "--dry-run", "--",
                                            "/bin/echo")
        self.assertEqual(plan["tool_credential_access"], {"policy": "consumer_managed",
                                                          "verified_by_supervisor": False,
                                                          "consumer_argv_guard": "not_applicable"})

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


class ToolPathsAndArgvGuardTests(unittest.TestCase):
    """(l) extra_denied_paths, role deny/read paths and (m) the consumer argv guard.

    The sandbox itself cannot be applied under the qualification guard; its
    enforcement is qualified in unguarded_test_tool_sandbox.py."""

    def setUp(self):
        self.env = Env("paths")
        self.env.credential.write(1, time.time() + 3600, in_place=False)

    def tools_roles(self, **tool_permissions):
        roles = dict(self.env.values["roles"])
        roles["tools"] = {"tool_permissions": dict({"extends": ":workspace", "network_enabled": False},
                                                   **tool_permissions)}
        return roles

    def config_with(self, **changes):
        return sup.Config(dict(self.env.values, schema=sup.CONFIG_SCHEMA, **changes), "inline")

    def refusal(self, **changes):
        with self.assertRaises(sup.SupervisorError) as ctx:
            self.config_with(**changes)
        return ctx.exception.reason, ctx.exception.detail

    def install_fake_codex(self):
        fake = support.script(self.env.root / "codex", (FIXTURES / "fake_codex.py").read_text())
        Path(str(fake) + ".json").write_text("{}")
        self.env.reconfigure(codex_bin=str(fake), codex_bin_sha256=support.sha256_file(fake))
        return fake

    def test_extra_denied_paths_are_validated_and_deduplicated(self):
        root = self.env.root
        cfg = self.config_with(extra_denied_paths=[str(root / "private"), str(root / "private"),
                                                   str(root / "other")])
        self.assertEqual(cfg.extra_denied_paths, [str(root / "private"), str(root / "other")])
        self.assertEqual(self.config_with().extra_denied_paths, [])
        most = [str(root / ("p%02d" % i)) for i in range(sup.MAX_PROFILE_PATHS)]
        self.assertEqual(self.config_with(extra_denied_paths=most).extra_denied_paths, most)
        cases = [("config_extra_denied_paths", str(root / "private")),
                 ("config_extra_denied_paths", {"path": str(root / "private")}),
                 ("config_extra_denied_paths", most + [str(root / "one-too-many")]),
                 ("config_path_not_absolute", ["relative/private"]),
                 ("config_path_not_absolute", [17]),
                 ("config_path_not_absolute", [""]),
                 ("config_path_not_normalized", [str(root / "a") + "/../b"]),
                 ("config_path_not_normalized", [str(root / "private") + "/"]),
                 ("config_path_not_normalized", [str(root) + "//private"]),
                 ("config_path_not_normalized", ["/" + str(root / "private")])]
        for reason, value in cases:
            with self.subTest(reason=reason, value=str(value)[-32:]):
                self.assertEqual(self.refusal(extra_denied_paths=value), (reason, "extra_denied_paths"))

    def test_role_deny_and_read_paths_are_validated(self):
        root = self.env.root
        data = str(root / "data")
        tools = self.config_with(roles=self.tools_roles(deny_paths=[data, data], read_paths=[data + "/public"]))
        self.assertEqual(tools.role("tools")["tool_permissions"],
                         {"extends": ":workspace", "network_enabled": False, "deny_paths": [data],
                          "read_paths": [data + "/public"]})
        plain = self.config_with(roles=self.tools_roles()).role("tools")["tool_permissions"]
        self.assertEqual((plain["deny_paths"], plain["read_paths"]), ([], []))
        most = [str(root / ("p%02d" % i)) for i in range(sup.MAX_PROFILE_PATHS)]
        self.config_with(roles=self.tools_roles(deny_paths=most, read_paths=[p + "/sub" for p in most]))
        key = "roles.tools.tool_permissions."
        cases = [("config_role_tool_permissions_invalid", key + "deny_paths", {"deny_paths": data}),
                 ("config_role_tool_permissions_invalid", key + "read_paths", {"read_paths": {"x": data}}),
                 ("config_role_tool_permissions_invalid", key + "deny_paths", {"deny_paths": most + [data]}),
                 ("config_role_tool_permissions_invalid", key + "read_paths", {"read_paths": most + [data]}),
                 ("config_path_not_absolute", key + "deny_paths", {"deny_paths": ["data"]}),
                 ("config_path_not_normalized", key + "read_paths", {"read_paths": [data + "/./public"]}),
                 ("config_path_not_normalized", key + "read_paths", {"read_paths": ["/" + data]}),
                 ("config_role_tool_permissions_invalid", "roles.tools", {"unknown_paths": [data]}),
                 ("config_role_read_path_equals_deny_path", key + "read_paths:" + data,
                  {"deny_paths": [data], "read_paths": [data]}),
                 ("config_role_read_path_equals_deny_path", key + "read_paths:" + data,
                  {"deny_paths": [data.upper()], "read_paths": [data]})]
        for reason, detail, change in cases:
            with self.subTest(reason=reason, change=sorted(change)):
                self.assertEqual(self.refusal(roles=self.tools_roles(**change)), (reason, detail))

    def test_read_paths_that_reopen_a_supervisor_deny_are_refused_at_config_load(self):
        root, home, runs = self.env.root, self.env.credential.home, self.env.runs_root
        extra, accented = str(root / "extra-denied"), str(root / "café")
        # Other spellings of the same path (macOS volumes ignore case and normalization) count too.
        cases = [("config_role_read_path_reopens_credential", str(home)),
                 ("config_role_read_path_reopens_credential", str(self.env.credential.path)),
                 ("config_role_read_path_reopens_credential", str(home.parent / home.name.upper())),
                 ("config_role_read_path_reopens_credential", str(home.parent / home.name.upper() / "auth.json")),
                 ("config_role_read_path_reopens_credential", str(self.env.lease_path.parent)),
                 ("config_role_read_path_reopens_credential", str(self.env.lease_path)),
                 ("config_role_read_path_reopens_credential", str(runs / "tools" / "r1" / "codex-home" / "auth.json")),
                 ("config_role_read_path_reopens_credential", str(runs / "dev" / "r2" / "codex-home" / "auth.json")),
                 ("config_role_read_path_reopens_credential", str(runs / "dev" / "r2" / "Codex-Home" / "AUTH.json")),
                 ("config_role_read_path_reopens_extra_deny", extra),
                 ("config_role_read_path_reopens_extra_deny", extra + "/inside"),
                 ("config_role_read_path_reopens_extra_deny", extra.upper()),
                 ("config_role_read_path_reopens_extra_deny", str(root / "café" / "inside"))]
        for reason, path in cases:
            with self.subTest(reason=reason, path=path[-40:]):
                self.assertEqual(self.refusal(extra_denied_paths=[extra, accented],
                                              roles=self.tools_roles(read_paths=[path])),
                                 (reason, "roles.tools.tool_permissions.read_paths:" + path))
        # Accepted: a carve-out below a role deny, and paths that merely contain a supervisor deny
        # (the more specific deny still applies).
        reads = [str(root / "data" / "public"), str(home.parent), str(runs / "tools"),
                 str(runs / "tools" / "r1" / "codex-home")]
        cfg = self.config_with(extra_denied_paths=[extra],
                               roles=self.tools_roles(deny_paths=[str(root / "data")], read_paths=reads))
        self.assertEqual(cfg.role("tools")["tool_permissions"]["read_paths"], reads)
        # The CLI refuses at config load, for every command, before any lease file exists.
        self.env.reconfigure(roles=self.tools_roles(read_paths=[str(home)]))
        for args in (("status",), ("run", "--role", "dev", "--run-id", "load1", "--", "/usr/bin/true")):
            code, result, _ = self.env.supervisor(*args)
            self.assertEqual((code, result["state"], result["refusal"]["reason"]),
                             (sup.EXIT_USAGE, "CONFIG_ERROR", "config_role_read_path_reopens_credential"))
        self.assertFalse(self.env.lease_path.exists())
        self.assertFalse(self.env.run_dir("dev", "load1").exists())

    def test_read_path_resolving_into_a_denied_path_is_refused_at_load_and_generation(self):
        alias = self.env.root / "alias"
        self.env.reconfigure(roles=self.tools_roles(read_paths=[str(alias / "sub")]))
        cfg = self.env.cfg()  # A physical path may change after config load.
        os.symlink(self.env.credential.home, alias)
        for run_id, extra in (("alias1", ()), ("alias2", ("--dry-run",))):
            code, result, _ = self.env.supervisor("run", "--role", "tools", "--run-id", run_id, *extra, "--",
                                                  "/usr/bin/true")
            self.assertEqual((code, result["refusal"]),
                             (sup.EXIT_USAGE, {"reason": "config_role_read_path_reopens_credential",
                                               "detail": "roles.tools.tool_permissions.read_paths:" + str(alias / "sub")}))
            self.assertFalse(self.env.run_dir("tools", run_id).exists())
        with self.assertRaises(sup.SupervisorError) as ctx:
            sup.profile_filesystem(cfg, sup.Layout(cfg, "tools", "alias3"), cfg.role("tools")["tool_permissions"])
        self.assertEqual(ctx.exception.reason, "role_read_path_resolves_into_denied_path")
        self.assertFalse(self.env.lease_path.exists())
        self.assertEqual(support.run_auth_links(self.env), [])

    def test_write_paths_and_minimal_read_are_explicit_and_validated(self):
        root = self.env.root
        data = str(root / "task")
        permissions = self.config_with(roles=self.tools_roles(
            extends=":read-only", minimal_read=True, deny_paths=["/"], write_paths=[data, data]))
        self.assertEqual(permissions.role("tools")["tool_permissions"],
                         {"extends": ":read-only", "network_enabled": False, "minimal_read": True,
                          "deny_paths": ["/"], "read_paths": [], "write_paths": [data]})
        plain = self.config_with(roles=self.tools_roles(minimal_read=False, write_paths=[]))
        self.assertEqual(plain.role("tools")["tool_permissions"],
                         {"extends": ":workspace", "network_enabled": False, "deny_paths": [], "read_paths": []})
        most = [str(root / ("write%02d" % i)) for i in range(sup.MAX_PROFILE_PATHS)]
        self.config_with(roles=self.tools_roles(write_paths=most))
        key = "roles.tools.tool_permissions."
        cases = [("config_role_tool_permissions_invalid", key + "write_paths", {"write_paths": data}),
                 ("config_role_tool_permissions_invalid", key + "write_paths", {"write_paths": most + [data]}),
                 ("config_path_not_absolute", key + "write_paths", {"write_paths": [":minimal"]}),
                 ("config_path_not_normalized", key + "write_paths", {"write_paths": [data + "/../other"]}),
                 ("config_role_write_path_root", key + "write_paths:/", {"write_paths": ["/"]}),
                 ("config_role_write_path_equals_deny_path", key + "write_paths:" + data,
                  {"deny_paths": [data.upper()], "write_paths": [data]})]
        cases.extend(("config_role_tool_permissions_invalid", "roles.tools", {"minimal_read": value})
                     for value in (None, 1, "true", []))
        for reason, detail, change in cases:
            with self.subTest(reason=reason, change=change):
                self.assertEqual(self.refusal(roles=self.tools_roles(**change)), (reason, detail))

    def test_write_paths_cannot_overlap_credentials_or_extra_denies(self):
        root, home, runs = self.env.root, self.env.credential.home, self.env.runs_root
        extra = root / "data" / "private"
        alias = root / "alias"
        alias.symlink_to(home)
        paths = [home, home / "nested", home.parent, self.env.credential.path,
                 self.env.lease_path, self.env.lease_path.parent, runs, runs / "tools",
                 runs / "tools" / "run", runs / "other" / "run" / "codex-home",
                 runs / "other" / "run" / "Codex-Home" / "AUTH.json", alias, alias / "nested"]
        for path in paths:
            with self.subTest(path=str(path)):
                self.assertEqual(self.refusal(roles=self.tools_roles(write_paths=[str(path)])),
                                 ("config_role_write_path_overlaps_credential",
                                  "roles.tools.tool_permissions.write_paths:" + str(path)))
        for path in (extra, extra / "nested", extra.parent):
            with self.subTest(extra=str(path)):
                self.assertEqual(self.refusal(extra_denied_paths=[str(extra)],
                                              roles=self.tools_roles(write_paths=[str(path)])),
                                 ("config_role_write_path_overlaps_extra_deny",
                                  "roles.tools.tool_permissions.write_paths:" + str(path)))
        # A run's work and result directories do not contain its auth home.
        self.config_with(roles=self.tools_roles(write_paths=[
            str(runs / "tools" / "run" / "work"), str(runs / "tools" / "run" / "results")]))

    def test_read_write_conflicts_include_nested_paths_and_aliases(self):
        read = self.env.root / "read"
        read.mkdir()
        alias = self.env.root / "alias"
        alias.symlink_to(read)
        for write in (read, read / "nested", read.parent, Path(str(read).upper()), alias):
            with self.subTest(write=str(write)):
                self.assertEqual(self.refusal(roles=self.tools_roles(read_paths=[str(read)], write_paths=[str(write)])),
                                 ("config_role_read_write_paths_overlap",
                                  "roles.tools.tool_permissions.read_paths:" + str(read)))

    def test_write_and_conflict_checks_repeat_after_config_load(self):
        alias = self.env.root / "alias"
        read = self.env.root / "read"
        read.mkdir()
        extra = self.env.root / "private"
        for target, permissions, reason in (
                (self.env.credential.home, {"write_paths": [str(alias)]},
                 "role_write_path_resolves_into_denied_path"),
                (self.env.lease_path.parent, {"write_paths": [str(alias)]},
                 "role_write_path_resolves_into_denied_path"),
                (extra, {"write_paths": [str(alias)]}, "role_write_path_resolves_into_denied_path"),
                (self.env.runs_root / "other" / "r" / "codex-home", {"write_paths": [str(alias)]},
                 "role_write_path_resolves_into_denied_path"),
                (read, {"read_paths": [str(read)], "write_paths": [str(alias)]},
                 "config_role_read_write_paths_overlap"),
                (read, {"deny_paths": [str(read)], "write_paths": [str(alias)]},
                 "config_role_write_path_equals_deny_path")):
            with self.subTest(target=str(target), reason=reason):
                cfg = self.config_with(extra_denied_paths=[str(extra)], roles=self.tools_roles(**permissions))
                alias.symlink_to(target)
                try:
                    with self.assertRaises(sup.SupervisorError) as ctx:
                        sup.render_codex_config(cfg, sup.Layout(cfg, "tools", "changed"), {},
                                                cfg.role("tools")["tool_permissions"])
                    self.assertEqual(ctx.exception.reason, reason)
                    with self.assertRaises(sup.SupervisorError):
                        self.config_with(extra_denied_paths=[str(extra)], roles=self.tools_roles(**permissions))
                finally:
                    alias.unlink()
        self.assertFalse(self.env.lease_path.exists())

    def test_minimal_read_and_exact_write_profile_preserve_denies_and_receipts(self):
        root = self.env.root
        task, runtime, extra = root / "task", root / "runtime", root / "private"
        self.env.reconfigure(extra_denied_paths=[str(extra)], roles=self.tools_roles(
            extends=":read-only", deny_paths=["/"], minimal_read=True,
            read_paths=[str(runtime)], write_paths=[str(task)]))
        code, receipt, proc = self.env.supervisor("run", "--role", "tools", "--run-id", "explicit", "--",
                                                  "/usr/bin/true")
        self.assertEqual((code, receipt["state"]), (0, "PASS"), proc.stderr)
        run = self.env.run_dir("tools", "explicit")
        profile = tomllib.loads((run / "codex-home" / "config.toml").read_text())["permissions"][PROFILE]
        self.assertEqual(profile, {"extends": ":read-only", "network": {"enabled": False}, "filesystem": {
            str(self.env.credential.home): "deny", str(run / "codex-home" / "auth.json"): "deny",
            str(self.env.lease_path.parent): "deny", str(extra): "deny", "/": "deny",
            ":minimal": "read", str(runtime): "read", str(task): "write"}})
        access = receipt["tool_credential_access"]
        self.assertEqual(access["write_paths"], [str(task)])
        self.assertEqual(access["read_paths"], [str(runtime)])
        self.assertIs(access["minimal_read"], True)
        self.assertEqual(receipt["supervisor_version"], "1.4.0")
        self.assertTrue(receipt["auth_link_final"]["released"])
        self.assertFalse(receipt["credential_contents_read_by_supervisor"])
        code, plan, _ = self.env.supervisor("run", "--role", "tools", "--run-id", "explicit-plan",
                                            "--dry-run", "--", "/usr/bin/true")
        self.assertEqual(code, 0)
        self.assertEqual(plan["tool_credential_access"]["write_paths"], [str(task)])
        self.assertIs(plan["tool_credential_access"]["minimal_read"], True)

    def test_generated_profile_composes_supervisor_and_role_denies_with_read_carve_outs(self):
        root, home = self.env.root, self.env.credential.home
        data = str(root / "data")
        reads = [data + "/public", str(home.parent)]
        # An extra deny that repeats a credential deny is kept once.
        self.env.reconfigure(extra_denied_paths=[str(root / "extra-denied"), str(home)],
                             roles=self.tools_roles(deny_paths=[data], read_paths=reads))
        code, receipt, proc = self.env.supervisor("run", "--role", "tools", "--run-id", "paths1", "--",
                                                  "/usr/bin/true")
        self.assertEqual((code, receipt["state"]), (0, "PASS"), proc.stderr)
        run = self.env.run_dir("tools", "paths1")
        denied = [str(home), str(run / "codex-home" / "auth.json"), str(self.env.lease_path.parent),
                  str(root / "extra-denied"), data]
        config = tomllib.loads((run / "codex-home" / "config.toml").read_text())
        self.assertEqual(config["default_permissions"], PROFILE)
        profile = config["permissions"][PROFILE]
        expected = {path: "deny" for path in denied}
        expected.update({path: "read" for path in reads})
        self.assertEqual(list(profile["filesystem"].items()), list(expected.items()))
        self.assertEqual((profile["extends"], profile["network"]), (":workspace", {"enabled": False}))
        access = receipt["tool_credential_access"]
        self.assertEqual((access["policy"], access["denied_paths"], access["read_paths"]),
                         ("supervisor_profile", denied, reads))
        self.assertEqual(access["consumer_argv_guard"], "not_a_codex_command")
        self.assertEqual(json.loads((run / "receipt.json").read_text())["tool_credential_access"], access)
        code, plan, _ = self.env.supervisor("run", "--role", "tools", "--run-id", "paths2", "--dry-run", "--",
                                            "/usr/bin/true")
        self.assertEqual(code, 0)
        self.assertEqual(plan["tool_credential_access"]["read_paths"], reads)
        link2 = str(self.env.run_dir("tools", "paths2") / "codex-home" / "auth.json")
        planned = {(link2 if path == denied[1] else path): mode for path, mode in expected.items()}
        self.assertEqual(tomllib.loads(plan["codex_config_toml"])["permissions"][PROFILE]["filesystem"], planned)
        # Roles without a generated profile get no profile, so extra_denied_paths does not apply.
        for role, policy in (("dev", "unrestricted"), ("harbor", "consumer_managed")):
            code, plan, _ = self.env.supervisor("run", "--role", role, "--run-id", "paths-" + role, "--dry-run",
                                                "--", "/usr/bin/true")
            self.assertEqual((code, plan["tool_credential_access"]["policy"]), (0, policy))
            self.assertNotIn("permissions", tomllib.loads(plan["codex_config_toml"]))
            self.assertNotIn("extra-denied", plan["codex_config_toml"])
        support.observe("profile_paths_generated", denied=len(denied), read=len(reads))

    def test_codex_argv_override_detection(self):
        for args, detail in ARGV_REFUSED:
            with self.subTest(args=args):
                self.assertEqual(sup.codex_profile_override(["/x/codex", *args]), detail)
        for args in ARGV_ALLOWED:
            with self.subTest(args=args):
                self.assertIsNone(sup.codex_profile_override(["/x/codex", *args]))

    def test_exact_network_domains_require_explicit_enable_and_valid_dns_names(self):
        valid = {"network_enabled": True, "network_allowed_domains": ["openrouter.ai", "api.example.invalid",
                                                                    "openrouter.ai"]}
        cfg = self.config_with(roles=self.tools_roles(**valid))
        self.assertEqual(cfg.role("tools")["tool_permissions"]["network_allowed_domains"],
                         ["openrouter.ai", "api.example.invalid"])
        self.config_with(roles=self.tools_roles(network_enabled=True,
                                               network_allowed_domains=["h%d.example.invalid" % i for i in range(64)]))
        invalid = [[], "openrouter.ai", [None], ["*"], ["*.openrouter.ai"], ["https://openrouter.ai"],
                   ["openrouter.ai:443"], ["openrouter.ai/"], ["OPENROUTER.AI"], ["openrouter.ai."],
                   ["localhost"], ["127.0.0.1"], ["127.1"], ["::1"], ["[::1]"], ["openrouter..ai"],
                   ["-bad.example"], ["bad-.example"], ["under_score.example"], ["例.example"],
                   ["a" * 64 + ".example"], ["openrouter.ai"] * 65]
        for domains in invalid:
            with self.subTest(domains=domains):
                self.assertEqual(self.refusal(roles=self.tools_roles(network_enabled=True,
                                                                    network_allowed_domains=domains)),
                                 ("config_role_network_domains_invalid",
                                  "roles.tools.tool_permissions.network_allowed_domains"))
        self.assertEqual(self.refusal(roles=self.tools_roles(network_allowed_domains=["openrouter.ai"])),
                         ("config_role_network_domains_invalid", "roles.tools.tool_permissions.network_allowed_domains"))
        plain = self.config_with(roles=self.tools_roles(network_enabled=True))
        layout = sup.Layout(plain, "tools", "plainnet")
        tree = sup.codex_config_tree(plain, layout, {}, plain.role("tools")["tool_permissions"])
        self.assertEqual(tree["permissions"][PROFILE]["network"], {"enabled": True})
        self.assertNotIn("network_proxy", tree["features"])
        self.assertNotIn("web_search", tree)

    def test_exact_network_profile_pins_proxy_search_and_preserves_credential_denies(self):
        roles = self.tools_roles(network_enabled=True, network_allowed_domains=["openrouter.ai"])
        self.env.reconfigure(roles=roles)
        code, receipt, proc = self.env.supervisor("run", "--role", "tools", "--run-id", "domainnet", "--",
                                                  PY, "-I", "-B", PROBE, "env-report",
                                                  str(self.env.root / "domainnet.json"), str(self.env.lease_path))
        self.assertEqual((code, receipt["state"]), (0, "PASS"), proc.stderr)
        run = self.env.run_dir("tools", "domainnet")
        tree = tomllib.loads((run / "codex-home" / "config.toml").read_text())
        self.assertEqual(tree["web_search"], "disabled")
        self.assertIs(tree["features"]["network_proxy"], True)
        self.assertIs(tree["features"]["apps"], False)
        self.assertNotIn("mcp_servers", tree)
        self.assertEqual(tree["permissions"][PROFILE]["network"], {
            "enabled": True, "proxy_url": "http://127.0.0.1:3128", "mode": "full",
            "enable_socks5": False, "enable_socks5_udp": False, "allow_upstream_proxy": False,
            "allow_local_binding": False, "dangerously_allow_non_loopback_proxy": False,
            "dangerously_allow_all_unix_sockets": False, "unix_sockets": {},
            "domains": {"openrouter.ai": "allow"}})
        filesystem = tree["permissions"][PROFILE]["filesystem"]
        for path in (self.env.credential.home, self.env.lease_path.parent, run / "codex-home" / "auth.json"):
            self.assertEqual(filesystem[str(path)], "deny")
        access = receipt["tool_credential_access"]
        self.assertEqual(access["network_allowed_domains"], ["openrouter.ai"])
        self.assertIs(access["network_proxy"], True)
        self.assertEqual(access["web_search"], "disabled")
        self.assertIn("no endpoint or model restriction", access["network_scope"])
        self.assertTrue(receipt["auth_link_final"]["released"])

    def test_exact_network_profile_rejects_search_config_conflicts(self):
        for level in ("global", "role"):
            for setting in ("live", "cached", "DISABLED", 1):
                roles = self.tools_roles(network_enabled=True, network_allowed_domains=["openrouter.ai"])
                extra = {"codex_config": {"web_search": setting}} if level == "global" else {}
                if level == "role":
                    roles["tools"]["codex_config"] = {"web_search": setting}
                with self.subTest(level=level, setting=setting):
                    self.assertEqual(self.refusal(roles=roles, **extra),
                                     ("config_role_network_web_search_must_be_disabled", "roles.tools"))
        roles = self.tools_roles(network_enabled=True, network_allowed_domains=["openrouter.ai"])
        cfg = self.config_with(roles=roles, codex_config={"web_search": "disabled"})
        with self.assertRaises(sup.SupervisorError) as ctx:
            sup.codex_config_tree(cfg, sup.Layout(cfg, "tools", "search"), {"web_search": "live"},
                                  cfg.role("tools")["tool_permissions"])
        self.assertEqual(ctx.exception.reason, "config_role_network_web_search_must_be_disabled")

    def test_exact_network_profile_argv_guard_pins_proxy_and_disabled_search(self):
        denied = [(["exec", "--search", "hello"], "--search"),
                  (["exec", "-c", 'web_search="live"', "hello"], "-c web_search"),
                  (["exec", "--config", 'web_search="cached"'], "--config web_search"),
                  (["exec", "-c", 'web_search.enabled=false'], "-c web_search")]
        for feature in ("network_proxy", "web_search", "web_search_cached", "web_search_request"):
            denied.extend([(["exec", "-c", "features.%s=false" % feature], "-c features." + feature),
                           (["exec", "-c", "features.%s.enabled=true" % feature], "-c features." + feature),
                           (["exec", "--enable", feature], "--enable " + feature),
                           (["exec", "--disable=" + feature], "--disable " + feature)])
        cfg = self.config_with(roles=self.tools_roles(network_enabled=True,
                                                     network_allowed_domains=["openrouter.ai"]))
        layout = sup.Layout(cfg, "tools", "netargv")
        for args, detail in denied:
            with self.subTest(args=args):
                argv = ["/x/codex", *args]
                self.assertEqual(sup.codex_profile_override(argv, restricted_network=True), detail)
                with self.assertRaises(sup.SupervisorError) as ctx:
                    sup.consumer_argv_guard(cfg, layout, cfg.role("tools"), argv)
                self.assertEqual((ctx.exception.reason, ctx.exception.detail),
                                 ("consumer_argv_overrides_permission_profile", detail))
                self.assertIsNone(sup.codex_profile_override(argv), "legacy roles keep their existing contract")
        for args in (["exec", "-c", 'web_search="disabled"'], ["exec", "-c", "web_search='disabled'"],
                     ["exec", "-c", "web_search=disabled"], ["exec", "--disable", "memories"],
                     ["exec", "--", "--search"]):
            self.assertIsNone(sup.codex_profile_override(["/x/codex", *args], restricted_network=True))

    def test_consumer_argv_guard_refuses_before_anything_is_created(self):
        fake = self.install_fake_codex()
        link = self.env.root / "codex-link"
        os.symlink(fake, link)
        (self.env.root / "copy").mkdir()
        copy = support.script(self.env.root / "copy" / "codex", "raise SystemExit(0)\n")
        self.env.reconfigure(roles=self.tools_roles())
        cases = [(str(fake), ["exec", "--sandbox", "danger-full-access", "say hi"], "--sandbox", ()),
                 (str(link), ["-c", 'sandbox_mode="danger-full-access"', "exec", "say hi"], "-c sandbox_mode", ()),
                 (str(copy), ["exec", "--profile", "other", "say hi"], "--profile", ()),
                 (str(fake), ["sandbox", "-P", ":danger-full-access", "-C", "{work_dir}", "--", "/bin/cat", "x"],
                  "-P", ("--dry-run",))]
        for index, (executable, args, detail, extra) in enumerate(cases):
            run_id = "argv%d" % index
            with self.subTest(detail=detail):
                code, result, _ = self.env.supervisor("run", "--role", "tools", "--run-id", run_id, *extra, "--",
                                                      executable, *args)
                self.assertEqual((code, result["state"], result["refusal"]),
                                 (sup.EXIT_USAGE, "CONFIG_ERROR",
                                  {"reason": "consumer_argv_overrides_permission_profile", "detail": detail}))
                self.assertFalse(self.env.run_dir("tools", run_id).exists())
        self.assertFalse(self.env.lease_path.exists(), "refused before any lease file or binding exists")
        self.assertFalse(self.env.binding().exists())
        support.observe("argv_guard_refusals", cases=len(cases), detector_cases=len(ARGV_REFUSED))

    def test_consumer_argv_guard_refuses_unsafe_project_roots(self):
        """-C/--cd and --add-dir become writable project roots: more specific than any deny above them,
        and able to rewrite the run's own config.toml and records."""
        fake = self.install_fake_codex()
        root, home = self.env.root, self.env.credential.home
        data = root / "data"
        data.mkdir()
        os.symlink(data, root / "alias")
        self.env.reconfigure(extra_denied_paths=[str(root / "extra-denied")],
                             roles=self.tools_roles(deny_paths=[str(data)], read_paths=[str(data / "public")]))

        def run_path(run_id, *parts):
            return str(self.env.run_dir("tools", run_id).joinpath(*parts))

        def lease_relative(run_id):
            return os.path.relpath(str(self.env.lease_path.parent / "x"), run_path(run_id, "work"))

        reopen, overlap = "consumer_argv_reopens_denied_path", "consumer_argv_root_overlaps_run_state"
        upper_home = str(home.parent / home.name.upper())
        cases = [("root1", ["exec", "-C", str(data / "sub"), "say hi"], reopen, "-C " + str(data / "sub")),
                 ("root2", ["exec", "--cd=" + str(root / "extra-denied"), "say hi"], reopen,
                  "--cd " + str(root / "extra-denied")),
                 ("root3", ["exec", "--add-dir", str(home / "auth.json"), "say hi"], reopen,
                  "--add-dir " + str(home / "auth.json")),
                 ("root4", ["exec", "--add-dir", lease_relative("root4"), "say hi"], reopen,
                  "--add-dir " + str(self.env.lease_path.parent / "x")),
                 ("root5", ["sandbox", "-P", PROFILE, "-C", str(data / "public"), "--", "/bin/true"], reopen,
                  "-C " + str(data / "public")),
                 ("root6", ["exec", "-C", str(root / "open"), "--add-dir", "../data/x", "say hi"], reopen,
                  "--add-dir " + str(data / "x")),
                 ("root7", ["exec", "-C", str(root / "alias" / "sub"), "say hi"], reopen,
                  "-C " + str(root / "alias" / "sub")),
                 ("root8", ["exec", "--add-dir", run_path("root8", "codex-home", "auth.json"), "say hi"], reopen,
                  "--add-dir " + run_path("root8", "codex-home", "auth.json")),
                 ("root9", ["exec", "--add-dir", upper_home + "/auth.json", "say hi"], reopen,
                  "--add-dir " + upper_home + "/auth.json"),
                 ("root10", ["exec", "-C", "/" + str(data / "sub"), "say hi"], reopen, "-C /" + str(data / "sub")),
                 ("root11", ["exec", "-C", "{run_dir}", "say hi"], overlap, "-C " + run_path("root11")),
                 ("root12", ["exec", "--add-dir", "{codex_home}/skills", "say hi"], overlap,
                  "--add-dir " + run_path("root12", "codex-home", "skills")),
                 ("root13", ["exec", "--add-dir", "{log_dir}", "say hi"], overlap,
                  "--add-dir " + run_path("root13", "state", "logs")),
                 ("root14", ["exec", "--add-dir", "/", "say hi"], overlap, "--add-dir /"),
                 ("root15", ["exec", "--add-dir", "~/../codex-home", "say hi"], overlap,
                  "--add-dir " + run_path("root15", "codex-home")),
                 ("root16", ["exec", "--add-dir", "~/" + os.path.relpath(str(home / "auth.json"),
                                                                        run_path("root16", "home")), "say hi"],
                  reopen, "--add-dir " + str(home / "auth.json")),
                 ("root17", ["exec", "--add-dir", str(self.env.runs_root / "tools" / "other" / "codex-home"),
                             "say hi"], overlap,
                  "--add-dir " + str(self.env.runs_root / "tools" / "other" / "codex-home")),
                 ("root18", ["exec", "-C", str(self.env.runs_root / "_owner"), "say hi"], overlap,
                  "-C " + str(self.env.runs_root / "_owner"))]
        for run_id, args, reason, detail in cases:
            with self.subTest(detail=detail[-48:]):
                code, result, _ = self.env.supervisor("run", "--role", "tools", "--run-id", run_id, "--dry-run",
                                                      "--", str(fake), *args)
                self.assertEqual((code, result["state"], result["refusal"]),
                                 (sup.EXIT_USAGE, "CONFIG_ERROR", {"reason": reason, "detail": detail}))
        code, result, _ = self.env.supervisor("run", "--role", "tools", "--run-id", "root99", "--", str(fake),
                                              "exec", "--add-dir", str(home / "auth.json"), "say hi")
        self.assertEqual((code, result["refusal"]["reason"]), (sup.EXIT_USAGE, reopen))
        self.assertFalse(self.env.run_dir("tools", "root99").exists())
        self.assertFalse(self.env.lease_path.exists())
        # Roots outside every deny and the run's own state, or above a deny (the more specific deny
        # still applies), are allowed.
        for run_id, args in (("roots-ok1", ["exec", "-C", str(root / "open"), "--add-dir", str(root / "other"), "x"]),
                             ("roots-ok2", ["exec", "--cd", str(home.parent), "--add-dir", "relative/dir", "x"]),
                             ("roots-ok3", ["sandbox", "-P", PROFILE, "-C", "{work_dir}", "--", "/bin/true"]),
                             ("roots-ok4", ["exec", "--add-dir", "{results}", "--add-dir", "{tmp}", "x"]),
                             ("roots-ok5", ["exec", "--add-dir", "{home}", "--add-dir", "~/notes", "x"])):
            with self.subTest(run_id=run_id):
                code, plan, _ = self.env.supervisor("run", "--role", "tools", "--run-id", run_id, "--dry-run", "--",
                                                    str(fake), *args)
                self.assertEqual((code, plan["tool_credential_access"]["consumer_argv_guard"]), (0, "checked"))
        support.observe("argv_guard_root_refusals", cases=len(cases) + 1, allowed=5)

    def judge_world(self):
        """The production Judge role: deny paths that contain the runs root (its top-level
        directory, as `/Users` does on the host, and the fake world root) plus `/Volumes`."""
        fake = self.install_fake_codex()
        top = "/" + self.env.runs_root.parts[1]
        roles = dict(self.env.values["roles"])
        roles["judge"] = dict(roles["judge"], tool_permissions={
            "extends": ":workspace", "network_enabled": False,
            "deny_paths": [top, str(self.env.root), "/Volumes"]})
        self.env.reconfigure(roles=roles, allow_shared_consumers=True)
        schema = self.env.root / "schemas" / "judge-schema.json"
        schema.parent.mkdir(mode=0o700)
        schema.write_text('{"type": "object"}')
        for deny in (top, str(self.env.root)):
            self.assertTrue(support.sup.is_within(self.env.runs_root, deny), "the deny contains the runs root")
        return fake, schema

    def test_own_directories_stay_roots_below_a_deny_that_contains_the_runs_root(self):
        """H1: the exact production Judge command line passes the guard and runs (fake Codex,
        --stdin inherit, --lease shared); so do -C ., --cd= and --add-dir {results}."""
        fake, schema = self.judge_world()
        code, plan, proc = self.env.supervisor("run", "--role", "judge", "--run-id", "jd-dry", "--stdin", "inherit",
                                               "--lease", "shared", "--dry-run", "--", str(fake), *judge_args(schema))
        self.assertEqual((code, plan["tool_credential_access"]["consumer_argv_guard"]), (0, "checked"), proc.stdout)
        required_denies = {"/" + self.env.runs_root.parts[1], str(self.env.root), "/Volumes"}
        self.assertTrue(required_denies.issubset(plan["tool_credential_access"]["denied_paths"]))
        proc = subprocess.run(self.env.supervisor_argv("run", "--role", "judge", "--run-id", "jd-run", "--stdin",
                                                       "inherit", "--lease", "shared", "--deadline-seconds", "60",
                                                       "--", str(fake), *judge_args(schema)),
                              input="Return the verdict JSON for the fake case.\n", capture_output=True, text=True,
                              env=self.env.parent_env(), cwd=str(self.env.root), timeout=180)
        receipt = support.last_json(proc.stdout)
        self.assertEqual((proc.returncode, receipt["state"], receipt["child_exit_code"]), (0, "PASS", 0), proc.stderr)
        self.assertEqual((receipt["lease"]["mode"], receipt["tool_credential_access"]["consumer_argv_guard"]),
                         ("shared", "checked"))
        run = self.env.run_dir("judge", "jd-run")
        self.assertEqual(receipt["argv"][receipt["argv"].index("-C") + 1], str(run / "work"))
        verdict = json.loads((run / "results" / "verdict.json").read_text())
        self.assertGreater(verdict["stdin_bytes"], 0, "the prompt reached the consumer on stdin")
        variants = [("jd-dot", ("-C", ".")), ("jd-cd-eq", ("--cd={work_dir}",)),
                    ("jd-add-results", ("--add-dir", "{results}")),
                    ("jd-mixed", ("-C", "{work_dir}", "--add-dir", "../results", "--add-dir", "{tmp}",
                                  "--add-dir", "~", "--add-dir", "~/notes")),
                    ("jd-inside", ("--cd={work_dir}/sub", "--add-dir", "{results}/out"))]
        for run_id, root_args in variants:
            with self.subTest(run_id=run_id):
                code, plan, proc = self.env.supervisor("run", "--role", "judge", "--run-id", run_id, "--stdin",
                                                       "inherit", "--lease", "shared", "--dry-run", "--", str(fake),
                                                       *judge_args(schema, root_args))
                self.assertEqual((code, plan["tool_credential_access"]["consumer_argv_guard"]), (0, "checked"),
                                 proc.stdout)
        support.observe("argv_guard_own_dirs_below_ancestor_deny", judge_exact="PASS", variants=len(variants))

    def test_other_directories_below_a_deny_that_contains_the_runs_root_stay_refused(self):
        """H1: only the run's own work, results, tmp and home directories are exempt."""
        fake, schema = self.judge_world()
        root, home = self.env.root, self.env.credential.home
        os.symlink(home, root / "alias")
        reopen, overlap = "consumer_argv_reopens_denied_path", "consumer_argv_root_overlaps_run_state"

        def run_path(run_id, *parts):
            return str(self.env.run_dir("judge", run_id).joinpath(*parts))

        cases = [("jx1", ("-C", str(root / "elsewhere")), reopen, "-C " + str(root / "elsewhere")),
                 ("jx2", ("--add-dir", "{codex_home}"), reopen, "--add-dir " + run_path("jx2", "codex-home")),
                 ("jx3", ("--add-dir", "{run_dir}"), reopen, "--add-dir " + run_path("jx3")),
                 ("jx4", ("--add-dir", "{log_dir}"), reopen, "--add-dir " + run_path("jx4", "state", "logs")),
                 ("jx5", ("-C", "{work_dir}/../codex-home"), reopen, "-C " + run_path("jx5", "codex-home")),
                 ("jx6", ("--add-dir", "~/../state"), reopen, "--add-dir " + run_path("jx6", "state")),
                 ("jx7", ("--add-dir", str(home)), reopen, "--add-dir " + str(home)),
                 ("jx8", ("--add-dir", str(root / "alias")), reopen, "--add-dir " + str(root / "alias")),
                 ("jx9", ("-C", str(self.env.run_dir("judge", "other-run") / "work")), reopen,
                  "-C " + str(self.env.run_dir("judge", "other-run") / "work")),
                 ("jx10", ("--add-dir", "{results}/../../other-run/results"), reopen,
                  "--add-dir " + str(self.env.run_dir("judge", "other-run") / "results")),
                 ("jx11", ("--add-dir", "/"), overlap, "--add-dir /")]
        for run_id, root_args, reason, detail in cases:
            with self.subTest(run_id=run_id):
                code, result, _ = self.env.supervisor("run", "--role", "judge", "--run-id", run_id, "--dry-run",
                                                      "--", str(fake), *judge_args(schema, root_args))
                self.assertEqual((code, result["state"], result["refusal"]),
                                 (sup.EXIT_USAGE, "CONFIG_ERROR", {"reason": reason, "detail": detail}))
        # A root that is inside an own directory as written but resolves elsewhere is not exempt.
        cfg = self.env.cfg()
        layout = sup.Layout(cfg, "judge", "jx-resolved")
        layout.work_dir.mkdir(mode=0o700, parents=True)
        os.symlink(str(root / "elsewhere"), layout.work_dir / "escape")
        denied = sup.permission_denials(cfg, layout) + cfg.role("judge")["tool_permissions"]["deny_paths"]
        violation = sup.codex_project_root_violation([str(fake), "exec", "--add-dir", "escape", "x"], layout,
                                                     denied, cfg.runs_root)
        self.assertEqual(violation, (reopen, "--add-dir " + str(layout.work_dir / "escape")))
        self.assertIsNone(sup.codex_project_root_violation([str(fake), "exec", "-C", str(layout.work_dir),
                                                            "--add-dir", ".", "x"], layout, denied, cfg.runs_root))
        support.observe("argv_guard_other_dirs_below_ancestor_deny", refused=len(cases) + 1)

    def test_the_own_directory_exemption_needs_the_exact_spelling(self):
        """N5: run ids may differ only in letter case, and on a case-sensitive runs root `JX-CASE` and
        `jx-case` are two runs. Only the exact spelling of this run's own directories is exempt; any
        other spelling is checked like every other root (on a case-insensitive volume it names the
        same directory, so this can only refuse more)."""
        fake, schema = self.judge_world()
        reopen = "consumer_argv_reopens_denied_path"
        upper = self.env.run_dir("judge", "JX-CASE")
        for run_id, root_args, detail in (("jx-case", ("-C", str(upper / "work")), "-C " + str(upper / "work")),
                                          ("jx-case", ("--add-dir", str(upper / "results")),
                                           "--add-dir " + str(upper / "results"))):
            with self.subTest(detail=detail[-24:]):
                code, result, _ = self.env.supervisor("run", "--role", "judge", "--run-id", run_id, "--dry-run",
                                                      "--", str(fake), *judge_args(schema, root_args))
                self.assertEqual((code, result.get("state"), result.get("refusal")),
                                 (sup.EXIT_USAGE, "CONFIG_ERROR", {"reason": reopen, "detail": detail}))
        code, plan, _ = self.env.supervisor("run", "--role", "judge", "--run-id", "jx-case", "--dry-run", "--",
                                            str(fake), *judge_args(schema, ("-C", "{work_dir}")))
        self.assertEqual((code, plan["tool_credential_access"]["consumer_argv_guard"]), (0, "checked"))
        support.observe("argv_guard_own_dirs_exact_spelling", other_case_refused=2, exact_allowed=1)

    def test_consumer_argv_guard_resolves_the_executable_like_the_launch(self):
        """A bare name is looked up in the role's PATH and a name is matched in any case; an executable
        that cannot be resolved is inspected as Codex."""
        fake = self.install_fake_codex()
        bin_dir = self.env.root / "bin"
        bin_dir.mkdir()
        os.symlink(fake, bin_dir / "codex-0.162.1")
        (self.env.root / "upper").mkdir()
        upper = support.script(self.env.root / "upper" / "CODEX", "raise SystemExit(0)\n")
        release = support.script(self.env.root / "upper" / "codex-latest", "raise SystemExit(0)\n")
        roles = self.tools_roles()
        roles["tools"]["path_env"] = str(bin_dir) + ":/usr/bin:/bin"
        self.env.reconfigure(roles=roles)
        refused = [("exe1", ["codex-0.162.1", "exec", "--sandbox", "danger-full-access", "say hi"], "--sandbox"),
                   ("exe2", [str(upper), "exec", "-p", "other", "say hi"], "-p"),
                   ("exe3", ["no-such-tool-anywhere", "--yolo"], "--yolo"),
                   ("exe6", [str(release), "exec", "--ignore-user-config", "say hi"], "--ignore-user-config")]
        for run_id, argv, detail in refused:
            with self.subTest(argv=argv[0]):
                code, result, _ = self.env.supervisor("run", "--role", "tools", "--run-id", run_id, "--dry-run", "--",
                                                      *argv)
                self.assertEqual((code, result["refusal"]),
                                 (sup.EXIT_USAGE, {"reason": "consumer_argv_overrides_permission_profile",
                                                   "detail": detail}))
        code, plan, _ = self.env.supervisor("run", "--role", "tools", "--run-id", "exe4", "--dry-run", "--", "echo",
                                            "--sandbox", "danger-full-access")
        self.assertEqual((code, plan["tool_credential_access"]["consumer_argv_guard"]), (0, "not_a_codex_command"))
        code, plan, _ = self.env.supervisor("run", "--role", "tools", "--run-id", "exe5", "--dry-run", "--",
                                            "codex-0.162.1", "exec", "say hi")
        self.assertEqual((code, plan["tool_credential_access"]["consumer_argv_guard"]), (0, "checked"))

    def test_consumer_argv_guard_allows_the_supervisor_profile_and_out_of_scope_consumers(self):
        fake = self.install_fake_codex()
        self.env.reconfigure(roles=self.tools_roles())
        code, receipt, proc = self.env.supervisor("run", "--role", "tools", "--run-id", "ok1", "--", str(fake),
                                                  "-c", 'model="fake-model"', "--version")
        self.assertEqual((code, receipt["state"], receipt["child_exit_code"]), (0, "PASS", 0), proc.stderr)
        self.assertEqual(receipt["argv"], [str(fake), "-c", 'model="fake-model"', "--version"])
        self.assertEqual(receipt["tool_credential_access"]["consumer_argv_guard"], "checked")
        code, plan, _ = self.env.supervisor("run", "--role", "tools", "--run-id", "ok2", "--dry-run", "--", str(fake),
                                            "sandbox", "--permission-profile", PROFILE, "-C", "{work_dir}", "--",
                                            "/bin/cat", "-s")
        self.assertEqual((code, plan["tool_credential_access"]["consumer_argv_guard"]), (0, "checked"))
        self.assertEqual(plan["argv"][5], str(self.env.run_dir("tools", "ok2") / "work"))
        # Out of scope: a program that is not Codex, and roles without a generated profile.
        code, receipt, _ = self.env.supervisor("run", "--role", "tools", "--run-id", "ok3", "--", "/bin/echo",
                                               "--sandbox", "danger-full-access")
        self.assertEqual((code, receipt["tool_credential_access"]["consumer_argv_guard"]),
                         (0, "not_a_codex_command"))
        code, receipt, _ = self.env.supervisor("run", "--role", "dev", "--run-id", "ok4", "--", str(fake),
                                               "--sandbox", "read-only", "--version")
        self.assertEqual((code, receipt["state"], receipt["tool_credential_access"]["consumer_argv_guard"]),
                         (0, "PASS", "not_applicable"))
        support.observe("argv_guard_allowed", launched=3, detector_cases=len(ARGV_ALLOWED))


if __name__ == "__main__":
    unittest.main()
