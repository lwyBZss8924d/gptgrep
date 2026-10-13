"""Owner-approved reader seccomp profile: vendored upstream, derivation and provenance; no Docker."""
from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SECCOMP = Path(__file__).resolve().parents[1] / "seccomp"
APPROVED_SHA256 = "769a5dbb42b7b10edb07b516be7077e4a602a214a4c55ec892abd6d323bab48b"
ENGINE_COMMIT = "daa0cb7f23594cdbcce5002e370027d3fd36ffd7"


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
            for marker in ("/Users/", "/home/", "/private/"):
                self.assertNotIn(marker, data)


if __name__ == "__main__":
    unittest.main()
