"""Synthetic Harbor interface fixtures only; no Codex/runtime/auth/provider calls."""
import asyncio
import hashlib
import json
import os
import shlex
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from harbor.environments.base import ExecResult
from harbor.models.agent.context import AgentContext
from gptgrep_harbor_agent import FrozenCodexAgent


PROMPT = "SYNTHETIC QUESTION\n$(touch PWNED) `echo PWNED` ' \" --model other"
DIGEST = "c"*64
PHASE = {"id":"MOCK-phase", "origin_receipt_sha256":"d"*64,
         "clock_namespace":"synthetic-only", "issued_monotonic_ns":1,
         "deadline_monotonic_ns":2, "deadline_unix_ms":3}
PROFILE = {"model":"gpt-6-luna", "reasoning_effort":"max", "service_tier":"fast"}


class FakeEnvironment:
    """No real environment inheritance or external operations."""
    def __init__(self):
        self.calls = []
        self.uploads = []
        self.fail = False
        self.transport_error = False
        self.missing_usage = False
        self.change_phase = False

    async def exec(self, command, **kwargs):
        self.calls.append((command, kwargs))
        args = shlex.split(command)
        if "--run" not in args:
            return ExecResult(return_code=0, stdout=json.dumps({"schema_version":1,
                "status":"admitted_source_only", "control_sha256":DIGEST, "requested":PROFILE,
                "phase":PHASE, "prompt_path":"/logs/agent/phase/prompt.txt", "output_dir":"/logs/agent/phase",
                "prompt_sha256":hashlib.sha256(PROMPT.encode()).hexdigest(), "execution_qualified":False}))
        if self.transport_error:
            raise RuntimeError("synthetic transport loss")
        obs = {"input_tokens":11, "cached_input_tokens":2, "output_tokens":3,
               "completed_turns":1, "unique_items":1}
        return ExecResult(return_code=9 if self.fail else 0, stdout=json.dumps({"schema_version":1,
            "status":"failed" if self.fail else "answered_ungraded", "failure":"native_nonzero" if self.fail else None,
            "control_sha256":DIGEST, "phase":{**PHASE, "deadline_unix_ms":99} if self.change_phase else PHASE,
            "observations":{} if self.missing_usage else obs, "native_exit_code":9 if self.fail else 0, "native_signal":None}))

    async def upload_file(self, source_path, target_path):
        source = Path(source_path)
        self.uploads.append((target_path, source.read_bytes(), stat.S_IMODE(source.stat().st_mode)))

    def __getattr__(self, name):
        raise AssertionError(f"unsupported environment operation: {name}")


class AgentTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.logs = Path(self.temp.name).resolve()
        self.env = FakeEnvironment()

    def tearDown(self):
        self.temp.cleanup()

    def agent(self, **kwargs):
        options = {"launcher_path":"/app/harness/gptgrep-codex-native-agent", "control_path":"/logs/agent/phase/control.json",
                   "control_sha256":DIGEST, "user":"agent", "transport_timeout_sec":30}
        options.update(kwargs)
        return FrozenCodexAgent(self.logs, model_name="gpt-6-luna", **options)

    async def test_setup_is_check_only_no_install_auth_config_or_environment_fallback(self):
        agent = self.agent()
        with patch.object(FrozenCodexAgent, "_env_sources", side_effect=AssertionError("host fallback forbidden")):
            await agent.setup(self.env)
        self.assertEqual(len(self.env.calls), 1)
        command, kwargs = self.env.calls[0]
        self.assertNotIn("--run", command)
        self.assertNotIn("install", command)
        self.assertEqual(kwargs["env"], {})
        self.assertEqual(kwargs["user"], "agent")
        self.assertEqual(self.env.uploads, [])
        self.assertFalse(agent.capabilities.atif)
        self.assertIsNone(agent.MODEL_CONNECTION)

    async def test_instruction_is_exact_file_bytes_and_never_shell_command(self):
        agent = self.agent()
        await agent.setup(self.env)
        context = AgentContext()
        await agent.run(PROMPT, self.env, context)
        self.assertEqual(self.env.uploads, [("/logs/agent/phase/prompt.txt", PROMPT.encode(), 0o600)])
        self.assertNotIn(PROMPT, self.env.calls[-1][0])
        self.assertEqual(shlex.split(self.env.calls[-1][0])[-1], "--run")
        self.assertEqual(context.n_input_tokens, 11)
        self.assertIsNone(context.cost_usd)
        self.assertIsNone(context.model_usage)
        self.assertTrue(all(v is None for v in context.metadata["native_agent"]["actual"].values()))
        self.assertEqual(context.metadata["native_agent"]["phase"], PHASE)
        self.assertEqual(stat.S_IMODE((self.logs/"native-adapter").stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE((self.logs/"native-adapter/receipt.json").stat().st_mode), 0o600)
        self.assertEqual(len(self.env.calls), 2)

    async def test_missing_usage_stays_null_not_zero(self):
        agent = self.agent()
        self.env.missing_usage = True
        await agent.setup(self.env)
        context = AgentContext()
        await agent.run(PROMPT, self.env, context)
        self.assertIsNone(context.n_input_tokens)
        self.assertIsNone(context.n_cache_tokens)
        self.assertIsNone(context.n_output_tokens)
        self.assertIsNone(context.cost_usd)

    async def test_nonzero_retains_partial_usage_and_never_relaunches(self):
        agent = self.agent()
        self.env.fail = True
        await agent.setup(self.env)
        context = AgentContext()
        with self.assertRaisesRegex(RuntimeError, "no retry"):
            await agent.run(PROMPT, self.env, context)
        self.assertEqual(context.n_input_tokens, 11)
        with self.assertRaises(RuntimeError):
            await agent.run(PROMPT, self.env, context)
        self.assertEqual(len(self.env.calls), 2)
        self.assertEqual(len(self.env.uploads), 1)

    async def test_transport_unknown_never_relaunches_and_context_not_empty(self):
        agent = self.agent()
        self.env.transport_error = True
        await agent.setup(self.env)
        context = AgentContext()
        with self.assertRaises(RuntimeError):
            await agent.run(PROMPT, self.env, context)
        self.assertFalse(context.is_empty())
        self.assertIsNone(context.metadata["native_agent"]["paid_request_count"])
        with self.assertRaises(RuntimeError):
            await agent.run(PROMPT, self.env, context)
        self.assertEqual(len(self.env.calls), 2)

    async def test_wrong_prompt_refused_before_upload_or_launch(self):
        agent = self.agent()
        await agent.setup(self.env)
        with self.assertRaises(ValueError):
            await agent.run(PROMPT+" changed", self.env, AgentContext())
        self.assertEqual(len(self.env.calls), 1)
        self.assertEqual(self.env.uploads, [])

    async def test_changed_original_phase_refused(self):
        agent = self.agent()
        self.env.change_phase = True
        await agent.setup(self.env)
        context = AgentContext()
        with self.assertRaises(ValueError):
            await agent.run(PROMPT, self.env, context)
        self.assertEqual(context.metadata["native_agent"]["status"], "transport_or_admission_unknown")

    async def test_setup_and_run_cannot_be_reused(self):
        agent = self.agent()
        await agent.setup(self.env)
        with self.assertRaises(RuntimeError):
            await agent.setup(self.env)
        await agent.run(PROMPT, self.env, AgentContext())
        with self.assertRaises(RuntimeError):
            await agent.run(PROMPT, self.env, AgentContext())

    async def test_log_symlink_refused_without_host_scan(self):
        target = self.logs/"target"
        target.mkdir()
        alias = self.logs/"alias"
        alias.symlink_to(target, target_is_directory=True)
        agent = self.agent()
        agent.logs_dir = alias
        await agent.setup(self.env)
        with self.assertRaises(ValueError):
            await agent.run(PROMPT, self.env, AgentContext())
        self.assertEqual(self.env.uploads, [])

    def test_unsupported_controls_and_missing_model_fail_before_environment_effects(self):
        for field, value in [("config",{}),("version","latest"),("resume",True),("skills_dir","/skills"),
                             ("mcp_servers",[{}]),("extra_env",{"ANY":"SYNTHETIC"}),("load_trajectory","/old"),
                             ("override_setup_timeout_sec",100),("user","root")]:
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.agent(**{field:value})
        with self.assertRaises(ValueError):
            FrozenCodexAgent(self.logs)
        self.assertEqual(self.env.calls, [])

    def test_private_modes_under_owned_subprocess_umasks(self):
        for mask in (0o000, 0o077):
            with tempfile.TemporaryDirectory(dir=self.logs) as directory:
                result = subprocess.run([sys.executable, "-B", __file__, "--mode-fixture", directory, str(mask)],
                                        env={"PATH":"/usr/bin:/bin", "TMPDIR":str(self.logs)},
                                        capture_output=True, text=True, timeout=30, check=False)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual(json.loads(result.stdout),{"directory_mode":448,"receipt_mode":384})


async def mode_fixture(directory, mask):
    # Only this fresh subprocess changes its umask, never the parent process/global config.
    os.umask(mask)
    logs = Path(directory).resolve()
    agent = FrozenCodexAgent(logs,model_name="gpt-6-luna",launcher_path="/app/harness/gptgrep-codex-native-agent",
                            control_path="/logs/agent/phase/control.json",control_sha256=DIGEST,
                            user="agent",transport_timeout_sec=30)
    environment = FakeEnvironment()
    await agent.setup(environment)
    await agent.run(PROMPT,environment,AgentContext())
    print(json.dumps({"directory_mode":stat.S_IMODE((logs/"native-adapter").stat().st_mode),
                      "receipt_mode":stat.S_IMODE((logs/"native-adapter/receipt.json").stat().st_mode)}))


if __name__ == "__main__":
    if len(sys.argv)==4 and sys.argv[1]=="--mode-fixture":
        asyncio.run(mode_fixture(sys.argv[2],int(sys.argv[3])))
    else:
        unittest.main(verbosity=2)
