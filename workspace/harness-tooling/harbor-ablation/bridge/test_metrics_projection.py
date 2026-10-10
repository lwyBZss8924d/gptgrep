import ast
from pathlib import Path
import re
from types import SimpleNamespace
import unittest


def selected_populate():
    source = Path(__file__).with_name("gptgrep_harbor_agent.py")
    tree = ast.parse(source.read_text())
    owner = next(node for node in tree.body
                 if isinstance(node, ast.ClassDef) and node.name == "FrozenCodexAgent")
    method = next(node for node in owner.body
                  if isinstance(node, ast.FunctionDef) and node.name == "populate")
    namespace = {"re": re}
    exec(compile(ast.Module(body=[method], type_ignores=[]), str(source), "exec"), namespace)
    return namespace["populate"]


class MetricsProjection(unittest.TestCase):
    def project(self, latency):
        owner = SimpleNamespace(_admission={"requested": {}, "phase": {}},
                                options=SimpleNamespace(control_sha256="e" * 64))
        context = SimpleNamespace()
        receipt = {"observations": {}, "status": "answered_ungraded", "failure": None,
                   "native_exit_code": 0, "native_signal": None, "latency_ms": latency}
        result = selected_populate()(owner, receipt, context)
        self.assertEqual(context.metadata["native_agent"], result)
        return result

    def test_preserves_component_without_inventing_query_span(self):
        result = self.project(25)
        self.assertEqual(result["native_latency_ms"], 25)
        self.assertEqual(result["native_latency_scope"], "native_execution_component")
        self.assertIsNone(result["query_wall_ms"])
        self.assertEqual(result["query_wall_scope"], "unavailable")

    def test_missing_component_stays_unknown(self):
        self.assertIsNone(self.project(None)["native_latency_ms"])

    def test_rejects_invalid_numeric_observation(self):
        for invalid in [True, -1, 1.5, 2**64]:
            with self.subTest(value=invalid), self.assertRaises(ValueError):
                self.project(invalid)


if __name__ == "__main__":
    unittest.main()
