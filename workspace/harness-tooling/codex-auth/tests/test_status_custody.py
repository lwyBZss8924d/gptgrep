"""Synthetic status metadata custody; no native auth or real credential access."""
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import codex_auth_supervisor as sup


class StatusCustodyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        canonical = self.root / "canonical"
        locks = self.root / "locks"
        canonical.mkdir(mode=0o700)
        locks.mkdir(mode=0o700)
        lease = locks / "fixture.lock"
        self.cfg = SimpleNamespace(canonical_auth_home=canonical, canonical_auth_file=canonical / "auth.json",
            lease_path=lease, binding_path=canonical / sup.BINDING_NAME,
            holders_dir=Path(str(lease) + ".holders"), ledger_path=Path(str(lease) + ".ledger.jsonl"),
            quarantine_path=Path(str(lease) + ".quarantine"), auth_block_path=Path(str(lease) + ".auth-blocked"),
            auth_source_id="fixture", ps_bin="/bin/ps", refresh_max_age_seconds=60,
            blocker_url="http://127.0.0.1:0/oauth/token")
        self.cfg.holders_dir.mkdir(mode=0o700)
        self.write(self.cfg.canonical_auth_file, '{"synthetic_canary":"must-not-read"}')
        self.write(lease, "")
        info = lease.stat()
        self.binding = {"schema": sup.BINDING_SCHEMA, "auth_source_id": "fixture", "lease_path": str(lease),
                        "lease_dev": info.st_dev, "lease_ino": info.st_ino}
        self.write(self.cfg.binding_path, json.dumps(self.binding))
        self.entry = {"schema": sup.LEDGER_SCHEMA, "auth_source_id": "fixture", "decision": "fixture"}
        self.write(self.cfg.ledger_path, json.dumps(self.entry) + "\n")

    def write(self, path, value):
        path.write_text(value)
        path.chmod(0o600)

    def guarded_read(self):
        real = os.read
        credential = self.cfg.canonical_auth_file.stat()
        def guard(fd, size):
            opened = os.fstat(fd)
            self.assertNotEqual((opened.st_dev, opened.st_ino), (credential.st_dev, credential.st_ino))
            return real(fd, size)
        return mock.patch.object(sup.os, "read", side_effect=guard)

    def holder(self, run_id="one", released=None):
        record = {"pid": 4321, "host_id": "synthetic-host", "process_start_time": "synthetic-generation",
                  "run_id": run_id, "role": "reader", "operation": "run", "mode": "exclusive",
                  "acquired_at": "synthetic-time", "released_at": released}
        self.write(self.cfg.holders_dir / ("reader--" + run_id + ".json"), json.dumps(record))
        return record

    def test_status_only_requires_search_access_to_outer_ancestors(self):
        if os.geteuid() == 0:
            self.skipTest("root bypasses the search-only ancestor permission boundary")
        flags = sup._search_dir_flags()
        if not any(flags & getattr(os, name, 0) for name in ("O_SEARCH", "O_PATH")):
            self.skipTest("the platform has no lookup-only directory descriptor")
        self.root.chmod(0o100)
        try:
            with mock.patch.object(sup, "blocker_probe", return_value="closed"), self.guarded_read():
                status, code = sup.command_status(self.cfg, None)
            self.assertEqual(code, sup.EXIT_PASS)
            self.assertTrue(status["metadata_observation_complete"])
            self.assertEqual(status["canonical"]["preflight"], "ok")
            self.assertEqual(status["lease"]["probe"], "free")
            self.assertTrue(status["lease"]["binding"]["full_identity_matches_config"])
            self.assertTrue(status["holder_scan"]["complete"])
            self.assertTrue(status["ledger_scan"]["complete"])
            self.assertFalse(status["credential_contents_read_by_supervisor"])
        finally:
            self.root.chmod(0o700)

    def test_valid_ledger_tail_is_bounded_and_preserves_last_five(self):
        self.write(self.cfg.ledger_path, "".join(json.dumps(dict(self.entry, index=n)) + "\n" for n in range(8)))
        with self.guarded_read():
            rows, scan = sup.status_ledger(self.cfg)
        self.assertTrue(scan["complete"])
        self.assertEqual([row["index"] for row in rows], [3, 4, 5, 6, 7])
        self.assertLessEqual(scan["bytes"], scan["max_bytes"])

    def test_ledger_symlink_hardlink_fifo_and_oversize_are_rejected_before_read(self):
        for kind in ("symlink", "hardlink", "fifo", "oversize"):
            with self.subTest(kind=kind):
                self.cfg.ledger_path.unlink()
                if kind == "symlink":
                    self.cfg.ledger_path.symlink_to(self.cfg.canonical_auth_file)
                elif kind == "hardlink":
                    os.link(self.cfg.canonical_auth_file, self.cfg.ledger_path)
                elif kind == "fifo":
                    os.mkfifo(self.cfg.ledger_path, 0o600)
                else:
                    self.write(self.cfg.ledger_path, "")
                    with self.cfg.ledger_path.open("r+b") as handle:
                        handle.truncate(sup.STATUS_LEDGER_LIMIT + 1)
                with mock.patch.object(sup.os, "read", side_effect=AssertionError("content must not be read")):
                    rows, scan = sup.status_ledger(self.cfg)
                self.assertEqual(rows, [])
                self.assertFalse(scan["complete"])
                self.assertTrue(scan["errors"])

    def test_parent_symlink_is_rejected_before_content_read(self):
        alias = self.root / "alias"
        alias.symlink_to(self.cfg.ledger_path.parent, target_is_directory=True)
        self.cfg.ledger_path = alias / self.cfg.ledger_path.name
        with mock.patch.object(sup.os, "read", side_effect=AssertionError("content must not be read")):
            rows, scan = sup.status_ledger(self.cfg)
        self.assertEqual(rows, [])
        self.assertFalse(scan["complete"])

    def test_canonical_inode_is_refused_even_if_selected_as_record(self):
        with mock.patch.object(sup.os, "read", side_effect=AssertionError("credential must not be read")):
            with self.assertRaises(sup.SupervisorError) as raised:
                sup._status_read_bytes(self.cfg, self.cfg.canonical_auth_file)
        self.assertEqual(raised.exception.reason, "status_metadata_is_credential")

    def test_forward_canonical_alias_to_metadata_is_refused_before_read(self):
        for path in (self.cfg.ledger_path, self.cfg.binding_path):
            with self.subTest(path=path.name):
                self.cfg.canonical_auth_file.unlink()
                self.cfg.canonical_auth_file.symlink_to(path)
                with mock.patch.object(sup.os, "read", side_effect=AssertionError("credential target must not be read")):
                    with self.assertRaises(sup.SupervisorError) as raised:
                        sup._status_read_bytes(self.cfg, path)
                self.assertEqual(raised.exception.reason, "status_canonical_identity_unsafe")

    def test_ledger_growth_and_replacement_during_read_are_unknown(self):
        real_read = os.read
        for kind in ("growth", "replacement"):
            with self.subTest(kind=kind):
                self.write(self.cfg.ledger_path, json.dumps(self.entry) + "\n")
                touched = False
                def mutate(fd, size):
                    nonlocal touched
                    value = real_read(fd, size)
                    if not touched:
                        touched = True
                        if kind == "growth":
                            with self.cfg.ledger_path.open("a") as handle:
                                handle.write("{}\n")
                        else:
                            other = self.root / "replacement"
                            self.write(other, json.dumps(self.entry) + "\n")
                            os.replace(other, self.cfg.ledger_path)
                    return value
                with mock.patch.object(sup.os, "read", side_effect=mutate):
                    rows, scan = sup.status_ledger(self.cfg)
                self.assertEqual(rows, [])
                self.assertFalse(scan["complete"])

    def test_full_binding_checks_source_path_device_and_inode(self):
        with self.guarded_read():
            good = sup.status_binding(self.cfg)
        self.assertTrue(good["matches_config"])
        self.assertTrue(good["full_identity_matches_config"])
        for key, value in (("auth_source_id", "other"), ("lease_path", str(self.root / "other.lock")),
                           ("lease_dev", -1), ("lease_ino", -1), ("lease_ino", True)):
            with self.subTest(key=key, value=value):
                self.write(self.cfg.binding_path, json.dumps(dict(self.binding, **{key: value})))
                with self.guarded_read():
                    result = sup.status_binding(self.cfg)
                self.assertFalse(result["matches_config"])
                self.assertFalse(result["full_identity_matches_config"])

    def test_binding_marker_and_status_marker_credential_aliases_are_not_read(self):
        for path in (self.cfg.binding_path, self.cfg.quarantine_path, self.cfg.auth_block_path):
            if path.exists():
                path.unlink()
            path.symlink_to(self.cfg.canonical_auth_file)
        with self.guarded_read():
            self.assertFalse(sup.status_binding(self.cfg)["full_identity_matches_config"])
            for path, schema in ((self.cfg.quarantine_path, "gptgrep.codex-auth.quarantine.v1"),
                                 (self.cfg.auth_block_path, "gptgrep.codex-auth.auth-block.v1")):
                self.assertTrue(sup.status_marker(self.cfg, path, schema)["unreadable"])

    def test_holder_empty_missing_malformed_and_unreleased_are_distinct(self):
        rows, scan = sup.status_holders(self.cfg)
        self.assertEqual(rows, [])
        self.assertTrue(scan["complete"])
        self.cfg.holders_dir.rmdir()
        rows, scan = sup.status_holders(self.cfg)
        self.assertFalse(scan["complete"])
        self.cfg.holders_dir.mkdir(mode=0o700)
        self.write(self.cfg.holders_dir / "reader--bad.json", "{")
        rows, scan = sup.status_holders(self.cfg)
        self.assertFalse(scan["complete"])
        self.holder()
        with mock.patch.object(sup, "process_alive", return_value=False), self.guarded_read():
            rows, scan = sup.status_holders(self.cfg)
        self.assertFalse(scan["complete"])
        self.assertEqual(len(rows), 1)
        self.assertFalse(rows[0]["alive"])

    def test_holder_credential_link_is_unknown_without_reading_it(self):
        (self.cfg.holders_dir / "reader--bad.json").symlink_to(self.cfg.canonical_auth_file)
        with mock.patch.object(sup.os, "read", side_effect=AssertionError("credential must not be read")):
            rows, scan = sup.status_holders(self.cfg)
        self.assertEqual(rows, [])
        self.assertFalse(scan["complete"])

    def test_released_owner_relogin_maintenance_records_remain_supported(self):
        for role in ("_owner", "_relogin", "_maintenance"):
            record = self.holder(role[1:], released="synthetic-release-time")
            (self.cfg.holders_dir / ("reader--" + record["run_id"] + ".json")).unlink()
            record["role"] = role
            self.write(self.cfg.holders_dir / (role + "--" + record["run_id"] + ".json"), json.dumps(record))
        with self.guarded_read():
            rows, scan = sup.status_holders(self.cfg)
        self.assertEqual(rows, [])
        self.assertTrue(scan["complete"])
        self.assertEqual(scan["scanned_entries"], 3)

    def test_holder_count_bound_is_unknown_and_never_reported_empty_complete(self):
        for n in range(3):
            self.holder(str(n), released="released")
        with mock.patch.object(sup, "STATUS_HOLDER_LIMIT", 2):
            rows, scan = sup.status_holders(self.cfg)
        self.assertEqual(rows, [])
        self.assertFalse(scan["complete"])
        self.assertEqual(scan["scanned_entries"], 3)

    def test_released_history_above_old_256_limit_remains_complete(self):
        for n in range(257):
            self.holder("released-%d" % n, released="synthetic-release-time")
        with self.guarded_read():
            rows, scan = sup.status_holders(self.cfg)
        self.assertEqual(rows, [])
        self.assertTrue(scan["complete"])
        self.assertEqual(scan["scanned_entries"], 257)
        self.assertEqual(scan["max_entries"], 4096)

    def test_complete_status_exposes_strict_fields_and_unknown_ledger(self):
        with mock.patch.object(sup, "probe_lease", return_value="free"), \
                mock.patch.object(sup, "blocker_probe", return_value={"closed": True}), self.guarded_read():
            status, code = sup.command_status(self.cfg, None)
        self.assertEqual(code, sup.EXIT_PASS)
        self.assertTrue(status["metadata_observation_complete"])
        self.assertTrue(status["lease"]["binding"]["full_identity_matches_config"])
        self.assertTrue(status["holder_scan"]["complete"])
        self.cfg.ledger_path.unlink()
        self.cfg.ledger_path.symlink_to(self.cfg.canonical_auth_file)
        with mock.patch.object(sup, "probe_lease", return_value="free"), \
                mock.patch.object(sup, "blocker_probe", return_value={"closed": True}), self.guarded_read():
            status, _code = sup.command_status(self.cfg, None)
        self.assertFalse(status["metadata_observation_complete"])
        self.assertFalse(status["ledger_scan"]["complete"])
        self.assertFalse(status["credential_contents_read_by_supervisor"])

    def test_lease_probe_uses_the_selected_safe_file_and_retains_no_lock(self):
        self.assertEqual(sup.probe_lease(self.cfg), "free")
        self.cfg.lease_path.unlink()
        self.cfg.lease_path.symlink_to(self.cfg.canonical_auth_file)
        with self.guarded_read():
            self.assertEqual(sup.probe_lease(self.cfg), "unsafe")


if __name__ == "__main__":
    unittest.main()
