from __future__ import annotations

"""Integration-style tests for the Python wrapper API.

These tests verify the end-to-end path:
Python wrapper -> Rust extension -> on-disk image.
"""

import os
import tempfile
import unittest

from userspace_fs_api import NotFoundError, UserSpaceFS


class WrapperTests(unittest.TestCase):
    def setUp(self) -> None:
        """Create a fresh disk image per test for isolation."""
        self.tmp = tempfile.TemporaryDirectory(prefix="usfs-")
        self.image = os.path.join(self.tmp.name, "test.fsimg")
        UserSpaceFS.format(self.image, 4096)
        self.fs = UserSpaceFS.mount(self.image)

    def tearDown(self) -> None:
        """Best-effort cleanup of mounted filesystem and temp directory."""
        self.fs.unmount()
        self.tmp.cleanup()

    def test_create_write_read_truncate_remove(self) -> None:
        # Create file and write initial payload.
        self.fs.mkdir("/docs")
        self.fs.create("/docs/a.txt")
        with self.fs.open_file("/docs/a.txt") as f:
            n = f.write(b"hello world")
            self.assertEqual(n, 11)
        # Read full payload back.
        with self.fs.open_file("/docs/a.txt") as f:
            self.assertEqual(f.read(128), b"hello world")
        # Truncate and verify resulting prefix.
        self.fs.truncate("/docs/a.txt", 5)
        with self.fs.open_file("/docs/a.txt") as f:
            self.assertEqual(f.read(128), b"hello")
        # Remove and ensure file is no longer addressable.
        self.fs.remove("/docs/a.txt")
        self.assertEqual(self.fs.readdir("/docs"), [])
        with self.assertRaises(NotFoundError):
            self.fs.remove("/docs/a.txt")

    def test_remount_persistence(self) -> None:
        # Write data before unmount.
        self.fs.mkdir("/a")
        self.fs.create("/a/file")
        with self.fs.open_file("/a/file") as f:
            f.write(b"persist")
        # Remount and verify persisted contents.
        self.fs.unmount()
        self.fs = UserSpaceFS.mount(self.image)
        with self.fs.open_file("/a/file") as f:
            self.assertEqual(f.read(128), b"persist")


if __name__ == "__main__":
    unittest.main()
