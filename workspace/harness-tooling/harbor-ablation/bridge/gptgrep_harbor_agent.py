"""Harbor 0.24 glue for an externally provisioned, one-shot standard Codex exec.

No installer, account resolver, host config/auth copies or model-connection fallback.
The outer owner must qualify Linux/runtime/auth/mounts/resources before a real trial.
"""
import hashlib
import json
import os
import re
import shlex
import stat
import tempfile
from pathlib import Path, PurePosixPath

from pydantic import ConfigDict, Field, field_validator

from harbor.agents.base import BaseAgent
from harbor.agents.capabilities import AgentCapabilities
from harbor.agents.options import AgentOptions
from harbor.models.agent.context import AgentContext


def remote_path(value: str) -> str:
    path = PurePosixPath(value)
    if not path.is_absolute() or str(path) != value or ".." in path.parts or "\\" in value or "\x00" in value:
        raise ValueError("absolute canonical sandbox path required")
    return value


class NativeOptions(AgentOptions):
    model_config = ConfigDict(extra="forbid", strict=True)
    launcher_path: str
    control_path: str
    control_sha256: str
    user: str
    # This limits transport only; the Rust owner-supplied absolute phase is never renewed.
    transport_timeout_sec: int = Field(ge=1, le=86400)

    @field_validator("launcher_path", "control_path")
    @classmethod
    def path(cls, value):
        return remote_path(value)

    @field_validator("control_sha256")
    @classmethod
    def digest(cls, value):
        if re.fullmatch("[0-9a-f]{64}", value) is None:
            raise ValueError("SHA256 required")
        return value

    @field_validator("user")
    @classmethod
    def nonroot(cls, value):
        if value == "root" or re.fullmatch("[a-z_][a-z0-9_-]{0,63}", value) is None:
            raise ValueError("explicit nonroot sandbox user required")
        return value


def private_new(path: Path, data: bytes):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "wb") as file:
        file.write(data)
        file.flush()
        os.fsync(file.fileno())


def owned_parent(path: Path):
    if not path.is_absolute():
        raise ValueError("local log root must be absolute")
    for parent in [*reversed(path.parents), path]:
        metadata = parent.lstat()
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
            raise ValueError("local log ancestor custody")
    if path.stat().st_uid != os.geteuid():
        raise ValueError("local log owner")


def bounded_object(text: str | None):
    if text is None or len(text.encode("utf-8")) > 65536:
        raise ValueError("bounded launcher receipt required")
    value = json.loads(text)
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise ValueError("launcher receipt schema")
    return value


class FrozenCodexAgent(BaseAgent):
    capabilities = AgentCapabilities()
    MODEL_CONNECTION = None
    options_model = NativeOptions

    @staticmethod
    def name():
        return "gptgrep-frozen-codex"

    def version(self):
        return "0.0.0-source-candidate"

    @classmethod
    def parse_options(cls, kwargs=None, env=None):
        if env:
            raise ValueError("extra environment is unsupported")
        # Explicit validation, no inherited Env/Cli or process-environment fallback.
        return NativeOptions.model_validate(kwargs or {})

    def __init__(self, logs_dir, model_name=None, logger=None, mcp_servers=None, skills_dir=None,
                 *, extra_env=None, load_trajectory=None, environment_logs_dir=None, **kwargs):
        if model_name != "gpt-6-luna":
            raise ValueError("explicit SPEC reader model gpt-6-luna required")
        if mcp_servers or skills_dir is not None or extra_env or load_trajectory is not None:
            raise ValueError("MCP, Skills, extra env and trajectory loading are unsupported")
        if "override_setup_timeout_sec" in kwargs:
            raise ValueError("setup timeout override is unsupported")
        super().__init__(logs_dir, model_name, logger, environment_logs_dir=environment_logs_dir, **kwargs)
        self._state = "new"
        self._admission = None

    def command(self, run=False):
        args = [self.options.launcher_path, "--control", self.options.control_path,
                "--control-sha256", self.options.control_sha256]
        if run:
            args.append("--run")
        # Harbor exposes command:str. Only fixed admitted paths/digest appear here.
        # Instruction bytes never appear in the shell command.
        return shlex.join(args)

    async def setup(self, environment):
        if self._state != "new":
            raise RuntimeError("setup is one-shot")
        self._state = "setup-started"
        result = await environment.exec(self.command(), timeout_sec=self.options.transport_timeout_sec,
                                        user=self.options.user, env={})
        receipt = bounded_object(result.stdout)
        if result.return_code != 0 or receipt.get("status") != "admitted_source_only" or receipt.get("control_sha256") != self.options.control_sha256:
            self._state = "failed"
            raise RuntimeError("source admission failed")
        if receipt.get("requested") != {"model":"gpt-6-luna", "reasoning_effort":"max", "service_tier":"fast"}:
            raise ValueError("requested reader profile mismatch")
        remote_path(receipt["prompt_path"])
        remote_path(receipt["output_dir"])
        if re.fullmatch("[0-9a-f]{64}", receipt["prompt_sha256"]) is None:
            raise ValueError("prompt digest required")
        self._admission = receipt
        self._state = "admitted"

    def populate(self, value, context):
        # Only typed numeric observations and fixed requested/unknown fields enter context.
        observations = value.get("observations", {})
        if not isinstance(observations, dict):
            raise ValueError("observations schema")
        clean = {}
        for name in ["completed_turns", "unique_items", "input_tokens", "cached_input_tokens",
                     "cache_write_input_tokens", "output_tokens", "reasoning_output_tokens"]:
            item = observations.get(name)
            if item is not None and (type(item) is not int or item < 0 or item > 2**64-1):
                raise ValueError("observations number")
            clean[name] = item
        status = value.get("status")
        if status not in ("answered_ungraded", "failed"):
            raise ValueError("terminal status")
        failure = value.get("failure")
        if failure is not None and (not isinstance(failure, str) or re.fullmatch("[a-z_]{1,64}", failure) is None):
            raise ValueError("failure code")
        if (status == "answered_ungraded" and failure is not None) or (status == "failed" and failure is None):
            raise ValueError("status/failure mismatch")
        for key in ("native_exit_code", "native_signal"):
            number = value.get(key)
            if number is not None and (type(number) is not int or not 0 <= number <= 255):
                raise ValueError("exit observation")
        context.n_input_tokens = clean["input_tokens"]
        context.n_cache_tokens = clean["cached_input_tokens"]
        context.n_output_tokens = clean["output_tokens"]
        context.cost_usd = None
        context.model_usage = None  # Returned model identity is absent from basic exec JSONL.
        summary = {"status":status, "failure":failure, "requested":self._admission["requested"],
                   "actual":{key:None for key in ("model", "provider", "reasoning_effort", "service_tier", "paid_request_count", "cost_usd")},
                   "observations":clean, "native_exit_code":value.get("native_exit_code"), "native_signal":value.get("native_signal"),
                   "execution_qualified":False, "control_sha256":self.options.control_sha256,
                   "clock_scope":"external phase metadata; not a Jev RunClock join", "phase":self._admission["phase"]}
        context.metadata = {"native_agent":summary}
        return summary

    async def run(self, instruction, environment, context: AgentContext):
        if self._state != "admitted":
            raise RuntimeError("admitted one-shot setup required")
        self._state = "run-started"  # No retry after transport/launch failure.
        context.metadata = {"native_agent":{"status":"transport_or_admission_unknown", "execution_qualified":False,
                                             "paid_request_count":None, "cost_usd":None}}
        prompt = instruction.encode("utf-8")
        if not prompt or len(prompt) > 65536 or hashlib.sha256(prompt).hexdigest() != self._admission["prompt_sha256"]:
            raise ValueError("frozen prompt mismatch")
        owned_parent(self.logs_dir)
        local = self.logs_dir / "native-adapter"
        local.mkdir(mode=0o700)  # Fresh only; never chmod caller-owned directories.
        with tempfile.TemporaryDirectory(prefix="prompt-", dir=local) as directory:
            source = Path(directory) / "prompt.txt"
            private_new(source, prompt)
            await environment.upload_file(source, self._admission["prompt_path"])
        try:
            result = await environment.exec(self.command(True), timeout_sec=self.options.transport_timeout_sec,
                                            user=self.options.user, env={})
            value = bounded_object(result.stdout)
            if value.get("control_sha256") != self.options.control_sha256 or value.get("phase") != self._admission["phase"]:
                raise ValueError("terminal control or original phase mismatch")
            summary = self.populate(value, context)
            private_new(local / "receipt.json", (json.dumps(summary, sort_keys=True)+"\n").encode())
            self._state = "terminal"
            if result.return_code != 0 or summary["status"] != "answered_ungraded":
                raise RuntimeError("native execution failed; no retry")
        except Exception:
            self._state = "failed"
            if context.is_empty():
                context.metadata = {"native_agent":{"status":"transport_or_admission_unknown", "execution_qualified":False,
                                                     "paid_request_count":None, "cost_usd":None}}
            raise
