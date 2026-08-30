//! Storage format 1, shaped around the five public domain objects.
//!
//! The storage schema follows the five public objects. Private tables only
//! represent nested or sparse attributes of those objects.

pub(crate) const STORAGE_FORMAT: u32 = 1;

pub(crate) const CREATE_SCHEMA: &str = r"
PRAGMA foreign_keys = ON;

-- Physical evidence ---------------------------------------------------------
-- Domain fields are `id`, `path`, and `runtime`. The remaining columns are
-- operational checkpoint and diagnostic state used by `index sync/status`.
CREATE TABLE trace_sources (
    id                  INTEGER PRIMARY KEY,
    adapter             TEXT    NOT NULL,
    path                TEXT    NOT NULL UNIQUE,
    file_size           INTEGER NOT NULL DEFAULT 0,
    modified_ns         INTEGER NOT NULL DEFAULT 0,
    indexed_bytes       INTEGER NOT NULL DEFAULT 0,
    indexed_lines       INTEGER NOT NULL DEFAULT 0,
    indexed_fingerprint TEXT    NOT NULL DEFAULT '',
    status              TEXT    NOT NULL DEFAULT 'new',
    last_error          TEXT,
    updated_at          TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK (adapter IN ('codex', 'claude', 'pi'))
);

CREATE TABLE trace_records (
    id              INTEGER PRIMARY KEY,
    source_id       INTEGER NOT NULL REFERENCES trace_sources(id) ON DELETE CASCADE,
    seq             INTEGER NOT NULL,
    byte_offset     INTEGER NOT NULL,
    byte_length     INTEGER NOT NULL,
    raw_hash        TEXT    NOT NULL,
    ts_ms           INTEGER,
    parse_status    TEXT    NOT NULL,
    parse_error     TEXT,
    native_type     TEXT,
    oversized       INTEGER NOT NULL DEFAULT 0,
    UNIQUE(source_id, seq),
    CHECK (byte_length >= 0)
);

-- Program facts -------------------------------------------------------------
CREATE TABLE domain_sessions (
    id                  INTEGER PRIMARY KEY,
    runtime             TEXT    NOT NULL,
    native_id           TEXT    NOT NULL,
    UNIQUE(runtime, native_id),
    CHECK (runtime IN ('codex', 'claude', 'pi'))
);

-- Relational storage for Session.source_ids and the Session facts contributed
-- by each Source. This lets one changed Source be replaced without erasing the
-- other Sources that support the same logical Session.
CREATE TABLE session_sources (
    session_id INTEGER NOT NULL REFERENCES domain_sessions(id) ON DELETE CASCADE,
    source_id  INTEGER NOT NULL UNIQUE REFERENCES trace_sources(id) ON DELETE CASCADE,
    identity_record_id INTEGER NOT NULL REFERENCES trace_records(id),
    created_at INTEGER,
    name TEXT,
    working_directory TEXT,
    PRIMARY KEY(session_id, source_id)
);

-- Sparse storage for the two optional Session attributes. The row kind is
-- projected back into forked_from or delegated_from; it is not public data.
CREATE TABLE session_parents (
    session_id        INTEGER NOT NULL REFERENCES domain_sessions(id) ON DELETE CASCADE,
    source_id         INTEGER NOT NULL REFERENCES trace_sources(id) ON DELETE CASCADE,
    kind              TEXT    NOT NULL,
    target_native_id  TEXT,
    target_locator    TEXT,
    record_id         INTEGER NOT NULL REFERENCES trace_records(id),
    PRIMARY KEY(session_id, kind, source_id),
    CHECK (kind IN ('forked_from', 'delegated_from')),
    CHECK ((target_native_id IS NOT NULL) <> (target_locator IS NOT NULL))
);

CREATE TABLE domain_loops (
    id              INTEGER PRIMARY KEY,
    session_id      INTEGER NOT NULL REFERENCES domain_sessions(id) ON DELETE CASCADE,
    source_id       INTEGER NOT NULL REFERENCES trace_sources(id) ON DELETE CASCADE,
    ordinal         INTEGER NOT NULL,
    session_position INTEGER NOT NULL,
    native_id       TEXT,
    start_record_id INTEGER NOT NULL REFERENCES trace_records(id),
    end_record_id   INTEGER REFERENCES trace_records(id),
    outcome         TEXT,
    model           TEXT,
    usage           TEXT,
    UNIQUE(source_id, ordinal),
    CHECK (session_position >= 0),
    CHECK (outcome IS NULL OR outcome IN ('completed', 'interrupted', 'failed')),
    CHECK (outcome IS NULL OR end_record_id IS NOT NULL),
    CHECK (model IS NULL OR json_valid(model)),
    CHECK (usage IS NULL OR json_valid(usage))
);

CREATE UNIQUE INDEX loops_session_position
    ON domain_loops(session_id, session_position);

-- Deduplicated bounded text referenced from Item Semantic JSON. Hashes are
-- computed from the complete Source text; `published_bytes` distinguishes
-- prefixes produced under different observation bounds. This uniqueness is a
-- correctness constraint, so it remains present during cold builds and after
-- an interrupted process.
CREATE TABLE content_blobs (
    id               INTEGER PRIMARY KEY,
    hash             BLOB    NOT NULL,
    published_bytes  INTEGER NOT NULL,
    text             TEXT    NOT NULL,
    full_bytes       INTEGER NOT NULL,
    estimated_tokens INTEGER NOT NULL
);

CREATE UNIQUE INDEX content_blobs_hash
    ON content_blobs(hash, published_bytes);

CREATE TABLE domain_items (
    id            INTEGER PRIMARY KEY,
    session_id    INTEGER NOT NULL REFERENCES domain_sessions(id) ON DELETE CASCADE,
    source_id     INTEGER NOT NULL REFERENCES trace_sources(id) ON DELETE CASCADE,
    loop_id       INTEGER REFERENCES domain_loops(id) ON DELETE CASCADE,
    loop_position INTEGER,
    occurred_at   INTEGER,
    semantic      TEXT    NOT NULL,
    CHECK (loop_position IS NULL OR loop_id IS NOT NULL),
    CHECK (json_valid(semantic))
);

CREATE UNIQUE INDEX items_loop_position
    ON domain_items(loop_id, loop_position)
    WHERE loop_id IS NOT NULL AND loop_position IS NOT NULL;

-- Relational storage for Item.record_ids. Array order has no semantics.
CREATE TABLE item_records (
    item_id   INTEGER NOT NULL REFERENCES domain_items(id) ON DELETE CASCADE,
    record_id INTEGER NOT NULL REFERENCES trace_records(id),
    PRIMARY KEY(item_id, record_id)
);

-- Sparse private evidence used to resolve the optional Session id inside a
-- delegation, subagent activity, or subagent report. It is not a public
-- relation: the domain relationship remains an attribute of Item Semantic.
-- The Runtime-native target is retained because rebuilding the target Source
-- may replace its database-local numeric Session id.
CREATE TABLE item_session_links (
    item_id          INTEGER PRIMARY KEY REFERENCES domain_items(id) ON DELETE CASCADE,
    target_runtime   TEXT NOT NULL,
    target_native_id TEXT NOT NULL,
    json_member      TEXT NOT NULL,
    CHECK (target_runtime IN ('codex', 'claude', 'pi')),
    CHECK (json_member IN ('child_session_id', 'subagent_session_id', 'source_session_id'))
);

-- Resolution triggers use this index while a cold build has deferred the
-- public query indexes. Without it, every newly discovered Session would scan
-- all previously written Item links.
CREATE INDEX item_session_links_target
    ON item_session_links(target_runtime, target_native_id);

CREATE TRIGGER resolve_item_session_links_after_session_insert
AFTER INSERT ON domain_sessions
BEGIN
    UPDATE domain_items
       SET semantic = json_set(
           semantic,
           '$.value.' || (
               SELECT json_member FROM item_session_links WHERE item_id = domain_items.id
           ),
           NEW.id
       )
     WHERE id IN (
         SELECT item_id FROM item_session_links
          WHERE target_runtime = NEW.runtime AND target_native_id = NEW.native_id
     );
END;

CREATE TRIGGER clear_item_session_links_after_session_delete
AFTER DELETE ON domain_sessions
BEGIN
    UPDATE domain_items
       SET semantic = json_remove(
           semantic,
           '$.value.' || (
               SELECT json_member FROM item_session_links WHERE item_id = domain_items.id
           )
       )
     WHERE id IN (
         SELECT item_id FROM item_session_links
          WHERE target_runtime = OLD.runtime AND target_native_id = OLD.native_id
     );
END;

-- Access structure rebuilt from Item semantic text. rowid is items.id.
CREATE VIRTUAL TABLE item_search USING fts5(
    text,
    content = '',
    tokenize = 'trigram',
    detail = 'none',
    contentless_delete = 1
);
";

pub(crate) const CREATE_SECONDARY_INDEXES: &str = r"
CREATE INDEX IF NOT EXISTS session_sources_identity_record ON session_sources(identity_record_id);
CREATE INDEX IF NOT EXISTS session_sources_created_at ON session_sources(created_at DESC);
CREATE INDEX IF NOT EXISTS session_sources_working_directory ON session_sources(working_directory);
CREATE INDEX IF NOT EXISTS session_parents_native_target ON session_parents(target_native_id);
CREATE INDEX IF NOT EXISTS session_parents_locator_target ON session_parents(target_locator);
CREATE INDEX IF NOT EXISTS session_parents_record ON session_parents(record_id);
CREATE INDEX IF NOT EXISTS loops_start_record ON domain_loops(start_record_id);
CREATE INDEX IF NOT EXISTS loops_end_record ON domain_loops(end_record_id);
CREATE INDEX IF NOT EXISTS items_session ON domain_items(session_id, occurred_at, id);
CREATE INDEX IF NOT EXISTS items_source ON domain_items(source_id, id);
CREATE INDEX IF NOT EXISTS items_loop ON domain_items(loop_id, loop_position, id);
CREATE INDEX IF NOT EXISTS items_role ON domain_items(json_extract(semantic, '$.role'), id);
CREATE INDEX IF NOT EXISTS item_records_record ON item_records(record_id);
";

// Unlike the query indexes above, this index enforces a storage invariant used
// by the Semantic Blob upsert. It is part of CREATE_SCHEMA for new databases
// and is repeated here so a write-mode open can repair databases left by an
// interrupted older cold build that deferred the constraint.
pub(crate) const ENSURE_REQUIRED_INDEXES: &str = r"
CREATE UNIQUE INDEX IF NOT EXISTS content_blobs_hash
    ON content_blobs(hash, published_bytes);
";

pub(crate) const DROP_SECONDARY_INDEXES: &str = r"
DROP INDEX IF EXISTS session_sources_identity_record;
DROP INDEX IF EXISTS session_sources_created_at;
DROP INDEX IF EXISTS session_sources_working_directory;
DROP INDEX IF EXISTS session_parents_native_target;
DROP INDEX IF EXISTS session_parents_locator_target;
DROP INDEX IF EXISTS session_parents_record;
DROP INDEX IF EXISTS loops_start_record;
DROP INDEX IF EXISTS loops_end_record;
DROP INDEX IF EXISTS items_session;
DROP INDEX IF EXISTS items_source;
DROP INDEX IF EXISTS items_loop;
DROP INDEX IF EXISTS items_role;
DROP INDEX IF EXISTS item_records_record;
";

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use rusqlite::Connection;

    use super::{CREATE_SCHEMA, CREATE_SECONDARY_INDEXES, STORAGE_FORMAT};

    #[test]
    fn creates_the_minimal_v1_schema() {
        assert_eq!(STORAGE_FORMAT, 1);
        let connection = Connection::open_in_memory().expect("open database");
        connection
            .execute_batch(CREATE_SCHEMA)
            .expect("create schema");
        connection
            .execute_batch(CREATE_SECONDARY_INDEXES)
            .expect("create indexes");

        let tables = connection
            .prepare("SELECT name FROM sqlite_schema WHERE type IN ('table', 'view')")
            .expect("prepare schema query")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query schema")
            .collect::<Result<HashSet<_>, _>>()
            .expect("collect schema");

        for expected in [
            "trace_sources",
            "trace_records",
            "domain_sessions",
            "session_sources",
            "session_parents",
            "domain_loops",
            "content_blobs",
            "domain_items",
            "item_records",
            "item_session_links",
            "item_search",
        ] {
            assert!(tables.contains(expected), "missing {expected}");
        }
        for removed in [
            "turns",
            "message_details",
            "tool_call_details",
            "session_stats",
        ] {
            assert!(!tables.contains(removed), "legacy table {removed} survived");
        }

        let item_columns = connection
            .prepare("SELECT name FROM pragma_table_info('domain_items') ORDER BY cid")
            .expect("prepare Item column query")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query Item columns")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect Item columns");
        assert_eq!(
            item_columns,
            [
                "id",
                "session_id",
                "source_id",
                "loop_id",
                "loop_position",
                "occurred_at",
                "semantic",
            ]
        );

        let loop_columns = connection
            .prepare("SELECT name FROM pragma_table_info('domain_loops') ORDER BY cid")
            .expect("prepare Loop column query")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query Loop columns")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect Loop columns");
        assert_eq!(
            loop_columns,
            [
                "id",
                "session_id",
                "source_id",
                "ordinal",
                "session_position",
                "native_id",
                "start_record_id",
                "end_record_id",
                "outcome",
                "model",
                "usage",
            ]
        );

        let role_index: String = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'index' AND name = 'items_role'",
                [],
                |row| row.get(0),
            )
            .expect("read Item role index");
        assert!(role_index.contains("json_extract(semantic, '$.role')"));
    }

    #[test]
    fn item_session_links_follow_rebuilt_session_ids() {
        let connection = Connection::open_in_memory().expect("open database");
        connection
            .execute_batch(CREATE_SCHEMA)
            .expect("create schema");
        connection
            .execute_batch(
                r#"
                INSERT INTO trace_sources(id, adapter, path)
                VALUES (1, 'codex', '/tmp/parent.jsonl');
                INSERT INTO domain_sessions(id, runtime, native_id)
                VALUES (10, 'codex', 'parent');
                INSERT INTO domain_items(id, session_id, source_id, semantic)
                VALUES (
                    20,
                    10,
                    1,
                    '{"role":"agent.delegation","value":{"has_images":false},"evidence_strength":"structural"}'
                );
                INSERT INTO item_session_links(
                    item_id, target_runtime, target_native_id, json_member)
                VALUES (20, 'codex', 'child', 'child_session_id');
                "#,
            )
            .expect("store unresolved Item Session link");

        assert_eq!(item_child_session_id(&connection), None);

        connection
            .execute(
                "INSERT INTO domain_sessions(id, runtime, native_id)
                 VALUES (30, 'codex', 'child')",
                [],
            )
            .expect("store target Session");
        assert_eq!(item_child_session_id(&connection), Some(30));

        connection
            .execute("DELETE FROM domain_sessions WHERE id = 30", [])
            .expect("remove target Session during rebuild");
        assert_eq!(item_child_session_id(&connection), None);

        connection
            .execute(
                "INSERT INTO domain_sessions(id, runtime, native_id)
                 VALUES (31, 'codex', 'child')",
                [],
            )
            .expect("store rebuilt target Session");
        assert_eq!(item_child_session_id(&connection), Some(31));
    }

    fn item_child_session_id(connection: &Connection) -> Option<i64> {
        connection
            .query_row(
                "SELECT json_extract(semantic, '$.value.child_session_id')
                   FROM domain_items WHERE id = 20",
                [],
                |row| row.get(0),
            )
            .expect("read Item Session link")
    }
}
