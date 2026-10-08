"""Installed Harbor config parsing only: no tasks, containers, agents or network."""
import unittest
from importlib.metadata import version

import format_bridge as bridge


class NetworkPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if version("harbor") != "0.24.0":
            raise RuntimeError("harbor_version_mismatch")

    def options(self, **changes):
        return {"cpus": 1, "memory_mb": 512, "timeout_sec": 60,
                "network_mode": "no-network", **changes}

    def configured(self, arm="control", **changes):
        return bridge.config({"task_id": "ordinary-source-only", "arm": arm,
                              "options": self.options(**changes)}, production=True)

    def assert_verifier_offline(self, cfg):
        self.assertEqual(cfg.verifier.network_mode.value, "no-network")
        self.assertEqual(cfg.verifier.environment.network_mode.value, "no-network")
        self.assertFalse(cfg.verifier.allowed_hosts)
        self.assertFalse(cfg.verifier.environment.allowed_hosts)
        self.assertEqual(cfg.verifier.environment_mode.value, "separate")

    def test_absent_and_explicit_empty_default_are_offline(self):
        for options in ({}, {"allowed_hosts": []}):
            for arm in ("control", "gptgrep"):
                cfg = self.configured(arm, **options)
                self.assertEqual(cfg.environment.network_mode.value, "no-network")
                self.assertEqual(cfg.agent.network_mode.value, "no-network")
                self.assertFalse(cfg.environment.allowed_hosts)
                self.assertFalse(cfg.agent.allowed_hosts)
                self.assertNotIn("allowed_hosts", cfg.model_dump_toml())
                self.assert_verifier_offline(cfg)

    def test_equal_reader_allowlists_parse_and_roundtrip_without_verifier_inheritance(self):
        hosts = ["api.openai.com", "openrouter.ai", "192.0.2.1", "2001:db8::1"]
        readers = []
        for arm in ("control", "gptgrep"):
            cfg = self.configured(arm, network_mode="allowlist", allowed_hosts=hosts)
            cfg = bridge.TaskConfig.model_validate_toml(cfg.model_dump_toml())
            self.assertEqual(cfg.environment.network_mode.value, "allowlist")
            self.assertEqual(cfg.agent.network_mode.value, "allowlist")
            self.assertEqual(cfg.environment.allowed_hosts, hosts)
            self.assertEqual(cfg.agent.allowed_hosts, hosts)
            self.assert_verifier_offline(cfg)
            readers.append((cfg.environment.model_dump(), cfg.agent.model_dump()))
        self.assertEqual(*readers)

    def test_cross_policy_empty_missing_null_and_duplicate_refused(self):
        cases = [
            {"network_mode": "allowlist"},
            {"network_mode": "allowlist", "allowed_hosts": []},
            {"network_mode": "allowlist", "allowed_hosts": None},
            {"allowed_hosts": None},
            {"network_mode": "public"},
            {"allowed_hosts": ["api.openai.com"]},
            {"network_mode": "allowlist", "allowed_hosts": ["api.openai.com", "api.openai.com"]},
            {"network_mode": "allowlist", "allowed_hosts": "api.openai.com"},
        ]
        for case in cases:
            with self.subTest(case=case), self.assertRaises(ValueError):
                self.configured(**case)

    def test_malformed_wildcard_cidr_url_credential_port_and_unbounded_hosts_refused(self):
        hosts = ["", "*.example.com", "api.*.example.com", "https://api.openai.com",
                 "user:password@api.openai.com", "192.0.2.0/24", "2001:db8::/32",
                 "api.openai.com:443", "api.openai.com/path", "API.OPENAI.COM",
                 "api.openai.com.", " api.openai.com", "api.openai.com\n", "-bad.example.com",
                 "bad-.example.com", "a..example.com", "bad_host.example.com", "é.example.com",
                 "2001:0db8::1", "a" * 64 + ".example.com", ".".join(["a" * 63] * 4), 42]
        for host in hosts:
            with self.subTest(host=host), self.assertRaises(ValueError):
                self.configured(network_mode="allowlist", allowed_hosts=[host])
        with self.assertRaises(ValueError):
            self.configured(network_mode="allowlist", allowed_hosts=[f"host-{i}.example.com" for i in range(33)])
        self.configured(network_mode="allowlist", allowed_hosts=[f"host-{i}.example.com" for i in range(32)])

    def test_synthetic_frontdoor_cannot_enable_network(self):
        with self.assertRaises(ValueError):
            bridge.config({"task_id": "ordinary-source-only", "arm": "control",
                           "options": self.options(network_mode="allowlist", allowed_hosts=["api.openai.com"])})

    def test_numeric_aliases_and_scoped_ipv6_refused_canonical_hosts_parse(self):
        refused = ["192.0.2.01", "127.1", "127.0.1", "2130706433", "127",
                   "0177.0.0.1", "0x7f000001", "0x7f.0.0.1", "0x7f.0x1", "0x",
                   "256.1.1.1", "4294967296", "example.123", "example.0x10",
                   "fe80::1%eth0", "fe80::1%25en0", "2001:db8::1%2", "::ffff:192.0.2.01"]
        admitted = ["192.0.2.1", "2001:db8::1", "::1", "api2.example.com",
                    "123.api.example.com", "127.example.com", "0xrouter.example.com", "localhost"]
        for host in refused:
            opt = self.options(network_mode="allowlist", allowed_hosts=[host])
            with self.subTest(host=host, stage="helper"), self.assertRaises(ValueError):
                bridge.reader_network_policy(opt, production=True)
            for arm in ("control", "gptgrep"):
                with self.subTest(host=host, arm=arm), self.assertRaises(ValueError):
                    self.configured(arm, network_mode="allowlist", allowed_hosts=[host])
        for host in admitted:
            opt = self.options(network_mode="allowlist", allowed_hosts=[host])
            self.assertEqual(bridge.reader_network_policy(opt, production=True)["allowed_hosts"], [host])
            for arm in ("control", "gptgrep"):
                with self.subTest(host=host, arm=arm):
                    cfg = self.configured(arm, network_mode="allowlist", allowed_hosts=[host])
                    cfg = bridge.TaskConfig.model_validate_toml(cfg.model_dump_toml())
                    self.assertEqual(cfg.environment.allowed_hosts, [host])
                    self.assertEqual(cfg.agent.allowed_hosts, [host])
                    self.assert_verifier_offline(cfg)


if __name__ == "__main__":
    unittest.main(verbosity=2)
