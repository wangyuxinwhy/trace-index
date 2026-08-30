---
title: Configuration Reference
description: Configuration schema, path resolution, defaults, provenance, and precedence.
---

# Configuration Reference

Trace Index configuration selects the rebuildable database, the trace roots used when `index sync` receives no positional paths, and the database-wide indexing policy. Runtime adapters are detected from Source contents; a root does not select an adapter.

The default configuration path is `$XDG_CONFIG_HOME/trace-index/config.toml`, falling back to `$HOME/.config/trace-index/config.toml`. If no configuration exists, built-in defaults remain usable and the root list is empty.

The default database path is `$XDG_DATA_HOME/trace-index/index.sqlite`, falling back to `$HOME/.local/share/trace-index/index.sqlite`.

```toml
schema_version = 1
database = "~/.local/share/trace-index/index.sqlite"

[indexing]
max_indexed_record_bytes = 16777216
max_published_text_bytes = 65536

[[roots]]
label = "personal-codex"
path = "~/.codex/sessions"

[[roots]]
label = "work-claude"
path = "~/.claude/projects"
```

Fields:

- `schema_version`: configuration contract version. It defaults to `1` for compatibility with files created before this field existed; new files write it explicitly.
- `database`: optional path to the rebuildable SQLite index.
- `indexing.max_indexed_record_bytes`: maximum bytes retained and parsed for one JSONL Record. The built-in value is `16777216`.
- `indexing.max_published_text_bytes`: maximum UTF-8 bytes published for one Semantic text value. The built-in value is `65536`.
- `roots`: zero or more files or directories recursively searched for `.jsonl` Sources.
- `roots[].label`: optional diagnostic label. It does not select Codex, Pi, or Claude Code.
- `roots[].path`: Source file or directory path.

`roots[].name` from configuration files created by Trace Index 0.1.0 remains accepted as an alias for `label`; new files write `label`.

Unknown fields, an unsupported `schema_version`, zero indexing limits, empty paths, and duplicate resolved root paths are errors.

## Paths

Every path returned by `config show` is absolute.

- Relative paths in the configuration file are resolved against that file's directory.
- Relative `--config`, `--db`, `TRACE_INDEX_CONFIG`, and `TRACE_INDEX_DB` paths are resolved against the process working directory.
- Empty `TRACE_INDEX_CONFIG` and `TRACE_INDEX_DB` values are errors because they are explicit overrides; empty XDG variables are ignored.
- A leading `~` or `~/` expands to the current home directory. Other user-home forms such as `~other` are rejected.
- `XDG_CONFIG_HOME` and `XDG_DATA_HOME`, when non-empty, must be absolute.

Paths are not canonicalized during loading because a database or root may not exist yet. `config check` and `index sync` canonicalize existing roots when they need physical identity.

## Precedence and provenance

The configuration file is selected in this order:

1. `--config`
2. `TRACE_INDEX_CONFIG`
3. the platform default configuration path

The database is selected independently:

1. `--db`
2. `TRACE_INDEX_DB`
3. `database` in the selected file
4. the platform default database path

Indexing limits for a new or empty database come from `[indexing]` and then built-in defaults. After a database contains Sources, its stored policy is authoritative and ordinary synchronization reuses it. The two `index sync --max-*-bytes` flags are explicit one-run requests; a request that conflicts with a non-empty database is rejected.

Positional `index sync PATHS...` replace configured roots for that invocation. There is no environment-variable root list.

`config show` returns resolved absolute configuration values together with `cli`, `environment`, `file`, or `default` origin metadata. For indexing limits, these are the values that would initialize a new or empty database. The command does not open the database; use `index status` for an established policy.

## Indexing policy consistency

The two indexing limits change which Source content becomes a published fact, so the first synchronization stores them as one database-wide policy. The database is the single owner of that policy after it contains Sources; changing `[indexing]` affects new or empty databases, not existing indexed facts. A conflicting CLI override is rejected. Create a new database and reindex every Source when changing policy; `--rebuild` does not change the policy at any scope.

`index status` and every synchronization report return the stored policy.

## Configuration commands

- `config init` creates and validates one file without overwriting, creating a database, or synchronizing traces. It records the built-in initial indexing values; an existing non-empty database still owns its established policy. `--root PATH` is repeatable, `--discover` adds only standard Runtime roots that currently exist, and `--db` writes an initial database path.
- `config show` resolves values and reports their origins without inspecting the filesystem.
- `config check` performs a read-only preflight of configured root existence, type, canonical duplication, and database path ancestry. Parse and schema errors exit before a report. In a returned report, Error issues make `valid` false and cause exit 1; Warning-only reports exit 0. `configured_sync_ready` additionally requires at least one configured root.
