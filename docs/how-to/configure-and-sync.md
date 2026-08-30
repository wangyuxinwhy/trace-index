---
title: Configure and Synchronize Traces
description: Configure Codex, Pi, and Claude Code roots, synchronize them, and inspect coverage.
---

# Configure and Synchronize Traces

Create a configuration file once:

```bash
trace-index config init
```

The generated file sets the database path and leaves example roots commented out. Enable only the Runtime roots that should be indexed:

```toml
database = "~/.local/share/trace-index/index.sqlite"

[[roots]]
name = "codex"
path = "~/.codex/sessions"

[[roots]]
name = "pi"
path = "~/.pi/agent/sessions"

[[roots]]
name = "claude"
path = "~/.claude/projects"
```

`config init` refuses to overwrite an existing file. Before indexing, inspect the fully resolved database and root paths:

```bash
trace-index config show
```

Relative paths are resolved against the configuration file's directory. `~` expands to the current user's home directory.

## Synchronize configured or one-off paths

Synchronize every configured root:

```bash
trace-index index sync
```

To index only particular files or directories, pass them explicitly. Positional paths replace the configured roots for that invocation:

```bash
trace-index index sync /path/to/one.jsonl /path/to/session-directory
```

Synchronization is the only operation that updates the index. `schema`, `query`, `record`, and `asset` commands never refresh it implicitly.

Normal synchronization skips unchanged Sources, reads append-only growth incrementally, and rebuilds a Source when its already indexed prefix changed. Use `--rebuild` only when matching Sources must be re-read even though their indexed prefixes still match:

```bash
trace-index index sync --rebuild
```

The physical and domain layers update differently:

- Newline-complete Runtime Records are appended to the physical layer and counted by `indexed_records`.
- A changed Source is projected again into the five domain objects: Source, Record, Session, Loop, and Item. Reprojection is necessary because a later Record can establish a Session identity, close a Loop, connect an Item to a Loop, or provide another physical witness for an Item.
- `metrics.writes` reports the rows written as `records`, `sessions`, `loops`, and `items`. `indexed_items` reports the Items projected during the run.

The index reads complete newline-terminated Records only. A partial trailing line remains outside the index until a later append completes it.

## Choose progress output

The final machine-readable result is written to stdout. Progress, when enabled, is written only to stderr, so it cannot corrupt the JSON result.

| `--progress` | stderr behavior |
| --- | --- |
| `auto` | Use `human` for a terminal and `ndjson` otherwise |
| `human` | Refresh a compact progress line for a person watching the run |
| `ndjson` | Emit one JSON progress object per update |
| `off` | Emit no progress |

Use `--include-sources` only when the result must include every discovered Source. Without it, ordinary successful Source entries are omitted while exceptional entries remain visible.

## Interpret skipped and failed Sources

One Source does not stop the remaining roots from being processed.

- `skipped` means a discovered `.jsonl` is not a supported trace, or its first Record is not complete yet. It contributes to `skipped_files`, and the command may still exit with status 0.
- `failed` means a supported trace could not be read or projected. It contributes to `failed_files`, includes an error, and makes the command exit non-zero after the other Sources have been processed.

Each Source is committed in one transaction. SQLite WAL mode lets read commands observe a consistent snapshot while synchronization continues; a reader never sees half of one Source update.

## Check the published snapshot

Inspect coverage without scanning trace files again:

```bash
trace-index index status
```

`incomplete_sources` counts Sources whose trailing Record was incomplete at the last synchronization. `observed_roles` reports the `semantic.role` values actually published as Items in this index. It describes the current corpus, not every role the compiled Adapters could produce.

For row and field names, inspect the current query contract rather than guessing:

```bash
trace-index schema list
trace-index schema get sources --compact
trace-index schema get records --compact
trace-index schema get sessions --compact
trace-index schema get loops --compact
trace-index schema get items --compact
```
