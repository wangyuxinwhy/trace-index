# Changelog

All notable public changes to Trace Index are documented here.

## [0.5.2] - 2026-11-22

### Changed

- Accelerate incremental `index sync` on active append-only Sources with prefix-preserving domain persistence (`sync_session`). Unchanged Session Items, completed Loops, and unchanged prefix Items of the active tail Loop are preserved in place—reusing existing `loop_id`s, `item_id`s, FTS5 entries, and covering `content_blobs(hash)` index lookups—while rewriting only from the first diverged Item or Loop onward so relative `item_id` ordering and all SQL/FTS5 results remain identical to a full rebuild. Storage Format 4 is unchanged; existing Format 4 indexes need no reindexing.
- Modularize storage semantic encoding, incremental persistence, the parallel Source preparation pipeline, and the Codex/Claude runtime adapters into focused submodules with zero behavioral change.

## [0.5.1] - 2026-11-22

### Changed

- Accelerate Source discovery by short-circuiting Antigravity conversation directories directly to `.system_generated/logs`, using `DirEntry::file_type()`, and reusing canonical paths and `stat` metadata across discovery and synchronization.
- Decode `.jsonl.zst` archives into a bounded (`32 MiB`) reusable in-memory buffer during indexing (spilling larger archives transparently to an anonymous temporary file) to eliminate temporary-file disk write/read overhead for most compressed rollouts.
- Pipeline new-Source preparation (decoding, line framing, BLAKE3 hashing, JSON parsing, projection, and shell parsing) across a bounded pool of up to 4 worker threads while preserving exact deterministic discovery order and single-writer per-Source `SQLite` transactions. Storage Format 4 is unchanged; existing Format 4 indexes need no reindexing.
- Document the SQLite unary `+call.item_id` expression-index seek idiom for reverse-direction `call LEFT JOIN output` queries in `how-to/query-evidence`.

## [0.5.0] - 2026-11-22

### Added

- Storage Format 4 adds the `item_search_terms` FTS5 access relation and `search_query()` SQL helper alongside `item_search`. Pre-tokenizing Semantic text into CJK bigrams, lowercased words, and split code identifiers (`camelCase`, `PascalCase`, `snake_case`, preserving `_`) enables 2+ character CJK recall, exact-adjacency 3+ character CJK phrase queries, multi-word mixed queries, and native `bm25(item_search_terms)` ranking while keeping `item_search(trigram_query('...'))` for 3+ character literal substrings and symbols.
- Compress deduplicated Semantic Blobs (`>= 128` bytes when smaller) with per-blob `zstd` in Storage Format 4 and decode them transparently through the public `blobs` view, and store `item_records` `WITHOUT ROWID`.
- Unwrap common POSIX command wrappers (`nice`, `timeout`, `env`, `nohup`, `command`) into inner `ShellStatement.invocations` and extract bounded static nested `sh`/`bash`/`zsh -c` scripts into additional `ShellFragment` entries while preserving outer invocations.

### Changed

- Format 2 and Format 3 indexes remain readable in place without reindexing, with Format-aware query error hints. Create a separate Format 4 database with `--db` and synchronize your available Sources for new writes. Keep existing Format 2 or Format 3 databases that contain history whose original Sources are no longer available.

## [0.4.3] - 2026-11-21

### Added

- Add Format-3-compatible partial indexes on `tool.output` `call_item_id` links and tool-call `tool_name` + `occurred_at` to accelerate tool-call-to-output joins and tool-name queries. Storage Format 3 is unchanged; existing Format 3 indexes gain both indexes automatically on the next synchronization.
- Add contract-grounded error recovery hints for all `fts5:` errors on `item_search`, missing SQLite tables or columns (`schema list` / `schema get <RELATION> --compact`), and unknown CLI flags or subcommands (while preserving empty stdout and exit codes).

## [0.4.2] - 2026-11-21

### Added

- Publish a statically linked `x86_64-unknown-linux-musl` release archive and select it by default on x86-64 Linux in `install.sh` so prebuilt binaries run across older glibc distributions (such as Debian 10 / GLIBC 2.28) as well as musl environments. Add `--target` to `install.sh`; `x86_64-unknown-linux-gnu` remains published.
- Add covering indexes on Items by `(semantic_role, occurred_at)` and `(occurred_at)` to accelerate role- and time-ordered queries. Storage Format 3 is unchanged; existing Format 3 indexes gain the new indexes automatically on the next synchronization.
- Return a targeted hint pointing to `item_search(trigram_query('...'))` when an FTS5 phrase query is rejected by the `detail=none` trigram index.

### Changed

- Accelerate `index status` with a single read-only WAL snapshot and indexed role aggregation, and reduce read-only open and schema-validation overhead.
- Speed up indexing persistence and Blob interning by serializing `Semantic<BlobRef>` directly without intermediate JSON trees, linking `tool.output` items inline, skipping no-op Loop position rewrites, and reducing redundant Blob lookups on cold builds and appends.
- Lower indexing peak memory and I/O overhead by removing duplicate text previews and unused fields in Runtime adapters, reusing one shell parser and record buffer across synchronization, skipping pre-resume Record hashing on incremental appends, buffering `.zst` decompression at 256 KiB, and keeping temporary SQLite B-trees in memory on write connections.
- Unify and tighten RFC 3339 timestamp validation across all Runtime adapters.

## [0.4.1] - 2026-11-21

### Added

- Index Codex `.jsonl.zst` rollouts directly and verify or export Records from the decoded stream. Sibling plain and compressed rollouts share one Source across an encoding switch.

### Changed

- Preserve existing projections when a Source's indexed bytes are unchanged after a metadata or encoding change.
- Add an index for reading the first or last Items of a Session in Item ID order. Storage Format 3 is unchanged; existing Format 3 indexes gain the new index automatically on the next synchronization.

## [0.4.0] - 2026-11-20

### Added

- Explicit local Antigravity CLI JSONL input, with conversation and Shell call queries and byte-level Record evidence.
- Storage Format 3 adds the `antigravity` Runtime. Format 2 indexes remain readable. Create a separate index and synchronize your available Sources for new writes. Keep Format 2 indexes that contain history no longer present in supported Source files.

## [0.3.4] - 2026-11-16

### Fixed

- Claude Code synthetic tool observations no longer close the surrounding Loop. Code Mode's internal tool calls and results remain with the request and final answer, preserving combined model usage.
- Empty virtual companion tool results preserve their structured stdout and stderr when one result block owns that output. Explicit message content keeps precedence.

Existing indexes require `trace-index index sync --rebuild` for affected Claude Code Sources. Storage Format 2 is unchanged.

## [0.3.3] - 2026-11-07

### Fixed

- Codex native command outputs now publish `runtime_truncated: true` when the Runtime's durable trace contains its fixed command-output persistence truncation marker. This preserves the retained head and tail while making the omitted middle explicit.

## [0.3.2] - 2026-11-04

### Fixed

- Claude Code fork traces use native `turnPosition` to preserve distinct inherited turns when the Runtime rewrites their `promptId` values to one shared id. Traces without `turnPosition`, queued steering, delayed tool output, and Stop-hook continuation retain their existing routing behavior.

## [0.3.1] - 2026-10-18

### Fixed

- Claude Code Loop usage counts each native response once, replacing repeated content-block snapshots with the last complete observation and routing late replays to their original Loop. Synthetic model messages do not contribute usage.
- Pi usage replaces repeated assistant entry snapshots without merging distinct calls with equal counters. Legacy Codex usage ignores repeated unchanged Session-cumulative observations; turn-scoped counters retain precedence.

### Changed

- Document Runtime usage identities, partial-observation limits, Session accounting scope, and the required `index sync --rebuild` after this accounting correction. Storage Format 2 and caller-owned metadata storage are unchanged.

## [0.3.0] - 2026-10-13

### Added

- Independent persistent Session metadata with whole-document JSON set/get/delete, optional atomic version checks, and rebuild-safe Runtime-native associations.
- The read-only `session_metadata` SQL relation, including unmatched targets, with immediate visibility after committed writes and no index synchronization.
- Metadata storage configuration with independent CLI, environment, file, and default selections; metadata input and key meanings remain caller-defined.

### Changed

- Schema discovery reads declarations without requiring an index. The five trace domain objects and Storage Format 2 are unchanged.

## [0.2.2] - 2026-10-13

### Fixed

- Pi extension-triggered runs no longer extend a completed Loop. Idle custom messages remain Session context until subsequent execution establishes a new Loop, and notifications alone do not create a Loop.
- Query timeout diagnostics retain the configured duration, including fractional seconds; execution budgets are unchanged.
- Pi system prompt and tool-loadout declarations no longer open a Loop or misclassify the first Human request as steering. Prompt patches, compaction checkpoints, and context edits are published without rewriting historical messages; retry recovery stays in its outer Loop. Tool duration and per-response model metadata are preserved.
- Codex turn-scoped `token_usage_record` counters update unfinished and completed Loops without adding legacy usage twice. Native `CommandExecution` completion events expose commands executed through JavaScript orchestration, with correlated outputs and source witnesses; event order and repeated completions preserve arguments, working directories, and output boundaries.
- Claude Code model-context and progress-reminder attachments use their known Runtime roles instead of `runtime.unknown`.
- Claude Code fork-context reference headers identify child traces and preserve parent links and context-reference evidence without copying inherited parent messages or opening a Loop.

### Changed

- The query how-to now states that every stored time is a UTC instant, shows how to write a calendar-date boundary with an explicit offset, and explains how to format results with the reader's fixed offset instead of the host-dependent SQLite `'localtime'` modifier. Storage, relations, ordering, and JSON output are unchanged.

## [0.2.1] - 2026-08-31

### Changed

- Root Help now teaches general investigation defaults for orienting with the conversation spine, narrowing candidates, matching evidence depth to claims, reconstructing chronology, preserving missing coverage, checking completeness boundaries, and stopping when the requested claim is supported.

## [0.2.0] - 2026-08-30

### Changed

- Configuration now reports value provenance, resolves effective paths absolutely, honors `XDG_DATA_HOME`, and provides a read-only `config check` preflight.
- `config show` now returns structured `config_file`, `database`, and `indexing` selections with origin metadata; roots use optional `label` instead of `name`. `config init` now nests that effective configuration under `config` rather than returning a top-level `config_file`.
- `config init` now writes a versioned validated configuration, honors `--db`, accepts repeatable `--root`, and can explicitly `--discover` existing standard Runtime roots without creating the database or synchronizing traces.
- Indexing byte limits are a persisted database-wide policy. A non-empty index owns and reuses that policy, rejecting explicit CLI requests that would otherwise mix incompatible projections.
- Storage Format 2 adds indexing-policy metadata. Storage Format 1 indexes must be rebuilt from their Source JSONL files.

## [0.1.0] - 2026-08-30

Initial public release.

### Added

- Bounded-memory, append-aware indexing for Codex, Pi, and Claude Code JSONL traces.
- Five source-traceable domain objects: Source, Record, Session, Loop, and Item.
- Typed Item Semantic roles and values across supported Agent runtimes.
- A bounded, read-only public SQL query plane with discoverable schemas.
- Trigram candidate retrieval for literal Semantic text search.
- Source-backed Record verification, inspection, export, and inline Asset extraction.
- Compact machine-readable output, explicit incomplete-result hints, bundled Agent documentation, and an Agent skill.
- Storage Format 1. Databases created by non-public development builds must be rebuilt.

[Unreleased]: https://github.com/wangyuxinwhy/trace-index/compare/v0.3.3...HEAD
[0.3.3]: https://github.com/wangyuxinwhy/trace-index/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/wangyuxinwhy/trace-index/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/wangyuxinwhy/trace-index/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/wangyuxinwhy/trace-index/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/wangyuxinwhy/trace-index/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/wangyuxinwhy/trace-index/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/wangyuxinwhy/trace-index/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/wangyuxinwhy/trace-index/releases/tag/v0.1.0
