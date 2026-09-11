# Trace Index for Python

Native PyO3 bindings to the same Rust engine as the CLI. No CLI executable,
subprocess, service, or credentials are required. Python 3.10+ is supported.

SQL remains the query entry point. This page covers installation and routes you
to standard Python Help and the shared tutorial, rather than duplicating their
API and domain contracts.

## Build and install

From the repository, with the repository Rust toolchain installed:

```bash
uvx maturin build --release --locked --manifest-path python/Cargo.toml
# Install the wheel matching your platform into your chosen Python environment:
uv pip install --python /path/to/python target/wheels/<wheel-filename>.whl
```

The wheel bundles its Rust implementation, SQLite, docs, and schema. The Python
package version matches that engine; a separately installed CLI can be another
version. No package or release publication is implied by building a wheel.

## Start with standard Python Help

```python
import trace_index as ti

help(ti)
print(ti.doc("start-here"))
help(ti.Index.query)
```

`help(ti)` establishes the same conceptual model and evidence boundaries as CLI
root Help. The common text comes from Rust; Semantic role meanings come from the
domain types and Relation names come from public Schema declarations. Python
adds its own discovery syntax, return types, exceptions and connection lifetime.
The shared context is composed into `__doc__`, not exposed as a new Help protocol.

The bundled `start-here` tutorial supplies CLI and Python entry points around one
set of domain explanations and SQL examples. Help and bundled docs work offline,
without loading configuration or opening an index.

## Discover only the detail you need

| Question | Python entry point |
| --- | --- |
| How do I select or open an index? | `help(ti.Index)` |
| How do I explicitly create or refresh its facts? | `help(ti.Index.sync)` |
| How do parameters and query errors work? | `help(ti.Index.query)` |
| What are the budget defaults and limits? | `help(ti.QueryBudget)` |
| What does a partial result mean? | `help(ti.QueryResult)` |
| What are the current Relations and columns? | `index.schema()` / `index.schema("items")` |
| What does physical inspection return? | `help(ti.Record.inspect)` |
| Which conditional workflows are available? | `ti.docs()` / `ti.doc(topic)` |

Use `object.__doc__` and `inspect.signature(object)` when programmatic inspection
is more convenient. Method docstrings own local API details; generated `.pyi`
files preserve those docstrings for tools that read stubs. Configuration creation,
byte-exact export and media extraction remain explicit CLI operations.

## Development checks

```bash
python3 scripts/generate_python_stubs.py --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --document-private-items
uvx maturin build --locked --manifest-path python/Cargo.toml
# In an environment with that wheel installed, from the repository root:
TRACE_INDEX_CLI="$PWD/target/debug/trace-index" /path/to/python -m unittest discover -s python/tests -v
```

`__init__.pyi` is generated from the Python facade declarations and local
docstrings. Change the owner in `__init__.py`, then run
`python3 scripts/generate_python_stubs.py`. Shared bootstrap knowledge is owned
by `src/interface/help.rs`; neither frontend keeps a separate copy. Rust report
shapes and public Schema declarations remain engine-owned. Tests exercise
installed-wheel Help, standard doctests, shared tutorial examples, and the
compiled CLI, including exact common-context and budget-default parity.
