"""Self-check of the qualification runner's guard (seatbelt on macOS).

Proves the test process tree cannot reach non-loopback hosts, the system proxy
or the real credential roots. Only metadata access to the roots themselves is
attempted, and the guard denies it.
"""
import errno
import os
import socket
import unittest

import support


def connect_errno(host, port):
    family = socket.AF_INET6 if ":" in host else socket.AF_INET
    with socket.socket(family, socket.SOCK_STREAM) as sock:
        sock.settimeout(2)
        try:
            sock.connect((host, port))
        except OSError as exc:
            return exc.errno or errno.ETIMEDOUT
    return 0


@unittest.skipUnless(support.SANDBOX == "seatbelt", "runs only under the qualification runner's seatbelt guard")
class NetworkGuardTests(unittest.TestCase):
    def test_non_loopback_outbound_is_denied(self):
        # RFC 5737 TEST-NET address: never a real endpoint.
        self.assertEqual(connect_errno("192.0.2.1", 443), errno.EPERM)

    def test_configured_system_proxy_ports_are_denied(self):
        ports = [int(p) for p in os.environ.get("GPTGREP_AUTH_TEST_PROXY_PORTS", "").split(",") if p]
        for port in ports:
            with self.subTest(port=port):
                self.assertEqual(connect_errno("127.0.0.1", port), errno.EPERM)

    def test_reserved_loopback_port_and_closed_blocker_port(self):
        port = support.next_port()
        with socket.socket() as server:
            server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            server.bind(("127.0.0.1", port))
            server.listen(1)
            self.assertEqual(connect_errno("127.0.0.1", port), 0)
        self.assertEqual(connect_errno("127.0.0.1", 9), errno.ECONNREFUSED)

    def test_real_credential_roots_are_unreachable(self):
        for root in support.real_credential_roots():
            with self.subTest(root=root.name):
                try:
                    os.lstat(root)
                    reachable = True
                except PermissionError:
                    reachable = False
                except FileNotFoundError:
                    reachable = False
                self.assertFalse(reachable)


if __name__ == "__main__":
    unittest.main()
