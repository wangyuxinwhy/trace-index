---
title: Configure and Synchronize Traces
description: Configure Runtime trace roots, synchronize them, and inspect coverage.
---

# Configure and Synchronize Traces

Create a configuration file once. To opt in to every standard Runtime root that currently exists:

```bash
trace-index config init --discover
```

For precise scope, provide one or more roots explicitly instead. `config init` does not create the database or synchronize any traces:

```bash
trace-index config init \
  --root ~/.codex/sessions \
  --root /work/selected-traces
```

The generated file records the schema version, initial indexing policy, and only the roots explicitly selected or discovered. Paths supplied to `config init` are written as absolute paths. `database` is omitted unless initialization receives `--db`; the platform default remains visible through `config show`.

```toml
schema_version = 1

[indexing]
max_indexed_record_bytes = 16777216
max_published_text_bytes = 65536

[[roots]]
path = "/absolute/path/to/.codex/sessions"

[[roots]]
path = "/work/selected-traces"
```

Roots added by `--discover` carry the diagnostic labels `codex`, `pi`, `claude`, or `antigravity`.

`config init` refuses to overwrite an existing file. Before indexing, inspect the resolved database, origins, initial policy, and root paths, then run the read-only root and database-path preflight:

```bash
trace-index config show
trace-index config check
```

Relative paths are resolved against the configuration file's directory. `~` expands to the current user's home directory.

The root label is diagnostic only. Trace Index detects each Source using Runtime evidence. Antigravity CLI detection also uses its native transcript path.

## Synchronize configured or one-off paths

Synchronize every configured root:

```bash
trace-index index sync
```

To index only particular files or directories, pass them explicitly. Positional paths replace the configured roots for that invocation:

```bash
trace-index index sync /path/to/one.jsonl /path/to/session-directory
```

`config init --discover` includes `~/.gemini/antigravity-cli/brain` when present; on an existing configuration, add that root or pass it as a positional path to opt in to Antigravity CLI history. Format 2, Format 3, and Format 4 databases remain readable in place but cannot be synchronized by this release (`config check` emits a `database_format_upgrade_required` warning when an existing database uses an older format). Select a new Format 5 database with `--db` (or update `database` in `config.toml`), supply the available roots you want to index, and if `index watch` is installed, reinstall it against the new database. Keep existing older databases if they contain history whose original Sources are unavailable.

Synchronization is the only operation that updates the index. `schema`, `query`, `record`, and `asset` commands never refresh it implicitly.

The first synchronization persists `max_indexed_record_bytes` and `max_published_text_bytes` as one database-wide policy. Once the index contains Sources, that stored policy is authoritative and ordinary synchronization reuses it even if the configuration file later changes. A conflicting `--max-record-bytes` or `--max-text-bytes` request is rejected. Use a new database and reindex every Source to change this policy; `--rebuild` does not replace it.

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

- `skipped` covers files without a complete, recognizable trace header and Sources excluded by Runtime evidence or sibling selection. The per-Source result explains the reason. Skips contribute to `skipped_files`, and the command may still exit with status 0.
- `failed` means a Source could not be read, decoded, or projected. Failures contribute to `failed_files`, include an error, and make the command exit non-zero after the other Sources have been processed.

Each Source is committed in one transaction. SQLite WAL mode lets read commands observe a consistent snapshot while synchronization continues; a reader never sees half of one Source update.

## Check the published snapshot

Inspect coverage, freshness, and live watch state without scanning trace files again:

```bash
trace-index index status
```

`incomplete_sources` counts Sources whose trailing Record was incomplete at the last synchronization. `watch` reports whether an `index watch` process currently holds the advisory lock for this database (`watch.active`) and whether a user-level OS service definition is installed (`watch.service_installed`). `observed_roles` reports the `semantic.role` values actually published as Items in this index. It describes the current corpus, not every role the compiled Adapters could produce.

## Keep the index fresh with event-driven watch (opt-in)

Installing `trace-index` never registers a background process or scheduled task. When you want the index to stay close to fresh automatically without hand-writing service files, activate the built-in event-driven watcher explicitly:

```bash
# Run in the foreground (Ctrl-C to stop):
trace-index index watch run

# Or opt in to a user-level OS service (macOS LaunchAgent / Linux systemd --user):
trace-index index watch install
trace-index index watch status

# Remove the user-level OS service at any time:
trace-index index watch uninstall
```

`index watch` observes configured `[[roots]]` (or explicit `PATHS` for `index watch run`) using OS filesystem notifications, coalesces rapid writes per file (`--debounce-ms`, `--max-wait-ms`), runs targeted incremental synchronization on only the changed trace files, and performs a low-frequency incremental safety-net reconcile (`--reconcile-secs`, `0` to disable; unchanged complete files are skipped by size and modification time) to recover from lagged or dropped OS events. It ignores the database, lock, and non-trace files, opens `SQLite` only during active sync batches, and holds an exclusive advisory lock so at most one watcher runs per database. Use `--dry-run` on `install` or `uninstall` to inspect the generated service path and definition without modifying anything.

Read commands (`query`, `schema`, `record`, `asset`, `index status`) remain strictly read-only whether or not `index watch` is active. Default to querying directly; run `index sync` explicitly only when newly written traces are required and watch is inactive (or when immediate freshness is needed).

For row and field names, inspect the current query contract rather than guessing:

```bash
trace-index schema list
trace-index schema get sources records sessions loops items --compact
```
