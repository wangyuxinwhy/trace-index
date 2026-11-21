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
use serde::Serialize;

use super::blob::BlobCache;
use super::db::to_sql_i64;
use crate::adapters::projection::{
    BoundedText, ItemDetail, ItemProjection, SessionProjection, SyntaxProjection,
};
use crate::adapters::semantic;
use crate::domain::{
    ByteRange, Compaction, Completeness, Context as SemanticContext, ContextCategory, Delegation,
    EvidenceStrength, Instruction, InstructionCategory, Invocation, ItemId, PipelinePosition,
    Reasoning, Redirect, Semantic, SemanticRole, SemanticValue, SessionId, ShellFragment,
    ShellStatement, ShellToolCall, SubagentActivity, SubagentReport, Text, ToolCall, ToolOutput,
};
use crate::indexing::telemetry::{IndexTelemetry, PersistTable};

/// Public SQL access encoding for top-level bounded Semantic text.
///
/// The domain model carries `TextContent`; persistence stores that text in
/// `content_blobs` and serializes `{ "blob_id": ... }` directly inside
/// `Semantic<BlobRef>` without constructing an intermediate JSON DOM tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct BlobRef {
    blob_id: i64,
}

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
            .prepare_cached(
                "INSERT INTO domain_loops(
                 source_id, session_id, ordinal, session_position, native_id,
                 start_record_id, end_record_id, outcome, model, usage)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )
            .context("failed to prepare Loop insert")?
            .execute(params![
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
            ])
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
    let loops = transaction
        .prepare_cached(
            "SELECT l.id, l.session_position
               FROM domain_loops l
               JOIN trace_records r ON r.id = l.start_record_id
               LEFT JOIN session_sources ss ON ss.source_id = l.source_id
              WHERE l.session_id = ?1
              ORDER BY COALESCE(r.ts_ms, ss.created_at, 9223372036854775807),
                       l.source_id, l.ordinal, l.id",
        )
        .context("failed to prepare Session Loop ordering")?
        .query_map([session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .context("failed to query Session Loop ordering")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to collect Session Loop ordering")?;
    if loops.is_empty()
        || loops
            .iter()
            .enumerate()
            .all(|(position, (_, current))| i64::try_from(position).ok() == Some(*current))
    {
        return Ok(());
    }

    let maximum = loops
        .iter()
        .map(|(_, current)| *current)
        .max()
        .expect("loops is non-empty");
    let temporary_base = maximum
        .checked_add(i64::try_from(loops.len()).context("Loop count exceeds SQLite INTEGER")?)
        .and_then(|value| value.checked_add(1))
        .context("Loop position exceeds SQLite INTEGER")?;
    let mut update = transaction
        .prepare_cached("UPDATE domain_loops SET session_position = ?1 WHERE id = ?2")
        .context("failed to prepare Session Loop ordering update")?;
    for (position, (loop_id, _)) in loops.iter().enumerate() {
        let temporary = temporary_base
            .checked_add(i64::try_from(position).context("Loop position exceeds SQLite INTEGER")?)
            .context("Loop position exceeds SQLite INTEGER")?;
        update
            .execute(params![temporary, loop_id])
            .context("failed to stage Session Loop ordering")?;
    }
    for (position, (loop_id, _)) in loops.iter().enumerate() {
        update
            .execute(params![
                i64::try_from(position).context("Loop position exceeds SQLite INTEGER")?,
                loop_id
            ])
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
    json_buffer: Vec<u8>,
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
            json_buffer: Vec::with_capacity(512),
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
        let (primary_record_id, secondary_record_id) = item_record_ids(item, self.record_ids)?;
        let role = normalized_role(&item.semantic_role, &item.detail)?;
        let target_session_id =
            if let Some(target_native_id) = item.linked_session_native_id.as_deref() {
                let _ = session_link_member(role)?;
                self.resolve_session_target(target_native_id)?
                    .map(SessionId)
            } else {
                None
            };
        let (inline_call_item_id, pending_call_id) = match (&item.detail, role) {
            (ItemDetail::ToolOutput { call_id, .. }, SemanticRole::ToolOutput) => {
                if let Some(&call_item_id) = self.call_items.get(call_id) {
                    (Some(ItemId(call_item_id)), None)
                } else {
                    (None, Some(call_id.as_str()))
                }
            }
            _ => (None, None),
        };
        let (blob_source, searchable_text) = item_text_sources(item, role);
        let has_nonempty_text = blob_source
            .and_then(|content| content.text.as_deref())
            .is_some_and(|text| !text.is_empty());
        let blob_ref = blob_source
            .map(|content| {
                self.blobs
                    .intern(self.transaction, content, self.telemetry)
                    .map(|blob_id| BlobRef { blob_id })
            })
            .transpose()?;
        let stored_semantic = Semantic {
            role,
            value: stored_semantic_value(
                item,
                role,
                blob_ref,
                target_session_id,
                inline_call_item_id,
            )?,
            evidence_strength: evidence_strength(&item.basis),
        };
        crate::domain::validate_semantic_with_text(&stored_semantic, has_nonempty_text)?;

        self.json_buffer.clear();
        serde_json::to_writer(&mut self.json_buffer, &stored_semantic)?;
        let semantic_json = std::str::from_utf8(&self.json_buffer)?;

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
                semantic_json,
            ])
            .context("failed to store Item")?;
        self.telemetry
            .record_persist(PersistTable::Items, started.elapsed());
        self.telemetry.count_item();
        let item_id = self.transaction.last_insert_rowid();

        {
            let mut insert_evidence = self
                .transaction
                .prepare_cached("INSERT INTO item_records(item_id, record_id) VALUES (?1, ?2)")
                .context("failed to prepare Item Record evidence insert")?;
            insert_evidence
                .execute(params![item_id, primary_record_id])
                .context("failed to store Item Record evidence")?;
            if let Some(record_id) = secondary_record_id {
                insert_evidence
                    .execute(params![item_id, record_id])
                    .context("failed to store Item Record evidence")?;
            }
        }
        self.remember_tool_link(item, item_id, pending_call_id);
        self.remember_session_link(item, item_id, role)?;
        if item.searchable
            && let Some(text) = searchable_text
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

    fn remember_tool_link(
        &mut self,
        item: &ItemProjection,
        item_id: i64,
        pending_call_id: Option<&str>,
    ) {
        if let ItemDetail::ToolCall { call_id, .. } = &item.detail {
            self.call_items.insert(call_id.clone(), item_id);
        } else if let Some(call_id) = pending_call_id {
            self.pending_outputs.push((item_id, call_id.to_owned()));
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

fn item_record_ids(
    item: &ItemProjection,
    record_ids: &HashMap<u64, i64>,
) -> Result<(i64, Option<i64>)> {
    let primary = record_id(record_ids, item.record_seq, "Item")?;
    let secondary = match item.ui_seq {
        Some(seq) => {
            let id = record_id(record_ids, seq, "Item secondary witness")?;
            (id != primary).then_some(id)
        }
        None => None,
    };
    Ok((primary, secondary))
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

/// Selects the Adapter text that backs the public Semantic `TextContent` and
/// optional full-text search entry without cloning the bounded string.
///
/// A Tool Output keeps its text in the typed detail rather than the general
/// Item preview and is not indexed into conversation search. Every other
/// text-bearing Semantic value is built from `preview`. Tool arguments and
/// parsed shell structure remain ordinary typed Semantic data; Blob is not a
/// generic JSON externalization mechanism.
fn item_text_sources(
    item: &ItemProjection,
    role: SemanticRole,
) -> (Option<&BoundedText>, Option<&str>) {
    match role {
        SemanticRole::AgentToolCall | SemanticRole::AgentToolCallShell => (None, None),
        SemanticRole::ToolOutput => {
            let ItemDetail::ToolOutput { output, .. } = &item.detail else {
                unreachable!("tool output role must carry tool output detail")
            };
            let blob = output.as_ref().filter(|content| content.text.is_some());
            (blob, None)
        }
        _ => {
            let blob = item
                .preview
                .as_ref()
                .filter(|content| content.text.is_some());
            let searchable_text = blob.and_then(|content| content.text.as_deref());
            (blob, searchable_text)
        }
    }
}

fn stored_semantic_value(
    item: &ItemProjection,
    role: SemanticRole,
    blob: Option<BlobRef>,
    target_session_id: Option<SessionId>,
    call_item_id: Option<ItemId>,
) -> Result<SemanticValue<BlobRef>> {
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
                text: blob,
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
            let ItemDetail::ToolOutput { facts, .. } = &item.detail else {
                unreachable!()
            };
            SemanticValue::ToolOutput(ToolOutput {
                call_item_id,
                text: blob,
                exit_code: facts.exit_code,
                failed: facts.nonzero_exit,
                duration_ms: facts.duration_ms,
                runtime_truncated: facts.truncated,
                runtime_output_tokens: facts.output_tokens,
            })
        }
        SemanticRole::AgentDelegation => SemanticValue::Delegation(Delegation {
            text: blob,
            has_images,
            child_session_id: target_session_id,
        }),
        SemanticRole::SubagentActivity => SemanticValue::SubagentActivity(SubagentActivity {
            text: blob,
            has_images,
            subagent_session_id: target_session_id,
        }),
        SemanticRole::SubagentReport => SemanticValue::SubagentReport(SubagentReport {
            text: blob,
            has_images,
            source_session_id: target_session_id,
        }),
        SemanticRole::RuntimeCompactionSummary => {
            SemanticValue::Compaction(Compaction { summary: blob })
        }
        SemanticRole::RuntimeInstructions => SemanticValue::Instruction(Instruction {
            text: blob,
            category: instruction_category(&item.semantic_role),
        }),
        SemanticRole::RuntimeContext => SemanticValue::Context(SemanticContext {
            text: blob,
            category: context_category(&item.semantic_role),
            has_images,
        }),
        _ => SemanticValue::Text(Text {
            text: blob,
            has_images,
        }),
    };
    Ok(value)
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
        semantic::RUNTIME_INSTRUCTIONS
        | semantic::RUNTIME_PROJECT_INSTRUCTIONS
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

/// Converts a domain `Semantic<TextContent>` into its SQL `Semantic<BlobRef>`
/// access encoding for contract-equivalence tests.
#[cfg(test)]
fn stored_semantic(semantic: &Semantic, blob_id: Option<i64>) -> Result<serde_json::Value> {
    let blob_ref = |has_text: bool| -> Result<Option<BlobRef>> {
        match (has_text, blob_id) {
            (true, Some(blob_id)) => Ok(Some(BlobRef { blob_id })),
            (false, None) => Ok(None),
            (true, None) => bail!("text-bearing Semantic value was stored without a Blob"),
            (false, Some(_)) => bail!("text-bearing Semantic value was not externalized"),
        }
    };
    let value = match &semantic.value {
        SemanticValue::Text(value) => SemanticValue::Text(Text {
            text: blob_ref(value.text.is_some())?,
            has_images: value.has_images,
        }),
        SemanticValue::Reasoning(value) => SemanticValue::Reasoning(Reasoning {
            representation: value.representation,
            text: blob_ref(value.text.is_some())?,
        }),
        SemanticValue::ToolCall(value) => {
            ensure!(
                blob_id.is_none(),
                "text-bearing Semantic value has no text member"
            );
            SemanticValue::ToolCall(value.clone())
        }
        SemanticValue::ShellToolCall(value) => {
            ensure!(
                blob_id.is_none(),
                "text-bearing Semantic value has no text member"
            );
            SemanticValue::ShellToolCall(value.clone())
        }
        SemanticValue::ToolOutput(value) => SemanticValue::ToolOutput(ToolOutput {
            call_item_id: value.call_item_id,
            text: blob_ref(value.text.is_some())?,
            exit_code: value.exit_code,
            failed: value.failed,
            duration_ms: value.duration_ms,
            runtime_truncated: value.runtime_truncated,
            runtime_output_tokens: value.runtime_output_tokens,
        }),
        SemanticValue::Delegation(value) => SemanticValue::Delegation(Delegation {
            text: blob_ref(value.text.is_some())?,
            has_images: value.has_images,
            child_session_id: value.child_session_id,
        }),
        SemanticValue::SubagentActivity(value) => {
            SemanticValue::SubagentActivity(SubagentActivity {
                text: blob_ref(value.text.is_some())?,
                has_images: value.has_images,
                subagent_session_id: value.subagent_session_id,
            })
        }
        SemanticValue::SubagentReport(value) => SemanticValue::SubagentReport(SubagentReport {
            text: blob_ref(value.text.is_some())?,
            has_images: value.has_images,
            source_session_id: value.source_session_id,
        }),
        SemanticValue::Compaction(value) => SemanticValue::Compaction(Compaction {
            summary: blob_ref(value.summary.is_some())?,
        }),
        SemanticValue::Instruction(value) => SemanticValue::Instruction(Instruction {
            text: blob_ref(value.text.is_some())?,
            category: value.category,
        }),
        SemanticValue::Context(value) => SemanticValue::Context(SemanticContext {
            text: blob_ref(value.text.is_some())?,
            category: value.category,
            has_images: value.has_images,
        }),
    };
    Ok(serde_json::to_value(Semantic {
        role: semantic.role,
        value,
        evidence_strength: semantic.evidence_strength,
    })?)
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

    fn test_tool_call(seq: u64, call_id: &str) -> crate::adapters::projection::ItemProjection {
        crate::adapters::projection::ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms: Some(1_000 + i64::try_from(seq).expect("seq")),
            semantic_role: crate::adapters::semantic::AGENT_TOOL_CALL.to_owned(),
            basis: crate::adapters::semantic::basis::RECORD_KIND.to_owned(),
            preview: None,
            detail: crate::adapters::projection::ItemDetail::ToolCall {
                call_id: call_id.to_owned(),
                name: Some("read".to_owned()),
                cmd: None,
                cmd_lang: None,
                workdir: None,
                args: None,
                syntax: None,
            },
            linked_session_native_id: None,
            searchable: false,
        }
    }

    fn test_tool_output(seq: u64, call_id: &str) -> crate::adapters::projection::ItemProjection {
        crate::adapters::projection::ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms: Some(1_000 + i64::try_from(seq).expect("seq")),
            semantic_role: crate::adapters::semantic::TOOL_OUTPUT.to_owned(),
            basis: crate::adapters::semantic::basis::RECORD_KIND.to_owned(),
            preview: None,
            detail: crate::adapters::projection::ItemDetail::ToolOutput {
                call_id: call_id.to_owned(),
                output: Some(crate::adapters::projection::BoundedText::bounded("ok", 64)),
                facts: crate::adapters::projection::ToolOutputFacts::default(),
            },
            linked_session_native_id: None,
            searchable: false,
        }
    }

    #[test]
    fn tool_output_links_inline_when_call_precedes_output_and_defers_when_out_of_order() {
        use std::collections::HashMap;
        use tempfile::tempdir;

        use super::ItemWriter;
        use crate::indexing::telemetry::IndexTelemetry;
        use crate::storage::blob::BlobCache;
        use crate::storage::db::Store;

        let dir = tempdir().expect("temp dir");
        let mut store = Store::open(&dir.path().join("index.sqlite")).expect("open store");
        let tx = store.connection_mut().transaction().expect("begin tx");
        tx.execute(
            "INSERT INTO trace_sources(adapter, path) VALUES ('codex', '/tmp/t.jsonl')",
            [],
        )
        .expect("insert source");
        let source_id = tx.last_insert_rowid();
        let mut record_ids = HashMap::new();
        for seq in 0_u64..4 {
            tx.execute(
                "INSERT INTO trace_records(source_id, seq, byte_offset, byte_length, raw_hash, native_type, parse_status)
                 VALUES (?1, ?2, ?2, 1, 'h', 't', 'ok')",
                rusqlite::params![source_id, i64::try_from(seq).expect("seq")],
            )
            .expect("insert record");
            record_ids.insert(seq, tx.last_insert_rowid());
        }
        tx.execute(
            "INSERT INTO domain_sessions(runtime, native_id) VALUES ('codex', 's1')",
            [],
        )
        .expect("insert session");
        let session_id = tx.last_insert_rowid();

        let mut blobs = BlobCache::default();
        let mut telemetry = IndexTelemetry::new();
        let mut writer = ItemWriter::new(
            &tx,
            source_id,
            session_id,
            "codex".to_owned(),
            &record_ids,
            &mut blobs,
            &mut telemetry,
        );
        // In-order pair: call (seq 0) before output (seq 1)
        writer
            .write(&test_tool_call(0, "c-inline"), None, None)
            .expect("write call");
        writer
            .write(&test_tool_output(1, "c-inline"), None, None)
            .expect("write output");
        assert!(
            writer.pending_outputs.is_empty(),
            "in-order tool output should resolve inline without deferred UPDATE"
        );
        // Out-of-order pair: output (seq 2) before call (seq 3)
        writer
            .write(&test_tool_output(2, "c-deferred"), None, None)
            .expect("write early output");
        assert_eq!(writer.pending_outputs.len(), 1);
        writer
            .write(&test_tool_call(3, "c-deferred"), None, None)
            .expect("write late call");
        writer.finish().expect("finish writer");

        let rows: Vec<(i64, String)> = tx
            .prepare("SELECT id, semantic FROM domain_items ORDER BY id")
            .expect("prepare")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        assert_eq!(rows.len(), 4);
        let inline_out: serde_json::Value = serde_json::from_str(&rows[1].1).expect("json");
        assert_eq!(inline_out["value"]["call_item_id"], rows[0].0);
        let deferred_out: serde_json::Value = serde_json::from_str(&rows[2].1).expect("json");
        assert_eq!(deferred_out["value"]["call_item_id"], rows[3].0);
    }

    #[test]
    fn normalize_loop_positions_skips_updates_when_already_ordered() {
        use tempfile::tempdir;

        use super::normalize_loop_positions;
        use crate::storage::db::Store;

        let dir = tempdir().expect("temp dir");
        let mut store = Store::open(&dir.path().join("index.sqlite")).expect("open store");
        let tx = store.connection_mut().transaction().expect("begin tx");
        tx.execute(
            "INSERT INTO trace_sources(adapter, path) VALUES ('codex', '/tmp/t.jsonl')",
            [],
        )
        .expect("insert source");
        let source_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO domain_sessions(runtime, native_id) VALUES ('codex', 's1')",
            [],
        )
        .expect("insert session");
        let session_id = tx.last_insert_rowid();

        let mut rec_ids = Vec::new();
        for (seq, ts) in [(0_i64, 100_i64), (1, 200)] {
            tx.execute(
                "INSERT INTO trace_records(source_id, seq, byte_offset, byte_length, raw_hash, ts_ms, native_type, parse_status)
                 VALUES (?1, ?2, ?2, 1, 'h', ?3, 't', 'ok')",
                rusqlite::params![source_id, seq, ts],
            )
            .expect("insert record");
            rec_ids.push(tx.last_insert_rowid());
        }
        tx.execute(
            "INSERT INTO session_sources(session_id, source_id, identity_record_id, created_at)
             VALUES (?1, ?2, ?3, 100)",
            rusqlite::params![session_id, source_id, rec_ids[0]],
        )
        .expect("insert session_source");
        for (pos, rec_id) in rec_ids.iter().enumerate() {
            let pos = i64::try_from(pos).expect("pos");
            tx.execute(
                "INSERT INTO domain_loops(source_id, session_id, ordinal, session_position, start_record_id)
                 VALUES (?1, ?2, ?3, ?3, ?4)",
                rusqlite::params![source_id, session_id, pos, rec_id],
            )
            .expect("insert loop");
        }

        let before = tx.total_changes();
        normalize_loop_positions(&tx, session_id).expect("normalize ordered loops");
        assert_eq!(
            tx.total_changes(),
            before,
            "already-ordered loops should not run redundant UPDATE statements"
        );

        // Now reverse timestamps and confirm reordering still runs cleanly.
        tx.execute(
            "UPDATE trace_records SET ts_ms = 300 WHERE id = ?1",
            [rec_ids[0]],
        )
        .expect("update ts");
        normalize_loop_positions(&tx, session_id).expect("normalize reordered loops");
        let positions: Vec<(i64, i64)> = tx
            .prepare("SELECT ordinal, session_position FROM domain_loops ORDER BY session_position")
            .expect("prepare")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        assert_eq!(positions, vec![(1, 0), (0, 1)]);
    }
}
