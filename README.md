# User-Space Filesystem (Rust + Python)

This repository implements a user-space filesystem with block-based storage,
directory traversal, file IO, and metadata journaling semantics.

The project is intentionally split into:

- **Rust core** (`rust/src/lib.rs`): on-disk structures, allocation, path/inode
  operations, and the native filesystem engine.
- **Python layer** (`python/`): ergonomic API wrapper, CLI commands, demo script,
  and test suite.

## What This Codebase Provides

- File lifecycle operations: create, open, read, write, truncate, remove
- Directory operations: mkdir, readdir, path-based traversal
- Block-backed disk image storage
- Metadata journaling markers (`TX_BEGIN`, `TX_COMMIT`) with mount-time replay checks
- Native Python extension module backed by Rust (`pyo3`)

## Tech Stack

- **Rust**: core implementation for performance and memory safety
- **PyO3**: Rust-to-Python bindings (`userspace_fs` extension module)
- **Maturin**: build/package workflow for Python-native wheel generation
- **Python 3.13**: API wrapper, CLI tooling, demo, and tests
- **GitHub Actions**: CI pipeline for build + wrapper test verification

## Repository Layout

- `rust/src/lib.rs`: Rust filesystem engine + Python extension entrypoint
- `python/userspace_fs_api.py`: Pythonic wrapper over native extension
- `python/fs_cli.py`: command-line interface (`mkfs`, `mkdir`, `ls`, `write`, `cat`, `truncate`, `rm`)
- `python/demo.py`: end-to-end smoke demonstration
- `python/test_wrapper.py`: wrapper integration tests
- `Cargo.toml`, `pyproject.toml`: Rust/Python build configuration

## Prerequisites

- Rust toolchain (`rustc`, `cargo`) installed via `rustup`
- Python 3.13
- `pip`

## Install and Build

From the repository root:

```bash
py -3.13 -m pip install maturin
py -3.13 -m maturin build --release -i "C:\Users\<you>\AppData\Local\Programs\Python\Python313\python.exe"
py -3.13 -m pip install --force-reinstall target\wheels\userspace_fs-0.1.0-cp313-cp313-win_amd64.whl
```

If you are using a virtual environment, you can also run:

```bash
maturin develop
```

## Run and Verify

### Demo workflow

```bash
py -3.13 python\demo.py
```

### Test workflow

```bash
py -3.13 python\test_wrapper.py
```

Expected test signal:

- `Ran 2 tests`
- `OK`

### CLI usage

```bash
py -3.13 python\fs_cli.py --image demo.fsimg mkfs --blocks 4096
py -3.13 python\fs_cli.py --image demo.fsimg mkdir /docs
py -3.13 python\fs_cli.py --image demo.fsimg write /docs/hello.txt "hello from cli"
py -3.13 python\fs_cli.py --image demo.fsimg ls /docs
py -3.13 python\fs_cli.py --image demo.fsimg cat /docs/hello.txt
py -3.13 python\fs_cli.py --image demo.fsimg truncate /docs/hello.txt 5
py -3.13 python\fs_cli.py --image demo.fsimg rm /docs/hello.txt
```

## CI

CI is defined in `.github/workflows/ci.yml` and runs on push/pull request:

1. Set up Python and Rust
2. Build extension wheel with `maturin`
3. Install generated wheel
4. Run `python/test_wrapper.py`
