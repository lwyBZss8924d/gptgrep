"""Explicit evaluator-host verifier for an already prepared finite native phase.

This is a two-phase pilot: collect answers, then root freezes exact preparation
and authority. It never prepares a live phase from a template or invokes readers.
"""
import asyncio
import hashlib
import json
import os
from pathlib import Path
import signal
import stat

from harbor.models.task.config import ArtifactConfig
from harbor.models.verifier.result import VerifierResult
from harbor.trial.artifact_handler import artifact_host_path
from harbor.verifier.base import BaseVerifier

CAP = 65536


def digest(data):
    return hashlib.sha256(data).hexdigest()


def pairs(items):
    result = {}
    for key, value in items:
        if key in result:
            raise ValueError("duplicate_field")
        result[key] = value
    return result


def decode(data):
    return json.loads(data, object_pairs_hook=pairs,
                      parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite")))


def exact(value, keys):
    if not isinstance(value, dict) or set(value) != set(keys):
        raise ValueError("typed_binding")


class Snapshot:
    """Bounded held no-follow custody; no recursive filesystem discovery."""
    def __init__(self, path, expected=None, cap=CAP):
        self.path = Path(path)
        if not self.path.is_absolute() or ".." in self.path.parts:
            raise ValueError("absolute_path")
        directory = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        try:
            for part in self.path.parts[1:-1]:
                child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                                dir_fd=directory)
                os.close(directory)
                directory = child
            self.fd = os.open(self.path.name,
                              os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK,
                              dir_fd=directory)
        finally:
            os.close(directory)
        try:
            info = os.fstat(self.fd)
            if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= cap:
                raise ValueError("input_bound")
            self.identity = (info.st_dev, info.st_ino)
            self.cap = cap
            self.data = self.read()
            self.sha = digest(self.data)
            if expected is not None and self.sha != expected:
                raise ValueError("input_digest")
        except BaseException:
            self.close()
            raise

    def read(self):
        os.lseek(self.fd, 0, os.SEEK_SET)
        data = bytearray()
        while True:
            chunk = os.read(self.fd, min(4096, self.cap + 1 - len(data)))
            if not chunk:
                break
            data.extend(chunk)
            if len(data) > self.cap:
                raise ValueError("input_bound")
        return bytes(data)

    def fresh(self):
        with Snapshot(self.path, self.sha, self.cap) as now:
            if now.identity != self.identity or self.read() != self.data:
                raise ValueError("input_changed")

    def close(self):
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


class OriginalNativeJudgeVerifier(BaseVerifier):
    def __init__(self, *, phase_map, phase_map_sha256, **kwargs):
        super().__init__(**kwargs)
        self.phase_map = phase_map
        self.phase_map_sha = phase_map_sha256

    async def _invoke(self, broker, control, control_sha):
        # Exact program pin and fixed argv only. No shell, inherited secrets or account RPC.
        child = await asyncio.create_subprocess_exec(
            broker, "execute-app-server", "--control", control,
            "--control-sha256", control_sha, "--run",
            stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE, env={"PATH": "/usr/bin:/bin"}, start_new_session=True)

        async def bounded(pipe, retain):
            result = bytearray()
            total = 0
            while data := await pipe.read(4096):
                total += len(data)
                if total > CAP:
                    raise ValueError("broker_output_bound")
                if retain:
                    result.extend(data)
            return bytes(result)

        async def finish():
            out, _ = await asyncio.gather(bounded(child.stdout, True), bounded(child.stderr, False))
            code = await child.wait()
            if code != 0:
                raise ValueError("broker_unavailable")
            return decode(out)

        work = asyncio.create_task(asyncio.wait_for(finish(), 180))
        try:
            return await asyncio.shield(work)
        except asyncio.CancelledError:
            # Let the broker's original <=120s guard stop its owned native group.
            # Cancellation does not grant another invocation or a reward.
            try:
                await asyncio.shield(work)
            except BaseException:
                pass
            raise
        except BaseException:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            await child.wait()
            # A forced broker stop is lifecycle-unknown, never a provider terminal claim.
            raise

    async def verify(self):
        held = []
        def snapshot(path, sha=None, cap=CAP):
            value = Snapshot(path, sha, cap)
            held.append(value)
            return value
        try:
            plan = snapshot(self.phase_map, self.phase_map_sha)
            mapping = decode(plan.data)
            exact(mapping, ["schema_version", "broker", "entries"])
            if mapping["schema_version"] != "gptgrep.harbor.native-judge-map.v1" \
                    or not isinstance(mapping["entries"], list) or not 1 <= len(mapping["entries"]) <= 8:
                raise ValueError("phase_map")
            trial = str(self.trial_paths.trial_dir)
            task = str(self.task.paths.task_dir)
            keys = ["trial_dir", "task_dir", "step_name", "control", "control_sha256",
                    "answer_sha256", "reference_sha256"]
            for entry in mapping["entries"]:
                exact(entry, keys)
            matches = [entry for entry in mapping["entries"]
                       if entry["trial_dir"] == trial and entry["task_dir"] == task
                       and entry["step_name"] == self.step_name]
            if len(matches) != 1:
                raise ValueError("finite_trial_binding")
            entry = matches[0]
            artifact = ArtifactConfig(source="/app/answer.json", destination="answer.json")
            artifacts = (self.trial_paths.step_artifacts_dir(self.step_name)
                         if self.step_name else self.trial_paths.artifacts_dir)
            manifest = decode(snapshot(artifacts / "manifest.json").data)
            if not isinstance(manifest, list) or len(manifest) > 128:
                raise ValueError("artifact_manifest")
            matches = [item for item in manifest if isinstance(item, dict)
                       and item.get("destination") == "answer.json"]
            if len(matches) != 1 or any(matches[0].get(key) != value for key, value in
                                       {"source": "/app/answer.json", "type": "file", "status": "ok"}.items()) \
                    or matches[0].get("service") not in (None, "main"):
                raise ValueError("artifact_provenance")
            answer = snapshot(artifact_host_path(artifacts, artifact), entry["answer_sha256"])
            value = decode(answer.data)
            exact(value, ["answer"])
            if not isinstance(value["answer"], str) or not value["answer"].strip():
                raise ValueError("answer_contract")
            tests = (self.task.paths.step_tests_dir(self.step_name)
                     if self.step_name else self.task.paths.tests_dir)
            reference = snapshot(tests / "reference.json", entry["reference_sha256"])
            ref = decode(reference.data)
            exact(ref, ["question", "answer", "answer_format"])
            converted = json.dumps({"schema_version": "gptgrep.judge.reference.v1", **ref},
                                   ensure_ascii=False, sort_keys=True, separators=(",", ":"),
                                   allow_nan=False).encode()
            control_snapshot = snapshot(entry["control"], entry["control_sha256"])
            control = decode(control_snapshot.data)
            if control["schema_version"] != "gptgrep.judge.native-control.v2" or control["role"] != "verifier_only":
                raise ValueError("control_contract")
            phase = decode(snapshot(control["phase"]["binding"]["path"],
                                    control["phase"]["binding"]["sha256"]).data)
            if phase["trial_id"] != Path(trial).name or phase["task_id"] != Path(task).name \
                    or phase["answer_sha256"] != answer.sha or phase["reference_sha256"] != digest(converted):
                raise ValueError("collected_phase_binding")
            prepared = Path(control["request"]["root"])
            snapshot(prepared / "request-manifest.json", control["request"]["manifest_sha256"])
            snapshot(prepared / "answer.json", answer.sha)
            snapshot(prepared / "reference.json", digest(converted))
            snapshot(prepared / "judge-source.py", phase["judge_source_sha256"], 1024 * 1024)
            snapshot(prepared / "request.json", phase["request_sha256"], 1024 * 1024)
            broker_spec = mapping["broker"]
            exact(broker_spec, ["path", "sha256", "bytes"])
            if type(broker_spec["bytes"]) is not int or not 0 < broker_spec["bytes"] <= 64 * 1024 * 1024:
                raise ValueError("broker_bound")
            broker = snapshot(broker_spec["path"], broker_spec["sha256"], broker_spec["bytes"])
            if len(broker.data) != broker_spec["bytes"]:
                raise ValueError("broker_bound")
            for value in held:
                value.fresh()
            result = await self._invoke(str(broker.path), str(control_snapshot.path), control_snapshot.sha)
            for value in held:
                value.fresh()
            report = result.get("report", {})
            observations = report.get("observations", {})
            verdict = report.get("verdict", {})
            if result.get("schema_version") != "gptgrep.judge.app-server-result.v1" \
                    or result.get("publication_status") != "published" \
                    or not isinstance(result.get("publication_sha256"), str) \
                    or result.get("no_retry") is not True or report.get("status") != "completed_actual" \
                    or report.get("judge_status") != "completed" \
                    or report.get("control_sha256") != control_snapshot.sha \
                    or report.get("answer_sha256") != answer.sha \
                    or report.get("reference_sha256") != digest(converted) \
                    or report.get("judge_source_sha256") != phase["judge_source_sha256"] \
                    or report.get("request_sha256") != phase["request_sha256"] \
                    or report.get("request_manifest_sha256") != control["request"]["manifest_sha256"] \
                    or observations.get("observation_origin") != "native_app_server_v2" \
                    or any(observations.get(key) is not True for key in
                           ("wrapper_spawned", "wrapper_leader_reaped", "pipe_eof_observed", "terminal_sequence_observed")) \
                    or any(not isinstance(observations.get(key), str) or not observations[key] for key in
                           ("transport_thread_id", "native_turn_id", "transport_final_item_id")) \
                    or type(verdict.get("equivalent")) is not bool or type(verdict.get("abstained")) is not bool:
                raise ValueError("completed_verdict_required")
            output = Path(control["output_root"])
            observed = snapshot(output / "app-server-observation.json", result["publication_sha256"])
            if decode(observed.data) != report:
                raise ValueError("published_report_binding")
            raw = snapshot(output / "native-final.json", report["raw_final_sha256"], 8192)
            final = decode(raw.data)
            exact(final, ["equivalent", "abstained", "reason"])
            if type(final["equivalent"]) is not bool or type(final["abstained"]) is not bool \
                    or not isinstance(final["reason"], str) \
                    or any(final[key] != verdict[key] for key in ("equivalent", "abstained")):
                raise ValueError("private_verdict_binding")
            # Original semantic correctness; abstention is observed independently.
            return VerifierResult(rewards={"reward": 1.0 if verdict["equivalent"] else 0.0})
        except (Exception, asyncio.CancelledError):
            # Never print remote errors, reason, gold, prompt, account payloads or path data.
            return VerifierResult(rewards=None)
        finally:
            for value in reversed(held):
                value.close()
