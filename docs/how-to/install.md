---
title: Install Trace Index
description: Install a verified prebuilt binary or build Trace Index from crates.io.
---

# Install Trace Index

Trace Index publishes native binaries for x86-64 Linux, Intel macOS, and Apple Silicon macOS. Install the latest release without Rust or Cargo:

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  'https://github.com/wangyuxinwhy/trace-index/releases/latest/download/install.sh' | sh
```

Then confirm what was installed:

```bash
trace-index --version
trace-index docs list
```

The default destination is `~/.local/bin/trace-index`. The installer never uses `sudo`; add that directory to `PATH` if needed.

## Install one explicit version

Download the installer before passing options:

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  'https://github.com/wangyuxinwhy/trace-index/releases/latest/download/install.sh' \
  -o /tmp/trace-index-install.sh
sh /tmp/trace-index-install.sh --version 0.4.2 --bin-dir "$PWD/bin"
```

The script supports:

```text
--version VERSION   install an explicit version instead of latest
--bin-dir DIR       install somewhere other than ~/.local/bin
--target TARGET     override the detected release target triple
--base-url URL      override the GitHub Releases root
--dry-run           resolve the target without downloading or changing files
```

On x86-64 Linux, the installer selects the statically linked `x86_64-unknown-linux-musl` binary by default so it runs across older glibc distributions as well as musl environments. `x86_64-unknown-linux-gnu` is also published and can be selected explicitly with `--target`.

It requires an absolute destination and HTTPS. Plain HTTP is accepted only with explicit `--allow-http` for the repository's local end-to-end test.

Each archive contains only `trace-index` and `LICENSE`. The installer downloads its sibling SHA-256 file, rejects unexpected archive paths, verifies that the binary reports the requested version, and then atomically replaces the destination.

## Install from crates.io

With Rust 1.95 or newer:

```bash
cargo install trace-index --version 0.4.2 --locked
trace-index --version
```

`cargo install` compiles locally and can take longer than downloading a native release. Both installation paths provide the same CLI, bundled documentation, and public Schema.

## Verify the installer locally

Repository maintainers can test the complete package, download, checksum, and installation path without publishing:

```bash
./scripts/test-installer.sh
```
