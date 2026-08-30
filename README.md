# Trace Index

Trace Index is a local, bounded-memory query plane for Agent trace files. It indexes Codex, Pi, and Claude Code JSONL traces into a source-traceable, read-only SQL surface while preserving the path back to the original Records.

[Documentation](https://wangyuxinwhy.github.io/trace-index/) · [Start here](https://wangyuxinwhy.github.io/trace-index/start-here) · [Install](https://wangyuxinwhy.github.io/trace-index/how-to/install)

## Install

Install from crates.io when a Rust toolchain is available:

```bash
cargo install trace-index --version 0.1.0 --locked
```

Or install a verified prebuilt binary on Apple Silicon macOS, Intel macOS, or x86-64 Linux:

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  'https://github.com/wangyuxinwhy/trace-index/releases/latest/download/install.sh' | sh
```

## Start in 60 seconds

Create a configuration and enable the trace roots you want to index:

```bash
trace-index config init
trace-index config show
trace-index index sync
trace-index index status
```

Discover the version-matched domain model and SQL contract before querying:

```bash
trace-index --help
trace-index docs get start-here
trace-index schema list
trace-index schema get items --compact
trace-index query run \
  "SELECT json_extract(semantic, '$.role') AS role, COUNT(*) AS item_count FROM items GROUP BY role"
```

Read commands never synchronize or mutate the index. Run `index sync` explicitly when newly written traces are required.

## For Agents

Root Help establishes the complete conceptual model needed to query correctly. Conditional workflows, exact Semantic value shapes, and search mechanics are available through bundled, version-matched documentation:

```bash
trace-index docs list
trace-index docs get how-to/query-evidence
trace-index docs get how-to/search-literals
trace-index schema get items
```

The documentation website also publishes `llms.txt`, `llms-full.txt`, and a Markdown representation of every page.

## Development

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --document-private-items --locked
npm ci
npm run docs:build
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the public contract and verification expectations.
