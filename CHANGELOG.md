# Changelog

All notable public changes to Trace Index are documented here.

## [0.6.0] - 2026-11-26

### Added

- Add multi-relation `trace-index schema get <OBJECT>...` (preserving requested order and deduplicating repeated names) and `--format json|tsv` tabular output on `query run` and `query template run` (`PR #36`). TSV output writes a header row and backslash-escaped rows (`\\`, `\t`, `\n`, `\r`) to stdout while keeping stderr empty unless a row limit, output budget, or cell-truncation bound is hit (emitting an actionable `notice:` with exit status 0).
- Add native SQLite parameter binding (`--params-json '<JSON>'` and repeatable `-p/--param key=value`) to `query run`, plus a transparent named SQL template engine (`trace-index query template list|show|run`, `PR #37`):
  - Ships 6 built-in read-only `.sql` templates (`recent-sessions`, `search-sessions`, `search-items`, `session-transcript`, `item-context`, `shell-calls`) with typed parameters (`string`, `int`, `bool`, `enum`, `list`, `time`, `session_ref`, `item_ref`), line-granular `-- #if` conditionals, zero text interpolation (100% SQLite `:param` binding), and `--render` escape hatch to inspect and customize SQL.
  - Supports user-defined `.sql` templates under `<config_dir>/templates/*.sql` (never searching CWD, never shadowing built-in names) validated read-only by `config check`.
- Emit a `database_format_upgrade_required` warning from `trace-index config check` when an existing database uses an older Storage Format (`PR #38`).

### Changed

- **Storage Format 5** (`PR #38`): promote term-based full-text search into the single canonical `item_search` FTS5 access relation (`unicode61 tokenchars '_'`, `detail='full'`, `contentless_delete=1`), removing the redundant trigram table and `item_search_terms` split and simplifying the public SQL surface from 9 to **8 relations**.
  - Tokenizer now preserves `_`-delimited identifier segments (`pkg_CoreEngine_Worker`, `slot_7t_context`) and supports trailing `prefix*` queries on ≥4-character ASCII tokens without prefix-index disk bloat.
  - Cuts FTS5 disk footprint by **-58.5% (2.41× smaller)**, `persist_ms.item_search` by **1.93×**, and cold-rebuild wall time by **1.43×** on real multi-runtime corpora.
  - Adds prepare-time SQLite authorizer guards so Format ≥ 5 queries reading `item_search` without `search_query(...)` or calling removed `trigram_query(...)` fail with actionable hints instead of silently returning zero rows.
  - Preserves read-only compatibility with Format 2, Format 3, and Format 4 databases (built-in search templates automatically route Format 4 databases to legacy `item_search_terms`). Reindexing into a new Format 5 database is required for future `index sync` / `index watch` writes.
- Accelerate no-op and incremental `index sync` / `index watch` reconciliation (`PR #35`) via single-query bulk `source_state` preloading, `prepare_cached` statement reuse, and `transcript_full.jsonl`-first Antigravity discovery (**9.08×** faster `prepare` / **2.51×** faster total wall time under macOS Background QoS on a 4.52 GB / 4,630-file corpus).
- Align root/leaf Help, all bundled documentation, `.agents/skills/trace-index/SKILL.md`, and `README.md` with Storage Format 5, built-in/user SQL templates, parameterized `query run`, and `--format tsv` (`PR #39`).

## [0.5.6] - 2026-11-24

### Changed

- Add a true `O(tail)` append-only fast path for Antigravity transcripts (`PR #32`):
  - Verify `[0..indexed_bytes]` cryptographically in a single 256 KiB chunked pass while seeding the new whole-prefix `BLAKE3` hasher, then read and project only newly appended tail Records into a compact tail delta without re-parsing historical prefix lines or loading existing `trace_records`, `domain_loops`, `domain_items`, or `item_records` rows.
  - Produces bit-identical database state to a full `--rebuild` and falls back cleanly to a transactional rebuild on any prefix modification, truncation, or concurrent state change; accelerates hot 34.3 MB Antigravity single-step appends by **4.1x** wall / **5.5x** sync / **16.4x** persist under Normal QoS and **7.3x–37.7x** wall / **23x–35.5x** persist under macOS Background QoS (`index watch`).
- Skip redundant `index watch` notifications for Antigravity compact `transcript.jsonl` files when a sibling `transcript_full.jsonl` already exists on disk (`PR #33`), avoiding wasted database opens and shadow rejections during active conversations. Storage Format 4 is unchanged; existing Format 4 indexes require no reindexing.

## [0.5.5] - 2026-11-23

### Added

- Add a reproducible multi-runtime performance benchmark harness (`scripts/bench.py`) supporting deterministic synthetic corpus generation (Codex, Claude Code, Pi, Antigravity), read-only live database query/status benchmarking, isolated CoW/copy append-only benchmarking, and A/B comparison tables.

### Changed

- Accelerate append-only incremental `index sync` and `index watch` updates on large active Sources with zero Storage Format change and bit-for-bit identical output:
  - Move append-only prefix verification, file scanning, JSON parsing, and Session projection outside `TransactionBehavior::Immediate`, cutting `SQLite` write-lock hold time by 7.5x–11.8x on large rollouts while verifying the existing prefix digest in one sequential read pass.
  - Defer `tree-sitter-bash` shell command parsing until an Item is written or diverges from its persisted prefix row, and verify unchanged prefix Items in O(1) without repeated `content_blobs` B-tree lookups or JSON re-serialization.
  - Skip full `serde_json::Value` DOM allocation during verified prefix scans for provably ignored Antigravity (`GENERIC`/`CHECKPOINT`) and Codex (`world_state`, summary-less `compacted`) records.
- Optimize `index status` `observed_roles` aggregation over `items_role_occurred` using a dense `source_id -> adapter` lookup table, adapter bitmask, and active-role run caching (~1.23x faster end-to-end on a 4.2 GiB / 2.34M-item index with identical output). Storage Format 4 is unchanged; existing Format 4 indexes require no reindexing.

## [0.5.4] - 2026-11-22

### Added

- Add `--list-sections` and `--section <HEADING>` to `trace-index docs get` (both Markdown and JSON outputs) so Agents and humans can inspect topic outlines and read specific sections without triggering output truncation on long reference pages.
- Include the standard Antigravity CLI trace root (`~/.gemini/antigravity-cli/brain`, label `antigravity`) in `trace-index config init --discover` alongside Codex, Pi, and Claude Code.

### Changed

- Maintain `SQLite` query-planner statistics (`sqlite_stat1`) via bounded `PRAGMA analysis_limit = 400; PRAGMA optimize(0x10002);` after non-empty synchronizations (cold builds, updated syncs, and one-time backfill for existing Format 4 databases without `sqlite_stat1`), accelerating reverse `INNER JOIN` queries on `items` by orders of magnitude with sub-millisecond steady-state overhead. Storage Format 4 is unchanged; existing Format 4 indexes gain statistics automatically on their next sync without reindexing.
- Surface direct discovery hints for `docs get reference/public-schema --section 'Semantic SQL encoding'`, `--list-sections`, exact index-accelerated `items` JSON paths, and `item_search_terms(search_query('...'))` `OR` combination in root Help, `schema get items`, bundled docs, and the `trace-index` Skill.
- Retry transient `launchctl bootstrap` exit-5 (`EIO`) races during macOS `index watch install` hot reloads after `bootout`.

## [0.5.3] - 2026-11-22

### Added

- Add opt-in event-driven incremental watching (`trace-index index watch run|status|install|uninstall`). Watches configured `[[roots]]` (or explicit `PATHS`) via OS filesystem notifications (`FSEvents` on macOS, `inotify` on Linux) with per-file debouncing (`--debounce-ms`, `--max-wait-ms`) and a low-frequency incremental safety-net reconcile (`--reconcile-secs`, `0` to disable), while opening `SQLite` only during active batches and enforcing single-watcher exclusivity via stdlib advisory locks.
- Add opt-in user-level OS background service management (`index watch install` / `uninstall`, supporting macOS `launchd` LaunchAgents and Linux `systemd --user` with `--dry-run` preview). Installing `trace-index` itself never registers background processes.
- Expose live per-database `watch` state (`active`, `service_installed`, and runtime metadata) in read-only `index status` and `index watch status`, and update Agent-facing Help, bundled documentation, and Skill guidance. Storage Format 4 is unchanged; existing Format 4 indexes need no reindexing.

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
