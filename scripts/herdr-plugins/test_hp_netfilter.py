"""Tests for hp_netfilter.py. No network access: `python3 -m unittest discover -s scripts/herdr-plugins`."""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from hp_netfilter import address_refusal, port_refusal, request_refusal, resolved_target  # noqa: E402


class AddressTest(unittest.TestCase):
    def test_refuses_non_public_addresses(self):
        for a in (
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "169.254.0.1",
            "100.64.0.1",
            "100.127.255.254",
            "224.0.0.251",
            "239.255.255.250",
            "255.255.255.255",
            "240.0.0.1",
            "198.18.0.1",
            "192.0.2.1",
            "::",
            "::1",
            "fd00:ec2::254",
            "fc00::1",
            "fe80::1",
            "fe80::1%en0",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "2002:a9fe:a9fe::1",
            "2001:0:4136:e378::1",
            "2001:db8::1",
            "not-an-ip",
            "",
        ):
            with self.subTest(address=a):
                self.assertIsNotNone(address_refusal(a))

    def test_allows_public_addresses(self):
        for a in ("140.82.112.3", "8.8.8.8", "1.1.1.1", "2606:4700:4700::1111", "2a00:1450:4001:80b::200e"):
            with self.subTest(address=a):
                self.assertIsNone(address_refusal(a))

    def test_ports(self):
        self.assertIsNone(port_refusal(80))
        self.assertIsNone(port_refusal(443))
        for p in (22, 25, 8080, 8443, 6379, 0, 65535):
            with self.subTest(port=p):
                self.assertIsNotNone(port_refusal(p))


class ResolvedTargetTest(unittest.TestCase):
    def test_picks_the_first_public_address(self):
        self.assertEqual(resolved_target(["140.82.112.3", "140.82.112.4"], 443), ("140.82.112.3", None))

    def test_refuses_a_name_with_any_private_address(self):
        addr, why = resolved_target(["140.82.112.3", "169.254.169.254"], 443)
        self.assertIsNone(addr)
        self.assertIn("169.254.169.254", why)

    def test_refuses_other_ports_and_empty_answers(self):
        self.assertIsNone(resolved_target(["140.82.112.3"], 22)[0])
        self.assertIsNone(resolved_target([], 443)[0])


class RequestTest(unittest.TestCase):
    allowed = {"https://example.com/a.tar.gz"}

    def test_only_get_and_head(self):
        for mode in ("record", "verify"):
            for method in ("POST", "PUT", "DELETE", "PATCH", "OPTIONS", "get"):
                with self.subTest(mode=mode, method=method):
                    self.assertIsNotNone(request_refusal(mode, method, "https://example.com/a.tar.gz", self.allowed))
            for method in ("GET", "HEAD"):
                self.assertIsNone(request_refusal(mode, method, "https://example.com/a.tar.gz", self.allowed))

    def test_verify_allows_only_locked_urls(self):
        self.assertIsNotNone(request_refusal("verify", "GET", "https://example.com/b.tar.gz", self.allowed))
        self.assertIsNotNone(request_refusal("verify", "GET", "https://example.com/a.tar.gz?x=1", self.allowed))

    def test_record_allows_any_url(self):
        self.assertIsNone(request_refusal("record", "GET", "https://example.com/b.tar.gz", self.allowed))


if __name__ == "__main__":
    unittest.main()
