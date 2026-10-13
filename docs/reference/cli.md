---
title: CLI Reference
description: Stable command tree and capability boundaries.
---

# CLI Reference

Every command in the current binary, and what each one does:

The first `trace-index --help` screen establishes the complete prerequisite model and query boundary an Agent needs before choosing a command. Progressive disclosure starts after that point: noun and verb Help own exact arguments, defaults, limits, side effects, and the smallest command-specific example; bundled How-to pages own conditional workflows such as text search, shell analysis, configuration precedence, and physical export.

<!-- generated: cli-table — run `UPDATE_DOCS=1 cargo test generated_documentation` to refresh -->

| Command | What it does |
| --- | --- |
| `session` | Store and read caller-defined Session data, independent of trace indexing |
| `session metadata` | Associate a Session with arbitrary caller-defined JSON (no business schema) |
| `session metadata set` | Replace the whole JSON document; no merge, history or implicit index sync |
| `session metadata get` | Read one whole document and its version; absence is found=false, JSON null is found=true |
| `session metadata delete` | Remove the association, retaining its version; does not delete trace or index data |
| `docs` | Read documentation bundled with the current CLI |
| `docs list` | List bundled documentation topics |
| `docs get` | Get one documentation topic |
| `docs search` | Search bundled documentation topics |
| `config` | Initialize and inspect trace-index configuration |
| `config init` | Create a new configuration file without overwriting an existing one |
| `config show` | Show resolved configuration values and their origins |
| `config check` | Validate configuration and configured root readiness without mutation |
| `index` | Maintain and inspect the current trace index |
| `index sync` | Incrementally index supported Agent trace files or directories |
| `index status` | Show index coverage, parse status, and source freshness |
| `schema` | Describe the stable public SQL query contract |
| `schema list` | List public SQL relations and their descriptions |
| `schema get` | Get one public SQL relation's schema |
| `query` | Execute one bounded read-only SQL statement against public relations |
| `query run` | Run one bounded read-only SQL statement |
| `record` | Inspect, verify, or export physical JSONL records |
| `record inspect` | Verify and inspect one display-safe source-backed Record representation |
| `record verify` | Verify one Record against its original source bytes |
| `record export` | Write one byte-exact verified Record to a file |
| `asset` | Extract inline media referenced by Records |
| `asset extract` | Decode one inline asset and write it to a file |

<!-- /generated -->

Use `trace-index --help`, `trace-index <noun> --help`, and `trace-index <noun> <verb> --help` for version-matched concepts and command details.

`docs`, `config`, `schema`, and `session metadata` can operate without opening the index. `index sync` writes the trace index; `session metadata set/delete` write the independent caller-owned metadata store. Queries open the selected index and metadata store read-only. See [Session Metadata](../how-to/session-metadata.md) for whole-document replacement and version checks.

The CLI intentionally has no shortcut command for a query that SQL already expresses. A new command must establish a safety or side-effect boundary, expose a capability SQL cannot reliably express, or be justified by repeated experiments.
