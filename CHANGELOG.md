# Changelog

All notable public changes to Trace Index are documented here.

## [Unreleased]

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

[Unreleased]: https://github.com/wangyuxinwhy/trace-index/compare/v0.2.2...HEAD
[0.2.2]: https://github.com/wangyuxinwhy/trace-index/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/wangyuxinwhy/trace-index/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/wangyuxinwhy/trace-index/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/wangyuxinwhy/trace-index/releases/tag/v0.1.0
