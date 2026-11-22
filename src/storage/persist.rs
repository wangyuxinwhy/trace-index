//! Persists one affected Session into `SQLite` storage.
//!
//! Adapters use a private projection before publication. This boundary
//! enforces the five-object model: runtime work cycles become Loops, lifecycle
//! bookkeeping does not become an Item, tool outputs remain separate Items,
//! and every retained Item stores all of its Record witnesses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension as _, Transaction, params};

use super::blob::BlobCache;
use super::db::to_sql_i64;
use super::search::tokenize_for_term_index;
use super::semantic_encode::{
    BlobRef, evidence_strength, item_text_sources, normalized_role, session_link_member,
    stored_semantic_value,
};
use crate::adapters::projection::{ItemDetail, ItemProjection, LoopProjection, SessionProjection};
use crate::adapters::semantic;
use crate::domain::{ItemId, Semantic, SemanticRole, SessionId};
use crate::indexing::telemetry::{IndexTelemetry, PersistTable};

pub(crate) fn clear_projection(transaction: &Transaction<'_>, source_id: i64) -> Result<()> {
    transaction
        .execute(
            "DELETE FROM item_search WHERE rowid IN (
             SELECT id FROM domain_items WHERE source_id = ?1)",
            [source_id],
        )
        .context("failed to clear searchable Items")?;
    transaction
        .execute(
            "DELETE FROM item_search_terms WHERE rowid IN (
             SELECT id FROM domain_items WHERE source_id = ?1)",
            [source_id],
        )
        .context("failed to clear term-searchable Items")?;

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

pub(super) struct ProjectedLoopRow<'a> {
    pub(super) ordinal: i64,
    pub(super) native_id: Option<&'a str>,
    pub(super) start_record_id: i64,
    pub(super) end_record_id: Option<i64>,
    pub(super) outcome: Option<&'static str>,
    pub(super) model: Option<String>,
    pub(super) usage: Option<String>,
}

impl<'a> ProjectedLoopRow<'a> {
    pub(super) fn from_projection(
        loop_index: usize,
        projected: &'a LoopProjection,
        record_ids: &HashMap<u64, i64>,
    ) -> Result<Self> {
        let ordinal = i64::try_from(loop_index).context("Loop ordinal exceeds SQLite INTEGER")?;
        let start_record_id = record_id(record_ids, projected.start_seq, "Loop start")?;
        let (end_record_id, outcome) = loop_end(projected, record_ids)?;
        let model = projected
            .model
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let usage = projected
            .usage
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        Ok(Self {
            ordinal,
            native_id: projected.native_id.as_deref(),
            start_record_id,
            end_record_id,
            outcome,
            model,
            usage,
        })
    }
}

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

    let mut next_session_position = next_loop_session_position(transaction, session_id)?;
    for (loop_index, projected_loop) in session.loops.iter().enumerate() {
        let row = ProjectedLoopRow::from_projection(loop_index, projected_loop, record_ids)?;
        let loop_id = insert_loop(
            transaction,
            source_id,
            session_id,
            next_session_position,
            &row,
            writer.telemetry,
        )?;
        next_session_position = next_session_position
            .checked_add(1)
            .context("Loop position exceeds SQLite INTEGER")?;

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

pub(super) fn next_loop_session_position(
    transaction: &Transaction<'_>,
    session_id: i64,
) -> Result<i64> {
    transaction
        .query_row(
            "SELECT COALESCE(MAX(session_position) + 1, 0)
               FROM domain_loops
              WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .context("failed to allocate temporary Loop positions")
}

pub(super) fn insert_loop(
    transaction: &Transaction<'_>,
    source_id: i64,
    session_id: i64,
    session_position: i64,
    row: &ProjectedLoopRow<'_>,
    telemetry: &mut IndexTelemetry,
) -> Result<i64> {
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
            row.ordinal,
            session_position,
            row.native_id,
            row.start_record_id,
            row.end_record_id,
            row.outcome,
            row.model,
            row.usage,
        ])
        .context("failed to store Loop")?;
    telemetry.record_persist(PersistTable::Loops, started.elapsed());
    telemetry.count_loop();
    Ok(transaction.last_insert_rowid())
}

/// Reorders every Loop after one Source of a multi-Source Session changes.
///
/// Runtime timestamps establish the historical order in which work occurred.
/// A Source timestamp is the fallback for a Runtime that omits a Loop-start
/// timestamp; storage identity is only the final deterministic tie-breaker.
/// The two update passes keep the unique constraint valid throughout.
pub(super) fn normalize_loop_positions(
    transaction: &Transaction<'_>,
    session_id: i64,
) -> Result<()> {
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

pub(super) struct PendingOutput {
    pub(super) item_id: i64,
    pub(super) call_id: String,
    pub(super) stored_link: Option<i64>,
}

pub(super) struct ExistingSessionLink {
    pub(super) target_runtime: String,
    pub(super) target_native_id: String,
    pub(super) json_member: String,
}

pub(super) struct ExistingItem {
    pub(super) id: i64,
    pub(super) loop_position: Option<i64>,
    pub(super) occurred_at: Option<i64>,
    pub(super) semantic: String,
    pub(super) call_item_id: Option<i64>,
    pub(super) record_ids: Vec<i64>,
    pub(super) session_link: Option<ExistingSessionLink>,
}

pub(super) struct ItemWriter<'a, 'connection> {
    pub(super) transaction: &'a Transaction<'connection>,
    source_id: i64,
    session_id: i64,
    runtime: String,
    record_ids: &'a HashMap<u64, i64>,
    blobs: &'a mut BlobCache,
    pub(super) telemetry: &'a mut IndexTelemetry,
    call_items: HashMap<String, i64>,
    session_targets: HashMap<String, Option<i64>>,
    pending_outputs: Vec<PendingOutput>,
    json_buffer: Vec<u8>,
    item_total: u64,
}

impl<'a, 'connection> ItemWriter<'a, 'connection> {
    pub(super) fn new(
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

    /// Checks whether an existing persisted Item is semantically and structurally
    /// identical to `item` at `loop_position`, preserving its `item_id` and
    /// recording its `ToolCall` / deferred `ToolOutput` linkage when matched.
    pub(super) fn try_preserve_existing(
        &mut self,
        item: &ItemProjection,
        existing: &ExistingItem,
        loop_position: Option<i64>,
    ) -> Result<bool> {
        if existing.loop_position != loop_position || existing.occurred_at != item.ts_ms {
            return Ok(false);
        }
        let (primary_record_id, secondary_record_id) = item_record_ids(item, self.record_ids)?;
        if !record_witnesses_match(&existing.record_ids, primary_record_id, secondary_record_id) {
            return Ok(false);
        }

        let role = normalized_role(&item.semantic_role, &item.detail)?;
        let target_session_id =
            if let Some(target_native_id) = item.linked_session_native_id.as_deref() {
                let json_member = session_link_member(role)?;
                let Some(link) = existing.session_link.as_ref() else {
                    return Ok(false);
                };
                if link.target_runtime != self.runtime
                    || link.target_native_id != target_native_id
                    || link.json_member != json_member
                {
                    return Ok(false);
                }
                self.resolve_session_target(target_native_id)?
                    .map(SessionId)
            } else {
                if existing.session_link.is_some() {
                    return Ok(false);
                }
                None
            };

        let (inline_call_item_id, pending_call_id) = match (&item.detail, role) {
            (ItemDetail::ToolOutput { call_id, .. }, SemanticRole::ToolOutput) => {
                if let Some(&call_item_id) = self.call_items.get(call_id) {
                    (Some(ItemId(call_item_id)), None)
                } else {
                    // Out-of-order or unresolved ToolOutput: compare against its
                    // currently stored link and reconcile in `finish()`.
                    (existing.call_item_id.map(ItemId), Some(call_id.as_str()))
                }
            }
            _ => (None, None),
        };

        let (blob_source, _searchable_text) = item_text_sources(item, role);
        let has_nonempty_text = blob_source
            .and_then(|content| content.text.as_deref())
            .is_some_and(|text| !text.is_empty());
        let blob_ref = match blob_source {
            Some(content) => {
                let Some(blob_id) = self.blobs.lookup_existing(self.transaction, content)? else {
                    return Ok(false);
                };
                Some(BlobRef { blob_id })
            }
            None => None,
        };

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
        let semantic_matches = if self.json_buffer.as_slice() == existing.semantic.as_bytes() {
            true
        } else if target_session_id.is_some() || pending_call_id.is_some() {
            // `json_set` from Session-link triggers or deferred ToolOutput linking
            // appends the updated member at the end of `$.value`.
            let expected: serde_json::Value = serde_json::from_slice(&self.json_buffer)?;
            let actual: serde_json::Value = serde_json::from_str(&existing.semantic)?;
            expected == actual
        } else {
            false
        };
        if !semantic_matches {
            return Ok(false);
        }

        if let ItemDetail::ToolCall { call_id, .. } = &item.detail {
            self.call_items.insert(call_id.clone(), existing.id);
        } else if let Some(call_id) = pending_call_id {
            self.pending_outputs.push(PendingOutput {
                item_id: existing.id,
                call_id: call_id.to_owned(),
                stored_link: existing.call_item_id,
            });
        }
        self.item_total += 1;
        Ok(true)
    }

    /// Writes one selected Item. Returns false for lifecycle-only projections,
    /// which remain Records and must not consume a Loop position.
    pub(super) fn write(
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
            self.write_search_indexes(item_id, text)?;
        }
        self.item_total += 1;
        Ok(true)
    }

    fn write_search_indexes(&mut self, item_id: i64, text: &str) -> Result<()> {
        let started = Instant::now();
        self.transaction
            .prepare_cached("INSERT INTO item_search(rowid, text) VALUES (?1, ?2)")
            .context("failed to prepare Item search insert")?
            .execute(params![item_id, text])
            .context("failed to store Item search text")?;
        let terms = tokenize_for_term_index(text);
        if !terms.is_empty() {
            self.transaction
                .prepare_cached("INSERT INTO item_search_terms(rowid, text) VALUES (?1, ?2)")
                .context("failed to prepare Item term search insert")?
                .execute(params![item_id, terms])
                .context("failed to store Item term search text")?;
        }
        self.telemetry
            .record_persist(PersistTable::ItemSearch, started.elapsed());
        Ok(())
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
            self.pending_outputs.push(PendingOutput {
                item_id,
                call_id: call_id.to_owned(),
                stored_link: None,
            });
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

    pub(super) fn finish(self) -> Result<WriteCounts> {
        for pending in self.pending_outputs {
            let desired = self.call_items.get(&pending.call_id).copied();
            if desired == pending.stored_link {
                continue;
            }
            match desired {
                Some(call_item_id) => {
                    resolve_tool_output_link(self.transaction, pending.item_id, call_item_id)?;
                }
                None => {
                    clear_tool_output_link(self.transaction, pending.item_id)?;
                }
            }
        }
        Ok(WriteCounts {
            items: self.item_total,
        })
    }
}

fn record_witnesses_match(existing: &[i64], primary: i64, secondary: Option<i64>) -> bool {
    match secondary {
        None => existing == [primary],
        Some(secondary) => existing == [primary, secondary] || existing == [secondary, primary],
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

fn clear_tool_output_link(transaction: &Transaction<'_>, item_id: i64) -> Result<()> {
    transaction
        .prepare_cached(
            "UPDATE domain_items
                SET semantic = json_remove(semantic, '$.value.call_item_id')
              WHERE id = ?1",
        )
        .context("failed to prepare ToolOutput link removal")?
        .execute(params![item_id])
        .context("failed to clear ToolOutput call Item")?;
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "a sparse Session parent attribute has one target and one witness"
)]
pub(super) fn store_parent_candidate(
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

pub(super) fn normalize_parent_locator(source_locator: &str, locator: &str) -> String {
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

pub(super) fn record_id(record_ids: &HashMap<u64, i64>, seq: u64, fact: &str) -> Result<i64> {
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
    projected: &LoopProjection,
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

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::resolve_tool_output_link;

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
