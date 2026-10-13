---
title: Architecture
description: Module boundaries, SQLite lifecycle, and feature admission principles.
---

# Architecture

Trace Index is a single Rust binary backed by a rebuildable SQLite derivative.

```mermaid
flowchart TD
    source["Runtime JSONL Sources"] --> ingest["Bounded Record recognition and evidence"]
    ingest --> adapter["Stateful Runtime Adapter"]
    adapter --> domain["Source · Record · Session · Loop · Item"]
    domain --> persist["Transactional persistence"]
    persist --> sqlite["SQLite Storage Format"]
    sqlite --> public["Five domain Relations"]
    sqlite --> blob["Blob text access"]
    sqlite --> search["Item text candidate index"]
    public --> query["Bounded read-only Query Plane"]
    blob --> query
    search --> query
```

## Code boundaries

- `ingest` discovers Sources, reads complete Records within explicit bounds, and maintains byte locators and fingerprints.
- `adapters` detects Codex, Pi, and Claude Code and projects ordered Records into adapter-neutral facts. It owns Runtime mappings and private rule diagnostics.
- `domain` declares the Semantic role/value contract, evidence strength, Loop outcomes, and typed nested values.
- `storage` defines Storage Format 2, persists the database-wide indexing policy together with physical and domain facts, privately deduplicates bounded Semantic text, publishes five domain Relations plus the `blobs` and `item_search` access Relations, and validates database shape.
- `indexing` coordinates per-Source synchronization, status, progress, and telemetry.
- `interface` owns configuration, bundled documentation, public-schema discovery, bounded SQL, and stable machine output.
- `shell` extracts the shell structure embedded in supported Semantic tool-call values. Its structs are nested Item values rather than additional public domain entities.

## Stateful projection

Record framing is physical and local. Domain projection is stateful because Session identity, Loop boundaries, multi-Record Item evidence, request-versus-steering, and tool call/output correlation depend on surrounding Records.

An Adapter processes one ordered candidate file and emits facts only after their required structural evidence exists. A Source that resumes an existing logical context contributes to the same Session; replacing that Source's projection must not erase facts contributed by the Session's other Sources. A candidate file without a confirmed Session identity is skipped atomically rather than being published as a partial Source.

## Incremental physical input, replaceable domain projection

The physical layer indexes complete appended Records and fingerprints the indexed prefix. A changed prefix triggers a Source rebuild. The domain projection for a changed Source is replaced coherently because later Records can resolve earlier optional fields or add another witness to an existing Item.

Each Source update is transactional. Readers see either the previous committed projection or the new one, never half of a Session contribution. WAL mode lets read commands use a consistent snapshot while `index sync` writes.

## Query contract

Only explicit `index sync` opens the trace index for writes. `session metadata set/delete` write a separate persistent caller-owned store. Schema discovery reads declarations without opening a database. Queries, status, and evidence inspection open the index read-only. Query setup attaches the selected metadata store read-only and installs a connection-local view; user SQL cannot attach arbitrary files or mutate either store. SQL execution retains its time and output budgets.

The trace SQL surface is `sources`, `records`, `sessions`, `loops`, `items`, `blobs`, and `item_search`. `session_metadata` separately exposes caller-owned data, left-joined by stable Runtime identity; it carries no Runtime Record evidence. The first five publish domain objects. `items.semantic` encodes the domain Semantic as `{role, value, evidence_strength}`; only its top-level TextContent changes representation, becoming a BlobRef resolved through `blobs`. This SQL encoding is an interface choice, not a second domain model. `blobs` and `item_search` provide demand-driven text access and candidate retrieval without becoming domain entities. Private tables may still be readable because the local caller owns the database, but they carry storage mechanics rather than public meaning.

`item_search` is maintained from selected Semantic text. It accelerates candidate recall and returns to `items` for meaning and provenance, then to `blobs` for exact text. Non-text queries never need the Blob access path.

## Rebuildable evidence derivative

The database is not an archive. Raw bytes remain in Sources, and Record locators plus fingerprints provide the verification path. Storage Format changes rebuild the derivative from those Sources instead of migrating private tables indefinitely.

The design keeps complexity proportional to observed needs. A new public entity, View, command, or stored relation must solve a real problem that the five objects and bounded SQL cannot express simply. Convenience and hypothetical future cases do not expand the contract.

## Persistent caller data

Session metadata has one complete JSON value per `(runtime, native_id)`. The tool does not assign key meanings or automatically merge documents. Metadata is kept in a separate SQLite store so index replacement and Storage Format changes cannot destroy user data. Whole-document compare-and-set transactions prevent lost updates when callers opt in; deletion retains its revision rather than resetting it. SQL reads the selected store directly without a synchronization cache. Unmatched metadata remains visible with a null index-local Session key.

The metadata module owns its independent format, replacement transactions and query attachment. Its data is persistent and cannot be regenerated from traces. Metadata corruption or unsupported formats fail explicitly; recovery must preserve this store. [Session Metadata](../how-to/session-metadata.md) documents backups and the absence of document history.
