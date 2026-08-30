# Contributing

Trace Index keeps a small public vocabulary and a strict evidence-versus-interpretation boundary. Before proposing a permanent entity, command, or shortcut, show which capability, safety boundary, or repeated experiment requires it.

## Development

The minimum supported Rust version is 1.95. The repository toolchain includes the formatter and Clippy.

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
CARGO_TARGET_DIR=target/package-verify cargo package --locked
```

`cargo package` needs its own target directory. Its verification step compiles a copy of the crate under `target/package/`, and sharing `target/debug` makes Cargo record that frozen copy as the bin target's sources. Every later `cargo build` then reports `Fresh` no matter what you edit, and hand-testing silently exercises the previous binary. Use `cargo clean -p trace-index` if a shared run already happened.

An Adapter's facts come from the code that writes the trace. Codex and Pi are open source and the Pi distribution ships unminified JavaScript; Claude Code publishes an SDK. Inferring a format from traces is the fallback, used where the source cannot answer and labelled as inference when it is — a trace is one version's output under one configuration, while the source states what the format is. Searching a shipped binary is not reading the source: a stripped build cannot distinguish a text marker from a serialization field name.

A change to the projection changes what every existing index contains, so a claim about it needs an index the current code built. State which facts the change should move before rebuilding; conservation of the facts it should not change is usually the sharper check. Private trace corpora and evaluation assets must never be added to the public repository.

Changes to the CLI, public SQL relations, machine output, Storage Format, or bundled documentation must include contract tests. Indexing changes should preserve bounded memory, per-Source transactions, source-byte verification, and append/rewrite coverage.

Use natural self-hosted Agent tasks to validate discoverability and friction. Do not encode the expected tool sequence in an experiment prompt.
