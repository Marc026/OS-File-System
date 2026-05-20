# Rust/Python Implementation Notes

This document focuses on the implementation details of the active
Rust + Python architecture in this repository.

## Architecture Overview

### Rust core (`rust/src/lib.rs`)

The Rust layer implements:

- Superblock, inode, directory entry, and bitmap logic
- Path resolution and directory traversal
- File block read/write/truncate behavior
- Metadata journaling markers and mount-time replay checks
- Native extension boundary exported to Python via `pyo3`

Rust is used for the core because this layer is performance-sensitive and
benefits from compile-time memory safety guarantees.

### Python layer (`python/`)

The Python layer provides:

- `userspace_fs_api.py`: Pythonic wrapper (`UserSpaceFS`) over Rust extension
- `fs_cli.py`: operational CLI for common filesystem workflows
- `demo.py`: smoke demonstration
- `test_wrapper.py`: integration-style tests

Python is used for developer ergonomics, scripting, and straightforward test/CLI integration.

## Build and Installation

### Option A: Build wheel and install (recommended for Windows)

```bash
py -3.13 -m pip install maturin
py -3.13 -m maturin build --release -i "C:\Users\<you>\AppData\Local\Programs\Python\Python313\python.exe"
py -3.13 -m pip install --force-reinstall target\wheels\userspace_fs-0.1.0-cp313-cp313-win_amd64.whl
```

### Option B: Editable-like local install (virtualenv)

```bash
pip install maturin
maturin develop
```

## Running the Project

### Demo

```bash
py -3.13 python\demo.py
```

### Tests

```bash
py -3.13 python\test_wrapper.py
```

### CLI

```bash
py -3.13 python\fs_cli.py --image demo.fsimg mkfs --blocks 4096
py -3.13 python\fs_cli.py --image demo.fsimg mkdir /docs
py -3.13 python\fs_cli.py --image demo.fsimg write /docs/hello.txt "hello from cli"
py -3.13 python\fs_cli.py --image demo.fsimg ls /docs
py -3.13 python\fs_cli.py --image demo.fsimg cat /docs/hello.txt
py -3.13 python\fs_cli.py --image demo.fsimg truncate /docs/hello.txt 5
py -3.13 python\fs_cli.py --image demo.fsimg rm /docs/hello.txt
```

## Behavior Notes

- Journal records currently use marker-style transactions (`TX_BEGIN`, `TX_COMMIT`).
- Mount validates marker pairing and clears journal region after replay checks.
- Directory deletion requires the directory to be empty.
- File data uses direct block pointers up to configured direct-pointer capacity.
