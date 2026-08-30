//! Persists one affected Session into storage format 1.
//!
//! Adapters use a private projection before publication. This boundary
//! enforces the five-object model: runtime work cycles become Loops, lifecycle
//! bookkeeping does not become an Item, tool outputs remain separate Items,
//! and every retained Item stores all of its Record witnesses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension as _, Transaction, params};

use super::blob::BlobCache;
use super::db::to_sql_i64;
use crate::adapters::projection::{
    BoundedText, ItemDetail, ItemProjection, SessionProjection, SyntaxProjection,
};
use crate::adapters::semantic;
use crate::domain::{
    ByteRange, Compaction, Completeness, Context as SemanticContext, ContextCategory, Delegation,
    EvidenceStrength, Instruction, InstructionCategory, Invocation, PipelinePosition, Reasoning,
    Redirect, Semantic, SemanticRole, SemanticValue, ShellFragment, ShellStatement, ShellToolCall,
    SubagentActivity, SubagentReport, Text, ToolCall, ToolOutput,
};
use crate::indexing::telemetry::{IndexTelemetry, PersistTable};

pub(crate) fn clear_projection(transaction: &Transaction<'_>, source_id: i64) -> Result<()> {
    transaction
        .execute(
            "DELETE FROM item_search WHERE rowid IN (
             SELECT id FROM domain_items WHERE source_id = ?1)",
            [source_id],
        )
        .context("failed to clear searchable Items")?;

    // Loop Items disappear through the Loop foreign key, while Session-level
    // Items have no Loop to cascade from. Delete by Source explicitly so both
    // shapes obey the same replace-one-Source transaction.
    transaction
        .execute("DELETE FROM domain_items WHERE source_id = ?1", [source_id])
        .context("failed to clear Source Items")?;
    transaction
        .execute("DELETE FROM domain_loops WHERE source_id = ?1", [source_id])
        .context("failed to clear Source Loops")?;
    transaction
        .execute(
            "DELETE FROM session_parents WHERE source_id = ?1",
            [source_id],
        )
        .context("failed to clear Source Session parent evidence")?;

    let session_id = transaction
        .query_row(
            "SELECT session_id FROM session_sources WHERE source_id = ?1",
            [source_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .context("failed to find the Source Session")?;
    transaction
        .execute(
            "DELETE FROM session_sources WHERE source_id = ?1",
            [source_id],
        )
        .context("failed to unlink the Source from its Session")?;
    if let Some(session_id) = session_id {
        let remaining: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM session_sources WHERE session_id = ?1)",
                [session_id],
                |row| row.get(0),
            )
            .context("failed to inspect remaining Session Sources")?;
        if !remaining {
            transaction
                .execute("DELETE FROM domain_sessions WHERE id = ?1", [session_id])
                .context("failed to clear orphaned Session")?;
        }
    }
    Ok(())
}

pub(crate) struct WriteCounts {
    pub(crate) items: u64,
}

#[allow(
    clippy::too_many_arguments,
    reason = "the indexer supplies one complete publication candidate"
)]
pub(crate) fn write_session(
    transaction: &Transaction<'_>,
    source_id: i64,
    session: &SessionProjection,
    record_ids: &HashMap<u64, i64>,
    blobs: &mut BlobCache,
    telemetry: &mut IndexTelemetry,
) -> Result<WriteCounts> {
    let (session_id, runtime) =
        store_session(transaction, source_id, session, record_ids, telemetry)?;

    let mut writer = ItemWriter::new(
        transaction,
        source_id,
        session_id,
        runtime,
        record_ids,
        blobs,
        telemetry,
    );
    for projected_item in &session.items {
        writer.write(projected_item, None, None)?;
    }

    let next_session_position: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(session_position) + 1, 0)
               FROM domain_loops
              WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .context("failed to allocate temporary Loop positions")?;
    for (loop_index, projected_loop) in session.loops.iter().enumerate() {
        let source_ordinal =
            i64::try_from(loop_index).context("Loop ordinal exceeds SQLite INTEGER")?;
        let session_position = next_session_position
            .checked_add(source_ordinal)
            .context("Loop position exceeds SQLite INTEGER")?;
        let start_record_id = record_id(record_ids, projected_loop.start_seq, "Loop start")?;
        let (end_record_id, outcome) = loop_end(projected_loop, record_ids)?;
        let model = projected_loop
            .model
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let usage = projected_loop
            .usage
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let started = Instant::now();
        transaction
            .execute(
                "INSERT INTO domain_loops(
                 source_id, session_id, ordinal, session_position, native_id,
                 start_record_id, end_record_id, outcome, model, usage)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    source_id,
                    session_id,
                    source_ordinal,
                    session_position,
                    projected_loop.native_id,
                    start_record_id,
                    end_record_id,
                    outcome,
                    model,
                    usage
                ],
            )
            .context("failed to store Loop")?;
        writer
            .telemetry
            .record_persist(PersistTable::Loops, started.elapsed());
        writer.telemetry.count_loop();
        let loop_id = transaction.last_insert_rowid();

        let mut loop_position = 0_u64;
        for projected_item in &projected_loop.items {
            if writer.write(
                projected_item,
                Some(loop_id),
                Some(to_sql_i64(loop_position, "Loop position")?),
            )? {
                loop_position += 1;
            }
        }
    }
    let counts = writer.finish()?;
    normalize_loop_positions(transaction, session_id)?;
    Ok(counts)
}

/// Reorders every Loop after one Source of a multi-Source Session changes.
///
/// Runtime timestamps establish the historical order in which work occurred.
/// A Source timestamp is the fallback for a Runtime that omits a Loop-start
/// timestamp; storage identity is only the final deterministic tie-breaker.
/// The two update passes keep the unique constraint valid throughout.
fn normalize_loop_positions(transaction: &Transaction<'_>, session_id: i64) -> Result<()> {
    let loop_ids = transaction
        .prepare_cached(
            "SELECT l.id
               FROM domain_loops l
               JOIN trace_records r ON r.id = l.start_record_id
               LEFT JOIN session_sources ss ON ss.source_id = l.source_id
              WHERE l.session_id = ?1
              ORDER BY COALESCE(r.ts_ms, ss.created_at, 9223372036854775807),
                       l.source_id, l.ordinal, l.id",
        )
        .context("failed to prepare Session Loop ordering")?
        .query_map([session_id], |row| row.get::<_, i64>(0))
        .context("failed to query Session Loop ordering")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to collect Session Loop ordering")?;
    if loop_ids.is_empty() {
        return Ok(());
    }

    let maximum: i64 = transaction
        .query_row(
            "SELECT MAX(session_position) FROM domain_loops WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .context("failed to inspect Session Loop positions")?;
    let temporary_base = maximum
        .checked_add(i64::try_from(loop_ids.len()).context("Loop count exceeds SQLite INTEGER")?)
        .and_then(|value| value.checked_add(1))
        .context("Loop position exceeds SQLite INTEGER")?;
    for (position, loop_id) in loop_ids.iter().enumerate() {
        let temporary = temporary_base
            .checked_add(i64::try_from(position).context("Loop position exceeds SQLite INTEGER")?)
            .context("Loop position exceeds SQLite INTEGER")?;
        transaction
            .execute(
                "UPDATE domain_loops SET session_position = ?1 WHERE id = ?2",
                params![temporary, loop_id],
            )
            .context("failed to stage Session Loop ordering")?;
    }
    for (position, loop_id) in loop_ids.iter().enumerate() {
        transaction
            .execute(
                "UPDATE domain_loops SET session_position = ?1 WHERE id = ?2",
                params![
                    i64::try_from(position).context("Loop position exceeds SQLite INTEGER")?,
                    loop_id
                ],
            )
            .context("failed to store Session Loop ordering")?;
    }
    Ok(())
}

fn store_session(
    transaction: &Transaction<'_>,
    source_id: i64,
    session: &SessionProjection,
    record_ids: &HashMap<u64, i64>,
    telemetry: &mut IndexTelemetry,
) -> Result<(i64, String)> {
    let (runtime, source_locator): (String, String) = transaction
        .query_row(
            "SELECT adapter, path FROM trace_sources WHERE id = ?1",
            [source_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .context("failed to read Source runtime")?;
    let identity_record_id = record_id(record_ids, session.start_seq, "Session identity")?;

    let started = Instant::now();
    let session_id: i64 = transaction
        .query_row(
            "INSERT INTO domain_sessions(runtime, native_id)
         VALUES (?1, ?2)
         ON CONFLICT(runtime, native_id) DO UPDATE SET native_id = excluded.native_id
         RETURNING id",
            params![runtime, session.session_uuid],
            |row| row.get(0),
        )
        .context("failed to store Session")?;
    telemetry.record_persist(PersistTable::Sessions, started.elapsed());
    telemetry.count_session();

    transaction
        .execute(
            "INSERT INTO session_sources(
             session_id, source_id, identity_record_id, created_at, name, working_directory)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(source_id) DO UPDATE SET
             session_id = excluded.session_id,
             identity_record_id = excluded.identity_record_id,
             created_at = excluded.created_at,
             name = excluded.name,
             working_directory = excluded.working_directory",
            params![
                session_id,
                source_id,
                identity_record_id,
                session.started_at,
                session.title,
                session.cwd
            ],
        )
        .context("failed to store Session Sources")?;

    let forked_from_locator = session
        .forked_from_locator
        .as_deref()
        .map(|locator| normalize_parent_locator(&source_locator, locator));
    store_parent_candidate(
        transaction,
        session_id,
        source_id,
        "forked_from",
        session.forked_from_native_id.as_deref(),
        forked_from_locator.as_deref(),
        session.forked_from_record_seq,
        record_ids,
    )?;
    store_parent_candidate(
        transaction,
        session_id,
        source_id,
        "delegated_from",
        session.delegated_from_native_id.as_deref(),
        None,
        session.delegated_from_record_seq,
        record_ids,
    )?;
    Ok((session_id, runtime))
}

struct ItemWriter<'a, 'connection> {
    transaction: &'a Transaction<'connection>,
    source_id: i64,
    session_id: i64,
    runtime: String,
    record_ids: &'a HashMap<u64, i64>,
    blobs: &'a mut BlobCache,
    telemetry: &'a mut IndexTelemetry,
    call_items: HashMap<String, i64>,
    session_targets: HashMap<String, Option<i64>>,
    pending_outputs: Vec<(i64, String)>,
    item_total: u64,
}

impl<'a, 'connection> ItemWriter<'a, 'connection> {
    fn new(
        transaction: &'a Transaction<'connection>,
        source_id: i64,
        session_id: i64,
        runtime: String,
        record_ids: &'a HashMap<u64, i64>,
        blobs: &'a mut BlobCache,
        telemetry: &'a mut IndexTelemetry,
    ) -> Self {
        Self {
            transaction,
            source_id,
            session_id,
            runtime,
            record_ids,
            blobs,
            telemetry,
            call_items: HashMap::new(),
            session_targets: HashMap::new(),
            pending_outputs: Vec::new(),
            item_total: 0,
        }
    }

    /// Writes one selected Item. Returns false for lifecycle-only projections,
    /// which remain Records and must not consume a Loop position.
    fn write(
        &mut self,
        item: &ItemProjection,
        loop_id: Option<i64>,
        loop_position: Option<i64>,
    ) -> Result<bool> {
        if item.semantic_role == semantic::RUNTIME_LIFECYCLE {
            return Ok(false);
        }
        let evidence = item_record_ids(item, self.record_ids)?;
        let (role, mut value) = semantic_value(item)?;
        if let Some(target_native_id) = item.linked_session_native_id.as_deref() {
            let target_session_id = self.resolve_session_target(target_native_id)?;
            set_session_link(&mut value, role, target_session_id)?;
        }
        let semantic = Semantic {
            role,
            value: value.clone(),
            evidence_strength: evidence_strength(&item.basis),
        };
        crate::domain::validate_semantic(&semantic)?;
        let blob_id = semantic_blob(item, &value)
            .map(|content| self.blobs.intern(self.transaction, content, self.telemetry))
            .transpose()?;
        let stored_semantic = stored_semantic(&semantic, blob_id)?;

        let started = Instant::now();
        self.transaction
            .prepare_cached(
                "INSERT INTO domain_items(
                 source_id, session_id, loop_id, loop_position, occurred_at, semantic)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .context("failed to prepare Item insert")?
            .execute(params![
                self.source_id,
                self.session_id,
                loop_id,
                loop_position,
                item.ts_ms,
                serde_json::to_string(&stored_semantic)?,
            ])
            .context("failed to store Item")?;
        self.telemetry
            .record_persist(PersistTable::Items, started.elapsed());
        self.telemetry.count_item();
        let item_id = self.transaction.last_insert_rowid();

        for record_id in evidence {
            self.transaction
                .prepare_cached("INSERT INTO item_records(item_id, record_id) VALUES (?1, ?2)")
                .context("failed to prepare Item Record evidence insert")?
                .execute(params![item_id, record_id])
                .context("failed to store Item Record evidence")?;
        }
        self.remember_tool_link(item, item_id, &value);
        self.remember_session_link(item, item_id, role)?;
        if item.searchable
            && let Some(text) = semantic_text(&value)
        {
            let started = Instant::now();
            self.transaction
                .prepare_cached("INSERT INTO item_search(rowid, text) VALUES (?1, ?2)")
                .context("failed to prepare Item search insert")?
                .execute(params![item_id, text])
                .context("failed to store Item search text")?;
            self.telemetry
                .record_persist(PersistTable::ItemSearch, started.elapsed());
        }
        self.item_total += 1;
        Ok(true)
    }

    fn remember_tool_link(&mut self, item: &ItemProjection, item_id: i64, value: &SemanticValue) {
        match (&item.detail, value) {
            (ItemDetail::ToolCall { call_id, .. }, _) => {
                self.call_items.insert(call_id.clone(), item_id);
            }
            (ItemDetail::ToolOutput { call_id, .. }, SemanticValue::ToolOutput(_)) => {
                self.pending_outputs.push((item_id, call_id.clone()));
            }
            _ => {}
        }
    }

    fn remember_session_link(
        &mut self,
        item: &ItemProjection,
        item_id: i64,
        role: SemanticRole,
    ) -> Result<()> {
        let Some(target_native_id) = item.linked_session_native_id.as_deref() else {
            return Ok(());
        };
        ensure!(
            !target_native_id.is_empty(),
            "Item Session target cannot be empty"
        );
        let json_member = session_link_member(role)?;
        self.transaction
            .prepare_cached(
                "INSERT INTO item_session_links(
                     item_id, target_runtime, target_native_id, json_member)
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .context("failed to prepare Item Session link insert")?
            .execute(params![
                item_id,
                self.runtime,
                target_native_id,
                json_member
            ])
            .context("failed to store Item Session link evidence")?;
        Ok(())
    }

    fn resolve_session_target(&mut self, target_native_id: &str) -> Result<Option<i64>> {
        if let Some(target) = self.session_targets.get(target_native_id) {
            return Ok(*target);
        }
        let target = self
            .transaction
            .query_row(
                "SELECT id FROM domain_sessions
                  WHERE runtime = ?1 AND native_id = ?2",
                params![self.runtime, target_native_id],
                |row| row.get(0),
            )
            .optional()
            .context("failed to resolve Item Session target")?;
        self.session_targets
            .insert(target_native_id.to_owned(), target);
        Ok(target)
    }

    fn finish(self) -> Result<WriteCounts> {
        for (item_id, call_id) in self.pending_outputs {
            if let Some(call_item_id) = self.call_items.get(&call_id) {
                resolve_tool_output_link(self.transaction, item_id, *call_item_id)?;
            }
        }
        Ok(WriteCounts {
            items: self.item_total,
        })
    }
}

fn session_link_member(role: SemanticRole) -> Result<&'static str> {
    match role {
        SemanticRole::AgentDelegation => Ok("child_session_id"),
        SemanticRole::SubagentActivity => Ok("subagent_session_id"),
        SemanticRole::SubagentReport => Ok("source_session_id"),
        _ => {
            bail!("only delegation, subagent activity, and subagent report may reference a Session")
        }
    }
}

fn set_session_link(
    value: &mut SemanticValue,
    role: SemanticRole,
    target_session_id: Option<i64>,
) -> Result<()> {
    let target_session_id = target_session_id.map(crate::domain::SessionId);
    match (role, value) {
        (SemanticRole::AgentDelegation, SemanticValue::Delegation(value)) => {
            value.child_session_id = target_session_id;
        }
        (SemanticRole::SubagentActivity, SemanticValue::SubagentActivity(value)) => {
            value.subagent_session_id = target_session_id;
        }
        (SemanticRole::SubagentReport, SemanticValue::SubagentReport(value)) => {
            value.source_session_id = target_session_id;
        }
        _ => {
            let _ = session_link_member(role)?;
            bail!("Item Session link role and Semantic value do not match")
        }
    }
    Ok(())
}

fn resolve_tool_output_link(
    transaction: &Transaction<'_>,
    item_id: i64,
    call_item_id: i64,
) -> Result<()> {
    transaction
        .prepare_cached(
            "UPDATE domain_items
                SET semantic = json_set(
                    semantic, '$.value.call_item_id', ?2)
              WHERE id = ?1",
        )
        .context("failed to prepare ToolOutput link update")?
        .execute(params![item_id, call_item_id])
        .context("failed to resolve ToolOutput call Item")?;
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "a sparse Session parent attribute has one target and one witness"
)]
fn store_parent_candidate(
    transaction: &Transaction<'_>,
    session_id: i64,
    source_id: i64,
    kind: &str,
    target_native_id: Option<&str>,
    target_locator: Option<&str>,
    record_seq: Option<u64>,
    record_ids: &HashMap<u64, i64>,
) -> Result<()> {
    let Some(record_seq) = record_seq else {
        return Ok(());
    };
    ensure!(
        target_native_id.is_some() ^ target_locator.is_some(),
        "Session parent must have exactly one target"
    );
    transaction
        .execute(
            "INSERT INTO session_parents(
             session_id, source_id, kind, target_native_id, target_locator, record_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id,
                source_id,
                kind,
                target_native_id,
                target_locator,
                record_id(record_ids, record_seq, "Session parent")?
            ],
        )
        .with_context(|| format!("failed to store Session {kind}"))?;
    Ok(())
}

fn normalize_parent_locator(source_locator: &str, locator: &str) -> String {
    let locator = Path::new(locator);
    let joined = if locator.is_absolute() {
        PathBuf::from(locator)
    } else {
        Path::new(source_locator)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(locator)
    };
    joined
        .canonicalize()
        .unwrap_or(joined)
        .to_string_lossy()
        .into_owned()
}

fn record_id(record_ids: &HashMap<u64, i64>, seq: u64, fact: &str) -> Result<i64> {
    record_ids
        .get(&seq)
        .copied()
        .with_context(|| format!("{fact} Record {seq} was not persisted"))
}

fn item_record_ids(item: &ItemProjection, record_ids: &HashMap<u64, i64>) -> Result<Vec<i64>> {
    let mut ids = vec![record_id(record_ids, item.record_seq, "Item")?];
    if let Some(seq) = item.ui_seq {
        let id = record_id(record_ids, seq, "Item secondary witness")?;
        if id != ids[0] {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn loop_end(
    projected: &crate::adapters::projection::LoopProjection,
    record_ids: &HashMap<u64, i64>,
) -> Result<(Option<i64>, Option<&'static str>)> {
    let outcome = match projected.outcome {
        Some(crate::domain::LoopOutcome::Completed) => Some("completed"),
        Some(crate::domain::LoopOutcome::Interrupted) => Some("interrupted"),
        Some(crate::domain::LoopOutcome::Failed) => Some("failed"),
        None => None,
    };
    let end_record_id = projected
        .end_record_seq
        .map(|seq| record_id(record_ids, seq, "Loop end"))
        .transpose()?;
    ensure!(
        outcome.is_none() || end_record_id.is_some(),
        "Loop outcome requires end evidence"
    );
    Ok((end_record_id, outcome))
}

fn preview(item: &ItemProjection) -> Option<crate::domain::TextContent> {
    item.preview.as_ref().and_then(BoundedText::content)
}

fn semantic_value(item: &ItemProjection) -> Result<(SemanticRole, SemanticValue)> {
    let role = normalized_role(&item.semantic_role, &item.detail)?;
    let text = preview(item);
    let has_images = matches!(
        &item.detail,
        ItemDetail::Message {
            has_images: true,
            ..
        }
    );
    let value = match role {
        SemanticRole::AgentReasoning => {
            let ItemDetail::Reasoning { representation } = &item.detail else {
                unreachable!("reasoning role must carry reasoning detail")
            };
            SemanticValue::Reasoning(Reasoning {
                representation: *representation,
                text,
            })
        }
        SemanticRole::AgentToolCall | SemanticRole::AgentToolCallShell => {
            let ItemDetail::ToolCall {
                name,
                workdir,
                args,
                syntax,
                ..
            } = &item.detail
            else {
                unreachable!()
            };
            let tool_call = ToolCall {
                tool_name: name.clone(),
                arguments: args.as_ref().and_then(semantic_arguments),
                working_directory: workdir.clone(),
            };
            if role == SemanticRole::AgentToolCallShell {
                SemanticValue::ShellToolCall(ShellToolCall {
                    tool_call,
                    shell_fragments: shell_fragments(syntax.as_ref())?,
                })
            } else {
                SemanticValue::ToolCall(tool_call)
            }
        }
        SemanticRole::ToolOutput => {
            let ItemDetail::ToolOutput { output, facts, .. } = &item.detail else {
                unreachable!()
            };
            SemanticValue::ToolOutput(ToolOutput {
                call_item_id: None,
                text: output.as_ref().and_then(BoundedText::content),
                exit_code: facts.exit_code,
                failed: facts.nonzero_exit,
                duration_ms: facts.duration_ms,
                runtime_truncated: facts.truncated,
                runtime_output_tokens: facts.output_tokens,
            })
        }
        SemanticRole::AgentDelegation => SemanticValue::Delegation(Delegation {
            text,
            has_images,
            child_session_id: None,
        }),
        SemanticRole::SubagentActivity => SemanticValue::SubagentActivity(SubagentActivity {
            text,
            has_images,
            subagent_session_id: None,
        }),
        SemanticRole::SubagentReport => SemanticValue::SubagentReport(SubagentReport {
            text,
            has_images,
            source_session_id: None,
        }),
        SemanticRole::RuntimeCompactionSummary => {
            SemanticValue::Compaction(Compaction { summary: text })
        }
        SemanticRole::RuntimeInstructions => SemanticValue::Instruction(Instruction {
            text,
            category: instruction_category(&item.semantic_role),
        }),
        SemanticRole::RuntimeContext => SemanticValue::Context(SemanticContext {
            text,
            category: context_category(&item.semantic_role),
            has_images,
        }),
        _ => SemanticValue::Text(Text { text, has_images }),
    };
    Ok((role, value))
}

/// Publishes complete structured arguments as JSON and complete scalar tool
/// input as a JSON string. Some Runtime tools, notably Codex web search, record
/// a plain string rather than a JSON object. A bounded prefix is not published
/// as though it were the complete argument; the supporting Record remains the
/// recovery path in that case.
fn semantic_arguments(arguments: &BoundedText) -> Option<serde_json::Value> {
    let text = arguments.text.as_deref()?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) != arguments.full_bytes {
        return None;
    }
    Some(serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_owned())))
}

fn normalized_role(role: &str, detail: &ItemDetail) -> Result<SemanticRole> {
    let role = match role {
        semantic::HUMAN_REQUEST => SemanticRole::HumanRequest,
        semantic::HUMAN_STEERING => SemanticRole::HumanSteering,
        semantic::AGENT_COMMENTARY => SemanticRole::AgentCommentary,
        semantic::AGENT_FINAL_ANSWER => SemanticRole::AgentFinalAnswer,
        semantic::AGENT_REASONING => SemanticRole::AgentReasoning,
        semantic::AGENT_TOOL_CALL
            if matches!(
                detail,
                ItemDetail::ToolCall {
                    syntax: Some(_),
                    ..
                }
            ) =>
        {
            SemanticRole::AgentToolCallShell
        }
        semantic::AGENT_TOOL_CALL => SemanticRole::AgentToolCall,
        semantic::AGENT_DELEGATION => SemanticRole::AgentDelegation,
        semantic::TOOL_OUTPUT => SemanticRole::ToolOutput,
        semantic::SUBAGENT_ACTIVITY => SemanticRole::SubagentActivity,
        semantic::SUBAGENT_REPORT => SemanticRole::SubagentReport,
        semantic::RUNTIME_COMPACTION => SemanticRole::RuntimeCompactionSummary,
        semantic::RUNTIME_NOTICE
        | semantic::RUNTIME_ABORT_NOTICE
        | semantic::RUNTIME_HOOK_OUTPUT => SemanticRole::RuntimeNotice,
        semantic::RUNTIME_STATE | semantic::RUNTIME_BUDGET | semantic::RUNTIME_FILE_CHANGE => {
            SemanticRole::RuntimeState
        }
        semantic::RUNTIME_PROJECT_INSTRUCTIONS
        | semantic::RUNTIME_USER_INSTRUCTIONS
        | semantic::RUNTIME_SKILL_INSTRUCTIONS
        | semantic::RUNTIME_PERMISSIONS
        | semantic::RUNTIME_COLLAB_MODE
        | semantic::RUNTIME_PLUGINS
        | semantic::RUNTIME_APPS
        | semantic::RUNTIME_PERSONALITY
        | semantic::RUNTIME_TOOL_CATALOG => SemanticRole::RuntimeInstructions,
        semantic::RUNTIME_SESSION_REFERENCE
        | semantic::RUNTIME_MEMORY
        | semantic::RUNTIME_ENV_CONTEXT
        | semantic::RUNTIME_INTERNAL_CONTEXT
        | semantic::RUNTIME_SLASH_COMMAND
        | semantic::RUNTIME_BASH_COMMAND
        | semantic::RUNTIME_IMAGE_ATTACHMENT
        | semantic::RUNTIME_IDE_CONTEXT
        | semantic::RUNTIME_FILE_CONTEXT => SemanticRole::RuntimeContext,
        semantic::RUNTIME_UNKNOWN => SemanticRole::RuntimeUnknown,
        _ => bail!("Adapter emitted unmapped semantic role {role:?}"),
    };
    Ok(role)
}

fn instruction_category(role: &str) -> Option<InstructionCategory> {
    match role {
        semantic::RUNTIME_PROJECT_INSTRUCTIONS => Some(InstructionCategory::Project),
        semantic::RUNTIME_USER_INSTRUCTIONS => Some(InstructionCategory::User),
        semantic::RUNTIME_SKILL_INSTRUCTIONS => Some(InstructionCategory::Skill),
        semantic::RUNTIME_PERMISSIONS => Some(InstructionCategory::Permission),
        semantic::RUNTIME_COLLAB_MODE => Some(InstructionCategory::Collaboration),
        semantic::RUNTIME_PLUGINS => Some(InstructionCategory::Plugin),
        semantic::RUNTIME_TOOL_CATALOG => Some(InstructionCategory::ToolCatalog),
        _ => None,
    }
}

fn context_category(role: &str) -> Option<ContextCategory> {
    match role {
        semantic::RUNTIME_ENV_CONTEXT => Some(ContextCategory::Environment),
        semantic::RUNTIME_MEMORY => Some(ContextCategory::Memory),
        semantic::RUNTIME_FILE_CONTEXT => Some(ContextCategory::File),
        semantic::RUNTIME_SESSION_REFERENCE => Some(ContextCategory::SessionReference),
        semantic::RUNTIME_INTERNAL_CONTEXT
        | semantic::RUNTIME_SLASH_COMMAND
        | semantic::RUNTIME_BASH_COMMAND => Some(ContextCategory::Internal),
        _ => None,
    }
}

fn evidence_strength(basis: &str) -> EvidenceStrength {
    if basis.split('+').any(semantic::basis_is_heuristic) {
        EvidenceStrength::Heuristic
    } else {
        EvidenceStrength::Structural
    }
}

fn semantic_text(value: &SemanticValue) -> Option<&str> {
    match value {
        SemanticValue::Text(value) => value.text.as_ref().map(|text| text.value.as_str()),
        SemanticValue::Reasoning(value) => value.text.as_ref().map(|text| text.value.as_str()),
        SemanticValue::Delegation(value) => value.text.as_ref().map(|text| text.value.as_str()),
        SemanticValue::SubagentActivity(value) => {
            value.text.as_ref().map(|text| text.value.as_str())
        }
        SemanticValue::SubagentReport(value) => value.text.as_ref().map(|text| text.value.as_str()),
        SemanticValue::Compaction(value) => value.summary.as_ref().map(|text| text.value.as_str()),
        SemanticValue::Instruction(value) => value.text.as_ref().map(|text| text.value.as_str()),
        SemanticValue::Context(value) => value.text.as_ref().map(|text| text.value.as_str()),
        _ => None,
    }
}

/// Selects the Adapter text that backs the public Semantic `TextContent`.
///
/// A Tool Output keeps its text in the typed detail rather than the general
/// Item preview. Every other current text-bearing Semantic value is built from
/// `preview`. Tool arguments and parsed shell structure remain ordinary typed
/// Semantic data; Blob is not a generic JSON externalization mechanism.
fn semantic_blob<'a>(item: &'a ItemProjection, value: &SemanticValue) -> Option<&'a BoundedText> {
    match (&item.detail, value) {
        (ItemDetail::ToolOutput { output, .. }, SemanticValue::ToolOutput(value))
            if value.text.is_some() =>
        {
            output.as_ref()
        }
        (_, value) if semantic_text(value).is_some() => item.preview.as_ref(),
        _ => None,
    }
}

/// Produces the complete SQL Semantic encoding stored with an Item.
///
/// The domain Semantic uses `TextContent`; its stable SQL representation uses
/// `{blob_id}` for top-level bounded text. The public Blob relation resolves
/// that reference without forcing every Item query to join and rebuild JSON.
fn stored_semantic(semantic: &Semantic, blob_id: Option<i64>) -> Result<serde_json::Value> {
    let value = serde_json::to_value(&semantic.value)?;
    let mut stored = serde_json::json!({
        "role": semantic.role.as_str(),
        "value": value,
        "evidence_strength": semantic.evidence_strength.as_str(),
    });
    let text_member = match &semantic.value {
        SemanticValue::Text(_)
        | SemanticValue::Reasoning(_)
        | SemanticValue::ToolOutput(_)
        | SemanticValue::Delegation(_)
        | SemanticValue::SubagentActivity(_)
        | SemanticValue::SubagentReport(_)
        | SemanticValue::Instruction(_)
        | SemanticValue::Context(_) => Some("text"),
        SemanticValue::Compaction(_) => Some("summary"),
        SemanticValue::ToolCall(_) | SemanticValue::ShellToolCall(_) => None,
    };

    if let Some(blob_id) = blob_id {
        let member = text_member.context("text-bearing Semantic value has no text member")?;
        let value = stored
            .get_mut("value")
            .and_then(serde_json::Value::as_object_mut)
            .context("Semantic value is not an object")?;
        let replaced = value.insert(member.to_owned(), serde_json::json!({ "blob_id": blob_id }));
        ensure!(
            replaced.is_some(),
            "text-bearing Semantic value was not externalized"
        );
    } else if let Some(member) = text_member {
        let has_inline_text = stored
            .get("value")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|value| value.contains_key(member));
        ensure!(
            !has_inline_text,
            "text-bearing Semantic value was stored without a Blob"
        );
    }
    Ok(stored)
}

fn shell_fragments(syntax: Option<&SyntaxProjection>) -> Result<Vec<ShellFragment>> {
    let Some(syntax) = syntax else {
        return Ok(Vec::new());
    };
    let mut fragments = Vec::new();
    for (fragment_index, fragment) in syntax.fragments.iter().enumerate() {
        if fragment.parent.is_some() {
            continue;
        }
        let mut statements = Vec::new();
        for (statement_index, statement) in syntax.statements.iter().enumerate() {
            if statement.fragment != fragment_index {
                continue;
            }
            let invocations = syntax
                .invocations
                .iter()
                .filter(|value| value.statement == statement_index)
                .map(|value| Invocation {
                    program: value.program.clone(),
                    argv: serde_json::from_str(&value.argv).unwrap_or_default(),
                })
                .collect();
            let redirects = syntax
                .redirects
                .iter()
                .filter(|value| value.statement == statement_index)
                .map(|value| Redirect {
                    source_fd: value.source_fd_raw.clone(),
                    operator: value.operator.clone(),
                    target: value.target_raw.clone(),
                    range: ByteRange {
                        start: u64::from(value.start_byte),
                        end: u64::from(value.end_byte),
                    },
                })
                .collect();
            statements.push(ShellStatement {
                range: ByteRange {
                    start: u64::from(statement.start_byte),
                    end: u64::from(statement.end_byte),
                },
                parent_position: statement
                    .parent
                    .map(|value| u64::try_from(value).unwrap_or(u64::MAX)),
                connector: statement.shell.as_ref().and_then(|value| {
                    (value.connector != "first").then(|| value.connector.to_owned())
                }),
                pipeline: statement
                    .shell
                    .as_ref()
                    .and_then(|value| value.pipeline_id.zip(value.pipeline_pos))
                    .map(|(id, position)| PipelinePosition {
                        id: u64::from(id),
                        position: u64::from(position),
                    }),
                invocations,
                redirects,
            });
        }
        fragments.push(ShellFragment {
            text: fragment
                .content
                .content()
                .context("Shell Fragment must retain bounded text")?,
            completeness: if fragment.parse_status == "parsed" {
                Completeness::Complete
            } else {
                Completeness::Partial
            },
            statements,
        });
    }
    ensure!(
        !fragments.is_empty(),
        "declared Shell call produced no root fragment"
    );
    Ok(fragments)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{resolve_tool_output_link, stored_semantic};
    use crate::domain::{
        Completeness, EvidenceStrength, Semantic, SemanticRole, SemanticValue, ShellFragment,
        ShellToolCall, Text, TextContent, ToolCall, ToolOutput,
    };

    #[test]
    fn text_capable_value_without_text_does_not_require_a_blob() {
        let semantic = Semantic {
            role: SemanticRole::RuntimeNotice,
            value: SemanticValue::Text(Text {
                text: None,
                has_images: true,
            }),
            evidence_strength: EvidenceStrength::Structural,
        };

        let stored = stored_semantic(&semantic, None).expect("store image-only Semantic");

        assert_eq!(stored["role"], "runtime.notice");
        assert_eq!(stored["value"]["has_images"], true);
        assert!(stored["value"].get("text").is_none());
    }

    #[test]
    fn tool_output_text_is_replaced_by_one_blob_reference() {
        let semantic = Semantic {
            role: SemanticRole::ToolOutput,
            value: SemanticValue::ToolOutput(ToolOutput {
                call_item_id: None,
                text: Some(TextContent {
                    value: "large output".to_owned(),
                    full_bytes: 12,
                    estimated_tokens: 3,
                }),
                exit_code: None,
                failed: None,
                duration_ms: None,
                runtime_truncated: None,
                runtime_output_tokens: None,
            }),
            evidence_strength: EvidenceStrength::Structural,
        };

        let stored = stored_semantic(&semantic, Some(42)).expect("store ToolOutput Semantic");

        assert_eq!(
            stored["value"]["text"],
            serde_json::json!({ "blob_id": 42 })
        );
        assert!(!stored.to_string().contains("large output"));
    }

    #[test]
    fn shell_fragment_text_remains_inline() {
        let semantic = Semantic {
            role: SemanticRole::AgentToolCallShell,
            value: SemanticValue::ShellToolCall(ShellToolCall {
                tool_call: ToolCall {
                    tool_name: Some("exec_command".to_owned()),
                    arguments: None,
                    working_directory: None,
                },
                shell_fragments: vec![ShellFragment {
                    text: TextContent {
                        value: "echo hello".to_owned(),
                        full_bytes: 10,
                        estimated_tokens: 3,
                    },
                    completeness: Completeness::Complete,
                    statements: Vec::new(),
                }],
            }),
            evidence_strength: EvidenceStrength::Structural,
        };

        let stored = stored_semantic(&semantic, None).expect("store Shell Semantic");

        assert_eq!(
            stored["value"]["shell_fragments"][0]["text"]["value"],
            "echo hello"
        );
        assert!(
            stored["value"]["shell_fragments"][0]["text"]
                .get("blob_id")
                .is_none()
        );
    }

    #[test]
    fn tool_output_link_update_preserves_the_blob_reference() {
        let mut connection = Connection::open_in_memory().expect("open database");
        connection
            .execute_batch(
                "CREATE TABLE domain_items(
                     id INTEGER PRIMARY KEY,
                     semantic TEXT NOT NULL CHECK(json_valid(semantic))
                 );
                 INSERT INTO domain_items(id, semantic) VALUES (
                     7,
                     '{\"role\":\"tool.output\",\"value\":{\"text\":{\"blob_id\":42}},\"evidence_strength\":\"structural\"}'
                 );",
            )
            .expect("create Item fixture");
        let transaction = connection.transaction().expect("begin transaction");

        resolve_tool_output_link(&transaction, 7, 3).expect("resolve ToolOutput call");
        let semantic: String = transaction
            .query_row(
                "SELECT semantic FROM domain_items WHERE id = 7",
                [],
                |row| row.get(0),
            )
            .expect("read linked Semantic");
        let semantic: serde_json::Value = serde_json::from_str(&semantic).expect("parse Semantic");

        assert_eq!(semantic["value"]["call_item_id"], 3);
        assert_eq!(semantic["value"]["text"]["blob_id"], 42);
    }
}
