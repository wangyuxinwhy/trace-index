//! Prefix-preserving incremental persistence for append-only Source updates.
//!
//! When an already-indexed Source appends new Records without altering its
//! earlier byte prefix, nearly all existing Loops and Items remain identical.
//! Deleting and reinserting every Item and FTS5 entry for a multi-thousand-item
//! active Session would dominate incremental sync time. This module preserves
//! the longest identical prefix of Loops and Items in place, updating Loop
//! metadata where needed and rewriting only from the first diverged Item or
//! Loop onward so relative `item_id` ordering matches a full rebuild.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result};
use rusqlite::{OptionalExtension as _, Transaction, params};

use super::blob::BlobCache;
use super::db::to_sql_i64;
use super::persist::{
    ExistingItem, ExistingSessionLink, ItemWriter, ProjectedLoopRow, WriteCounts, clear_projection,
    insert_loop, next_loop_session_position, normalize_loop_positions, normalize_parent_locator,
    record_id, store_parent_candidate, write_session,
};
use crate::adapters::projection::{ItemProjection, LoopProjection, SessionProjection};
use crate::adapters::semantic;
use crate::indexing::telemetry::{IndexTelemetry, PersistTable};

struct ExistingSessionSource {
    session_id: i64,
    runtime: String,
    native_id: String,
    identity_record_id: i64,
    created_at: Option<i64>,
    name: Option<String>,
    working_directory: Option<String>,
}

#[derive(PartialEq, Eq)]
struct ParentRow {
    kind: String,
    target_native_id: Option<String>,
    target_locator: Option<String>,
    record_id: i64,
}

struct ExistingLoop {
    id: i64,
    ordinal: i64,
    native_id: Option<String>,
    start_record_id: i64,
    end_record_id: Option<i64>,
    outcome: Option<String>,
    model: Option<String>,
    usage: Option<String>,
}

impl ExistingLoop {
    fn metadata_matches(&self, projected: &ProjectedLoopRow<'_>) -> bool {
        self.native_id.as_deref() == projected.native_id
            && self.end_record_id == projected.end_record_id
            && self.outcome.as_deref() == projected.outcome
            && self.model == projected.model
            && self.usage == projected.usage
    }
}

type LoopItemsMap = HashMap<i64, Vec<ExistingItem>>;

struct ExistingProjection {
    session_items: Vec<ExistingItem>,
    loops: Vec<ExistingLoop>,
    loop_items: LoopItemsMap,
}

/// Synchronizes one append-only Source's domain projection in place, falling
/// back to a full replace if the Source switched to a different Session.
pub(crate) fn sync_session(
    transaction: &Transaction<'_>,
    source_id: i64,
    session: &SessionProjection,
    record_ids: &HashMap<u64, i64>,
    blobs: &mut BlobCache,
    telemetry: &mut IndexTelemetry,
) -> Result<WriteCounts> {
    let (runtime, source_locator): (String, String) = transaction
        .query_row(
            "SELECT adapter, path FROM trace_sources WHERE id = ?1",
            [source_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .context("failed to read Source runtime")?;

    let existing_session = load_existing_session_source(transaction, source_id)?;
    let Some(existing_session) = existing_session
        .filter(|row| row.runtime == runtime && row.native_id == session.session_uuid)
    else {
        clear_projection(transaction, source_id)?;
        return write_session(
            transaction,
            source_id,
            session,
            record_ids,
            blobs,
            telemetry,
        );
    };

    let session_id = existing_session.session_id;
    sync_session_metadata(
        transaction,
        source_id,
        &source_locator,
        &existing_session,
        session,
        record_ids,
        telemetry,
    )?;
    let mut existing = load_existing_projection(transaction, source_id)?;

    let mut writer = ItemWriter::new(
        transaction,
        source_id,
        session_id,
        runtime,
        record_ids,
        blobs,
        telemetry,
    );
    let mut rewrite_tail = sync_session_items(&mut writer, &session.items, &mut existing)?;
    let mut next_session_position = next_loop_session_position(transaction, session_id)?;

    for (loop_index, projected_loop) in session.loops.iter().enumerate() {
        let row = ProjectedLoopRow::from_projection(loop_index, projected_loop, record_ids)?;
        if !rewrite_tail
            && let Some(existing_loop) = existing.loops.get(loop_index)
            && existing_loop.ordinal == row.ordinal
            && existing_loop.start_record_id == row.start_record_id
        {
            rewrite_tail = sync_existing_loop(
                &mut writer,
                existing_loop,
                &existing.loops[loop_index + 1..],
                existing
                    .loop_items
                    .get(&existing_loop.id)
                    .map_or(&[][..], Vec::as_slice),
                projected_loop,
                &row,
            )?;
            continue;
        }

        if !rewrite_tail && loop_index < existing.loops.len() {
            delete_loops(transaction, &existing.loops[loop_index..])?;
            rewrite_tail = true;
        }
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
        write_loop_items(&mut writer, loop_id, &projected_loop.items)?;
    }

    if !rewrite_tail && existing.loops.len() > session.loops.len() {
        delete_loops(transaction, &existing.loops[session.loops.len()..])?;
    }

    let counts = writer.finish()?;
    normalize_loop_positions(transaction, session_id)?;
    Ok(counts)
}

fn sync_existing_loop(
    writer: &mut ItemWriter<'_, '_>,
    existing_loop: &ExistingLoop,
    trailing_loops: &[ExistingLoop],
    existing_items: &[ExistingItem],
    projected_loop: &LoopProjection,
    row: &ProjectedLoopRow<'_>,
) -> Result<bool> {
    if !existing_loop.metadata_matches(row) {
        update_loop_metadata(writer.transaction, existing_loop.id, row, writer.telemetry)?;
    }
    let projected_items: Vec<&ItemProjection> = projected_loop
        .items
        .iter()
        .filter(|item| item.semantic_role != semantic::RUNTIME_LIFECYCLE)
        .collect();
    let matched = match_item_prefix(writer, &projected_items, existing_items, true)?;
    if matched == existing_items.len() && matched == projected_items.len() {
        return Ok(false);
    }

    let mut rewrite_tail = false;
    if !trailing_loops.is_empty() {
        // Preserve strict relative `item_id` ordering across Loops when an
        // earlier Loop's Item list changes.
        delete_loops(writer.transaction, trailing_loops)?;
        rewrite_tail = true;
    }
    delete_items(writer.transaction, &existing_items[matched..])?;
    for (position, item) in projected_items[matched..].iter().enumerate() {
        let loop_position = to_sql_i64(
            u64::try_from(matched + position).context("Loop position exceeds u64")?,
            "Loop position",
        )?;
        writer.write(item, Some(existing_loop.id), Some(loop_position))?;
    }
    Ok(rewrite_tail)
}

fn write_loop_items(
    writer: &mut ItemWriter<'_, '_>,
    loop_id: i64,
    items: &[ItemProjection],
) -> Result<()> {
    let mut loop_position = 0_u64;
    for projected_item in items {
        if writer.write(
            projected_item,
            Some(loop_id),
            Some(to_sql_i64(loop_position, "Loop position")?),
        )? {
            loop_position += 1;
        }
    }
    Ok(())
}

fn sync_session_metadata(
    transaction: &Transaction<'_>,
    source_id: i64,
    source_locator: &str,
    existing: &ExistingSessionSource,
    session: &SessionProjection,
    record_ids: &HashMap<u64, i64>,
    telemetry: &mut IndexTelemetry,
) -> Result<()> {
    let identity_record_id = record_id(record_ids, session.start_seq, "Session identity")?;
    if existing.identity_record_id != identity_record_id
        || existing.created_at != session.started_at
        || existing.name != session.title
        || existing.working_directory != session.cwd
    {
        let started = Instant::now();
        transaction
            .execute(
                "UPDATE session_sources
                    SET identity_record_id = ?2,
                        created_at = ?3,
                        name = ?4,
                        working_directory = ?5
                  WHERE source_id = ?1",
                params![
                    source_id,
                    identity_record_id,
                    session.started_at,
                    session.title,
                    session.cwd,
                ],
            )
            .context("failed to update Session Source metadata")?;
        telemetry.record_persist(PersistTable::Sessions, started.elapsed());
        telemetry.count_session();
    }

    let forked_from_locator = session
        .forked_from_locator
        .as_deref()
        .map(|locator| normalize_parent_locator(source_locator, locator));
    let mut expected_parents = Vec::with_capacity(2);
    if let Some(seq) = session.delegated_from_record_seq {
        expected_parents.push(ParentRow {
            kind: "delegated_from".to_owned(),
            target_native_id: session.delegated_from_native_id.clone(),
            target_locator: None,
            record_id: record_id(record_ids, seq, "Session parent")?,
        });
    }
    if let Some(seq) = session.forked_from_record_seq {
        expected_parents.push(ParentRow {
            kind: "forked_from".to_owned(),
            target_native_id: session.forked_from_native_id.clone(),
            target_locator: forked_from_locator.clone(),
            record_id: record_id(record_ids, seq, "Session parent")?,
        });
    }
    let existing_parents = load_existing_parents(transaction, source_id)?;
    if existing_parents != expected_parents {
        transaction
            .execute(
                "DELETE FROM session_parents WHERE source_id = ?1",
                [source_id],
            )
            .context("failed to refresh Session parent evidence")?;
        store_parent_candidate(
            transaction,
            existing.session_id,
            source_id,
            "forked_from",
            session.forked_from_native_id.as_deref(),
            forked_from_locator.as_deref(),
            session.forked_from_record_seq,
            record_ids,
        )?;
        store_parent_candidate(
            transaction,
            existing.session_id,
            source_id,
            "delegated_from",
            session.delegated_from_native_id.as_deref(),
            None,
            session.delegated_from_record_seq,
            record_ids,
        )?;
    }
    Ok(())
}

fn sync_session_items(
    writer: &mut ItemWriter<'_, '_>,
    projected: &[ItemProjection],
    existing: &mut ExistingProjection,
) -> Result<bool> {
    let projected_items: Vec<&ItemProjection> = projected
        .iter()
        .filter(|item| item.semantic_role != semantic::RUNTIME_LIFECYCLE)
        .collect();
    let matched = match_item_prefix(writer, &projected_items, &existing.session_items, false)?;
    if matched == existing.session_items.len() && matched == projected_items.len() {
        return Ok(false);
    }

    let mut rewrite_tail = false;
    if !existing.loops.is_empty() {
        delete_loops(writer.transaction, &existing.loops)?;
        existing.loops.clear();
        rewrite_tail = true;
    }
    delete_items(writer.transaction, &existing.session_items[matched..])?;
    for item in &projected_items[matched..] {
        writer.write(item, None, None)?;
    }
    Ok(rewrite_tail)
}

fn match_item_prefix(
    writer: &mut ItemWriter<'_, '_>,
    projected: &[&ItemProjection],
    existing: &[ExistingItem],
    in_loop: bool,
) -> Result<usize> {
    let common = projected.len().min(existing.len());
    let mut matched = 0_usize;
    while matched < common {
        let loop_position = if in_loop {
            Some(to_sql_i64(
                u64::try_from(matched).context("Loop position exceeds u64")?,
                "Loop position",
            )?)
        } else {
            None
        };
        if !writer.try_preserve_existing(projected[matched], &existing[matched], loop_position)? {
            break;
        }
        matched += 1;
    }
    Ok(matched)
}

fn load_existing_session_source(
    transaction: &Transaction<'_>,
    source_id: i64,
) -> Result<Option<ExistingSessionSource>> {
    transaction
        .prepare_cached(
            "SELECT ss.session_id, s.runtime, s.native_id,
                    ss.identity_record_id, ss.created_at, ss.name, ss.working_directory
               FROM session_sources ss
               JOIN domain_sessions s ON s.id = ss.session_id
              WHERE ss.source_id = ?1",
        )
        .context("failed to prepare existing Session lookup")?
        .query_row([source_id], |row| {
            Ok(ExistingSessionSource {
                session_id: row.get(0)?,
                runtime: row.get(1)?,
                native_id: row.get(2)?,
                identity_record_id: row.get(3)?,
                created_at: row.get(4)?,
                name: row.get(5)?,
                working_directory: row.get(6)?,
            })
        })
        .optional()
        .context("failed to load existing Session metadata")
}

fn load_existing_parents(transaction: &Transaction<'_>, source_id: i64) -> Result<Vec<ParentRow>> {
    let mut statement = transaction
        .prepare_cached(
            "SELECT kind, target_native_id, target_locator, record_id
               FROM session_parents
              WHERE source_id = ?1
              ORDER BY kind",
        )
        .context("failed to prepare Session parents query")?;
    let rows = statement
        .query_map([source_id], |row| {
            Ok(ParentRow {
                kind: row.get(0)?,
                target_native_id: row.get(1)?,
                target_locator: row.get(2)?,
                record_id: row.get(3)?,
            })
        })
        .context("failed to query Session parents")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to collect Session parents")?;
    Ok(rows)
}

fn load_existing_projection(
    transaction: &Transaction<'_>,
    source_id: i64,
) -> Result<ExistingProjection> {
    let loops = load_existing_loops(transaction, source_id)?;
    let mut records_by_item = load_existing_item_records(transaction, source_id)?;
    let mut links_by_item = load_existing_item_links(transaction, source_id)?;
    let (session_items, loop_items) = load_existing_items(
        transaction,
        source_id,
        loops.len(),
        &mut records_by_item,
        &mut links_by_item,
    )?;
    Ok(ExistingProjection {
        session_items,
        loops,
        loop_items,
    })
}

fn load_existing_loops(transaction: &Transaction<'_>, source_id: i64) -> Result<Vec<ExistingLoop>> {
    let mut statement = transaction
        .prepare_cached(
            "SELECT id, ordinal, native_id, start_record_id, end_record_id,
                    outcome, model, usage
               FROM domain_loops
              WHERE source_id = ?1
              ORDER BY ordinal",
        )
        .context("failed to prepare existing Loops query")?;
    let rows = statement
        .query_map([source_id], |row| {
            Ok(ExistingLoop {
                id: row.get(0)?,
                ordinal: row.get(1)?,
                native_id: row.get(2)?,
                start_record_id: row.get(3)?,
                end_record_id: row.get(4)?,
                outcome: row.get(5)?,
                model: row.get(6)?,
                usage: row.get(7)?,
            })
        })
        .context("failed to query existing Loops")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to collect existing Loops")?;
    Ok(rows)
}

fn load_existing_item_records(
    transaction: &Transaction<'_>,
    source_id: i64,
) -> Result<HashMap<i64, Vec<i64>>> {
    let mut records_by_item: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut statement = transaction
        .prepare_cached(
            "SELECT ir.item_id, ir.record_id
               FROM domain_items i
               JOIN item_records ir ON ir.item_id = i.id
              WHERE i.source_id = ?1",
        )
        .context("failed to prepare existing Item records query")?;
    let rows = statement
        .query_map([source_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .context("failed to query existing Item records")?;
    for row in rows {
        let (item_id, rec_id) = row.context("failed to read existing Item record")?;
        records_by_item.entry(item_id).or_default().push(rec_id);
    }
    Ok(records_by_item)
}

fn load_existing_item_links(
    transaction: &Transaction<'_>,
    source_id: i64,
) -> Result<HashMap<i64, ExistingSessionLink>> {
    let mut links_by_item: HashMap<i64, ExistingSessionLink> = HashMap::new();
    let mut statement = transaction
        .prepare_cached(
            "SELECT l.item_id, l.target_runtime, l.target_native_id, l.json_member
               FROM item_session_links l
               JOIN domain_items i ON i.id = l.item_id
              WHERE i.source_id = ?1",
        )
        .context("failed to prepare existing Item Session links query")?;
    let rows = statement
        .query_map([source_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                ExistingSessionLink {
                    target_runtime: row.get(1)?,
                    target_native_id: row.get(2)?,
                    json_member: row.get(3)?,
                },
            ))
        })
        .context("failed to query existing Item Session links")?;
    for row in rows {
        let (item_id, link) = row.context("failed to read existing Item Session link")?;
        links_by_item.insert(item_id, link);
    }
    Ok(links_by_item)
}

fn load_existing_items(
    transaction: &Transaction<'_>,
    source_id: i64,
    loop_capacity: usize,
    records_by_item: &mut HashMap<i64, Vec<i64>>,
    links_by_item: &mut HashMap<i64, ExistingSessionLink>,
) -> Result<(Vec<ExistingItem>, LoopItemsMap)> {
    let mut session_items = Vec::new();
    let mut loop_items: HashMap<i64, Vec<ExistingItem>> = HashMap::with_capacity(loop_capacity);
    let mut statement = transaction
        .prepare_cached(
            "SELECT id, loop_id, loop_position, occurred_at, semantic,
                    json_extract(semantic, '$.value.call_item_id')
               FROM domain_items
              WHERE source_id = ?1
              ORDER BY id",
        )
        .context("failed to prepare existing Items query")?;
    let rows = statement
        .query_map([source_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })
        .context("failed to query existing Items")?;
    for row in rows {
        let (id, loop_id, loop_position, occurred_at, semantic, call_item_id) =
            row.context("failed to read existing Item")?;
        let item = ExistingItem {
            id,
            loop_position,
            occurred_at,
            semantic,
            call_item_id,
            record_ids: records_by_item.remove(&id).unwrap_or_default(),
            session_link: links_by_item.remove(&id),
        };
        if let Some(loop_id) = loop_id {
            loop_items.entry(loop_id).or_default().push(item);
        } else {
            session_items.push(item);
        }
    }
    Ok((session_items, loop_items))
}

fn update_loop_metadata(
    transaction: &Transaction<'_>,
    loop_id: i64,
    row: &ProjectedLoopRow<'_>,
    telemetry: &mut IndexTelemetry,
) -> Result<()> {
    let started = Instant::now();
    transaction
        .prepare_cached(
            "UPDATE domain_loops
                SET native_id = ?2,
                    end_record_id = ?3,
                    outcome = ?4,
                    model = ?5,
                    usage = ?6
              WHERE id = ?1",
        )
        .context("failed to prepare Loop metadata update")?
        .execute(params![
            loop_id,
            row.native_id,
            row.end_record_id,
            row.outcome,
            row.model,
            row.usage,
        ])
        .context("failed to update Loop metadata")?;
    telemetry.record_persist(PersistTable::Loops, started.elapsed());
    telemetry.count_loop();
    Ok(())
}

fn delete_items(transaction: &Transaction<'_>, items: &[ExistingItem]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut del_search = transaction
        .prepare_cached("DELETE FROM item_search WHERE rowid = ?1")
        .context("failed to prepare Item search delete")?;
    let mut del_terms = transaction
        .prepare_cached("DELETE FROM item_search_terms WHERE rowid = ?1")
        .context("failed to prepare Item term search delete")?;
    let mut del_item = transaction
        .prepare_cached("DELETE FROM domain_items WHERE id = ?1")
        .context("failed to prepare Item delete")?;
    for item in items {
        del_search
            .execute([item.id])
            .context("failed to delete searchable Item")?;
        del_terms
            .execute([item.id])
            .context("failed to delete term-searchable Item")?;
        del_item
            .execute([item.id])
            .context("failed to delete Item")?;
    }
    Ok(())
}

fn delete_loops(transaction: &Transaction<'_>, loops: &[ExistingLoop]) -> Result<()> {
    if loops.is_empty() {
        return Ok(());
    }
    let mut del_search = transaction
        .prepare_cached(
            "DELETE FROM item_search WHERE rowid IN (
                 SELECT id FROM domain_items WHERE loop_id = ?1
             )",
        )
        .context("failed to prepare Loop search delete")?;
    let mut del_terms = transaction
        .prepare_cached(
            "DELETE FROM item_search_terms WHERE rowid IN (
                 SELECT id FROM domain_items WHERE loop_id = ?1
             )",
        )
        .context("failed to prepare Loop term search delete")?;
    let mut del_loop = transaction
        .prepare_cached("DELETE FROM domain_loops WHERE id = ?1")
        .context("failed to prepare Loop delete")?;
    for existing_loop in loops {
        del_search
            .execute([existing_loop.id])
            .context("failed to clear Loop searchable Items")?;
        del_terms
            .execute([existing_loop.id])
            .context("failed to clear Loop term-searchable Items")?;
        del_loop
            .execute([existing_loop.id])
            .context("failed to delete Loop")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use rusqlite::{Connection, params};
    use tempfile::tempdir;

    use super::sync_session;
    use crate::adapters::projection::{
        BoundedText, ItemDetail, ItemProjection, LoopProjection, SessionProjection, ToolOutputFacts,
    };
    use crate::adapters::semantic;
    use crate::domain::{LoopOutcome, Usage};
    use crate::indexing::telemetry::IndexTelemetry;
    use crate::interface::output::IndexMetrics;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::Store;
    use crate::storage::persist::write_session;

    fn text_item(seq: u64, role: &str, text: &str, searchable: bool) -> ItemProjection {
        ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms: Some(1_000 + i64::try_from(seq).expect("seq") * 100),
            semantic_role: role.to_owned(),
            basis: semantic::basis::RECORD_KIND.to_owned(),
            preview: Some(BoundedText::bounded(text, 256)),
            detail: ItemDetail::Message { has_images: false },
            linked_session_native_id: None,
            searchable,
        }
    }

    fn tool_call_item(seq: u64, call_id: &str) -> ItemProjection {
        ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms: Some(1_000 + i64::try_from(seq).expect("seq") * 100),
            semantic_role: semantic::AGENT_TOOL_CALL.to_owned(),
            basis: semantic::basis::RECORD_KIND.to_owned(),
            preview: None,
            detail: ItemDetail::ToolCall {
                call_id: call_id.to_owned(),
                name: Some("read_file".to_owned()),
                cmd: None,
                cmd_lang: None,
                workdir: None,
                args: Some("{\"path\":\"README.md\"}".to_owned()),
                syntax: None,
            },
            linked_session_native_id: None,
            searchable: false,
        }
    }

    fn tool_output_item(seq: u64, call_id: &str, text: &str) -> ItemProjection {
        ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms: Some(1_000 + i64::try_from(seq).expect("seq") * 100),
            semantic_role: semantic::TOOL_OUTPUT.to_owned(),
            basis: semantic::basis::RECORD_KIND.to_owned(),
            preview: None,
            detail: ItemDetail::ToolOutput {
                call_id: call_id.to_owned(),
                output: Some(BoundedText::bounded(text, 256)),
                facts: ToolOutputFacts::default(),
            },
            linked_session_native_id: None,
            searchable: false,
        }
    }

    fn session_fixture(
        uuid: &str,
        items: Vec<ItemProjection>,
        loops: Vec<LoopProjection>,
    ) -> SessionProjection {
        SessionProjection {
            session_uuid: uuid.to_owned(),
            identity_confirmed: true,
            start_seq: 0,
            started_at: Some(1_000),
            cwd: Some("/repo".to_owned()),
            title: Some("demo".to_owned()),
            forked_from_native_id: None,
            forked_from_locator: None,
            forked_from_record_seq: None,
            delegated_from_native_id: None,
            delegated_from_record_seq: None,
            items,
            loops,
        }
    }

    fn loop_fixture(
        native_id: Option<&str>,
        start_seq: u64,
        end_record_seq: Option<u64>,
        outcome: Option<LoopOutcome>,
        items: Vec<ItemProjection>,
    ) -> LoopProjection {
        LoopProjection {
            native_id: native_id.map(str::to_owned),
            start_seq,
            end_record_seq,
            outcome,
            model: None,
            usage: None,
            items,
        }
    }

    fn insert_records(
        tx: &rusqlite::Transaction<'_>,
        source_id: i64,
        seqs: std::ops::Range<u64>,
        map: &mut HashMap<u64, i64>,
    ) {
        for seq in seqs {
            let ts = 1_000 + i64::try_from(seq).expect("seq") * 100;
            tx.execute(
                "INSERT INTO trace_records(
                     source_id, seq, byte_offset, byte_length, raw_hash, ts_ms, native_type, parse_status
                 ) VALUES (?1, ?2, ?2, 10, 'h', ?3, 'event', 'ok')",
                params![source_id, i64::try_from(seq).expect("seq"), ts],
            )
            .expect("insert record");
            map.insert(seq, tx.last_insert_rowid());
        }
    }

    fn write_initial(
        store: &mut Store,
        path: &str,
        record_count: u64,
        session: &SessionProjection,
    ) -> (i64, HashMap<u64, i64>) {
        let mut blobs = BlobCache::default();
        let mut telemetry = IndexTelemetry::new();
        let tx = store.connection_mut().transaction().expect("tx");
        tx.execute(
            "INSERT INTO trace_sources(adapter, path) VALUES ('codex', ?1)",
            [path],
        )
        .expect("source");
        let source_id = tx.last_insert_rowid();
        let mut rec_ids = HashMap::new();
        insert_records(&tx, source_id, 0..record_count, &mut rec_ids);
        write_session(
            &tx,
            source_id,
            session,
            &rec_ids,
            &mut blobs,
            &mut telemetry,
        )
        .expect("initial write");
        tx.commit().expect("commit");
        (source_id, rec_ids)
    }

    fn run_sync(
        store: &mut Store,
        source_id: i64,
        new_seqs: std::ops::Range<u64>,
        record_ids: &mut HashMap<u64, i64>,
        session: &SessionProjection,
    ) -> (u64, IndexMetrics) {
        let mut blobs = BlobCache::default();
        let mut telemetry = IndexTelemetry::new();
        let tx = store.connection_mut().transaction().expect("tx");
        insert_records(&tx, source_id, new_seqs, record_ids);
        let counts = sync_session(
            &tx,
            source_id,
            session,
            record_ids,
            &mut blobs,
            &mut telemetry,
        )
        .expect("sync session");
        tx.commit().expect("commit");
        (counts.items, telemetry.finish(0, 0))
    }

    fn query_i64_column(conn: &Connection, sql: &str) -> Vec<i64> {
        conn.prepare(sql)
            .expect("prep")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect")
    }

    fn snapshot_domain(conn: &Connection) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
        let loops = conn
            .prepare(
                "SELECT ordinal, session_position, COALESCE(native_id,''), start_record_id,
                        COALESCE(end_record_id,-1), COALESCE(outcome,''), COALESCE(usage,'')
                   FROM domain_loops ORDER BY ordinal",
            )
            .expect("prep loops")
            .query_map([], |r| {
                Ok(format!(
                    "{}:{}:{}:{}:{}:{}:{}",
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .expect("query loops")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect loops");

        let items = conn
            .prepare(
                "SELECT COALESCE(l.ordinal, -1), COALESCE(i.loop_position, -1),
                        COALESCE(i.occurred_at, -1),
                        json_extract(i.semantic, '$.role'),
                        COALESCE(b.text, ''),
                        COALESCE(json_extract(i.semantic, '$.value.call_item_id'), -1),
                        (SELECT group_concat(ir.record_id, ',')
                           FROM (SELECT record_id FROM item_records WHERE item_id = i.id ORDER BY record_id) ir)
                   FROM domain_items i
                   LEFT JOIN domain_loops l ON l.id = i.loop_id
                   LEFT JOIN content_blobs b
                     ON b.id = COALESCE(
                          json_extract(i.semantic, '$.value.text.blob_id'),
                          json_extract(i.semantic, '$.value.summary.blob_id')
                        )
                  ORDER BY i.id",
            )
            .expect("prep items")
            .query_map([], |r| {
                Ok(format!(
                    "{}:{}:{}:{}:{}:{}:{}",
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .expect("query items")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect items");

        let trigram_hits = conn
            .prepare(
                "SELECT i.loop_position, json_extract(i.semantic, '$.role')
                   FROM item_search s
                   JOIN domain_items i ON i.id = s.rowid
                  WHERE item_search MATCH 'hel AND ell AND llo'
                  ORDER BY i.id",
            )
            .expect("prep fts")
            .query_map([], |r| {
                Ok(format!(
                    "{:?}:{}",
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, String>(1)?
                ))
            })
            .expect("query fts")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect fts");

        let term_hits = conn
            .prepare(
                "SELECT i.loop_position, json_extract(i.semantic, '$.role')
                   FROM item_search_terms s
                   JOIN domain_items i ON i.id = s.rowid
                  WHERE item_search_terms MATCH 'hello'
                  ORDER BY i.id",
            )
            .expect("prep fts terms")
            .query_map([], |r| {
                Ok(format!(
                    "{:?}:{}",
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, String>(1)?
                ))
            })
            .expect("query fts terms")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect fts terms");

        (loops, items, trigram_hits, term_hits)
    }

    #[test]
    fn incremental_sync_preserves_prefix_ids_and_matches_fresh_write() {
        let dir = tempdir().expect("temp dir");
        let mut inc_store = Store::open(&dir.path().join("inc.sqlite")).expect("open inc");
        let mut fresh_store = Store::open(&dir.path().join("fresh.sqlite")).expect("open fresh");

        let initial = session_fixture(
            "sess-1",
            vec![text_item(0, semantic::RUNTIME_ENV_CONTEXT, "env", false)],
            vec![
                loop_fixture(
                    Some("l0"),
                    1,
                    Some(2),
                    Some(LoopOutcome::Completed),
                    vec![
                        text_item(1, semantic::HUMAN_REQUEST, "hello world", true),
                        text_item(2, semantic::AGENT_FINAL_ANSWER, "hello back", true),
                    ],
                ),
                loop_fixture(
                    Some("l1"),
                    3,
                    None,
                    None,
                    vec![
                        text_item(3, semantic::HUMAN_REQUEST, "check readme", true),
                        tool_call_item(4, "call-1"),
                    ],
                ),
            ],
        );

        let (source_id, mut record_ids) =
            write_initial(&mut inc_store, "/tmp/s.jsonl", 5, &initial);
        let initial_loop_ids = query_i64_column(
            inc_store.connection(),
            "SELECT id FROM domain_loops ORDER BY ordinal",
        );
        let initial_item_ids = query_i64_column(
            inc_store.connection(),
            "SELECT id FROM domain_items ORDER BY id",
        );

        let mut appended = initial;
        appended.loops[1].end_record_seq = Some(6);
        appended.loops[1].outcome = Some(LoopOutcome::Completed);
        appended.loops[1].usage = Some(Usage {
            input: 100,
            cached: None,
            cache_write: None,
            output: 50,
            reasoning: None,
        });
        appended.loops[1]
            .items
            .push(tool_output_item(5, "call-1", "readme body"));
        appended.loops[1].items.push(text_item(
            6,
            semantic::AGENT_FINAL_ANSWER,
            "hello done",
            true,
        ));
        appended.loops.push(loop_fixture(
            Some("l2"),
            7,
            None,
            None,
            vec![text_item(7, semantic::HUMAN_REQUEST, "next hello", true)],
        ));

        let (projected_items, metrics) =
            run_sync(&mut inc_store, source_id, 5..8, &mut record_ids, &appended);
        assert_eq!(projected_items, 8);
        assert_eq!(metrics.writes.items, 3);
        assert_eq!(metrics.writes.loops, 2);
        assert_eq!(metrics.writes.sessions, 0);

        let after_loop_ids = query_i64_column(
            inc_store.connection(),
            "SELECT id FROM domain_loops ORDER BY ordinal",
        );
        let after_item_ids = query_i64_column(
            inc_store.connection(),
            "SELECT id FROM domain_items ORDER BY id",
        );
        assert_eq!(&after_loop_ids[..2], initial_loop_ids.as_slice());
        assert_eq!(&after_item_ids[..5], initial_item_ids.as_slice());

        write_initial(&mut fresh_store, "/tmp/s.jsonl", 8, &appended);
        let snap = snapshot_domain(inc_store.connection());
        assert_eq!(
            snap.2.len(),
            4,
            "all four 'hello' items indexed in trigram FTS"
        );
        assert_eq!(snap, snapshot_domain(fresh_store.connection()));
    }

    #[test]
    fn incremental_sync_handles_earlier_loop_edit_and_out_of_order_tool_output() {
        let dir = tempdir().expect("temp dir");
        let mut inc_store = Store::open(&dir.path().join("inc.sqlite")).expect("open inc");
        let mut fresh_store = Store::open(&dir.path().join("fresh.sqlite")).expect("open fresh");

        let initial = session_fixture(
            "sess-ooo",
            Vec::new(),
            vec![
                loop_fixture(
                    None,
                    0,
                    None,
                    None,
                    vec![
                        text_item(0, semantic::HUMAN_REQUEST, "hello early", true),
                        tool_output_item(1, "late-call", "output first"),
                    ],
                ),
                loop_fixture(
                    None,
                    2,
                    None,
                    None,
                    vec![text_item(2, semantic::HUMAN_REQUEST, "hello loop 1", true)],
                ),
            ],
        );

        let (source_id, mut record_ids) =
            write_initial(&mut inc_store, "/tmp/ooo.jsonl", 3, &initial);

        let mut updated = initial;
        updated.loops[1].items.push(text_item(
            3,
            semantic::AGENT_FINAL_ANSWER,
            "hello amended",
            true,
        ));
        updated.loops.push(loop_fixture(
            None,
            4,
            None,
            None,
            vec![tool_call_item(4, "late-call")],
        ));

        run_sync(&mut inc_store, source_id, 3..5, &mut record_ids, &updated);
        let linked_call_id: Option<i64> = inc_store
            .connection()
            .query_row(
                "SELECT json_extract(semantic, '$.value.call_item_id')
                   FROM domain_items
                  WHERE json_extract(semantic, '$.role') = 'tool.output'",
                [],
                |r| r.get(0),
            )
            .expect("query output link");
        let actual_call_id: i64 = inc_store
            .connection()
            .query_row(
                "SELECT id FROM domain_items
                  WHERE json_extract(semantic, '$.role') = 'agent.tool_call'",
                [],
                |r| r.get(0),
            )
            .expect("query call id");
        assert_eq!(linked_call_id, Some(actual_call_id));

        // Second sync with identical projection must be a complete zero-write no-op even
        // though `json_set` moved `call_item_id` inside the preserved ToolOutput semantic JSON.
        let (_, noop_metrics) =
            run_sync(&mut inc_store, source_id, 5..5, &mut record_ids, &updated);
        assert_eq!(noop_metrics.writes.items, 0);
        assert_eq!(noop_metrics.writes.loops, 0);
        assert_eq!(noop_metrics.writes.sessions, 0);

        write_initial(&mut fresh_store, "/tmp/ooo.jsonl", 5, &updated);
        assert_eq!(
            snapshot_domain(inc_store.connection()),
            snapshot_domain(fresh_store.connection())
        );
    }

    #[test]
    fn incremental_sync_rewrites_trailing_loops_on_earlier_item_edit_and_cleans_fts() {
        let dir = tempdir().expect("temp dir");
        let mut inc_store = Store::open(&dir.path().join("inc.sqlite")).expect("open inc");
        let mut fresh_store = Store::open(&dir.path().join("fresh.sqlite")).expect("open fresh");

        let initial = session_fixture(
            "sess-edit",
            Vec::new(),
            vec![
                loop_fixture(
                    None,
                    0,
                    Some(1),
                    Some(LoopOutcome::Completed),
                    vec![
                        text_item(0, semantic::HUMAN_REQUEST, "hello alpha", true),
                        text_item(1, semantic::AGENT_FINAL_ANSWER, "stale_needle_only", true),
                    ],
                ),
                loop_fixture(
                    None,
                    2,
                    None,
                    None,
                    vec![text_item(2, semantic::HUMAN_REQUEST, "hello beta", true)],
                ),
            ],
        );

        let (source_id, mut record_ids) =
            write_initial(&mut inc_store, "/tmp/edit.jsonl", 4, &initial);

        let mut revised = initial;
        revised.loops[0].items[1] = text_item(3, semantic::AGENT_FINAL_ANSWER, "hello gamma", true);
        revised.loops[0].end_record_seq = Some(3);
        run_sync(&mut inc_store, source_id, 4..4, &mut record_ids, &revised);

        let stale_trigram: i64 = inc_store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM item_search WHERE item_search MATCH 'sta AND tal AND ale'",
                [],
                |r| r.get(0),
            )
            .expect("trigram check");
        let stale_terms: i64 = inc_store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM item_search_terms WHERE item_search_terms MATCH 'stale'",
                [],
                |r| r.get(0),
            )
            .expect("terms check");
        assert_eq!(stale_trigram, 0);
        assert_eq!(stale_terms, 0);

        write_initial(&mut fresh_store, "/tmp/edit.jsonl", 4, &revised);
        assert_eq!(
            snapshot_domain(inc_store.connection()),
            snapshot_domain(fresh_store.connection())
        );

        // Switching session_uuid falls back cleanly to a full replace.
        revised.session_uuid = "sess-other".to_owned();
        run_sync(&mut inc_store, source_id, 4..4, &mut record_ids, &revised);
        let session_uuids: Vec<String> = inc_store
            .connection()
            .prepare("SELECT native_id FROM domain_sessions ORDER BY id")
            .expect("prep")
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        assert_eq!(session_uuids, vec!["sess-other"]);
    }
}
