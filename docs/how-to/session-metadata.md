---
title: Session Metadata
description: Store caller-defined JSON independently of the trace index and query its Session associations.
---

# Session Metadata

Session metadata is caller-owned JSON, not a Runtime fact or a generated interpretation. Trace Index accepts any complete JSON value and does not define keys or business semantics. Each `(runtime, native_id)` has at most one document. A set replaces the whole document; there is no automatic merge, inheritance, or history recovery.

## Select the metadata database

`config show` reports `metadata_database` with its resolved absolute path and origin. The default is `metadata.sqlite` in the platform data directory, alongside the default index. `--db` changes only the index. Select a separate metadata store explicitly when isolating projects:

```bash
trace-index --metadata-db /path/to/metadata.sqlite config show
```

Selection precedence is `--metadata-db`, `TRACE_INDEX_METADATA_DB`, `metadata_database` in configuration, then the platform default. Configuration-relative paths resolve against the configuration directory. Index and metadata paths must refer to different files.

## Set, get, and delete

Supply the Runtime name and its native Session identity, rather than the current index-local integer `session_id`. Metadata can be written before a Session has been indexed.

```bash
trace-index session metadata set --runtime pi --native-id SESSION_ID --stdin <<'JSON'
{"project":"example","review":{"needed":true}}
JSON

trace-index session metadata get --runtime pi --native-id SESSION_ID
trace-index session metadata delete --runtime pi --native-id SESSION_ID
```

Set accepts exactly one of `--json`, `--file`, or `--stdin`. It stores the original JSON text, including number literals and duplicate keys. Get returns the JSON value without interpreting it. JSON `null` is a document with `found: true`; no document has `found: false`. Get reads one whole document; use the bounded SQL query plane when only a field or a limited result is needed.

Set and delete return a compact committed receipt with `version` and `changed`. Identical stored text is a no-op; different formatting is a different stored document. Deleting an absent association is a no-op. A deletion retains its storage version, so recreating the document does not reset the version. Storage versions are outside the caller's JSON.

## Avoid lost updates

When multiple writers share a document, get its current JSON and version, modify the JSON according to the caller's protocol, and set with that version:

```bash
trace-index session metadata set --runtime pi --native-id SESSION_ID \
  --file revised.json --if-version VERSION_FROM_GET
```

A stale version fails without changing the document. A never-written key has version zero. Omitting `--if-version` requests unconditional whole-document replacement. Trace Index does not choose a merge policy for callers. After an interrupted write, get the document and version to determine the stored state; do not automatically bypass a version conflict.

## Query immediately

The next `query run` after a committed metadata write can read it without `index sync`. `session_metadata` reads the selected metadata store directly and joins its stable identity to the current index:

```sql
SELECT s.runtime, s.native_id, m.metadata
FROM sessions s
JOIN session_metadata m ON m.session_id = s.session_id
WHERE json_extract(m.metadata, '$.review.needed') = 1;
```

The JSON path and its meaning belong to this caller's example. Any other JSON protocol can use the same relation and SQLite JSON functions. Duplicate-key lookup and numeric conversions follow SQLite behavior; retaining the input text does not promise arbitrary-precision SQL arithmetic.

An association whose Session is absent from the selected index remains visible with `session_id IS NULL`. It does not create a Session:

```sql
SELECT runtime, native_id, metadata
FROM session_metadata
WHERE session_id IS NULL;
```

The existing SQL time and output budgets apply. Inspect completeness and truncation before treating a result as exhaustive. Queries still require an existing trace index; metadata get/set/delete can operate independently. Schema discovery uses declarations without opening either database.

## Persistence and read boundaries

Index rebuilds, index deletion, and Source moves do not delete metadata. Forked Sessions do not inherit it. Overwriting a document discards its former value; rebuilding the index cannot recover it.

Queries open the selected metadata store read-only through a tool-installed connection view. User SQL cannot attach arbitrary files or write either database. A missing metadata store gives an empty relation without creating files. A present but unreadable, corrupt, or unsupported store is an error, not an empty result. Metadata writes do not modify Runtime traces or the index. Index and metadata have separate snapshots, not a global atomic snapshot across both files.

Back up the metadata store with SQLite's consistent backup operation, or stop writers and checkpoint before copying it. Copying only the main file of an active WAL database is not a complete backup. Metadata is persistent user data and cannot be regenerated from Runtime traces; preserve it when recovering from format or corruption errors.
