"""Tests for unpack_results.py: `python3 -m unittest discover -s scripts/herdr-plugins`."""

import io
import os
import sys
import tarfile
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import unpack_results  # noqa: E402


def tar_bytes(members):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for info, data in members:
            if data is not None:
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
            else:
                tar.addfile(info)
    buf.seek(0)
    return buf


def reg(name, data=b"x"):
    return tarfile.TarInfo(name), data


def special(name, kind, target=""):
    info = tarfile.TarInfo(name)
    info.type = kind
    info.linkname = target
    return info, None


class UnpackTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dest = os.path.join(self.tmp.name, "out")
        self.log = []

    def tearDown(self):
        self.tmp.cleanup()

    def run_unpack(self, members):
        return unpack_results.unpack(tar_bytes(members), self.dest, self.log.append)

    def files(self):
        found = []
        for root, dirs, names in os.walk(self.dest):
            for n in names + dirs:
                p = os.path.join(root, n)
                self.assertFalse(os.path.islink(p), p)
            for n in names:
                found.append(os.path.relpath(os.path.join(root, n), self.dest))
        return sorted(found)

    def test_unpacks_regular_files_and_directories(self):
        d = tarfile.TarInfo("./grids")
        d.type = tarfile.DIRTYPE
        ok = self.run_unpack([(d, None), reg("./a-linux.json", b"{}"), reg("grids/x.svg", b"<svg/>")])
        self.assertTrue(ok, self.log)
        self.assertEqual(self.files(), ["a-linux.json", "grids/x.svg"])
        with open(os.path.join(self.dest, "a-linux.json"), "rb") as f:
            self.assertEqual(f.read(), b"{}")

    def test_skips_links_devices_and_unsafe_names(self):
        ok = self.run_unpack(
            [
                special("x\n/etc/shadow", tarfile.SYMTYPE, "/etc/shadow"),
                special("link-linux.json", tarfile.SYMTYPE, "/etc/shadow"),
                special("hard-linux.json", tarfile.LNKTYPE, "/etc/shadow"),
                special("dev", tarfile.CHRTYPE),
                special("fifo", tarfile.FIFOTYPE),
                reg("/etc/evil"),
                reg("../evil"),
                reg("a/../../evil"),
                reg("a/./b"),
                reg("x\n/etc/shadow"),
                reg("sp ace"),
                reg("ok-linux.json"),
            ]
        )
        self.assertFalse(ok)
        self.assertEqual(self.files(), ["ok-linux.json"])
        self.assertFalse(os.path.exists(os.path.join(self.tmp.name, "evil")))

    def test_never_writes_through_an_existing_path(self):
        ok = self.run_unpack([reg("a", b"first"), reg("a", b"second"), reg("a/b", b"x")])
        self.assertFalse(ok)
        with open(os.path.join(self.dest, "a"), "rb") as f:
            self.assertEqual(f.read(), b"first")

    def test_refuses_an_existing_destination(self):
        os.mkdir(self.dest)
        with self.assertRaises(FileExistsError):
            self.run_unpack([reg("a")])

    def test_size_and_count_limits(self):
        old = (unpack_results.MAX_FILE, unpack_results.MAX_ENTRIES)
        try:
            unpack_results.MAX_FILE = 4
            self.assertFalse(self.run_unpack([reg("big", b"12345"), reg("small", b"1234")]))
            self.assertEqual(self.files(), ["small"])
            self.tearDown()
            self.setUp()
            unpack_results.MAX_ENTRIES = 2
            self.assertFalse(self.run_unpack([reg("a"), reg("b"), reg("c")]))
            self.assertEqual(self.files(), ["a", "b"])
        finally:
            unpack_results.MAX_FILE, unpack_results.MAX_ENTRIES = old

    def test_garbage_stream(self):
        self.assertFalse(unpack_results.unpack(io.BytesIO(b"not a tar" * 100), self.dest, self.log.append))
        self.assertEqual(self.files(), [])


if __name__ == "__main__":
    unittest.main()
