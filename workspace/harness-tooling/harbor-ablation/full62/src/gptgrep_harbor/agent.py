"""Harbor 0.24 BaseAgent front door; no package installation at trial time."""
from __future__ import annotations

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from .common import MAX_ANSWER, PROFILE, put, require


class PairedNativeAgent(BaseAgent):
    @staticmethod
    def name():
        return "gptgrep-full62-native"

    def version(self):
        return "0.1.0"

    async def setup(self, environment: BaseEnvironment):
        require(self.model_name == PROFILE["model"], "harbor_model_mismatch")
        require(not self.mcp_servers and not self.skills_dir and self.load_trajectory is None,
                "unmatched_agent_extension")
        result = await environment.exec(
            "test -f /opt/full62/reader.json && test -d /data/corpus && "
            "test ! -e /tests && test ! -e /solution",
            timeout_sec=10)
        require(result.return_code == 0, "reader_image_contract")

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
