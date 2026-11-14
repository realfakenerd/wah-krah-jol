#!/usr/bin/env python3
"""Tests for the issue #467 cluster bounds audit against a synthetic registry."""

import io
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import diagnose_cluster_bounds


VERSION = "1.0.0"


def package_content(name, relative):
    return ("// synthetic source: " + name + "/" + relative + "\n").encode()


class SyntheticRegistry:
    """Build cached crate archives and installed sources under one registry root."""

    def __init__(self, root):
        self.root = Path(root)
        self.checksums = {}
        for name, paths in diagnose_cluster_bounds.FILES.items():
            contents = {relative: package_content(name, relative) for relative in paths}
            archive = self.root / "cache" / "idx" / (name + "-" + VERSION + ".crate")
            archive.parent.mkdir(parents=True, exist_ok=True)
            self.write_archive(archive, name, contents)
            self.checksums[name] = diagnose_cluster_bounds.sha256(archive.read_bytes())
            for relative, data in contents.items():
                installed = self.root / "src" / "idx" / (name + "-" + VERSION) / relative
                installed.parent.mkdir(parents=True, exist_ok=True)
                installed.write_bytes(data)

    @staticmethod
    def write_archive(archive, name, contents):
        package = name + "-" + VERSION
        with tarfile.open(archive, "w:gz") as tar:
            for relative, data in contents.items():
                member = tarfile.TarInfo(package + "/" + relative)
                member.size = len(data)
                tar.addfile(member, io.BytesIO(data))

    def lock(self, line_end="\n"):
        blocks = []
        for name in diagnose_cluster_bounds.FILES:
            blocks.append("[[package]]" + line_end
                          + 'name = "' + name + '"' + line_end
                          + 'version = "' + VERSION + '"' + line_end
                          + 'checksum = "' + self.checksums[name] + '"' + line_end)
        return "".join(blocks)


class CachedSourcesTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.registry = SyntheticRegistry(self.directory.name)

    def test_lf_lock_returns_sources_and_receipts(self):
        sources, receipts = diagnose_cluster_bounds.cached_sources(
            self.registry.lock("\n"), Path(self.directory.name))
        expected = {name + "/" + relative
                    for name, paths in diagnose_cluster_bounds.FILES.items()
                    for relative in paths}
        self.assertEqual(set(sources), expected)
        self.assertEqual(len(receipts), len(expected))
        self.assertEqual(len({receipt["source"] for receipt in receipts}), len(expected))
        for receipt in receipts:
            self.assertTrue(Path(receipt["source"]).is_file())
            self.assertRegex(receipt["sha256"], r"^[0-9a-f]{64}$")
            self.assertRegex(receipt["archive_sha256"], r"^[0-9a-f]{64}$")

    def test_crlf_lock_matches_lf_lock(self):
        lf_sources, lf_receipts = diagnose_cluster_bounds.cached_sources(
            self.registry.lock("\n"), Path(self.directory.name))
        crlf_lock = self.registry.lock("\n").replace("\n", "\r\n")
        crlf_sources, crlf_receipts = diagnose_cluster_bounds.cached_sources(
            crlf_lock, Path(self.directory.name))
        self.assertEqual(crlf_sources, lf_sources)
        self.assertEqual(crlf_receipts, lf_receipts)

    def test_wrong_checksum_raises(self):
        lock = self.registry.lock("\n").replace(
            self.registry.checksums["bevy_pbr"], "0" * 64)
        with self.assertRaises(ValueError):
            diagnose_cluster_bounds.cached_sources(lock, Path(self.directory.name))


if __name__ == "__main__":
    unittest.main()
