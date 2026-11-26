//! Typed parameter parsing, validation, and resolution for SQL templates.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use rusqlite::params;
use serde::Serialize;

use super::parser::{ListItemType, ParamSpec, ParamType, TemplateHeader};
use crate::adapters::timestamp::parse_timestamp_ms;
use crate::domain::SEMANTIC_ROLES;
use crate::interface::query::SqlParamValue;
use crate::storage::db::Store;

/// Resolved values and metadata produced from raw CLI template parameters.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedTemplateParams {
    pub(crate) bound: BTreeMap<String, SqlParamValue>,
    pub(crate) active_flags: BTreeMap<String, bool>,
    pub(crate) resolutions: BTreeMap<String, ParamResolution>,
}

/// Auditable resolution detail echoed in `--render` and query reports for
/// non-trivial typed parameters (`time` and `session_ref`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParamResolution {
    Time {
        input: String,
        epoch_ms: i64,
        now_ms: i64,
    },
    SessionRef {
        input: String,
        session_id: i64,
        runtime: String,
        native_id: String,
    },
}

/// Parses raw `-p key=value` template arguments against a template header and
/// resolves `time` and `session_ref` values.
///
/// # Errors
///
/// Returns an error for unknown/duplicate/missing parameters, type or range
/// violations, ambiguous or nonexistent `session_ref` targets, or naive
/// timestamps lacking an explicit timezone offset.
pub(crate) fn resolve_template_params(
    header: &TemplateHeader,
    raw_params: &[String],
    store: &Store,
) -> Result<ResolvedTemplateParams> {
    let now_ms = current_epoch_ms();
    resolve_template_params_at(header, raw_params, store, now_ms)
}

pub(crate) fn resolve_template_params_at(
    header: &TemplateHeader,
    raw_params: &[String],
    store: &Store,
    now_ms: i64,
) -> Result<ResolvedTemplateParams> {
    let provided = parse_raw_param_pairs(header, raw_params)?;
    let mut bound = BTreeMap::new();
    let mut active_flags = BTreeMap::new();
    let mut resolutions = BTreeMap::new();

    for (name, spec) in &header.params {
        if let Some(raw_value) = provided.get(name) {
            let (sql_val, active, resolution) = coerce_param(name, spec, raw_value, store, now_ms)?;
            bound.insert(name.clone(), sql_val);
            active_flags.insert(name.clone(), active);
            if let Some(detail) = resolution {
                resolutions.insert(name.clone(), detail);
            }
        } else if let Some(default) = &spec.default {
            let (sql_val, active, resolution) = coerce_default(name, spec, default, store, now_ms)?;
            bound.insert(name.clone(), sql_val);
            active_flags.insert(name.clone(), active);
            if let Some(detail) = resolution {
                resolutions.insert(name.clone(), detail);
            }
        } else if spec.required {
            bail!(
                "template {:?} requires parameter {:?}; pass --param {name}=<VALUE>",
                header.name,
                name
            );
        } else {
            bound.insert(name.clone(), SqlParamValue::Null);
            active_flags.insert(name.clone(), false);
        }
    }

    validate_item_ref_session_scope(header, &bound, &resolutions, store)?;

    Ok(ResolvedTemplateParams {
        bound,
        active_flags,
        resolutions,
    })
}

fn validate_item_ref_session_scope(
    header: &TemplateHeader,
    bound: &BTreeMap<String, SqlParamValue>,
    resolutions: &BTreeMap<String, ParamResolution>,
    store: &Store,
) -> Result<()> {
    let target_session = resolutions.values().find_map(|res| match res {
        ParamResolution::SessionRef { session_id, .. } => Some(*session_id),
        ParamResolution::Time { .. } => None,
    });
    let Some(expected_session_id) = target_session else {
        return Ok(());
    };
    for (name, spec) in &header.params {
        if spec.kind == ParamType::ItemRef
            && let Some(SqlParamValue::Int(item_id)) = bound.get(name)
        {
            let actual_session_id: i64 = store
                .connection()
                .query_row(
                    "SELECT session_id FROM items WHERE item_id = ?1",
                    params![item_id],
                    |row| row.get(0),
                )
                .map_err(|_| anyhow::anyhow!("item {item_id} not found for parameter {name:?}"))?;
            if actual_session_id != expected_session_id {
                bail!(
                    "parameter {name:?} item {item_id} belongs to session_id={actual_session_id}, not session_id={expected_session_id}"
                );
            }
        }
    }
    Ok(())
}

fn parse_raw_param_pairs(
    header: &TemplateHeader,
    raw_params: &[String],
) -> Result<BTreeMap<String, String>> {
    let mut provided = BTreeMap::new();
    for entry in raw_params {
        let Some((raw_key, raw_value)) = entry.split_once('=') else {
            bail!("invalid --param {entry:?}; expected key=value");
        };
        let key = raw_key.trim().trim_start_matches(':');
        if !header.params.contains_key(key) {
            let valid = header
                .params
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "unknown parameter {key:?} for template {:?}; valid parameters: {valid}",
                header.name
            );
        }
        if provided
            .insert(key.to_owned(), raw_value.to_owned())
            .is_some()
        {
            bail!(
                "duplicate parameter {key:?}; specify each parameter once (use comma-separated values for list parameters)"
            );
        }
    }
    Ok(provided)
}

fn coerce_default(
    name: &str,
    spec: &ParamSpec,
    default: &toml::Value,
    store: &Store,
    now_ms: i64,
) -> Result<(SqlParamValue, bool, Option<ParamResolution>)> {
    match default {
        toml::Value::Boolean(flag) => Ok((SqlParamValue::Int(i64::from(*flag)), *flag, None)),
        toml::Value::Integer(value) => Ok((SqlParamValue::Int(*value), true, None)),
        toml::Value::String(text) => coerce_param(name, spec, text, store, now_ms),
        toml::Value::Array(items) if spec.kind == ParamType::List => {
            let strings = items
                .iter()
                .map(|item| {
                    item.as_str()
                        .ok_or_else(|| anyhow::anyhow!("list default for {name:?} must be strings"))
                        .map(str::to_owned)
                })
                .collect::<Result<Vec<_>>>()?;
            let joined = strings.join(",");
            coerce_param(name, spec, &joined, store, now_ms)
        }
        _ => bail!("unsupported default value for parameter {name:?}"),
    }
}

fn coerce_param(
    name: &str,
    spec: &ParamSpec,
    raw: &str,
    store: &Store,
    now_ms: i64,
) -> Result<(SqlParamValue, bool, Option<ParamResolution>)> {
    match spec.kind {
        ParamType::String => Ok((SqlParamValue::Text(raw.to_owned()), true, None)),
        ParamType::Int => {
            let value: i64 = raw.trim().parse().map_err(|_| {
                anyhow::anyhow!("parameter {name:?} expects an integer, got {raw:?}")
            })?;
            if let Some(min) = spec.min
                && value < min
            {
                bail!("parameter {name:?} must be >= {min}, got {value}");
            }
            if let Some(max) = spec.max
                && value > max
            {
                bail!("parameter {name:?} must be <= {max}, got {value}");
            }
            Ok((SqlParamValue::Int(value), true, None))
        }
        ParamType::Bool => {
            let flag = match raw.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => true,
                "false" | "0" | "no" => false,
                _ => bail!("parameter {name:?} expects a boolean (true or false), got {raw:?}"),
            };
            Ok((SqlParamValue::Int(i64::from(flag)), flag, None))
        }
        ParamType::Enum => {
            let value = raw.trim();
            validate_enum_value(name, spec, value)?;
            Ok((SqlParamValue::Text(value.to_owned()), true, None))
        }
        ParamType::List => {
            let items = parse_list_items(name, spec, raw)?;
            let encoded = serde_json::to_string(&items)?;
            Ok((SqlParamValue::Text(encoded), true, None))
        }
        ParamType::Time => {
            let epoch_ms = parse_time_instant_ms(name, raw, now_ms)?;
            Ok((
                SqlParamValue::Int(epoch_ms),
                true,
                Some(ParamResolution::Time {
                    input: raw.to_owned(),
                    epoch_ms,
                    now_ms,
                }),
            ))
        }
        ParamType::SessionRef => {
            let (session_id, runtime, native_id) = resolve_session_ref(store, raw)?;
            Ok((
                SqlParamValue::Int(session_id),
                true,
                Some(ParamResolution::SessionRef {
                    input: raw.to_owned(),
                    session_id,
                    runtime,
                    native_id,
                }),
            ))
        }
        ParamType::ItemRef => {
            let item_id: i64 = raw.trim().parse().map_err(|_| {
                anyhow::anyhow!("parameter {name:?} expects an integer item_id, got {raw:?}")
            })?;
            if item_id < 1 {
                bail!("parameter {name:?} expects a positive item_id, got {item_id}");
            }
            let exists: bool = store.connection().query_row(
                "SELECT EXISTS(SELECT 1 FROM items WHERE item_id = ?1)",
                params![item_id],
                |row| row.get(0),
            )?;
            if !exists {
                bail!("item_id {item_id} not found for parameter {name:?}");
            }
            Ok((SqlParamValue::Int(item_id), true, None))
        }
    }
}

fn is_fts_excluded_role(role: &str) -> bool {
    matches!(
        role,
        "agent.tool_call" | "agent.tool_call.shell" | "tool.output"
    )
}

fn validate_semantic_role_membership(name: &str, value: &str, searchable_only: bool) -> Result<()> {
    if !SEMANTIC_ROLES.iter().any(|role| role.as_str() == value) {
        let valid = SEMANTIC_ROLES
            .iter()
            .map(|role| role.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!("invalid semantic role {value:?} for parameter {name:?}; expected one of: {valid}");
    }
    if searchable_only && is_fts_excluded_role(value) {
        bail!(
            "semantic role {value:?} is not indexed in full-text search (FTS excludes agent.tool_call, agent.tool_call.shell, and tool.output); use the `shell-calls` template or query `items`/`blobs` directly"
        );
    }
    Ok(())
}

fn validate_enum_value(name: &str, spec: &ParamSpec, value: &str) -> Result<()> {
    match spec.item_type {
        Some(ListItemType::SemanticRole) => validate_semantic_role_membership(name, value, false),
        Some(ListItemType::SearchableSemanticRole) => {
            validate_semantic_role_membership(name, value, true)
        }
        Some(ListItemType::String) | None => {
            if spec.values.iter().any(|candidate| candidate == value) {
                return Ok(());
            }
            bail!(
                "invalid value {value:?} for parameter {name:?}; expected one of: {}",
                spec.values.join(", ")
            );
        }
    }
}

fn parse_list_items(name: &str, spec: &ParamSpec, raw: &str) -> Result<Vec<String>> {
    let mut items = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            bail!("list parameter {name:?} contains an empty item in {raw:?}");
        }
        match spec.item_type {
            Some(ListItemType::SemanticRole) => {
                validate_semantic_role_membership(name, trimmed, false)?;
            }
            Some(ListItemType::SearchableSemanticRole) => {
                validate_semantic_role_membership(name, trimmed, true)?;
            }
            Some(ListItemType::String) | None => {}
        }
        items.push(trimmed.to_owned());
    }
    if items.is_empty() {
        bail!("list parameter {name:?} must not be empty");
    }
    Ok(items)
}

/// Parses a `time` parameter into UTC epoch milliseconds.
///
/// Accepts raw integer `epoch_ms`, deterministic relative lookbacks
/// (`<N><s|m|h|d|w>`), or RFC 3339 timestamps with an explicit timezone offset
/// (`Z` or `±HH:MM`). Naive local dates/timestamps are rejected.
pub(crate) fn parse_time_instant_ms(name: &str, raw: &str, now_ms: i64) -> Result<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("time parameter {name:?} must not be empty");
    }
    if let Ok(epoch_ms) = trimmed.parse::<i64>() {
        return Ok(epoch_ms);
    }
    if let Some(span_ms) = parse_relative_span_ms(trimmed) {
        return now_ms
            .checked_sub(span_ms)
            .ok_or_else(|| anyhow::anyhow!("relative time {raw:?} underflows epoch milliseconds"));
    }
    if !has_explicit_timezone_offset(trimmed) {
        bail!(
            "time parameter {name:?} value {raw:?} must be a relative span (e.g. 7d, 24h, 30m), integer epoch_ms, or RFC 3339 timestamp with an explicit offset (Z or ±HH:MM)"
        );
    }
    parse_timestamp_ms(trimmed).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid RFC 3339 timestamp {raw:?} for parameter {name:?}; expected e.g. 2026-01-15T00:00:00Z or 2026-01-15T08:00:00+08:00"
        )
    })
}

fn parse_relative_span_ms(text: &str) -> Option<i64> {
    if text.len() < 2 {
        return None;
    }
    let unit = text.as_bytes()[text.len() - 1].to_ascii_lowercase();
    let count: i64 = text[..text.len() - 1].parse().ok()?;
    if count < 0 {
        return None;
    }
    let multiplier = match unit {
        b's' => 1_000_i64,
        b'm' => 60_000_i64,
        b'h' => 3_600_000_i64,
        b'd' => 86_400_000_i64,
        b'w' => 604_800_000_i64,
        _ => return None,
    };
    count.checked_mul(multiplier)
}

fn has_explicit_timezone_offset(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return false;
    }
    if matches!(bytes.last(), Some(b'Z' | b'z')) {
        return true;
    }
    bytes.len() >= 25
        && matches!(bytes[bytes.len() - 6], b'+' | b'-')
        && bytes[bytes.len() - 3] == b':'
}

fn current_epoch_ms() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[derive(Debug, Clone)]
struct SessionCandidate {
    session_id: i64,
    runtime: String,
    native_id: String,
}

fn resolve_session_ref(store: &Store, raw: &str) -> Result<(i64, String, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("session_ref must not be empty");
    }

    if let Some(id_text) = trimmed.strip_prefix("id:") {
        let id: i64 = id_text
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid numeric session id in {raw:?}"))?;
        let matches = query_sessions(
            store,
            "SELECT session_id, runtime, native_id FROM sessions WHERE session_id = ?1",
            params![id],
        )?;
        return pick_unique_session(raw, &matches);
    }

    if let Some(native) = trimmed.strip_prefix("native:") {
        let matches = query_native_or_prefix(store, None, native)?;
        return pick_unique_session(raw, &matches);
    }

    if let Some((prefix, rest)) = trimmed.split_once(':')
        && !prefix.is_empty()
        && !rest.is_empty()
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        let matches = query_native_or_prefix(store, Some(prefix), rest)?;
        if !matches.is_empty() {
            return pick_unique_session(raw, &matches);
        }
    }

    if trimmed.bytes().all(|byte| byte.is_ascii_digit())
        && let Ok(id) = trimmed.parse::<i64>()
    {
        let matches = query_sessions(
            store,
            "SELECT session_id, runtime, native_id
               FROM sessions
              WHERE session_id = ?1 OR native_id = ?2
              ORDER BY session_id",
            params![id, trimmed],
        )?;
        return pick_unique_session(raw, &matches);
    }

    let matches = query_native_or_prefix(store, None, trimmed)?;
    pick_unique_session(raw, &matches)
}

fn query_native_or_prefix(
    store: &Store,
    runtime: Option<&str>,
    native: &str,
) -> Result<Vec<SessionCandidate>> {
    let exact = if let Some(rt) = runtime {
        query_sessions(
            store,
            "SELECT session_id, runtime, native_id
               FROM sessions
              WHERE runtime = ?1 AND native_id = ?2
              ORDER BY session_id",
            params![rt, native],
        )?
    } else {
        query_sessions(
            store,
            "SELECT session_id, runtime, native_id
               FROM sessions
              WHERE native_id = ?1
              ORDER BY session_id",
            params![native],
        )?
    };
    if !exact.is_empty() || native.len() < 8 {
        return Ok(exact);
    }
    let escaped = escape_like_prefix(native);
    if let Some(rt) = runtime {
        query_sessions(
            store,
            "SELECT session_id, runtime, native_id
               FROM sessions
              WHERE runtime = ?1 AND native_id LIKE ?2 ESCAPE '\\'
              ORDER BY session_id
              LIMIT 10",
            params![rt, escaped],
        )
    } else {
        query_sessions(
            store,
            "SELECT session_id, runtime, native_id
               FROM sessions
              WHERE native_id LIKE ?1 ESCAPE '\\'
              ORDER BY session_id
              LIMIT 10",
            params![escaped],
        )
    }
}

fn escape_like_prefix(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + 4);
    for ch in prefix.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

fn query_sessions<P: rusqlite::Params>(
    store: &Store,
    sql: &str,
    sql_params: P,
) -> Result<Vec<SessionCandidate>> {
    let mut stmt = store.connection().prepare(sql)?;
    let rows = stmt.query_map(sql_params, |row| {
        Ok(SessionCandidate {
            session_id: row.get(0)?,
            runtime: row.get(1)?,
            native_id: row.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn pick_unique_session(raw: &str, matches: &[SessionCandidate]) -> Result<(i64, String, String)> {
    match matches.len() {
        0 => bail!(
            "session not found for {raw:?}; inspect available sessions with `trace-index query template run recent-sessions`"
        ),
        1 => {
            let one = &matches[0];
            Ok((one.session_id, one.runtime.clone(), one.native_id.clone()))
        }
        _ => {
            let listing = matches
                .iter()
                .take(6)
                .map(|c| format!("id:{} ({}:{})", c.session_id, c.runtime, c.native_id))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "ambiguous session_ref {raw:?} matches multiple sessions: {listing}; specify id:<n> or <runtime>:<native_id>"
            );
        }
    }
}
