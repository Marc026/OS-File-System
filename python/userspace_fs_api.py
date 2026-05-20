from __future__ import annotations

"""Pythonic wrapper over the Rust-backed filesystem extension.

This layer intentionally keeps policy and ergonomics in Python:
- maps Rust runtime errors into typed Python exceptions
- offers context-manager based resource handling
- exposes bytes-first read/write helpers for callers
"""

from contextlib import contextmanager
from dataclasses import dataclass
from typing import Iterator

from userspace_fs import PyUserSpaceFs


class FilesystemError(Exception):
    pass


class NotFoundError(FilesystemError):
    pass


class AlreadyExistsError(FilesystemError):
    pass


class NotDirectoryError(FilesystemError):
    pass


class IsDirectoryError(FilesystemError):
    pass


def _map_error(exc: Exception) -> FilesystemError:
    """Translate string-based Rust errors into typed Python exceptions."""
    msg = str(exc).lower()
    # Map known message fragments to stable Python exception types.
    if "not found" in msg:
        return NotFoundError(str(exc))
    if "already exists" in msg:
        return AlreadyExistsError(str(exc))
    if "not a directory" in msg:
        return NotDirectoryError(str(exc))
    if "is a directory" in msg:
        return IsDirectoryError(str(exc))
    return FilesystemError(str(exc))


@dataclass
class FileHandle:
    """High-level file handle wrapper that owns a Rust file descriptor."""

    _fs: "UserSpaceFS"
    _fd: int
    _closed: bool = False

    def read(self, size: int = -1) -> bytes:
        # Prevent use-after-close bugs at the wrapper boundary.
        if self._closed:
            raise FilesystemError("file handle already closed")
        return self._fs.read(self._fd, size)

    def write(self, data: bytes) -> int:
        # Writes pass through to Rust and return bytes-written.
        if self._closed:
            raise FilesystemError("file handle already closed")
        return self._fs.write(self._fd, data)

    def close(self) -> None:
        # Close is idempotent to simplify caller cleanup logic.
        if not self._closed:
            self._fs.close(self._fd)
            self._closed = True

    def __enter__(self) -> "FileHandle":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()


class UserSpaceFS:
    """Entry point used by Python applications and tooling.

    The underlying storage and mutation logic stays in Rust (`PyUserSpaceFs`);
    this class focuses on error mapping and ergonomic lifecycle handling.
    """

    def __init__(self, inner: PyUserSpaceFs):
        # `inner` is the native extension object that owns real resources.
        self._inner = inner
        self._mounted = True

    @classmethod
    def format(cls, image_path: str, total_blocks: int) -> None:
        # Formatting is a static operation and does not require mount state.
        PyUserSpaceFs.format(image_path, total_blocks)

    @classmethod
    def mount(cls, image_path: str) -> "UserSpaceFS":
        try:
            # Wrap native object to provide Python-friendly API semantics.
            return cls(PyUserSpaceFs.mount(image_path))
        except Exception as exc:  # Rust exception mapped to python error
            raise _map_error(exc) from exc

    def unmount(self) -> None:
        # Avoid duplicate native unmount calls.
        if not self._mounted:
            return
        try:
            self._inner.unmount()
            self._mounted = False
        except Exception as exc:
            raise _map_error(exc) from exc

    def mkdir(self, path: str) -> None:
        try:
            self._inner.mkdir(path)
        except Exception as exc:
            raise _map_error(exc) from exc

    def create(self, path: str) -> None:
        try:
            self._inner.create(path)
        except Exception as exc:
            raise _map_error(exc) from exc

    def remove(self, path: str) -> None:
        try:
            self._inner.remove(path)
        except Exception as exc:
            raise _map_error(exc) from exc

    def readdir(self, path: str) -> list[str]:
        try:
            # Native return type is iterable; normalize to concrete list.
            return list(self._inner.readdir(path))
        except Exception as exc:
            raise _map_error(exc) from exc

    def open(self, path: str) -> FileHandle:
        try:
            fd = self._inner.open(path)
            # Return a wrapper handle that tracks closed-state in Python.
            return FileHandle(self, fd)
        except Exception as exc:
            raise _map_error(exc) from exc

    @contextmanager
    def open_file(self, path: str) -> Iterator[FileHandle]:
        """Open a file and guarantee close() even on exceptions."""
        fh = self.open(path)
        try:
            yield fh
        finally:
            fh.close()

    def close(self, fd: int) -> None:
        try:
            self._inner.close(fd)
        except Exception as exc:
            raise _map_error(exc) from exc

    def read(self, fd: int, size: int = -1) -> bytes:
        try:
            # Use a conservative default upper bound for "read all".
            if size < 0:
                size = 1 << 20
            return bytes(self._inner.read(fd, size))
        except Exception as exc:
            raise _map_error(exc) from exc

    def write(self, fd: int, data: bytes) -> int:
        try:
            # Ensure immutable bytes are passed into native layer.
            return int(self._inner.write(fd, bytes(data)))
        except Exception as exc:
            raise _map_error(exc) from exc

    def truncate(self, path: str, size: int) -> None:
        try:
            self._inner.truncate(path, size)
        except Exception as exc:
            raise _map_error(exc) from exc

    def __enter__(self) -> "UserSpaceFS":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.unmount()
