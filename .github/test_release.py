"""Behavioral coverage for native cache repair and untrusted tar boundaries."""

import hashlib
import importlib.util
import io
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("release", Path(__file__).with_name("release.py"))
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class NativePreparationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.destination = self.root / "native"
        (self.destination / "lib").mkdir(parents=True)
        self.library = self.destination / "lib/runtime.lib"
        self.library.write_bytes(b"previous usable library")
        self.payload = b"approved replacement"
        self.pin = {
            "files": [
                {"path": "lib/runtime.lib", "sha256": hashlib.sha256(self.payload).hexdigest()}
            ]
        }
        self.archive = self.root / "native.tar.bz2"

    def archive_with(self, name="native/lib/runtime.lib", payload=None, link=False):
        with tarfile.open(self.archive, "w:bz2") as output:
            member = tarfile.TarInfo(name)
            if link:
                member.type = tarfile.SYMTYPE
                member.linkname = "../../outside"
                output.addfile(member)
            else:
                data = self.payload if payload is None else payload
                member.size = len(data)
                output.addfile(member, io.BytesIO(data))

    def test_corrupt_extraction_preserves_previous_runtime(self):
        self.archive_with(payload=b"not approved")
        with self.assertRaises(ValueError):
            release.install_native(self.archive, self.destination, self.pin)
        self.assertEqual(self.library.read_bytes(), b"previous usable library")

    def test_repair_replaces_corrupt_runtime(self):
        self.archive_with()
        release.install_native(self.archive, self.destination, self.pin)
        self.assertEqual(self.library.read_bytes(), self.payload)
        self.assertTrue(release.native_files_valid(self.destination, self.pin))

    def test_failed_publication_restores_previous_runtime(self):
        self.archive_with()
        rename = Path.rename

        def fail_publication(path, target):
            if path.name == "native" and path.parent != self.root:
                raise OSError("publication failed")
            return rename(path, target)

        with patch.object(Path, "rename", fail_publication), self.assertRaises(ValueError):
            release.install_native(self.archive, self.destination, self.pin)
        self.assertEqual(self.library.read_bytes(), b"previous usable library")

    def test_unsafe_archive_members_never_escape_staging(self):
        for name, link in [("../outside", False), ("native/../../outside", False),
                           ("native/lib/runtime.lib", True), ("native/C:outside", False)]:
            with self.subTest(name=name, link=link):
                self.archive_with(name=name, link=link)
                with self.assertRaises(ValueError):
                    release.install_native(self.archive, self.destination, self.pin)
                self.assertEqual(self.library.read_bytes(), b"previous usable library")
                self.assertFalse((self.root / "outside").exists())

    def test_verified_cargo_archive_repairs_corrupt_download_without_network(self):
        cached = self.root / "cached.tar.bz2"
        cached.write_bytes(self.payload)
        destination = self.root / "downloads/native.tar.bz2"
        destination.parent.mkdir()
        destination.write_bytes(b"corrupt")
        pin = {"sha256": hashlib.sha256(self.payload).hexdigest(), "url": "invalid://unused"}
        release.acquire_file(destination, pin, [cached])
        self.assertEqual(destination.read_bytes(), self.payload)

    def test_failed_acquisition_preserves_existing_archive(self):
        destination = self.root / "download.tar.bz2"
        destination.write_bytes(b"previous archive")
        pin = {"sha256": hashlib.sha256(self.payload).hexdigest(), "url": "invalid://refused"}
        with self.assertRaises(ValueError):
            release.acquire_file(destination, pin)
        self.assertEqual(destination.read_bytes(), b"previous archive")

    def test_download_hash_mismatch_does_not_publish_or_remove_old_archive(self):
        destination = self.root / "download.tar.bz2"
        destination.write_bytes(b"previous archive")
        pin = {"sha256": hashlib.sha256(self.payload).hexdigest(), "url": "https://example.invalid/native"}
        with patch.object(release.urllib.request, "urlopen", return_value=io.BytesIO(b"truncated")):
            with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                release.acquire_file(destination, pin)
        self.assertEqual(destination.read_bytes(), b"previous archive")


if __name__ == "__main__":
    unittest.main()
