#!/usr/bin/env python3
"""Exercise the archive installer against temporary installs, without downloads."""

import io
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / "resources/installer/install.sh"


class ArchiveInstallTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="hot-installer-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.prefix = self.root / "prefix with spaces"
        self.bin = self.prefix / "bin"
        self.resources = self.prefix / "share/hot"
        self.archive = self.root / "release.tar.gz"
        self.harness = self.root / "install.sh"
        # Load the actual installer functions, omitting only its entry point.
        source, entrypoint = INSTALLER.read_text().rsplit('\nmain "$@"', 1)
        self.assertEqual(entrypoint.strip(), "")
        self.harness.write_text(
            source + '\nPREFIX="$TEST_PREFIX"\nAS=""\ninstall_archive "$TEST_ARCHIVE"\n'
        )
        self.make_archive()

    def make_archive(self, include_std=True):
        with tarfile.open(self.archive, "w:gz") as archive:
            for name, data, mode in [
                ("hot", b"#!/bin/sh\necho new\n", 0o755),
                ("release", b"new", 0o644),
            ]:
                member = tarfile.TarInfo(name)
                member.size, member.mode = len(data), mode
                member.uid, member.gid = 12345, 12346
                archive.addfile(member, io.BytesIO(data))
            if include_std:
                for name in ["pkg", "pkg/hot-std"]:
                    member = tarfile.TarInfo(name)
                    member.type, member.mode = tarfile.DIRTYPE, 0o755
                    member.uid, member.gid = 12345, 12346
                    archive.addfile(member)

    def old_install(self):
        self.bin.mkdir(parents=True)
        self.resources.mkdir(parents=True)
        (self.bin / "hot").write_text("old binary")
        (self.resources / "release").write_text("old")
        (self.resources / "obsolete").touch()
        (self.resources / "install-receipt.json").write_text('{"method":"archive"}')

    def run_installer(self, fail=""):
        shim_dir = self.root / "shim"
        shim_dir.mkdir()
        real_mv = shutil.which("mv")
        # Inject an I/O failure at a swap boundary; all other file operations
        # still run against the real temporary prefix.
        shim = shim_dir / "mv"
        shim.write_text(
            f"#!{sys.executable}\n"
            "import os, sys\n"
            "source, dest = sys.argv[-2:]\n"
            "prefix = os.environ['TEST_PREFIX']\n"
            "fail = os.environ['TEST_FAIL_SWAP']\n"
            "if (fail == 'binary' and dest == prefix + '/bin/hot') or "
            "(fail == 'resources' and source.endswith('/new') and dest == prefix + '/share/hot'):\n"
            "    sys.exit(1)\n"
            f"os.execv({real_mv!r}, [{real_mv!r}] + sys.argv[1:])\n"
        )
        shim.chmod(0o755)
        env = dict(os.environ, TEST_PREFIX=str(self.prefix), TEST_ARCHIVE=str(self.archive),
                   TEST_FAIL_SWAP=fail, PATH=str(shim_dir) + os.pathsep + os.environ["PATH"])
        return subprocess.run(["sh", str(self.harness)], env=env, capture_output=True, text=True)

    def assert_old_install(self):
        self.assertEqual((self.bin / "hot").read_text(), "old binary")
        self.assertEqual((self.resources / "release").read_text(), "old")
        self.assertTrue((self.resources / "obsolete").exists())
        self.assert_no_staging()

    def assert_no_staging(self):
        self.assertEqual(list(self.bin.glob(".hot.new.*")), [])
        self.assertEqual(list((self.prefix / "share").glob(".hot-install.*")), [])

    def test_fresh_install(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(subprocess.check_output([str(self.bin / "hot")], text=True), "new\n")
        self.assertTrue((self.resources / "install-receipt.json").is_file())
        self.assert_no_staging()

    def test_upgrade_removes_obsolete_resources(self):
        self.old_install()
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.resources / "release").read_text(), "new")
        self.assertFalse((self.resources / "obsolete").exists())
        self.assert_no_staging()

    def test_failed_resource_swap_restores_old_install(self):
        self.old_install()
        self.assertNotEqual(self.run_installer("resources").returncode, 0)
        self.assert_old_install()

    def test_failed_binary_swap_restores_old_install(self):
        self.old_install()
        self.assertNotEqual(self.run_installer("binary").returncode, 0)
        self.assert_old_install()

    def test_failed_first_install_removes_partial_resources(self):
        self.assertNotEqual(self.run_installer("binary").returncode, 0)
        self.assertFalse(self.resources.exists())
        self.assertFalse((self.bin / "hot").exists())
        self.assert_no_staging()

    def test_invalid_archive_preserves_old_install(self):
        self.old_install()
        self.make_archive(include_std=False)
        self.assertNotEqual(self.run_installer().returncode, 0)
        self.assert_old_install()

    @unittest.skipUnless(os.geteuid() == 0, "run as root to verify system-install ownership")
    def test_extracted_files_belong_to_installer(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        for path in [self.bin / "hot", self.resources / "release", self.resources / "pkg/hot-std"]:
            self.assertEqual(path.stat().st_uid, 0, str(path))
            self.assertNotEqual(path.stat().st_gid, 12346, str(path))


if __name__ == "__main__":
    unittest.main()
