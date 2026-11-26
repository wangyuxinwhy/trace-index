//! Read-only SQL and public-schema query interface.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::{Value as SqlValue, ValueRef};
use serde::Serialize;

use super::output::{SchemaColumn, SchemaObject, SchemaReport, SqlQueryReport};
use crate::storage::db::Store;
use crate::storage::public_schema::{
    PUBLIC_RELATIONS, find_public_relation, validate_public_relations,
};

/// Execution and output limits enforced on one read-only SQL query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryBudget {
    pub limit: usize,
    pub max_cell_bytes: usize,
    pub max_output_bytes: usize,
    pub timeout: Duration,
}

/// Typed named SQL parameter value bound via `SQLite` native parameter binding.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SqlParamValue {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

impl SqlParamValue {
    fn to_sqlite_value(&self) -> SqlValue {
        match self {
            Self::Null => SqlValue::Null,
            Self::Int(value) => SqlValue::Integer(*value),
            Self::Real(value) => SqlValue::Real(*value),
            Self::Text(value) => SqlValue::Text(value.clone()),
        }
    }
}

/// Parses `--params-json` and repeatable `-p key=value` CLI arguments into
/// typed SQL bind parameters.
///
/// `--params-json` preserves exact JSON types (`null`, integer, float, boolean
/// as `0`/`1`, string, or compact JSON text for arrays/objects). Repeatable
/// `--param key=value` pairs bind as text and override matching JSON keys.
///
/// # Errors
///
/// Returns an error if `--params-json` is not a valid JSON object, contains an
/// invalid parameter name, or a `--param` item is missing `=`.
pub fn parse_cli_query_params(
    params_json: Option<&str>,
    raw_params: &[String],
) -> Result<BTreeMap<String, SqlParamValue>> {
    let mut params = BTreeMap::new();
    if let Some(json_text) = params_json {
        let parsed: serde_json::Value =
            serde_json::from_str(json_text).context("--params-json must be valid JSON")?;
        let Some(object) = parsed.as_object() else {
            bail!("--params-json must be a JSON object mapping parameter names to values");
        };
        for (raw_key, value) in object {
            let key = normalize_param_name(raw_key)?;
            params.insert(key, json_to_sql_param(value)?);
        }
    }
    for entry in raw_params {
        let Some((raw_key, raw_value)) = entry.split_once('=') else {
            bail!("invalid --param {entry:?}; expected key=value");
        };
        let key = normalize_param_name(raw_key)?;
        params.insert(key, SqlParamValue::Text(raw_value.to_owned()));
    }
    Ok(params)
}

fn normalize_param_name(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_start_matches([':', '@', '$']);
    if trimmed.is_empty()
        || !trimmed
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || trimmed.as_bytes()[0].is_ascii_digit()
    {
        bail!("invalid SQL parameter name {raw:?}; use ASCII letters, digits, and '_'");
    }
    Ok(trimmed.to_owned())
}

fn json_to_sql_param(value: &serde_json::Value) -> Result<SqlParamValue> {
    match value {
        serde_json::Value::Null => Ok(SqlParamValue::Null),
        serde_json::Value::Bool(flag) => Ok(SqlParamValue::Int(i64::from(*flag))),
        serde_json::Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                Ok(SqlParamValue::Int(int))
            } else if let Some(real) = number.as_f64() {
                Ok(SqlParamValue::Real(real))
            } else {
                bail!("JSON number {number} exceeds 64-bit SQLite parameter range")
            }
        }
        serde_json::Value::String(text) => Ok(SqlParamValue::Text(text.clone())),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => Ok(SqlParamValue::Text(
            serde_json::to_string(value).context("failed to encode JSON parameter value")?,
        )),
    }
}

/// Executes one read-only SQL statement with bounded rows, cell size, and time.
///
/// # Errors
///
/// Returns an error for non-read-only or multi-statement SQL, duplicate output
/// column names, timeout, or `SQLite` decoding failures.
#[cfg(test)]
pub fn query_sql(store: &Store, sql: &str, budget: QueryBudget) -> Result<SqlQueryReport> {
    query_sql_with_params(store, sql, &BTreeMap::new(), budget)
}

/// Executes one parameterized read-only SQL statement with bounded rows, cell
/// size, and time.
///
/// Parameters are bound by introspecting `SQLite`'s own prepared statement
/// parameter list (`parameter_count` / `parameter_name`). Any parameter in
/// `params` not referenced by the prepared SQL is safely ignored.
///
/// # Errors
///
/// Returns an error for non-read-only or multi-statement SQL, missing or
/// positional parameters, duplicate output column names, timeout, or `SQLite`
/// decoding failures.
pub fn query_sql_with_params(
    store: &Store,
    sql: &str,
    params: &BTreeMap<String, SqlParamValue>,
    budget: QueryBudget,
) -> Result<SqlQueryReport> {
    if sql.trim().is_empty() {
        bail!("SQL must not be empty");
    }

    let started = Instant::now();
    let timeout = budget.timeout;
    store
        .connection()
        .progress_handler(10_000, Some(move || started.elapsed() >= timeout))?;
    let result = run_query(store, sql, params, budget);
    store
        .connection()
        .progress_handler(0, None::<fn() -> bool>)?;
    match result {
        Err(error) if is_query_interrupted(&error) => Err(error.context(format!(
            "query exceeded the {}-second timeout; narrow by source, project, actor, or time, or estimate candidate density with COUNT or GROUP BY before fetching rows",
            timeout.as_secs_f64()
        ))),
        Err(error) if has_literal_newline_escape(sql) && is_unrecognized_token(&error) => {
            Err(error.context(
                "SQL contains a literal `\\n`; pass real newlines with `--stdin` and a quoted heredoc, or use `--file`",
            ))
        }
        Err(error) if is_fts5_query_error(&error) => {
            let format = store
                .storage_format()
                .unwrap_or(crate::storage::schema::STORAGE_FORMAT);
            let hint = if format >= 4 {
                "use item_search_terms(search_query('...')) for word, identifier, or 2+ char CJK searches (supports bm25()), or item_search(trigram_query('...')) for 3+ char literal substrings; see 'trace-index docs get how-to/search-literals'".to_owned()
            } else {
                format!(
                    "this Format {format} index has only item_search (trigram, detail=none); pass 3+ char literal strings through item_search(trigram_query('...')) or build a Format 4 index for item_search_terms; see 'trace-index docs get how-to/search-literals'"
                )
            };
            Err(error.context(hint))
        }
        Err(error) if is_missing_schema_object_error(&error) => {
            let format = store
                .storage_format()
                .unwrap_or(crate::storage::schema::STORAGE_FORMAT);
            let hint = if format < 4 && error_mentions_item_search_terms(&error) {
                format!(
                    "item_search_terms requires a Format 4 index (this database is Format {format}); use item_search(trigram_query('...')) or narrow structurally via items and blobs, or build a Format 4 index"
                )
            } else {
                "inspect the current public relations and columns with 'trace-index schema list' or 'trace-index schema get <RELATION> --compact'".to_owned()
            };
            Err(error.context(hint))
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

fn is_fts5_query_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("fts5:"))
}

fn is_missing_schema_object_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string();
        message.contains("no such column:") || message.contains("no such table:")
    })
}

fn error_mentions_item_search_terms(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("item_search_terms"))
}

fn run_query(
    store: &Store,
    sql: &str,
    params: &BTreeMap<String, SqlParamValue>,
    budget: QueryBudget,
) -> Result<SqlQueryReport> {
    let executing = Arc::new(AtomicBool::new(false));
    let authorizer_executing = Arc::clone(&executing);
    store
        .connection()
        .authorizer(Some(move |context: AuthContext<'_>| {
            authorize_read_only_query(context, authorizer_executing.load(Ordering::Relaxed))
        }))?;
    let result = run_authorized_query(store, sql, params, budget, &executing);
    store
        .connection()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;
    result
}

fn bind_named_parameters(
    statement: &mut rusqlite::Statement<'_>,
    params: &BTreeMap<String, SqlParamValue>,
) -> Result<()> {
    for index in 1..=statement.parameter_count() {
        let Some(raw_name) = statement.parameter_name(index) else {
            bail!(
                "positional SQL placeholder ?{index} is not supported; use named parameters (:name) with --param or --params-json"
            );
        };
        let Some(name) = raw_name.strip_prefix([':', '@', '$']) else {
            bail!(
                "positional SQL placeholder {raw_name:?} is not supported; use named parameters (:name)"
            );
        };
        if name.is_empty() || name.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!(
                "positional SQL placeholder {raw_name:?} is not supported; use named parameters (:name)"
            );
        }
        let Some(value) = params.get(name) else {
            bail!(
                "missing SQL parameter {raw_name}; pass --param {name}=<VALUE> or include {name:?} in --params-json"
            );
        };
        statement.raw_bind_parameter(index, value.to_sqlite_value())?;
    }
    Ok(())
}

fn run_authorized_query(
    store: &Store,
    sql: &str,
    params: &BTreeMap<String, SqlParamValue>,
    budget: QueryBudget,
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

    bind_named_parameters(&mut statement, params)?;

    executing.store(true, Ordering::Relaxed);
    let result = (|| {
        let mut query = statement.raw_query();
        let mut output = Vec::with_capacity(budget.limit);
        let mut cells_truncated = 0;
        let mut output_bytes = 0_usize;
        let mut incomplete_reason = None;
        while output.len() < budget.limit {
            let Some(row) = query.next()? else {
                break;
            };
            let mut object = BTreeMap::new();
            let mut row_cells_truncated = 0;
            for (index, name) in columns.iter().enumerate() {
                let (value, truncated) = value_to_json(row.get_ref(index)?, budget.max_cell_bytes);
                row_cells_truncated += usize::from(truncated);
                object.insert(name.clone(), value);
            }
            let row_bytes = serde_json::to_vec(&object)?.len() + usize::from(!output.is_empty());
            if output_bytes.saturating_add(row_bytes) > budget.max_output_bytes {
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

/// Bounds user SQL to reading the index and tool-selected metadata store.
///
/// Reading is unrestricted, including the private storage tables behind the
/// public Relations. They hold local data the caller owns. The
/// declared public Relations remain the stable contract; private tables are free to
/// change with the storage format.
///
/// User SQL cannot select another database or modify either selected store.
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

/// Renders a query report as tab-separated values with a header row.
///
/// Cell strings escape `\`, `\t`, `\n`, and `\r` so every returned row stays
/// strictly one physical line with `columns.len() - 1` tab delimiters.
///
/// # Errors
///
/// Returns an error if writing to `output` or serializing a structured cell
/// fails.
pub(crate) fn write_query_tsv(report: &SqlQueryReport, output: &mut impl Write) -> Result<()> {
    for (index, column) in report.columns.iter().enumerate() {
        if index > 0 {
            output.write_all(b"\t").context("failed to write TSV tab")?;
        }
        write_escaped_tsv_str(output, column)?;
    }
    output
        .write_all(b"\n")
        .context("failed to write TSV header newline")?;

    for row in &report.rows {
        for (index, column) in report.columns.iter().enumerate() {
            if index > 0 {
                output.write_all(b"\t").context("failed to write TSV tab")?;
            }
            if let Some(value) = row.get(column) {
                write_tsv_value(output, value)?;
            }
        }
        output
            .write_all(b"\n")
            .context("failed to write TSV row newline")?;
    }
    Ok(())
}

/// Builds a stderr notice when TSV output is incomplete or truncated.
pub(crate) fn format_tsv_truncation_notice(report: &SqlQueryReport) -> Option<String> {
    if report.complete && report.cells_truncated == 0 {
        return None;
    }
    let mut details = Vec::new();
    if let Some(reason) = &report.incomplete_reason {
        details.push(format!("reason={reason}"));
    }
    details.push(format!("returned={}", report.returned));
    if report.cells_truncated > 0 {
        details.push(format!("cells_truncated={}", report.cells_truncated));
    }
    let prefix = if report.complete {
        "notice: TSV output contains truncated cells"
    } else {
        "notice: TSV output is incomplete"
    };
    let mut message = format!("{prefix} ({})", details.join(", "));
    if let Some(hint) = &report.hint {
        message.push_str(". ");
        message.push_str(hint);
    }
    Some(message)
}

fn write_tsv_value(output: &mut impl Write, value: &serde_json::Value) -> Result<()> {
    match value {
        serde_json::Value::Null => Ok(()),
        serde_json::Value::Bool(flag) => {
            write!(output, "{flag}").context("failed to write TSV boolean")
        }
        serde_json::Value::Number(number) => {
            write!(output, "{number}").context("failed to write TSV number")
        }
        serde_json::Value::String(text) => write_escaped_tsv_str(output, text),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            let encoded =
                serde_json::to_string(value).context("failed to serialize structured TSV cell")?;
            write_escaped_tsv_str(output, &encoded)
        }
    }
}

fn write_escaped_tsv_str(output: &mut impl Write, text: &str) -> Result<()> {
    let mut start = 0;
    for (index, byte) in text.bytes().enumerate() {
        let escaped: &[u8] = match byte {
            b'\\' => br"\\",
            b'\t' => br"\t",
            b'\n' => br"\n",
            b'\r' => br"\r",
            _ => continue,
        };
        if index > start {
            output
                .write_all(&text.as_bytes()[start..index])
                .context("failed to write TSV text")?;
        }
        output
            .write_all(escaped)
            .context("failed to write escaped TSV byte")?;
        start = index + 1;
    }
    if start < text.len() {
        output
            .write_all(&text.as_bytes()[start..])
            .context("failed to write TSV text tail")?;
    }
    Ok(())
}

/// Describes the stable public SQL relations.
///
/// An empty `requested` slice lists all public relations without columns; a
/// non-empty slice validates every name and returns each requested relation's
/// columns in caller order (deduplicating repeated names).
///
/// # Errors
///
/// Returns an error if any requested relation is unknown or the public
/// relations in the database do not match the compiled contract.
pub fn public_schema(
    store: &Store,
    requested: &[&str],
    include_descriptions: bool,
) -> Result<SchemaReport> {
    validate_public_relations(store.connection(), store.storage_format()?)?;
    describe_schema(requested, include_descriptions)
}

/// Describes declarations without opening or creating any database.
///
/// # Errors
/// Returns an error for an unknown relation name.
pub(crate) fn describe_schema(
    requested: &[&str],
    include_descriptions: bool,
) -> Result<SchemaReport> {
    if requested.is_empty() {
        let objects = PUBLIC_RELATIONS
            .iter()
            .map(|relation| SchemaObject {
                name: relation.name.to_owned(),
                description: include_descriptions.then(|| relation.description.trim().to_owned()),
                columns: Vec::new(),
            })
            .collect();
        return Ok(SchemaReport { objects });
    }

    let mut seen = BTreeSet::new();
    let mut objects = Vec::with_capacity(requested.len());
    for &name in requested {
        let Some(relation) = find_public_relation(name) else {
            bail!(
                "unknown public relation {name:?}\n\
                 Hint: run `trace-index schema list` to discover valid public relation names"
            );
        };
        if !seen.insert(relation.name) {
            continue;
        }
        let columns = relation
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
            .collect();
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

    use super::{QueryBudget, public_schema, query_sql};
    use crate::storage::db::Store;

    const TEST_BUDGET: QueryBudget = QueryBudget {
        limit: 10,
        max_cell_bytes: 1024,
        max_output_bytes: 64 * 1024,
        timeout: Duration::from_secs(1),
    };

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
                 INSERT INTO content_blobs(id, hash, text, full_bytes, estimated_tokens)
                 VALUES (
                     1, X'00', 'AgentFriendly 讨论智能体使用和知识索引', 52, 20
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
                 VALUES (1, 'AgentFriendly 讨论智能体使用和知识索引');
                 INSERT INTO item_search_terms(rowid, text)
                 VALUES (1, 'agentfriendly agent friendly 讨论 论智 智能 能体 体使 使用 用和 和知 知识 识索 索引');",
            )
            .expect("insert search fixture");
    }

    #[test]
    fn exposes_public_views_and_bounded_read_only_sql() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");

        let schema = public_schema(&store, &[], true).expect("public schema");
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
            QueryBudget {
                limit: 1,
                max_cell_bytes: 4,
                ..TEST_BUDGET
            },
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
            QueryBudget {
                max_output_bytes: 16,
                ..TEST_BUDGET
            },
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
            QueryBudget {
                max_cell_bytes: 8,
                ..TEST_BUDGET
            },
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

        assert!(query_sql(&store, "DELETE FROM items", TEST_BUDGET).is_err());
        assert!(query_sql(&store, "SELECT 1 AS value, 2 AS value", TEST_BUDGET).is_err());

        let syntax_error =
            query_sql(&store, "SELECT \\", TEST_BUDGET).expect_err("reject malformed SQL");
        assert!(!format!("{syntax_error:#}").contains("exactly one statement"));

        let escaped_newline = query_sql(&store, "SELECT 1\\nSELECT 2", TEST_BUDGET)
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

        let run = |sql: &str| query_sql(&store, sql, TEST_BUDGET);

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
            QueryBudget {
                timeout: Duration::ZERO,
                ..TEST_BUDGET
            },
        )
        .expect_err("interrupt long query");
        let message = format!("{timeout:#}");
        assert!(message.contains("0-second timeout"), "{message}");
        assert!(message.contains("COUNT or GROUP BY"), "{message}");
    }

    #[test]
    fn fractional_timeout_diagnostics_preserve_the_limit_and_query_recovery() {
        let directory = tempdir().expect("temp directory");
        let store = Store::open(&directory.path().join("index.sqlite")).expect("open store");
        for (limit, seconds) in [
            (Duration::from_millis(1), "0.001"),
            (Duration::from_millis(10), "0.01"),
        ] {
            let error = query_sql(
                &store,
                "WITH RECURSIVE counter(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM counter) SELECT SUM(n) FROM counter",
                QueryBudget {
                    max_output_bytes: 4096,
                    timeout: limit,
                    ..TEST_BUDGET
                },
            ).expect_err("interrupt unbounded recursion");
            let message = format!("{error:#}");
            assert!(
                message.starts_with(&format!("query exceeded the {seconds}-second timeout;")),
                "{message}"
            );
            assert_eq!(message.matches("query exceeded").count(), 1);
            let recovered = query_sql(
                &store,
                "SELECT 1 AS n",
                QueryBudget {
                    max_output_bytes: 4096,
                    ..TEST_BUDGET
                },
            )
            .expect("connection recovered after interruption");
            assert_eq!(recovered.rows[0]["n"], 1);
        }
    }

    #[test]
    fn filters_public_schema_to_one_named_view() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");

        let schema = public_schema(&store, &["items"], true).expect("filtered schema");
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
            public_schema(&store, &["missing"], true).expect_err("reject unknown relation");
        assert!(unknown.to_string().contains("trace-index schema list"));

        let multi = public_schema(&store, &["items", "sessions", "items"], false)
            .expect("multi-relation schema");
        assert_eq!(
            multi
                .objects
                .iter()
                .map(|object| object.name.as_str())
                .collect::<Vec<_>>(),
            ["items", "sessions"]
        );
        assert!(
            multi
                .objects
                .iter()
                .all(|object| object.description.is_none())
        );
        assert!(
            multi
                .objects
                .iter()
                .all(|object| !object.columns.is_empty())
        );
        assert!(
            public_schema(&store, &["sessions", "missing"], false).is_err(),
            "unknown relation in a multi-relation request must fail"
        );
    }

    #[test]
    fn exposes_trigram_candidates_through_the_public_sql_plane() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");
        insert_search_fixture(&store);

        let schema = public_schema(&store, &["item_search"], true).expect("search schema");
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
            TEST_BUDGET,
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
            TEST_BUDGET,
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
            TEST_BUDGET,
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
            TEST_BUDGET,
        )
        .expect("post-check exact text");
        assert_eq!(exact.returned, 0);
    }

    #[test]
    fn rejects_short_trigram_queries() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");

        let short = query_sql(&store, "SELECT trigram_query('中文') AS query", TEST_BUDGET)
            .expect_err("reject short trigram query");
        let message = format!("{short:#}");
        assert!(
            message.contains("item_search_terms(search_query('...'))"),
            "{message}"
        );
        assert!(
            message.contains("narrow by source, session, role, or time"),
            "{message}"
        );
    }

    #[test]
    fn hints_trigram_query_on_fts5_phrase_query_error() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");
        insert_search_fixture(&store);

        let error = query_sql(
            &store,
            "SELECT rowid FROM item_search WHERE item_search MATCH '\"agent friendly\"'",
            TEST_BUDGET,
        )
        .expect_err("reject FTS5 phrase query on detail=none index");
        let message = format!("{error:#}");
        assert!(
            message.contains("item_search(trigram_query('...'))"),
            "{message}"
        );
        assert!(
            message.contains("item_search_terms(search_query('...'))"),
            "{message}"
        );
    }

    #[test]
    fn searches_terms_short_cjk_identifiers_and_bm25_through_item_search_terms() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("open store");
        insert_search_fixture(&store);

        let schema =
            public_schema(&store, &["item_search_terms"], true).expect("term search schema");
        assert_eq!(schema.objects.len(), 1);
        assert_eq!(schema.objects[0].name, "item_search_terms");
        assert!(schema.objects[0].columns[0].nullable);

        // 2-character CJK query (unsupported by trigram_query)
        let cjk2 = query_sql(
            &store,
            "SELECT rowid, text, bm25(item_search_terms) AS score
               FROM item_search_terms(search_query('索引'))
              ORDER BY score",
            TEST_BUDGET,
        )
        .expect("query 2-char CJK");
        assert_eq!(cjk2.returned, 1);
        assert_eq!(cjk2.rows[0]["rowid"], 1);
        assert!(cjk2.rows[0]["text"].is_null());
        assert!(cjk2.rows[0]["score"].is_number());

        // 3+ character CJK requires exact adjacency ("智能 能体"), rejecting non-adjacent chars.
        let cjk_phrase = query_sql(
            &store,
            "SELECT rowid FROM item_search_terms(search_query('智能体'))",
            TEST_BUDGET,
        )
        .expect("query 3-char CJK phrase");
        assert_eq!(cjk_phrase.returned, 1);

        let cjk_nonadjacent = query_sql(
            &store,
            "SELECT rowid FROM item_search_terms(search_query('智能索引'))",
            TEST_BUDGET,
        )
        .expect("query non-adjacent 4-char CJK");
        assert_eq!(cjk_nonadjacent.returned, 0);

        // Split camelCase identifier + mixed CJK & English multi-word query.
        let mixed = query_sql(
            &store,
            "SELECT i.item_id, b.text
               FROM item_search_terms(search_query('friendly 知识')) t
               JOIN items i ON i.item_id = t.rowid
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')",
            TEST_BUDGET,
        )
        .expect("query mixed split identifier and 2-char CJK");
        assert_eq!(mixed.returned, 1);
        assert_eq!(mixed.rows[0]["item_id"], 1);
    }

    #[test]
    fn hints_format_3_readers_without_suggesting_missing_item_search_terms() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("format3.sqlite");
        let store = Store::open(&database).expect("open store");
        insert_search_fixture(&store);
        store
            .connection()
            .execute_batch(
                "DROP TABLE item_search_terms;
                 DROP VIEW blobs;
                 CREATE VIEW blobs AS
                 SELECT b.id AS \"blob_id\",
                        b.text AS \"text\",
                        b.full_bytes AS \"full_bytes\",
                        b.estimated_tokens AS \"estimated_tokens\"
                 FROM content_blobs b;
                 PRAGMA user_version = 3;",
            )
            .expect("downgrade to format 3");
        drop(store);

        let reader = Store::open_read_only(&database).expect("open format 3 read-only");
        let missing = query_sql(
            &reader,
            "SELECT rowid FROM item_search_terms(search_query('索引'))",
            TEST_BUDGET,
        )
        .expect_err("item_search_terms absent on format 3");
        let missing_msg = format!("{missing:#}");
        assert!(missing_msg.contains("Format 4 index"), "{missing_msg}");
        assert!(missing_msg.contains("Format 3"), "{missing_msg}");

        let fts_err = query_sql(
            &reader,
            "SELECT rowid FROM item_search WHERE item_search MATCH '\"agent friendly\"'",
            TEST_BUDGET,
        )
        .expect_err("fts5 error on format 3");
        let fts_msg = format!("{fts_err:#}");
        assert!(fts_msg.contains("Format 3 index"), "{fts_msg}");
        assert!(
            fts_msg.contains("item_search(trigram_query('...'))"),
            "{fts_msg}"
        );
    }
}
