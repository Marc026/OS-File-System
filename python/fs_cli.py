from __future__ import annotations

"""Small operator-facing CLI for validating and using the filesystem.

The CLI intentionally stays thin: all filesystem semantics are delegated to the
Python API wrapper, which delegates to the Rust core.
"""

import argparse
import sys

from userspace_fs_api import FilesystemError, UserSpaceFS


def main() -> int:
    """Parse command line arguments and execute one filesystem operation."""
    parser = argparse.ArgumentParser(prog="fs", description="User-space FS CLI")
    parser.add_argument("--image", required=True, help="filesystem image path")
    sub = parser.add_subparsers(dest="cmd", required=True)

    mkfs = sub.add_parser("mkfs", help="format image")
    mkfs.add_argument("--blocks", type=int, default=4096, help="total blocks")

    mkdir = sub.add_parser("mkdir", help="create directory")
    mkdir.add_argument("path")

    ls = sub.add_parser("ls", help="list directory")
    ls.add_argument("path")

    write = sub.add_parser("write", help="write UTF-8 text into file")
    write.add_argument("path")
    write.add_argument("text")

    cat = sub.add_parser("cat", help="print file bytes as UTF-8")
    cat.add_argument("path")

    rm = sub.add_parser("rm", help="remove file or empty directory")
    rm.add_argument("path")

    trunc = sub.add_parser("truncate", help="truncate file to size")
    trunc.add_argument("path")
    trunc.add_argument("size", type=int)

    args = parser.parse_args()
    try:
        # mkfs is the only operation that does not require an existing mount.
        if args.cmd == "mkfs":
            UserSpaceFS.format(args.image, args.blocks)
            return 0

        # All other operations happen within a mounted filesystem session.
        with UserSpaceFS.mount(args.image) as fs:
            if args.cmd == "mkdir":
                fs.mkdir(args.path)
            elif args.cmd == "ls":
                # Print one entry per line to stay shell-friendly.
                for name in fs.readdir(args.path):
                    print(name)
            elif args.cmd == "write":
                # `write` is treated as upsert for convenience in shell workflows.
                try:
                    fs.create(args.path)
                except FilesystemError:
                    pass
                with fs.open_file(args.path) as f:
                    f.write(args.text.encode("utf-8"))
            elif args.cmd == "cat":
                with fs.open_file(args.path) as f:
                    print(f.read(1 << 20).decode("utf-8"))
            elif args.cmd == "rm":
                fs.remove(args.path)
            elif args.cmd == "truncate":
                fs.truncate(args.path, args.size)
        return 0
    except FilesystemError as exc:
        # Use non-zero exit code for automation/CI compatibility.
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
