"""Minimal non-live container qualification for the Harbor gate (F1/F8, CTR-05 parts).

Runs only in run_qualification.py's unguarded pass with --docker-bin, --docker-host
and --docker-image (a local image with python3; never pulled). The Docker CLI
cannot reach the daemon's socket under the seatbelt guard. Every container uses
`--network none`, `--pull never`, `--rm`, a label unique to the test and a FAKE
credential bound read-only at /opt/full62-home/auth.json, as full62 does.

Qualifies against a real Docker engine (not Harbor itself):
* the supervisor's label gate: the label is seen, the lease is held until the
  container is gone, and an outliving container gives UNKNOWN with the guardian
  holding the lease;
* the read-only single-file bind refuses writes, and an in-place host rewrite
  (the refresh owner's save) is visible inside a running container;
* on Linux a connect to 127.0.0.1:0 fails before sending (the port-0 blocker).
"""
import json
import os
import subprocess
import time
import unittest

import support
from support import PY, FIXTURES, Env, sup

DOCKER_BIN = os.environ.get("GPTGREP_AUTH_TEST_DOCKER_BIN")
DOCKER_HOST = os.environ.get("GPTGREP_AUTH_TEST_DOCKER_HOST")
IMAGE = os.environ.get("GPTGREP_AUTH_TEST_DOCKER_IMAGE")
CONSUMER = str(FIXTURES / "container_consumer.py")


@unittest.skipUnless(support.SANDBOX == "proxy-trap" and DOCKER_BIN and DOCKER_HOST and IMAGE,
                     "runs only in the unguarded pass with --docker-bin/--docker-host/--docker-image")
class ContainerGateTests(unittest.TestCase):
    def setUp(self):
        self.env = Env("ctr")
        support.assert_fake_path(self.env.credential.home)
        self.label = "gptgrep.auth-job=ws-auth-qual-" + self.env.nonce
        roles = dict(self.env.values["roles"])
        roles["harbor"] = {"tool_permissions": "consumer-managed", "passthrough_env": ["DOCKER_HOST"],
                           "path_env": os.path.dirname(DOCKER_BIN) + ":/usr/bin:/bin:/usr/sbin:/sbin",
                           "stop_scope": "root-first", "term_grace_seconds": 60}
        self.env.reconfigure(roles=roles, docker={"bin": DOCKER_BIN, "env": {"DOCKER_HOST": DOCKER_HOST},
                                                  "wait_seconds": 60, "poll_seconds": 0.5})
        self.env.credential.write(1, time.time() + 3600, in_place=False)

    def tearDown(self):
        env = {"PATH": "/usr/bin:/bin", "DOCKER_HOST": DOCKER_HOST, "HOME": str(self.env.home)}
        ids = subprocess.run([DOCKER_BIN, "ps", "-a", "-q", "--filter", "label=" + self.label], env=env,
                             capture_output=True, text=True, timeout=60).stdout.split()
        if ids:
            subprocess.run([DOCKER_BIN, "rm", "-f", *ids], env=env, capture_output=True, timeout=60)

    def consume(self, run_id, mode, seconds, wait_seconds=None):
        if wait_seconds is not None:
            docker = dict(self.env.values["docker"], wait_seconds=wait_seconds)
            self.env.reconfigure(docker=docker)
        ready = self.env.root / ("ready-" + run_id)
        args = ["run", "--role", "harbor", "--run-id", run_id, "--harbor-job-label", self.label,
                "--deadline-seconds", "180", "--", PY, "-I", "-B", CONSUMER, DOCKER_BIN, IMAGE, self.label,
                str(self.env.credential.path), mode, str(seconds), "{results}/container.json", str(ready)]
        return self.env.popen(*args, env=self.env.parent_env(DOCKER_HOST=DOCKER_HOST)), ready

    def test_label_gate_holds_the_lease_until_the_container_is_gone(self):
        proc, _ready = self.consume("ctr-pass", "sleep", 6)
        out, err = proc.communicate(timeout=240)
        receipt = support.last_json(out)
        self.assertEqual(receipt["state"], "PASS", err[-1500:])
        docker = receipt["docker"]
        self.assertTrue(docker["label_matched"] and docker["cleared"], docker)
        self.assertGreaterEqual(docker["max_seen_during_run"], 1)
        self.assertGreater(receipt["timings"]["finished_at"], receipt["timings"]["child_exited_at"])
        support.observe("container_label_gate_pass", max_seen=docker["max_seen_during_run"], polls=docker["polls"])

    def test_outliving_container_keeps_the_lease_with_the_guardian(self):
        proc, _ready = self.consume("ctr-unknown", "sleep", 15, wait_seconds=2)
        out, err = proc.communicate(timeout=240)
        receipt = support.last_json(out)
        self.assertEqual(receipt["state"], "UNKNOWN", err[-1500:])
        self.assertTrue(receipt["docker"]["label_matched"])
        self.assertFalse(receipt["docker"]["cleared"])
        self.assertEqual(self.env.lease_probe(), "held_exclusive", "the guardian holds the lease")
        self.assertTrue(support.wait_for(lambda: self.env.lease_probe() == "free", 120, 1))
        guardian = json.loads((self.env.run_dir("harbor", "ctr-unknown") / "state" /
                               "lease-guardian-result.json").read_text())
        self.assertEqual(guardian["docker_label"], self.label)
        support.observe("container_outlives_child_guardian", released_after_container=True)

    def test_read_only_bind_in_place_adoption_and_linux_port_zero(self):
        proc, ready = self.consume("ctr-sem", "semantics", 30)
        self.assertTrue(support.wait_for(ready.exists, 120), "container did not report READY")
        ino = self.env.credential.meta()["ino"]
        self.env.credential.write(222, time.time() + 3600, in_place=True)  # the owner's in-place save
        self.assertEqual(self.env.credential.meta()["ino"], ino)
        out, err = proc.communicate(timeout=240)
        receipt = support.last_json(out)
        result = json.loads((self.env.run_dir("harbor", "ctr-sem") / "results" / "container.json").read_text())
        inside = result["inside"]
        self.assertGreater(inside["initial_size"], 0)
        self.assertEqual(inside["write_errno"], "EROFS", "the trial cannot write the credential")
        self.assertTrue(inside["host_rewrite_seen"], "a host in-place save is visible through the bind")
        self.assertEqual(inside["port0"], "ECONNREFUSED", "Linux refuses port 0 before sending")
        # The write happened during this consumer run, so the supervisor cannot attribute it.
        self.assertEqual(receipt["state"], "QUARANTINED")
        code, _, _ = self.env.supervisor("clear-quarantine", "--run-id", "ctr-clear", "--confirm", "--reason",
                                         "fake drill: simulated owner write during container check")
        self.assertEqual(code, 0)
        support.observe("container_semantics", write_errno=inside["write_errno"],
                        host_rewrite_seen=inside["host_rewrite_seen"], seen_after_seconds=inside["seen_after_seconds"],
                        port0=inside["port0"])


if __name__ == "__main__":
    unittest.main()
