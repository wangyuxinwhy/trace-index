# Changelog

All notable public changes to Trace Index are documented here.

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

[0.1.0]: https://github.com/wangyuxinwhy/trace-index/releases/tag/v0.1.0
