//! Agent-facing SQL contract for the five domain objects plus text search.

use std::fmt::Write as _;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension as _};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PublicRelationKind {
    View { from_sql: &'static str },
    VirtualTable,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PublicRelation {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) kind: PublicRelationKind,
    pub(crate) columns: &'static [PublicColumn],
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PublicColumn {
    pub(crate) name: &'static str,
    pub(crate) expression: Option<&'static str>,
    pub(crate) data_type: &'static str,
    pub(crate) nullable: bool,
    pub(crate) description: &'static str,
}

trait SqlColumnType {
    const DATA_TYPE: &'static str;
    const NULLABLE: bool = false;
}

impl SqlColumnType for String {
    const DATA_TYPE: &'static str = "TEXT";
}
impl SqlColumnType for i64 {
    const DATA_TYPE: &'static str = "INTEGER";
}
impl<T: SqlColumnType> SqlColumnType for Option<T> {
    const DATA_TYPE: &'static str = T::DATA_TYPE;
    const NULLABLE: bool = true;
}

macro_rules! public_view {
    (
        $(#[doc = $relation_doc:literal])+
        pub(crate) const $constant:ident = $name:literal {
            $($(#[doc = $column_doc:literal])+
                $column:ident: $column_type:ty = $expression:literal,)+
        }
        from $from_sql:literal;
    ) => {
        pub(crate) const $constant: PublicRelation = PublicRelation {
            name: $name,
            description: concat!($($relation_doc, "\n"),+),
            kind: PublicRelationKind::View { from_sql: $from_sql },
            columns: &[$(PublicColumn {
                name: stringify!($column), expression: Some($expression),
                data_type: <$column_type as SqlColumnType>::DATA_TYPE,
                nullable: <$column_type as SqlColumnType>::NULLABLE,
                description: concat!($($column_doc, "\n"),+),
            },)+],
        };
    };
}

macro_rules! public_virtual_table {
    (
        $(#[doc = $relation_doc:literal])+
        pub(crate) const $constant:ident = $name:literal {
            $($(#[doc = $column_doc:literal])+
                $column:ident: $column_type:ty,)+
        }
    ) => {
        pub(crate) const $constant: PublicRelation = PublicRelation {
            name: $name,
            description: concat!($($relation_doc, "\n"),+),
            kind: PublicRelationKind::VirtualTable,
            columns: &[$(PublicColumn {
                name: stringify!($column), expression: None,
                data_type: <$column_type as SqlColumnType>::DATA_TYPE,
                nullable: <$column_type as SqlColumnType>::NULLABLE,
                description: concat!($($column_doc, "\n"),+),
            },)+],
        };
    };
}

public_view! {
    /// Physical Runtime Trace inputs currently published by Trace Index.
    pub(crate) const SOURCES = "sources" {
        /// Opaque Source reference in this index.
        source_id: i64 = "s.id",
        /// Canonical absolute locator of the original JSONL file.
        locator: String = "s.path",
        /// Runtime that defines the Source's native structure.
        runtime: String = "s.adapter",
    }
    from "FROM trace_sources s";
}

public_view! {
    /// Boundary-complete physical Records. Raw bytes remain in the Source and
    /// can be read with `record inspect`.
    pub(crate) const RECORDS = "records" {
        /// Opaque Record reference in this index.
        record_id: i64 = "r.id",
        /// Source containing the Record.
        source_id: i64 = "r.source_id",
        /// Zero-based physical position inside the Source.
        source_position: i64 = "r.seq",
        /// JSON half-open byte range `{start, end}` in the Source.
        content_range: String = "json_object('start', r.byte_offset, 'end', r.byte_offset + r.byte_length)",
        /// BLAKE3 fingerprint of the original content bytes.
        fingerprint: String = "r.raw_hash",
        /// Runtime-provided occurrence time in epoch milliseconds.
        occurred_at: Option<i64> = "r.ts_ms",
    }
    from "FROM trace_records r";
}

public_view! {
    /// Runtime-maintained logical Agent contexts. Arrays and nested optional
    /// attributes are JSON values belonging to the Session object.
    pub(crate) const SESSIONS = "sessions" {
        /// Trace Index-local opaque join key. This is not the Runtime-native
        /// Session identity; use `native_id` when reporting the Codex, Pi, or
        /// Claude Code Session id to a user.
        session_id: i64 = "s.id",
        /// Runtime that owns the native Session identity.
        runtime: String = "s.runtime",
        /// Runtime-native identity of the continuing context. This is the
        /// externally recognizable Session id within the named `runtime`.
        native_id: String = "s.native_id",
        /// Record that directly states the Session identity.
        identity_record_id: i64 = "(SELECT identity_record_id FROM session_sources WHERE session_id = s.id ORDER BY source_id LIMIT 1)",
        /// Runtime-provided context creation time in epoch milliseconds.
        created_at: Option<i64> = "(SELECT MIN(created_at) FROM session_sources WHERE session_id = s.id)",
        /// Explicit human or Runtime name, never a generated summary.
        name: Option<String> = "(SELECT name FROM session_sources WHERE session_id = s.id AND name IS NOT NULL ORDER BY source_id LIMIT 1)",
        /// Working directory recorded when the Session was established.
        working_directory: Option<String> = "(SELECT working_directory FROM session_sources WHERE session_id = s.id AND working_directory IS NOT NULL ORDER BY source_id LIMIT 1)",
        /// JSON array of Source ids supporting this Session.
        source_ids: String = "COALESCE((SELECT json_group_array(source_id) FROM (SELECT source_id FROM session_sources WHERE session_id = s.id ORDER BY source_id)), json('[]'))",
        /// JSON object `{session_id, record_id}` when history was forked.
        forked_from: Option<String> = "COALESCE((SELECT json_object('session_id', parent.id, 'record_id', p.record_id) FROM session_parents p JOIN domain_sessions parent ON parent.runtime = s.runtime AND parent.native_id = p.target_native_id WHERE p.session_id = s.id AND p.kind = 'forked_from' AND parent.id <> s.id ORDER BY p.source_id LIMIT 1), (SELECT json_object('session_id', parent_source.session_id, 'record_id', p.record_id) FROM session_parents p JOIN trace_sources target_source ON target_source.path = p.target_locator JOIN session_sources parent_source ON parent_source.source_id = target_source.id WHERE p.session_id = s.id AND p.kind = 'forked_from' AND parent_source.session_id <> s.id ORDER BY p.source_id LIMIT 1))",
        /// JSON object `{session_id, record_id}` when a parent Agent delegated it.
        delegated_from: Option<String> = "(SELECT json_object('session_id', parent.id, 'record_id', p.record_id) FROM session_parents p JOIN domain_sessions parent ON parent.runtime = s.runtime AND parent.native_id = p.target_native_id WHERE p.session_id = s.id AND p.kind = 'delegated_from' AND parent.id <> s.id ORDER BY p.source_id LIMIT 1)",
    }
    from "FROM domain_sessions s";
}

public_view! {
    /// Outer Agent execution lifecycles. A missing `end` means no ending has
    /// been observed; an `end` without outcome means the boundary is known but
    /// the Runtime did not state a result.
    pub(crate) const LOOPS = "loops" {
        /// Opaque Loop reference in this index.
        loop_id: i64 = "l.id",
        /// Session containing the Loop.
        session_id: i64 = "l.session_id",
        /// Zero-based position inside the Session.
        session_position: i64 = "l.session_position",
        /// Runtime-native outer execution identity, when one exists.
        native_id: Option<String> = "l.native_id",
        /// Record that establishes the Loop.
        start_record_id: i64 = "l.start_record_id",
        /// Optional JSON object `{record_id, outcome?}` establishing the end.
        end: Option<String> = "CASE WHEN l.end_record_id IS NULL THEN NULL WHEN l.outcome IS NULL THEN json_object('record_id', l.end_record_id) ELSE json_object('record_id', l.end_record_id, 'outcome', l.outcome) END",
        /// Optional JSON model configuration `{id, effort?, context_window?}`
        /// observed for the Loop.
        model: Option<String> = "json(l.model)",
        /// Optional JSON normalized model usage
        /// `{input, cached?, cache_write?, output, reasoning?}` for the Loop.
        /// Cached and cache-write tokens are subsets of `input`; reasoning is
        /// a subset of `output`.
        usage: Option<String> = "json(l.usage)",
    }
    from "FROM domain_loops l";
}

public_view! {
    /// Individually queryable Agent program facts. `semantic` is the typed
    /// Trace Index meaning; `record_ids` identifies its physical evidence.
    pub(crate) const ITEMS = "items" {
        /// Opaque Item reference in this index.
        item_id: i64 = "i.id",
        /// Session containing the Item.
        session_id: i64 = "i.session_id",
        /// Loop containing the Item when structurally known.
        loop_id: Option<i64> = "i.loop_id",
        /// Zero-based observation position inside the Loop.
        loop_position: Option<i64> = "i.loop_position",
        /// Runtime-provided occurrence time in epoch milliseconds.
        occurred_at: Option<i64> = "i.occurred_at",
        /// Non-empty JSON array of physical Record witnesses.
        record_ids: String = "COALESCE((SELECT json_group_array(record_id) FROM (SELECT record_id FROM item_records WHERE item_id = i.id ORDER BY record_id)), json('[]'))",
        /// Stable SQL encoding `{role, value, evidence_strength}` of Trace
        /// Index's cross-Runtime Semantic. Top-level text members are Blob
        /// references `{blob_id}` resolved through `blobs`.
        semantic: String = "i.semantic",
    }
    from "FROM domain_items i";
}

public_view! {
    /// Bounded Semantic text addressable through Item BlobRef values. Blob rows
    /// are access data, not independent trace facts.
    pub(crate) const BLOBS = "blobs" {
        /// Opaque Blob reference used by Item Semantic values.
        blob_id: i64 = "b.id",
        /// Bounded text published in the index.
        text: String = "b.text",
        /// UTF-8 byte length before Trace Index applied its text bound.
        full_bytes: i64 = "b.full_bytes",
        /// Deterministic token estimate over the complete Source text.
        estimated_tokens: i64 = "b.estimated_tokens",
    }
    from "FROM content_blobs b";
}

public_virtual_table! {
    /// Trigram candidate index over explicitly documented Item semantic text.
    /// `rowid` is the matching `items.item_id`; exact-check candidate text.
    pub(crate) const ITEM_SEARCH = "item_search" {
        /// Contentless FTS MATCH operand. Reading it returns NULL; obtain exact
        /// text from the Item's BlobRef and `blobs`.
        text: Option<String>,
    }
}

pub(crate) const PUBLIC_RELATIONS: &[PublicRelation] =
    &[SOURCES, RECORDS, SESSIONS, LOOPS, ITEMS, BLOBS, ITEM_SEARCH];

impl PublicRelation {
    fn create_sql(self) -> Result<Option<String>> {
        let PublicRelationKind::View { from_sql } = self.kind else {
            return Ok(None);
        };
        let mut sql = format!("CREATE VIEW {} AS\nSELECT\n", quote_identifier(self.name));
        for (index, column) in self.columns.iter().enumerate() {
            if index > 0 {
                writeln!(sql, ",")?;
            }
            write!(
                sql,
                "    {} AS {}",
                column.expression.expect("View column expression"),
                quote_identifier(column.name)
            )?;
        }
        writeln!(sql, "\n{from_sql};")?;
        Ok(Some(sql))
    }
}

pub(crate) fn find_public_relation(name: &str) -> Option<&'static PublicRelation> {
    PUBLIC_RELATIONS
        .iter()
        .find(|relation| relation.name == name)
}

pub(crate) fn recreate_public_views(connection: &Connection) -> Result<()> {
    for relation in PUBLIC_RELATIONS.iter().rev() {
        if matches!(relation.kind, PublicRelationKind::View { .. }) {
            connection
                .execute_batch(&format!(
                    "DROP VIEW IF EXISTS {};",
                    quote_identifier(relation.name)
                ))
                .with_context(|| format!("failed to drop public view {}", relation.name))?;
        }
    }
    for relation in PUBLIC_RELATIONS {
        if let Some(sql) = relation.create_sql()? {
            connection
                .execute_batch(&sql)
                .with_context(|| format!("failed to create public view {}", relation.name))?;
        }
    }
    validate_public_relations(connection)
}

pub(crate) fn validate_public_relations(connection: &Connection) -> Result<()> {
    for relation in PUBLIC_RELATIONS {
        let expected_kind = match relation.kind {
            PublicRelationKind::View { .. } => "view",
            PublicRelationKind::VirtualTable => "table",
        };
        let actual_kind = connection
            .query_row(
                "SELECT type FROM main.sqlite_schema WHERE name = ?1",
                [relation.name],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .with_context(|| format!("failed to inspect public relation {}", relation.name))?;
        if actual_kind.as_deref() != Some(expected_kind) {
            bail!(
                "public relation {} must be a {expected_kind}, found {}",
                relation.name,
                actual_kind.as_deref().unwrap_or("nothing")
            );
        }
        let actual_columns = connection
            .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?
            .query_map([relation.name], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let expected_columns = relation
            .columns
            .iter()
            .map(|column| column.name)
            .collect::<Vec<_>>();
        if actual_columns != expected_columns {
            bail!(
                "public relation {} column mismatch: expected {:?}, found {:?}",
                relation.name,
                expected_columns,
                actual_columns
            );
        }
        connection
            .prepare(&format!(
                "SELECT * FROM {} LIMIT 0",
                quote_identifier(relation.name)
            ))
            .with_context(|| format!("public relation {} cannot be queried", relation.name))?;
    }
    Ok(())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{PUBLIC_RELATIONS, PublicRelationKind};

    #[test]
    fn exposes_five_domain_objects_blobs_and_search() {
        assert_eq!(
            PUBLIC_RELATIONS
                .iter()
                .map(|value| value.name)
                .collect::<Vec<_>>(),
            [
                "sources",
                "records",
                "sessions",
                "loops",
                "items",
                "blobs",
                "item_search"
            ]
        );
        let mut names = BTreeSet::new();
        let expected_columns = [
            ("sources", &["source_id", "locator", "runtime"][..]),
            (
                "records",
                &[
                    "record_id",
                    "source_id",
                    "source_position",
                    "content_range",
                    "fingerprint",
                    "occurred_at",
                ][..],
            ),
            (
                "sessions",
                &[
                    "session_id",
                    "runtime",
                    "native_id",
                    "identity_record_id",
                    "created_at",
                    "name",
                    "working_directory",
                    "source_ids",
                    "forked_from",
                    "delegated_from",
                ][..],
            ),
            (
                "loops",
                &[
                    "loop_id",
                    "session_id",
                    "session_position",
                    "native_id",
                    "start_record_id",
                    "end",
                    "model",
                    "usage",
                ][..],
            ),
            (
                "items",
                &[
                    "item_id",
                    "session_id",
                    "loop_id",
                    "loop_position",
                    "occurred_at",
                    "record_ids",
                    "semantic",
                ][..],
            ),
            (
                "blobs",
                &["blob_id", "text", "full_bytes", "estimated_tokens"][..],
            ),
            ("item_search", &["text"][..]),
        ];
        for (relation, (expected_name, expected)) in PUBLIC_RELATIONS.iter().zip(expected_columns) {
            assert!(names.insert(relation.name));
            assert_eq!(relation.name, expected_name);
            assert_eq!(
                relation
                    .columns
                    .iter()
                    .map(|column| column.name)
                    .collect::<Vec<_>>(),
                expected,
                "unexpected {} contract",
                relation.name
            );
            assert!(!relation.description.trim().is_empty());
            assert!(!relation.columns.is_empty());
            for column in relation.columns {
                assert!(!column.description.trim().is_empty());
                assert_eq!(
                    column.expression.is_some(),
                    matches!(relation.kind, PublicRelationKind::View { .. })
                );
            }
        }
    }

    #[test]
    fn declares_contentless_search_operand_nullable() {
        let search_text = PUBLIC_RELATIONS
            .iter()
            .find(|relation| relation.name == "item_search")
            .and_then(|relation| relation.columns.first())
            .expect("item_search text contract");
        assert!(search_text.nullable);
        assert!(search_text.description.contains("Reading it returns NULL"));
    }
}
