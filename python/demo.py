from userspace_fs_api import UserSpaceFS


def main() -> None:
    """Simple smoke workflow for manual verification."""
    image = "demo.fsimg"
    # Create a fresh image to make demo output deterministic.
    UserSpaceFS.format(image, 4096)
    with UserSpaceFS.mount(image) as fs:
        # Create namespace and file.
        fs.mkdir("/docs")
        fs.create("/docs/hello.txt")
        # Write and read full content.
        with fs.open_file("/docs/hello.txt") as f:
            f.write(b"hello from rust")
        with fs.open_file("/docs/hello.txt") as f:
            print(f.read(128).decode())
        print(fs.readdir("/docs"))
        # Truncate and verify shortened payload.
        fs.truncate("/docs/hello.txt", 5)
        with fs.open_file("/docs/hello.txt") as f:
            print(f.read(128).decode())
        # Remove file and show now-empty directory.
        fs.remove("/docs/hello.txt")
        print(fs.readdir("/docs"))


if __name__ == "__main__":
    main()
