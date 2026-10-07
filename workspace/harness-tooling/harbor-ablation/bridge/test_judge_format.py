"""Synthetic formatter contracts only; no benchmark source or rows are executed."""
import importlib.util
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("owned_judge_format", Path(__file__).with_name("judge_format.py"))
formatter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(formatter)

SCHEMA = {"type": "object", "properties": {"equivalent": {"type": "boolean"},
          "abstained": {"type": "boolean"}, "reason": {"type": "string"}},
          "required": ["equivalent", "abstained", "reason"], "additionalProperties": False}


def source(cap=12000):
    # Generic formatting markers, not the original rubric or task examples.
    return ("raise RuntimeError('upstream must never execute')\n"
            "PROMPT = '{question}|{answer}|{answer_format}|{response}'\n"
            f"SCHEMA = {SCHEMA!r}\nMAX_RESPONSE_CHARS = {cap}\nMODEL = 'source-history'\nEFFORT = 'source-effort'\n").encode()


def reference(answer="synthetic"):
    return json.dumps({"schema_version": "gptgrep.judge.reference.v1", "question": "  alpha\t\u001c beta\u2003gamma  ",
                       "answer": answer, "answer_format": "generic"}, ensure_ascii=False).encode()


class FormatterTests(unittest.TestCase):
    def output(self, text="response", answer="synthetic", cap=12000):
        return json.loads(formatter.format_buffers(source(cap), reference(answer), json.dumps({"answer": text}).encode()))

    def test_inert_ast_and_python_whitespace(self):
        self.assertEqual(self.output()["prompt"], "alpha beta gamma|synthetic|generic|response")

    def test_codepoint_truncation_and_original_artifact_digest(self):
        text = "中🙂x"
        result = self.output(text, cap=2)
        self.assertEqual(result["prompt"].split("|")[-1], "中🙂")
        self.assertEqual(result["original_response_characters"], 3)
        self.assertEqual(result["used_response_characters"], 2)
        self.assertTrue(result["response_truncated"])
        self.assertEqual(result["answer_sha256"], formatter.digest(json.dumps({"answer": text}).encode()))

    def test_python_reference_value_string_semantics(self):
        self.assertIn("|[True, None, 'text']|", self.output(answer=[True, None, "text"])["prompt"])

    def test_duplicate_missing_and_nonliteral_source_refused(self):
        for value in [source() + b"PROMPT = 'duplicate'\n", source().replace(b"EFFORT = 'source-effort'", b""),
                      source().replace(b"MODEL = 'source-history'", b"MODEL = str('expression')")]:
            with self.assertRaises((ValueError, TypeError)):
                formatter.format_buffers(value, reference(), b'{"answer":"response"}')

    def test_schema_and_cap_fail_closed(self):
        for value in [source(0), source(12001), source().replace(b"'additionalProperties': False", b"'additionalProperties': True"),
                      source().replace(b"'equivalent': {'type': 'boolean'}", b"'equivalent': {'type': 'string'}")]:
            with self.assertRaises(ValueError):
                formatter.format_buffers(value, reference(), b'{"answer":"response"}')

    def test_reference_and_answer_unknown_missing_empty_duplicate_refused(self):
        for answer in [b'{}', b'{"answer":false}', b'{"answer":" "}', b'{"answer":"one","answer":"two"}',
                       b'{"answer":"response","citation":[]}']:
            with self.assertRaises(ValueError):
                formatter.format_buffers(source(), reference(), answer)
        bad = json.loads(reference()); bad["source_document"] = "forbidden"
        with self.assertRaises(ValueError):
            formatter.format_buffers(source(), json.dumps(bad).encode(), b'{"answer":"response"}')

    def test_source_input_and_output_limits(self):
        with self.assertRaises(ValueError):
            formatter.format_buffers(b"x" * (formatter.MAX_SOURCE + 1), reference(), b'{"answer":"response"}')
        with self.assertRaises(ValueError):
            formatter.format_buffers(source(), reference(), b"x" * (formatter.MAX_ARTIFACT + 1))
        value = source().replace(b"'{question}|{answer}|{answer_format}|{response}'", repr("x" * formatter.MAX_OUTPUT).encode())
        with self.assertRaises(ValueError):
            formatter.format_buffers(value, reference(), b'{"answer":"response"}')

    def test_framing_refuses_oversize_partial_and_preserves_bytes(self):
        value = b"payload"
        self.assertEqual(formatter.read_frame(io.BytesIO(struct.pack("<Q", len(value)) + value), 16), value)
        for data in [b"short", struct.pack("<Q", 17), struct.pack("<Q", 7) + b"short"]:
            with self.assertRaises(ValueError):
                formatter.read_frame(io.BytesIO(data), 16)

    def test_fresh_request_and_validation_modes_under_owned_child_umasks(self):
        binary = os.environ.get("GPTGREP_JUDGE_TEST_BINARY")
        if not binary:
            self.skipTest("explicit newly built judge request binary required for mode fixture")
        python_sha = formatter.digest(Path(sys.executable).read_bytes())
        with tempfile.TemporaryDirectory() as scratch:
            scratch = Path(scratch).resolve()
            for mask in (0o000, 0o777):
                root = scratch / str(mask)
                root.mkdir(mode=0o700)
                original, ref, answer = root / "source.py", root / "reference.json", root / "answer.json"
                original.write_bytes(source())
                ref.write_bytes(reference())
                answer.write_bytes(b'{"answer":"synthetic response"}')
                output = root / "request"
                args = [binary, "prepare", "--judge-source", str(original), "--judge-source-sha256", formatter.digest(original.read_bytes()),
                        "--original-revision", "1" * 40, "--reference", str(ref), "--reference-sha256", formatter.digest(ref.read_bytes()),
                        "--answer", str(answer), "--answer-sha256", formatter.digest(answer.read_bytes()), "--python", sys.executable,
                        "--python-sha256", python_sha, "--output-dir", str(output)]
                result = subprocess.run(args, env={"LANG": "C.UTF-8", "LC_ALL": "C.UTF-8"}, cwd=root,
                                        umask=mask, capture_output=True, timeout=20, check=False)
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertEqual(output.stat().st_mode & 0o777, 0o700)
                self.assertEqual(set(p.name for p in output.iterdir()),
                                 {"request-manifest.json", "request.json", "judge-source.py", "reference.json", "answer.json"})
                for path in output.iterdir():
                    self.assertEqual(path.stat().st_mode & 0o777, 0o600)
                request = json.loads((output / "request.json").read_bytes())
                formatted = request["formatted"]
                verdict = {"schema_version": "gptgrep.judge.verdict.v1", "request_sha256": formatter.digest((output / "request.json").read_bytes()),
                           **{name: formatted[name] for name in ("judge_source_sha256", "reference_sha256", "answer_sha256", "verdict_schema_sha256")},
                           "verdict": {"equivalent": True, "abstained": True, "reason": ""}}
                verdict_path = root / "verdict.json"
                verdict_path.write_bytes(json.dumps(verdict).encode())
                validation = root / "validation"
                result = subprocess.run([binary, "validate-verdict", "--request-root", str(output),
                                         "--request-manifest-sha256", formatter.digest((output / "request-manifest.json").read_bytes()),
                                         "--verdict", str(verdict_path), "--verdict-sha256", formatter.digest(verdict_path.read_bytes()),
                                         "--python", sys.executable, "--python-sha256", python_sha, "--output-dir", str(validation)],
                                        env={"LANG": "C.UTF-8", "LC_ALL": "C.UTF-8"}, cwd=root, umask=mask,
                                        capture_output=True, timeout=20, check=False)
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertEqual(validation.stat().st_mode & 0o777, 0o700)
                self.assertEqual((validation / "validation.json").stat().st_mode & 0o777, 0o600)
                receipt = json.loads((validation / "validation.json").read_bytes())
                self.assertEqual(receipt["judge_status"], "unavailable")
                self.assertIs(receipt["reward_emitted"], False)


if __name__ == "__main__":
    unittest.main()
