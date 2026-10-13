"""Harbor 0.24 BaseAgent front door; no package installation at trial time."""
from __future__ import annotations

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from .common import ARMS, MAX_ANSWER, PROFILE, put, require

# Only the treatment's GPTgrep host process uses this job-level key, for its
# Decisions requests. Harbor applies an agent's environment to every command the
# agent runs in the trial container, setup included, with precedence over each
# command's own environment. The agent therefore withholds the key until setup
# has read the trial's arm, and then exposes it to treatment trials only.
PRODUCT_ONLY_ENV = ("OPENROUTER_API_KEY",)
SETUP_COMMAND = ("test -f /opt/full62/reader.json && test -d /data/corpus && "
                 "test ! -e /tests && test ! -e /solution && "
                 "python3 -B -c 'import json; print(json.load(open(\"/opt/full62/reader.json\"))[\"binding\"][\"arm\"])'")


class PairedNativeAgent(BaseAgent):
    _arm = None

    @staticmethod
    def name():
        return "gptgrep-full62-native"

    def version(self):
        return "0.1.0"

    @property
    def extra_env(self):
        """Harbor's exec overlay: the provider key only after setup found a treatment trial."""
        env = dict(self._extra_env)
        if self._arm != "gptgrep":
            for key in PRODUCT_ONLY_ENV:
                env.pop(key, None)
        return env

    async def setup(self, environment: BaseEnvironment):
        require(self.model_name == PROFILE["model"], "harbor_model_mismatch")
        require(not self.mcp_servers and not self.skills_dir and self.load_trajectory is None,
                "unmatched_agent_extension")
        result = await environment.exec(SETUP_COMMAND, timeout_sec=10)
        arm = result.stdout.strip() if isinstance(result.stdout, str) else None
        require(result.return_code == 0 and arm in ARMS, "reader_image_contract")
        self._arm = arm

    async def run(self, instruction: str, environment: BaseEnvironment, context: AgentContext):
        require(len(instruction.encode()) <= MAX_ANSWER, "instruction_size")
        source = self.logs_dir / "instruction.txt"
        put(source.absolute(), instruction.encode())
        await environment.upload_file(source, "/tmp/full62-instruction.txt")
        result = await environment.exec(
            "python3 -B -m gptgrep_harbor.reader --instruction /tmp/full62-instruction.txt",
            cwd="/data/corpus", env=self.extra_env)
        context.metadata = {"requested_profile": PROFILE, "reader_exit": result.return_code,
                            "all_attempt_accounting": "agent/operational.json",
                            "served_profile_verified": False}
        require(result.return_code == 0, "reader_wrapper_failed")
