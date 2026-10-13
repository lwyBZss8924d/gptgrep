"""Owner-approved reader seccomp profile: vendored upstream, derivation, provenance, README
acceptance record, runtime pin and generated compose. No engine; Docker's CLI only for the opt-in
offline compose render."""
from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import pwd
import re
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from gptgrep_harbor.runtime import READER_SECCOMP_BYTES, READER_SECCOMP_SHA256, reader_security_opt
import test_harbor_runtime as fixtures

SECCOMP = Path(__file__).resolve().parents[1] / "seccomp"
README = SECCOMP.parent / "README.md"
APPROVED_SHA256 = "769a5dbb42b7b10edb07b516be7077e4a602a214a4c55ec892abd6d323bab48b"
ENGINE_COMMIT = "daa0cb7f23594cdbcce5002e370027d3fd36ffd7"
# Acceptance probe 1 verbatim as the owner approved it, and as amended after the kernel refused
# bubblewrap's fresh /proc mount inside a reader container.
APPROVED_PROBE_1 = "bwrap --unshare-user --unshare-pid --ro-bind / / --proc /proc --dev /dev true"
AMENDED_PROBE_1 = "bwrap --unshare-user --unshare-pid --ro-bind / / --dev /dev true"


# Generic workstation path markers that no public source may carry.
MACHINE_PATH_MARKERS = ("/Users/", "/home/", "/private/")


def account_name():
    """The current account name, read at run time; no name is ever written into this source."""
    try:
        return pwd.getpwuid(os.getuid()).pw_name
    except (KeyError, OSError):
        return None


def account_name_patterns(name):
    """Where an account name leaks into text: a path component, a home shorthand or an identity.

    A bare word is no leak. Common account names such as "user", "docker",
    "owner", "github", "harbor" or "default" are ordinary words in the vendored
    upstream profile, its provenance and the derived rule comment.
    """
    escaped = re.escape(name)
    return (r"[/\\]" + escaped + r"(?![A-Za-z0-9_.-])",   # /srv/NAME/profile.json, C:\Users\NAME\, /x/NAME
            r"~" + escaped + r"(?![A-Za-z0-9_.-])",       # ~NAME/profile.json
            r"(?<![A-Za-z0-9_.+-])" + escaped + r"@")     # NAME@host, NAME@example.org


def assert_no_private_markers(case, text):
    """Fail on a machine path marker or the current account name, without echoing the name."""
    for marker in MACHINE_PATH_MARKERS:
        case.assertNotIn(marker, text)
    name = account_name()
    # Shorter names would also match ordinary path segments. The name counts only
    # in a path, home or identity shape, so an account such as "root" does not
    # match the syscall pivot_root and "user" does not match "user namespaces".
    if name and len(name) >= 4:
        found = any(re.search(pattern, text) for pattern in account_name_patterns(name))
        case.assertFalse(found, "the current account name appears in public source")


def load_derive():
    spec = importlib.util.spec_from_file_location("full62_reader_seccomp_derive", SECCOMP / "derive.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


derive = load_derive()
SEVEN = frozenset(derive.SYSCALLS)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def rules_by_name(profile):
    """Each syscall's rules without their name lists, in profile order."""
    result = {}
    for rule in profile["syscalls"]:
        conditions = {key: value for key, value in rule.items() if key != "names"}
        for name in rule["names"]:
            result.setdefault(name, []).append(conditions)
    return result


class ReaderSeccompProfileTests(unittest.TestCase):
    def setUp(self):
        self.upstream_bytes = (SECCOMP / "upstream/default.json").read_bytes()
        self.derived_bytes = (SECCOMP / "gptgrep-reader-seccomp.json").read_bytes()
        self.upstream = json.loads(self.upstream_bytes)
        self.derived = json.loads(self.derived_bytes)
        self.provenance = json.loads((SECCOMP / "provenance.json").read_bytes())

    def test_vendored_upstream_license_and_profile_match_their_provenance_pins(self):
        vendored = self.provenance["upstream"]["vendored"]
        self.assertEqual(vendored["path"], "upstream/default.json")
        self.assertEqual((len(self.upstream_bytes), sha(self.upstream_bytes)),
                         (vendored["bytes"], vendored["sha256"]))
        self.assertEqual((derive.UPSTREAM_BYTES, derive.UPSTREAM_SHA256), (vendored["bytes"], vendored["sha256"]))
        license_pin = self.provenance["upstream"]["license"]
        license_bytes = (SECCOMP / license_pin["path"]).read_bytes()
        self.assertEqual((len(license_bytes), sha(license_bytes)), (license_pin["bytes"], license_pin["sha256"]))
        self.assertEqual(license_pin["spdx"], "Apache-2.0")
        self.assertIn(b"Apache License\n                           Version 2.0, January 2004", license_bytes)
        derived = self.provenance["derived"]
        self.assertEqual((derived["path"], derived["derivation"]), ("gptgrep-reader-seccomp.json", "derive.py"))
        self.assertEqual((len(self.derived_bytes), sha(self.derived_bytes)), (derived["bytes"], derived["sha256"]))

    def test_derivation_reproduces_the_committed_profile_exactly(self):
        first = derive.derive(self.upstream_bytes)
        self.assertEqual(first, self.derived_bytes)
        self.assertEqual(derive.derive(self.upstream_bytes), first)
        self.assertEqual(sha(first), APPROVED_SHA256)
        self.assertTrue(first.endswith(b"}\n") and first.isascii())

    def test_derivation_cli_checks_the_committed_bytes_and_never_overwrites(self):
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            self.assertEqual(derive.main(["--check"]), 0)
        report = json.loads(stdout.getvalue())
        self.assertEqual((report["derived_sha256"], report["matches_committed"]), (APPROVED_SHA256, True))
        with tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"]) as temporary:
            output = Path(temporary) / "reader-profile.json"
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(derive.main(["--upstream", str(SECCOMP / "upstream/default.json"),
                                              "--output", str(output)]), 0)
            self.assertEqual(output.read_bytes(), self.derived_bytes)
            with self.assertRaises(FileExistsError), contextlib.redirect_stdout(io.StringIO()):
                derive.main(["--output", str(output)])
            self.assertEqual(output.read_bytes(), self.derived_bytes)
            changed = Path(temporary) / "changed.json"
            changed.write_bytes(self.derived_bytes.replace(b'"SCMP_ACT_ALLOW"', b'"SCMP_ACT_LOG"', 1))
            stdout = io.StringIO()
            with patch.object(derive, "DERIVED", changed), contextlib.redirect_stdout(stdout):
                self.assertEqual(derive.main(["--check"]), 1)
            self.assertIs(json.loads(stdout.getvalue())["matches_committed"], False)

    def test_derivation_accepts_only_the_pinned_upstream_bytes(self):
        same_length = self.upstream_bytes.replace(b'"defaultErrnoRet": 1', b'"defaultErrnoRet": 2', 1)
        self.assertEqual(len(same_length), len(self.upstream_bytes))
        for data in (self.upstream_bytes + b"\n", same_length, self.derived_bytes, b""):
            with self.subTest(size=len(data)), self.assertRaisesRegex(ValueError, "upstream_profile_pin"):
                derive.derive(data)

    def test_profile_differs_from_upstream_only_by_the_seven_syscalls(self):
        self.assertEqual(SEVEN, {"unshare", "clone", "clone3", "setns", "mount", "umount2", "pivot_root"})
        self.assertEqual(list(self.derived), list(self.upstream))
        for key in self.upstream:
            if key != "syscalls":
                self.assertEqual(self.derived[key], self.upstream[key], key)
        # Every derived rule but the last is an upstream rule, in upstream order,
        # with only the seven names removed; no rule gains or loses a condition.
        *kept, added = self.derived["syscalls"]
        remaining = iter(self.upstream["syscalls"])
        for rule in kept:
            for original in remaining:
                stripped = {**original, "names": [name for name in original["names"] if name not in SEVEN]}
                if stripped["names"]:
                    self.assertEqual(rule, stripped)
                    break
            else:
                self.fail("derived rule without an upstream origin: %r" % rule["names"])
        self.assertEqual([rule for rule in remaining if set(rule["names"]) - SEVEN], [])
        # The only addition: one unconditional allow for exactly the seven.
        self.assertEqual(added, {"names": list(derive.SYSCALLS), "action": "SCMP_ACT_ALLOW",
                                 "comment": derive.COMMENT})
        derived_rules = rules_by_name(self.derived)
        for name in SEVEN:
            self.assertEqual(derived_rules[name], [{"action": "SCMP_ACT_ALLOW", "comment": derive.COMMENT}], name)
        upstream_rules = rules_by_name(self.upstream)
        self.assertEqual(set(derived_rules), set(upstream_rules) | {"pivot_root"})
        # The provenance names exactly the upstream rules that held any of the seven.
        change = self.provenance["change"]
        touched = [index for index, rule in enumerate(self.upstream["syscalls"]) if SEVEN & set(rule["names"])]
        self.assertEqual([record["upstream_index"] for record in change["removed"]], touched)
        for record in change["removed"]:
            rule = self.upstream["syscalls"][record["upstream_index"]]
            self.assertEqual(record["names"], [name for name in rule["names"] if name in SEVEN])
            self.assertEqual(record["rule_kept_for_other_names"], bool(set(rule["names"]) - SEVEN))
        self.assertEqual(change["not_in_upstream"], sorted(SEVEN - set(upstream_rules)))
        self.assertEqual(change["not_in_upstream"], ["pivot_root"])

    def test_no_capability_or_arch_condition_is_broadened_for_any_other_syscall(self):
        upstream_rules, derived_rules = rules_by_name(self.upstream), rules_by_name(self.derived)
        others = set(upstream_rules) - SEVEN
        self.assertEqual(set(derived_rules) - SEVEN, others)
        for name in sorted(others):
            # Same actions, errno values, argument filters and capability/arch
            # includes and excludes, in the same order: nothing is relaxed.
            self.assertEqual(derived_rules[name], upstream_rules[name], name)
        # Capability-gated upstream rules keep their exact gates for every other name.
        for rule in self.derived["syscalls"][:-1]:
            self.assertFalse(SEVEN & set(rule["names"]), rule["names"])
        gated = [rule for rule in self.derived["syscalls"] if "includes" in rule or "excludes" in rule]
        self.assertTrue(gated)
        self.assertTrue(all(set(rule["names"]).isdisjoint(SEVEN) for rule in gated))
        self.assertNotIn("includes", self.derived["syscalls"][-1])
        self.assertNotIn("excludes", self.derived["syscalls"][-1])
        self.assertNotIn("args", self.derived["syscalls"][-1])

    def test_provenance_records_engine_upstream_change_and_owner_scope(self):
        value = self.provenance
        self.assertEqual(value["schema"], "gptgrep.harbor.reader-seccomp-provenance.v1")
        self.assertEqual((value["engine"]["version"], value["engine"]["source_tag"], value["engine"]["source_commit"]),
                         ("29.4.0", "docker-v29.4.0", ENGINE_COMMIT))
        self.assertEqual((value["upstream"]["commit"], value["upstream"]["path"]),
                         (ENGINE_COMMIT, "vendor/github.com/moby/profiles/seccomp/default.json"))
        self.assertEqual((value["upstream"]["module"], value["upstream"]["module_version"]),
                         ("github.com/moby/profiles/seccomp", "v0.1.0"))
        self.assertEqual(value["syscalls"], list(derive.SYSCALLS))
        approval = value["approval"]
        self.assertEqual(approval["owner_approved_utc_date"], "2026-10-13")
        self.assertIn("(owner-approved 2026-10-13)", derive.COMMENT)
        self.assertIn("two full62 reader roles only", approval["scope"])
        self.assertIn("Judge and verifier containers keep Docker's default seccomp profile", approval["unchanged"])
        self.assertIn("no added capabilities", approval["unchanged"])
        # Public source carries no workstation paths or identities.
        for data in (json.dumps(value), self.derived_bytes.decode()):
            assert_no_private_markers(self, data)

    def test_private_marker_guard_reads_the_account_name_at_run_time(self):
        name = account_name()
        if not name or len(name) < 4:
            self.skipTest("the current account name is unavailable or shorter than four characters")
        # A path or an identity with the name fails; the failure message omits it.
        for text in ("synthetic /srv/" + name + "/profile.json", name + "@synthetic-host"):
            with self.assertRaises(AssertionError) as caught:
                assert_no_private_markers(self, text)
            self.assertFalse(name in str(caught.exception), "the failure message echoes the account name")
        assert_no_private_markers(self, "pivot_" + name)
        for marker in MACHINE_PATH_MARKERS:
            with self.assertRaises(AssertionError):
                assert_no_private_markers(self, "synthetic " + marker + "x")

    def test_private_marker_guard_passes_clean_files_under_common_account_names(self):
        # Ordinary words in the committed public files are no leak, whatever the
        # account is called; the same names in a path, home or identity shape still fail.
        public = (json.dumps(self.provenance), self.derived_bytes.decode())
        for name in ("user", "docker", "owner", "github", "harbor", "default"):
            with self.subTest(name=name), patch(__name__ + ".account_name", return_value=name):
                for text in public:
                    assert_no_private_markers(self, text)
                assert_no_private_markers(self, "unprivileged " + name + "/mount namespaces, " + name + ".json")
                for leak in ("/srv/" + name + "/profile.json", "C:\\Users\\" + name + "\\profile.json",
                             "/srv/" + name, "~" + name + "/profile.json", name + "@synthetic-host",
                             "contact " + name + "@example.org"):
                    with self.assertRaises(AssertionError, msg=leak):
                        assert_no_private_markers(self, leak)


class ReaderSeccompReadmeTests(unittest.TestCase):
    """The README's acceptance record never presents an amended probe as the approved one,
    and records which probes the owner has accepted."""

    def setUp(self):
        text = README.read_text(encoding="utf-8")
        section = text.split("\n### Reader seccomp profile\n", 1)[1].split("\n### ", 1)[0]
        record = section.split("\n**Acceptance probes.**", 1)[1]
        self.probes = dict(re.findall(r"^(\d)\. (.+)$", record, flags=re.M))
        self.status = record.rstrip().rsplit("\n\n", 1)[1]

    def test_amended_probe_keeps_the_approved_form_and_is_not_reported_as_passed_as_approved(self):
        self.assertEqual(sorted(self.probes), ["1", "2", "3", "4", "5"])
        first = self.probes["1"]
        self.assertTrue(first.startswith("**Amended"), first)
        # Both forms stay visible: the approved one with its observed refusal, and the amendment.
        self.assertIn("`%s`" % APPROVED_PROBE_1, first)
        self.assertIn("`%s`" % AMENDED_PROBE_1, first)
        self.assertIn("refuses", first)
        # The owner accepted the amended criterion (bwrap without --proc) on 2026-10-13.
        self.assertRegex(first, r"owner accepted the amended criterion on 2026-10-13")
        self.assertNotRegex(first, r"not yet")
        # The status never counts probe 1 among the probes that passed as approved.
        self.assertNotRegex(self.status, r"Probes 1\s*(?:[-–]|to)\s*\d passed")
        self.assertRegex(self.status, r"Probe 1 passed[^.]*\bamended form")
        self.assertRegex(self.status, r"owner accepted the amended criterion on 2026-10-13")
        # Probes 1-3 are accepted; probes 4 and 5 stay pending.
        self.assertRegex(self.status, r"probes 1[–-]3 are therefore accepted")
        self.assertRegex(self.status, r"Probes 4 and 5 stay pending")
        self.assertNotRegex(self.status, r"acknowledg")


class ReaderSeccompMaterializationTests(unittest.TestCase):
    """The runtime pin and the generated per-role compose files; synthetic fixtures."""

    def setUp(self):
        fixtures.HarborMaterializationTests.setUp(self)
        self.approved = (SECCOMP / "gptgrep-reader-seccomp.json").read_bytes()
        self.broadened = self.approved.replace(b'"SCMP_ACT_ERRNO"', b'"SCMP_ACT_ALLOW"', 1)
        self.assertEqual(len(self.broadened), len(self.approved))

    def materializer(self):
        return fixtures.HarborMaterializationTests.materializer(self)

    def test_runtime_pins_the_committed_profile_and_the_template_leaves_it_unselected(self):
        self.assertEqual((READER_SECCOMP_BYTES, READER_SECCOMP_SHA256), (len(self.approved), sha(self.approved)))
        self.assertEqual(READER_SECCOMP_SHA256, APPROVED_SHA256)
        derived = json.loads((SECCOMP / "provenance.json").read_bytes())["derived"]
        self.assertEqual((derived["bytes"], derived["sha256"]), (READER_SECCOMP_BYTES, READER_SECCOMP_SHA256))
        template = json.loads((SECCOMP.parent / "runtime.template.json").read_bytes())
        self.assertIn("reader_seccomp_profile", template)
        self.assertIsNone(template["reader_seccomp_profile"])
        self.runtime["reader_seccomp_profile"] = template["reader_seccomp_profile"]
        with self.assertRaisesRegex(ValueError, "reader_seccomp_profile_required"):
            self.materializer()

    def test_both_reader_arms_carry_the_identical_option_and_the_judge_verifier_none(self):
        dataset = self.root / "dataset"
        result = self.materializer().run(dataset)
        expected = ["seccomp=" + self.runtime["reader_seccomp_profile"]["path"]]
        pairs = {}
        for record in result["tasks"]:
            task = dataset / record["task_name"]
            reader = json.loads((task / "environment/docker-compose.yaml").read_text())["services"]
            judge = json.loads((task / "tests/docker-compose.yaml").read_text())["services"]
            self.assertEqual((list(reader), list(judge)), (["main"], ["main"]))
            self.assertEqual(reader["main"]["security_opt"], expected)
            # Nothing else: no capability, privilege, systempaths or other security key.
            self.assertEqual(set(reader["main"]), {"platform", "labels", "volumes", "security_opt"})
            self.assertEqual(set(judge["main"]), {"platform", "labels", "volumes"})
            pairs.setdefault(record["binding"]["pair_id"], {})[record["binding"]["arm"]] = reader["main"]["security_opt"]
            # Docker reads the profile at its host path; no build context holds a copy.
            self.assertEqual([path for path in task.rglob("*") if path.is_file() and
                              path.stat().st_size == len(self.approved) and path.read_bytes() == self.approved], [])
        self.assertEqual(len(pairs), 62)
        for arms in pairs.values():
            self.assertEqual(arms, {"gptgrep": expected, "raw_only": expected})

    def test_missing_malformed_and_unapproved_pins_are_refused_before_any_task(self):
        good = dict(self.runtime["reader_seccomp_profile"])
        cases = [
            ("absent", "reader_seccomp_profile_required"),
            (None, "reader_seccomp_profile_required"),
            ({**good, "mode": "0644"}, "reader_seccomp_profile_pin"),
            ({"path": good["path"], "sha256": good["sha256"]}, "reader_seccomp_profile_pin"),
            ([good["path"], good["bytes"], good["sha256"]], "reader_seccomp_profile_pin"),
            ({**good, "path": "reader-seccomp.json"}, "reader_seccomp_profile_path"),
            ({**good, "path": good["path"] + "/"}, "reader_seccomp_profile_path"),
            ({**good, "path": str(self.root / "x" / ".." / "reader-seccomp.json")}, "reader_seccomp_profile_path"),
            ({**good, "path": str(self.root / "$HOME.json")}, "reader_seccomp_profile_path"),
            ({**good, "bytes": str(good["bytes"])}, "integer_range"),
            ({**good, "sha256": good["sha256"].upper()}, "sha256"),
            # Self-consistent pins of any other profile: Docker's default and a broadened copy.
            (fixtures.write_pin(self.root / "upstream-default.json", (SECCOMP / "upstream/default.json").read_bytes()),
             "reader_seccomp_profile_not_approved"),
            (fixtures.write_pin(self.root / "broadened.json", self.broadened), "reader_seccomp_profile_not_approved"),
        ]
        for value, code in cases:
            with self.subTest(value=value):
                if value == "absent":
                    self.runtime.pop("reader_seccomp_profile")
                else:
                    self.runtime["reader_seccomp_profile"] = value
                with self.assertRaisesRegex(ValueError, code):
                    self.materializer()
                self.runtime["reader_seccomp_profile"] = good
        with self.assertRaisesRegex(ValueError, "reader_seccomp_profile_required"):
            reader_security_opt(None)
        self.assertEqual(reader_security_opt(good), ["seccomp=" + good["path"]])
        self.assertFalse((self.root / "dataset").exists())

    def test_profile_mismatch_refuses_before_any_task_is_written(self):
        good = dict(self.runtime["reader_seccomp_profile"])
        profile = Path(good["path"])
        link = self.root / "linked-reader-seccomp.json"
        link.symlink_to(profile)
        changes = ((lambda: profile.write_bytes(self.broadened), "pin_mismatch"),
                   (lambda: profile.write_bytes(self.approved + b"\n"), "pin_mismatch"),
                   (lambda: profile.write_bytes(self.approved[:-1]), "pin_mismatch"),
                   (lambda: self.runtime.update(reader_seccomp_profile={**good, "path": str(link)}),
                    "physical_absolute_path"))
        for change, code in changes:
            with self.subTest(code=code):
                change()
                with self.assertRaisesRegex(ValueError, code):
                    self.materializer()
                profile.write_bytes(self.approved)
                self.runtime["reader_seccomp_profile"] = good
        self.runtime["reader_seccomp_profile"] = {**good, "path": str(self.root / "absent-reader-seccomp.json")}
        with self.assertRaises(FileNotFoundError):
            self.materializer()
        self.runtime["reader_seccomp_profile"] = good
        # A change after validation still stops run() before the first task or companion file.
        adapter = self.materializer()
        profile.write_bytes(self.broadened)
        dataset = self.root / "dataset"
        with self.assertRaisesRegex(ValueError, "pin_mismatch"):
            adapter.run(dataset)
        self.assertFalse(dataset.exists())
        for suffix in (".export.json", ".job.json"):
            self.assertFalse((self.root / ("dataset" + suffix)).exists())

    def test_docker_compose_config_renders_the_option_for_reader_roles_only(self):
        docker = os.environ.get("GPTGREP_TEST_DOCKER_CLI")
        if not docker:
            self.skipTest("set GPTGREP_TEST_DOCKER_CLI to an absolute Docker CLI path for the offline compose render")
        self.assertTrue(Path(docker).is_absolute())
        # Harbor's own build and prebuilt base files, merged as Harbor merges them.
        overlays = Path(importlib.util.find_spec("harbor.environments.docker").origin).parent
        dataset = self.root / "dataset"
        result = self.materializer().run(dataset)
        expected = ["seccomp=" + self.runtime["reader_seccomp_profile"]["path"]]
        # An absent engine socket: rendering neither needs nor reaches an engine.
        env = {"PATH": "/usr/bin:/bin", "HOME": str(self.root), "TMPDIR": os.environ["TMPDIR"],
               "DOCKER_HOST": "unix://" + str(self.root / "absent-engine.sock")}
        if os.environ.get("DOCKER_CONFIG"):
            env["DOCKER_CONFIG"] = os.environ["DOCKER_CONFIG"]  # Compose CLI plugin lookup only
        first = {}
        for record in result["tasks"]:
            first.setdefault(record["binding"]["arm"], dataset / record["task_name"])
        self.assertEqual(set(first), {"gptgrep", "raw_only"})
        for arm, task in sorted(first.items()):
            for directory, image in (("environment", arm), ("tests", "judge")):
                for overlay, variables in (("docker-compose-build.yaml", {"CONTEXT_DIR": str(task / directory)}),
                                           ("docker-compose-prebuilt.yaml",
                                            {"PREBUILT_IMAGE_NAME": self.runtime["images"][image]})):
                    with self.subTest(arm=arm, directory=directory, overlay=overlay):
                        completed = subprocess.run(
                            [docker, "compose", "--project-name", "full62-render",
                             "--project-directory", str(task / directory), "-f", str(overlays / overlay),
                             "-f", str(task / directory / "docker-compose.yaml"), "config", "--format", "json"],
                            env={**env, **variables}, cwd=str(task / directory), capture_output=True,
                            timeout=60, check=False)
                        self.assertEqual(completed.returncode, 0, completed.stderr.decode(errors="replace")[-2000:])
                        service = json.loads(completed.stdout)["services"]["main"]
                        self.assertEqual(service.get("security_opt"), expected if directory == "environment" else None)
                        self.assertFalse({"cap_add", "privileged"} & set(service))


if __name__ == "__main__":
    unittest.main()
