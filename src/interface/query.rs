//! Read-only SQL and public-schema query interface.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::ValueRef;

use super::output::{SchemaColumn, SchemaObject, SchemaReport, SqlQueryReport};
use crate::storage::db::Store;
use crate::storage::public_schema::{
    PUBLIC_RELATIONS, find_public_relation, validate_public_relations,
};

/// Executes one read-only SQL statement with bounded rows, cell size, and time.
///
/// # Errors
///
/// Returns an error for non-read-only or multi-statement SQL, duplicate output
/// column names, timeout, or `SQLite` decoding failures.
pub fn query_sql(
    store: &Store,
    sql: &str,
    limit: usize,
    max_cell_bytes: usize,
    max_output_bytes: usize,
    timeout: Duration,
) -> Result<SqlQueryReport> {
    if sql.trim().is_empty() {
        bail!("SQL must not be empty");
    }

    let started = Instant::now();
    store
        .connection()
        .progress_handler(10_000, Some(move || started.elapsed() >= timeout))?;
    let result = run_query(store, sql, limit, max_cell_bytes, max_output_bytes);
    store
        .connection()
        .progress_handler(0, None::<fn() -> bool>)?;
    match result {
        Err(error) if is_query_interrupted(&error) => Err(error.context(format!(
            "query exceeded the {}-second timeout; narrow by source, project, actor, or time, or estimate candidate density with COUNT or GROUP BY before fetching rows",
            timeout.as_secs()
        ))),
        Err(error) if has_literal_newline_escape(sql) && is_unrecognized_token(&error) => {
            Err(error.context(
                "SQL contains a literal `\\n`; pass real newlines with `--stdin` and a quoted heredoc, or use `--file`",
            ))
        }
        result => result,
    }
}

fn is_query_interrupted(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(|error| {
                matches!(
                    error,
                    rusqlite::Error::SqliteFailure(sqlite_error, _)
                        if sqlite_error.code == rusqlite::ErrorCode::OperationInterrupted
                )
            })
    })
}

fn has_literal_newline_escape(sql: &str) -> bool {
    sql.contains("\\n")
}

fn is_unrecognized_token(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("unrecognized token"))
}

fn run_query(
    store: &Store,
    sql: &str,
    limit: usize,
    max_cell_bytes: usize,
    max_output_bytes: usize,
) -> Result<SqlQueryReport> {
    let executing = Arc::new(AtomicBool::new(false));
    let authorizer_executing = Arc::clone(&executing);
    store
        .connection()
        .authorizer(Some(move |context: AuthContext<'_>| {
            authorize_read_only_query(context, authorizer_executing.load(Ordering::Relaxed))
        }))?;
    let result = run_authorized_query(
        store,
        sql,
        limit,
        max_cell_bytes,
        max_output_bytes,
        &executing,
    );
    store
        .connection()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;
    result
}

fn run_authorized_query(
    store: &Store,
    sql: &str,
    limit: usize,
    max_cell_bytes: usize,
    max_output_bytes: usize,
    executing: &AtomicBool,
) -> Result<SqlQueryReport> {
    let mut statement = store.connection().prepare(sql)?;
    if !statement.readonly() {
        bail!("only read-only SQL is allowed");
    }

    let columns = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let unique = columns.iter().collect::<BTreeSet<_>>();
    if unique.len() != columns.len() {
        bail!("query output has duplicate column names; use SQL aliases");
    }

    executing.store(true, Ordering::Relaxed);
    let result = (|| {
        let mut query = statement.query([])?;
        let mut output = Vec::with_capacity(limit);
        let mut cells_truncated = 0;
        let mut output_bytes = 0_usize;
        let mut incomplete_reason = None;
        while output.len() < limit {
            let Some(row) = query.next()? else {
                break;
            };
            let mut object = BTreeMap::new();
            let mut row_cells_truncated = 0;
            for (index, name) in columns.iter().enumerate() {
                let (value, truncated) = value_to_json(row.get_ref(index)?, max_cell_bytes);
                row_cells_truncated += usize::from(truncated);
                object.insert(name.clone(), value);
            }
            let row_bytes = serde_json::to_vec(&object)?.len() + usize::from(!output.is_empty());
            if output_bytes.saturating_add(row_bytes) > max_output_bytes {
                incomplete_reason = Some("output_budget".to_owned());
                break;
            }
            output_bytes += row_bytes;
            cells_truncated += row_cells_truncated;
            output.push(object);
        }

        if incomplete_reason.is_none() && query.next()?.is_some() {
            incomplete_reason = Some("row_limit".to_owned());
        }
        let complete = incomplete_reason.is_none();
        let hint = query_hint(incomplete_reason.as_deref(), cells_truncated);
        Ok(SqlQueryReport {
            complete,
            incomplete_reason,
            hint,
            returned: output.len(),
            cells_truncated,
            columns,
            rows: output,
        })
    })();
    executing.store(false, Ordering::Relaxed);
    result
}

fn query_hint(incomplete_reason: Option<&str>, cells_truncated: usize) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(reason) = incomplete_reason {
        parts.push(match reason {
        "row_limit" => "Only part of the result set was returned because the row limit was reached. Narrow the WHERE scope, aggregate before fetching rows, or continue with a stable, unique ORDER BY key; raise --limit only when the additional rows are necessary.".to_owned(),
        "output_budget" => "Only part of the result set was returned because the serialized rows reached the output budget. Return identifiers, counts, or shorter snippets first; remove large columns, narrow the WHERE scope, or continue with a stable, unique ORDER BY key before raising --max-output-bytes.".to_owned(),
        _ => "Only part of the result set was returned. Narrow the query or continue from a stable boundary before treating the result as exhaustive.".to_owned(),
        });
    }
    if cells_truncated > 0 {
        parts.push("One or more returned text or blob cells reached --max-cell-bytes and contain prefixes only. Do not treat those cell values as complete; project shorter snippets or fetch only selected identifiers with a larger cell budget.".to_owned());
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Bounds the Query Plane to reading this one database.
///
/// Reading is unrestricted, including the private storage tables behind the
/// public Relations. They hold the same local Trace data the caller owns. The
/// seven public Relations remain the stable contract; private tables are free to
/// change with the storage format.
///
/// What stays denied is everything that leaves the database or changes it.
/// `ATTACH` is the one that matters: read-only open flags do not stop it, and
/// it would reach any `SQLite` file on disk.
fn authorize_read_only_query(context: AuthContext<'_>, executing: bool) -> Authorization {
    match context.action {
        AuthAction::Select
        | AuthAction::Function { .. }
        | AuthAction::Recursive
        | AuthAction::Read { .. } => Authorization::Allow,
        AuthAction::Pragma {
            pragma_name: "data_version",
            pragma_value: None,
        } if executing => Authorization::Allow,
        _ => Authorization::Deny,
    }
}

fn value_to_json(value: ValueRef<'_>, max_bytes: usize) -> (serde_json::Value, bool) {
    match value {
        ValueRef::Null => (serde_json::Value::Null, false),
        ValueRef::Integer(value) => (serde_json::json!(value), false),
        ValueRef::Real(value) => (serde_json::json!(value), false),
        ValueRef::Text(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            let (text, truncated) = truncate_utf8(&text, max_bytes);
            (serde_json::Value::String(text.to_owned()), truncated)
        }
        ValueRef::Blob(bytes) => {
            let visible = &bytes[..bytes.len().min(max_bytes)];
            let mut hex = String::with_capacity(visible.len().saturating_mul(2));
            for byte in visible {
                use std::fmt::Write as _;
                write!(&mut hex, "{byte:02x}").expect("writing to String cannot fail");
            }
            (
                serde_json::json!({
                    "encoding": "hex",
                    "byte_length": bytes.len(),
                    "value": hex,
                }),
                visible.len() != bytes.len(),
            )
        }
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (&str, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (&value[..boundary], true)
}

/// Describes the stable public SQL relations.
///
/// # Errors
///
/// Returns an error if the requested relation is unknown or the public
/// relations in the database do not match the compiled contract.
pub fn public_schema(
    store: &Store,
    requested: Option<&str>,
    include_descriptions: bool,
) -> Result<SchemaReport> {
    if let Some(name) = requested
        && find_public_relation(name).is_none()
    {
        bail!(
            "unknown public relation {name:?}\n\
             Hint: run `trace-index schema list` to discover valid public relation names"
        );
    }

    validate_public_relations(store.connection())?;
    let mut objects = Vec::with_capacity(requested.map_or(PUBLIC_RELATIONS.len(), |_| 1));
    for relation in PUBLIC_RELATIONS
        .iter()
        .filter(|relation| requested.is_none_or(|requested| requested == relation.name))
    {
        let columns = if requested.is_some() {
            relation
                .columns
                .iter()
                .map(|column| SchemaColumn {
                    name: column.name.to_owned(),
                    data_type: column.data_type.to_owned(),
                    nullable: column.nullable,
                    // The Semantic object advertises the role vocabulary from
                    // the domain declaration so callers do not have to infer
                    // it from whichever roles happen to exist in one index.
                    description: include_descriptions.then(|| match column.name {
                        "semantic" => format!(
                            "{} {}",
                            column.description.trim(),
                            crate::domain::describe_semantic_roles()
                        ),
                        _ => column.description.trim().to_owned(),
                    }),
                })
                .collect()
        } else {
            Vec::new()
        };
        objects.push(SchemaObject {
            name: relation.name.to_owned(),
            description: include_descriptions.then(|| relation.description.trim().to_owned()),
            columns,
        });
    }
    Ok(SchemaReport { objects })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::tempdir;

    use super::{public_schema, query_sql};
    use crate::storage::db::Store;

    fn insert_search_fixture(store: &Store) {
        store
            .connection()
            .execute_batch(
                "INSERT INTO trace_sources(id, adapter, path)
                 VALUES (1, 'codex', '/tmp/session.jsonl');
                 INSERT INTO trace_records(
                     id, source_id, seq, byte_offset, byte_length, raw_hash,
                     native_type, parse_status
                 ) VALUES (1, 1, 0, 0, 100, 'hash', 'response_item/message', 'ok');
                 INSERT INTO domain_sessions(id, runtime, native_id)
                 VALUES (1, 'codex', 'session-fixture');
                 INSERT INTO session_sources(session_id, source_id, identity_record_id)
                 VALUES (1, 1, 1);
                 INSERT INTO domain_loops(
                     id, session_id, source_id, ordinal, session_position, start_record_id
                 ) VALUES (1, 1, 1, 0, 0, 1);
                 INSERT INTO content_blobs(
                     id, hash, published_bytes, text, full_bytes, estimated_tokens
                 ) VALUES (
                     1, X'00', 52,
                     'AgentFriendly 讨论智能体使用和知识索引', 52, 20
                 );
                 INSERT INTO domain_items(
                     id, session_id, source_id, loop_id, loop_position,
                     semantic
                 ) VALUES (
                     1, 1, 1, 1, 0,
                     json('{\"role\":\"human.request\",\"value\":{\"text\":{\"blob_id\":1},\"has_images\":false},\"evidence_strength\":\"structural\"}')
                 );
                 INSERT INTO item_records(item_id, record_id) VALUES (1, 1);
                 INSERT INTO item_search(rowid, text)
                 VALUES (1, 'AgentFriendly 讨论智能体使用和知识索引');",
            )
            .expect("insert search fixture");
    }

    #[test]
    fn exposes_public_views_and_bounded_read_only_sql() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");

        let schema = public_schema(&store, None, true).expect("public schema");
        assert!(schema.objects.iter().any(|object| object.name == "items"));
        assert!(schema.objects.iter().any(|object| object.name == "loops"));
        assert!(schema.objects.iter().any(|object| object.name == "blobs"));
        assert!(
            schema
                .objects
                .iter()
                .all(|object| object.columns.is_empty())
        );

        let report = query_sql(
            &store,
            "SELECT '中文abc' AS text UNION ALL SELECT 'second'",
            1,
            4,
            64 * 1024,
            Duration::from_secs(1),
        )
        .expect("query");
        assert!(!report.complete);
        assert_eq!(report.incomplete_reason.as_deref(), Some("row_limit"));
        assert!(
            report
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("stable, unique ORDER BY key"))
        );
        assert_eq!(report.returned, 1);
        assert_eq!(report.cells_truncated, 1);
        assert_eq!(report.rows[0]["text"], "中");
    }

    #[test]
    fn bounds_total_serialized_row_bytes() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");

        let report = query_sql(
            &store,
            "SELECT 'abcdefghijklmnopqrstuvwxyz' AS text",
            10,
            1024,
            16,
            Duration::from_secs(1),
        )
        .expect("query");
        assert!(!report.complete);
        assert_eq!(report.incomplete_reason.as_deref(), Some("output_budget"));
        assert!(
            report
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("shorter snippets"))
        );
        assert_eq!(report.returned, 0);
        assert_eq!(report.cells_truncated, 0);
    }

    #[test]
    fn explains_cell_truncation_even_when_every_row_was_returned() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");

        let report = query_sql(
            &store,
            "SELECT 'abcdefghijklmnopqrstuvwxyz' AS text",
            10,
            8,
            64 * 1024,
            Duration::from_secs(1),
        )
        .expect("query");
        assert!(report.complete);
        assert!(report.incomplete_reason.is_none());
        assert_eq!(report.cells_truncated, 1);
        assert!(
            report
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("prefixes only"))
        );
    }

    #[test]
    fn rejects_writes_and_duplicate_columns() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");

        assert!(
            query_sql(
                &store,
                "DELETE FROM items",
                10,
                1024,
                64 * 1024,
                Duration::from_secs(1),
            )
            .is_err()
        );
        assert!(
            query_sql(
                &store,
                "SELECT 1 AS value, 2 AS value",
                10,
                1024,
                64 * 1024,
                Duration::from_secs(1),
            )
            .is_err()
        );

        let syntax_error = query_sql(
            &store,
            "SELECT \\",
            10,
            1024,
            64 * 1024,
            Duration::from_secs(1),
        )
        .expect_err("reject malformed SQL");
        assert!(!format!("{syntax_error:#}").contains("exactly one statement"));

        let escaped_newline = query_sql(
            &store,
            "SELECT 1\\nSELECT 2",
            10,
            1024,
            64 * 1024,
            Duration::from_secs(1),
        )
        .expect_err("reject literal newline escape");
        let message = format!("{escaped_newline:#}");
        assert!(message.contains("literal `\\n`"), "{message}");
        assert!(message.contains("--stdin"), "{message}");
        assert!(message.contains("--file"), "{message}");
    }

    #[test]
    fn reads_storage_tables_but_refuses_to_leave_the_database() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");
        insert_search_fixture(&store);

        let run = |sql: &str| query_sql(&store, sql, 10, 1024, 64 * 1024, Duration::from_secs(1));

        // Storage tables are readable: they hold the caller's own Trace data,
        // and the public Relations are a contract rather than an access wall.
        let items = run("SELECT COUNT(*) AS n FROM items").expect("read storage table");
        assert_eq!(items.rows[0]["n"], 1);
        let schema = run("SELECT COUNT(*) AS n FROM sqlite_master").expect("read schema table");
        assert!(schema.rows[0]["n"].as_i64().is_some_and(|n| n > 0));

        // COUNT(*) over a named common table expression is the shape the
        // lineage walk in the docs produces, and it used to be denied.
        let counted = run("WITH x AS (SELECT item_id FROM items) SELECT COUNT(*) AS n FROM x")
            .expect("count over cte");
        assert_eq!(counted.rows[0]["n"], 1);

        // A CTE may shadow a storage table name without changing anything.
        let shadowed = run("WITH items AS (SELECT 1 AS a) SELECT COUNT(*) AS n FROM items")
            .expect("count over shadowing cte");
        assert_eq!(shadowed.rows[0]["n"], 1);

        // Leaving this database stays denied. Read-only open flags do not stop
        // ATTACH, so the authorizer is the only thing that does.
        let other = directory.path().join("other.sqlite");
        Store::open(&other).expect("second database");
        let attach = run(&format!("ATTACH '{}' AS other", other.display()))
            .expect_err("refuse to attach another database");
        assert!(format!("{attach:#}").contains("not authorized"));
        assert!(run("SELECT load_extension('x')").is_err());
    }

    #[test]
    fn explains_query_timeouts_with_recovery_guidance() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");

        let timeout = query_sql(
            &store,
            "WITH RECURSIVE counter(value) AS (
                 VALUES(1)
                 UNION ALL
                 SELECT value + 1 FROM counter WHERE value < 1000000
             )
             SELECT SUM(value) FROM counter",
            10,
            1024,
            64 * 1024,
            Duration::ZERO,
        )
        .expect_err("interrupt long query");
        let message = format!("{timeout:#}");
        assert!(message.contains("0-second timeout"), "{message}");
        assert!(message.contains("COUNT or GROUP BY"), "{message}");
    }

    #[test]
    fn filters_public_schema_to_one_named_view() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");

        let schema = public_schema(&store, Some("items"), true).expect("filtered schema");
        assert_eq!(schema.objects.len(), 1);
        assert_eq!(schema.objects[0].name, "items");
        assert_eq!(
            schema.objects[0]
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            [
                "item_id",
                "session_id",
                "loop_id",
                "loop_position",
                "occurred_at",
                "record_ids",
                "semantic",
            ]
        );
        assert!(schema.objects[0].columns.iter().all(|column| {
            !column.data_type.is_empty()
                && column
                    .description
                    .as_deref()
                    .is_some_and(|description| !description.is_empty())
        }));
        let item_id = schema.objects[0]
            .columns
            .iter()
            .find(|column| column.name == "item_id")
            .expect("item_id contract");
        assert_eq!(item_id.data_type, "INTEGER");
        assert!(!item_id.nullable);
        let unknown =
            public_schema(&store, Some("missing"), true).expect_err("reject unknown relation");
        assert!(unknown.to_string().contains("trace-index schema list"));
    }

    #[test]
    fn exposes_trigram_candidates_through_the_public_sql_plane() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");
        insert_search_fixture(&store);

        let schema = public_schema(&store, Some("item_search"), true).expect("search schema");
        assert_eq!(schema.objects.len(), 1);
        assert_eq!(schema.objects[0].name, "item_search");
        assert_eq!(
            schema.objects[0]
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["text"]
        );
        assert!(schema.objects[0].columns[0].nullable);

        let operand = query_sql(
            &store,
            "SELECT rowid, text FROM item_search(trigram_query('智能体使用'))",
            10,
            1024,
            64 * 1024,
            Duration::from_secs(1),
        )
        .expect("read contentless search operand");
        assert_eq!(operand.returned, 1);
        assert!(operand.rows[0]["text"].is_null());

        let report = query_sql(
            &store,
            "WITH candidates AS MATERIALIZED (
                 SELECT rowid AS item_id
                   FROM item_search(trigram_query('智能体使用'))
             )
             SELECT i.item_id, json_extract(i.semantic, '$.role') AS role
               FROM candidates c
               JOIN items i USING(item_id)
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE instr(b.text, '智能体使用') > 0",
            10,
            1024,
            64 * 1024,
            Duration::from_secs(1),
        )
        .expect("query indexed candidates");
        assert_eq!(report.returned, 1);
        assert_eq!(report.rows[0]["role"], "human.request");
    }

    #[test]
    fn keeps_candidate_generation_separate_from_exact_matching() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");
        insert_search_fixture(&store);

        let candidates = query_sql(
            &store,
            "SELECT rowid FROM item_search(trigram_query('agentfriendly'))",
            10,
            1024,
            64 * 1024,
            Duration::from_secs(1),
        )
        .expect("query case-folded candidates");
        assert_eq!(candidates.returned, 1);

        let exact = query_sql(
            &store,
            "SELECT i.item_id
               FROM item_search(trigram_query('agentfriendly')) c
               JOIN items i ON i.item_id = c.rowid
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE instr(b.text, 'agentfriendly') > 0",
            10,
            1024,
            64 * 1024,
            Duration::from_secs(1),
        )
        .expect("post-check exact text");
        assert_eq!(exact.returned, 0);
    }

    #[test]
    fn rejects_short_trigram_queries() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");

        let short = query_sql(
            &store,
            "SELECT trigram_query('中文') AS query",
            10,
            1024,
            64 * 1024,
            Duration::from_secs(1),
        )
        .expect_err("reject short trigram query");
        assert!(
            format!("{short:#}").contains("narrow by source, project, actor, or time"),
            "{short:#}"
        );
    }
}
