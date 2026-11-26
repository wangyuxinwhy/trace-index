//! Bounded parallel Source preparation pipeline for newly discovered traces.
//!
//! Cold builds and large batches of new traces spend most of their CPU time
//! decompressing, hashing, parsing JSON, projecting Sessions, and parsing shell
//! syntax — all independent of `SQLite`. Up to `MAX_PIPELINE_WORKERS` scoped
//! threads prepare newly discovered Sources in parallel while the main thread
//! persists each prepared Session sequentially in deterministic discovery order.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::{TransactionBehavior, params};
use serde_json::Value;

use super::indexer::{
    DiscoveredFile, IndexOptions, MAX_ADAPTER_DETECTION_BYTES, RunSink, SyncScratch,
    UnrecognizedSource, antigravity_replacement, detect_adapter, ensure_source_row,
    finalize_projected_session, index_source, insert_record, record_source_outcome, source_status,
    write_projected_session,
};
use super::telemetry::{IndexPhase, IndexTelemetry, PersistTable};
use crate::adapters::adapter::{AdapterKind, Projector, record_facts};
use crate::adapters::projection::{RecordFacts, SessionProjection};
use crate::ingest::source::{
    IndexingSource, READ_BUFFER_BYTES, RecordReadInto, alternate_encoding, is_compressed,
    open_for_indexing, prefix_hasher, read_bounded_record_into,
};
use crate::interface::output::SourceIndexResult;
use crate::shell::syntax;
use crate::storage::blob::BlobCache;
use crate::storage::db::{SourceState, Store, to_sql_i64};
use crate::storage::persist::clear_projection;

/// Maximum number of background threads preparing new Sources in parallel
/// before handing projected facts to the single `SQLite` writer thread.
const MAX_PIPELINE_WORKERS: usize = 4;
/// Minimum number of newly discovered Sources before spawning worker threads.
pub(super) const MIN_PIPELINE_FILES: usize = 4;
const WORKER_RETAINED_DECOMPRESS_BYTES: usize = 4 * 1024 * 1024;
const WORKER_RETAINED_RECORD_BYTES: usize = 1024 * 1024;

pub(super) fn pipeline_worker_count(file_count: usize) -> usize {
    if file_count < MIN_PIPELINE_FILES {
        return 1;
    }
    thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_PIPELINE_WORKERS)
}

pub(super) fn is_pipeline_eligible(
    file: &DiscoveredFile,
    existing_sources: &HashMap<String, SourceState>,
) -> bool {
    let path = file.path.to_string_lossy();
    if existing_sources.contains_key(path.as_ref()) {
        return false;
    }
    let alternate = alternate_encoding(&file.path);
    let alternate_str = alternate.to_string_lossy();
    !existing_sources.contains_key(alternate_str.as_ref())
}

pub(super) fn index_files_pipelined(
    store: &mut Store,
    files: &[DiscoveredFile],
    existing_sources: &HashMap<String, SourceState>,
    worker_count: usize,
    options: &IndexOptions,
    scratch: &mut SyncScratch,
    sink: &mut RunSink<'_>,
) {
    let in_flight_limit = worker_count + 1;
    let (job_tx, job_rx) = mpsc::sync_channel::<usize>(in_flight_limit);
    let (result_tx, result_rx) =
        mpsc::sync_channel::<(usize, Result<PreparedNewSource>)>(in_flight_limit);
    let shared_rx = Mutex::new(job_rx);

    thread::scope(|scope| {
        for _ in 0..worker_count {
            let rx = &shared_rx;
            let tx = result_tx.clone();
            scope.spawn(move || {
                let mut worker = WorkerScratch::new();
                loop {
                    let index = {
                        let Ok(guard) = rx.lock() else {
                            break;
                        };
                        guard.recv().ok()
                    };
                    let Some(index) = index else {
                        break;
                    };
                    let prepared = prepare_new_source(&files[index], options, &mut worker);
                    worker.trim_oversized_buffers();
                    if tx.send((index, prepared)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(result_tx);

        let mut next_dispatch = 0_usize;
        let mut in_flight = 0_usize;
        let mut ready: BTreeMap<usize, Result<PreparedNewSource>> = BTreeMap::new();

        for idx in 0..files.len() {
            if next_dispatch < idx {
                next_dispatch = idx;
            }
            while in_flight < in_flight_limit
                && next_dispatch < files.len()
                && is_pipeline_eligible(&files[next_dispatch], existing_sources)
            {
                if job_tx.send(next_dispatch).is_err() {
                    break;
                }
                next_dispatch += 1;
                in_flight += 1;
            }

            let file = &files[idx];
            scratch.blobs.begin_source();
            let outcome = if idx < next_dispatch {
                let prepared = loop {
                    if let Some(result) = ready.remove(&idx) {
                        break result;
                    }
                    let Ok((done_idx, result)) = result_rx.recv() else {
                        break Err(anyhow::anyhow!(
                            "source preparation worker exited unexpectedly"
                        ));
                    };
                    ready.insert(done_idx, result);
                };
                in_flight = in_flight.saturating_sub(1);
                prepared.and_then(|source| {
                    persist_prepared_new_source(
                        store,
                        file,
                        &source,
                        &mut scratch.blobs,
                        sink.telemetry,
                    )
                })
            } else {
                index_source(
                    store,
                    file,
                    options,
                    Some(existing_sources),
                    scratch,
                    sink.telemetry,
                    sink.progress,
                )
            };
            record_source_outcome(file, outcome, &mut scratch.blobs, sink);
        }
        drop(job_tx);
    });
}

struct WorkerScratch {
    shell_parser: syntax::ShellParser,
    record_buffer: Vec<u8>,
    decompress_buffer: Vec<u8>,
}

impl WorkerScratch {
    fn new() -> Self {
        Self {
            shell_parser: syntax::ShellParser::new(),
            record_buffer: Vec::with_capacity(READ_BUFFER_BYTES),
            decompress_buffer: Vec::new(),
        }
    }

    fn trim_oversized_buffers(&mut self) {
        if self.decompress_buffer.capacity() > WORKER_RETAINED_DECOMPRESS_BYTES {
            self.decompress_buffer.clear();
            self.decompress_buffer
                .shrink_to(WORKER_RETAINED_DECOMPRESS_BYTES);
        }
        if self.record_buffer.capacity() > WORKER_RETAINED_RECORD_BYTES {
            self.record_buffer.clear();
            self.record_buffer.shrink_to(WORKER_RETAINED_RECORD_BYTES);
        }
    }
}

struct PreparedRecord {
    seq: u64,
    byte_offset: u64,
    byte_length: u64,
    raw_hash: String,
    oversized: bool,
    facts: RecordFacts,
}

struct PreparedNewSource {
    adapter: AdapterKind,
    indexed_limit: u64,
    incomplete_tail: bool,
    fingerprint: String,
    records: Vec<PreparedRecord>,
    session: SessionProjection,
    malformed: u64,
    oversized: u64,
    processed_bytes: u64,
    prepare_duration: Duration,
    read_duration: Duration,
    project_duration: Duration,
}

fn open_and_detect_new_source<'a>(
    canonical: &Path,
    options: &IndexOptions,
    decompress_buffer: &'a mut Vec<u8>,
) -> Result<(IndexingSource<'a>, AdapterKind, u64, bool)> {
    let mut source = open_for_indexing(canonical, decompress_buffer)?;
    let adapter = detect_adapter(
        &mut source,
        canonical,
        options.max_record_bytes.max(MAX_ADAPTER_DETECTION_BYTES),
    )?;
    if is_compressed(canonical) && adapter != AdapterKind::Codex {
        return Err(UnrecognizedSource(format!(
            "{}: Zstandard input is supported for Codex traces",
            canonical.display()
        ))
        .into());
    }
    let logical_size = source.len();
    let indexed_limit = source.complete_prefix_length()?;
    let incomplete_tail = indexed_limit < logical_size;
    Ok((source, adapter, indexed_limit, incomplete_tail))
}

fn prepare_new_source(
    discovered: &DiscoveredFile,
    options: &IndexOptions,
    worker: &mut WorkerScratch,
) -> Result<PreparedNewSource> {
    let prepare_started = Instant::now();
    let canonical = &discovered.path;
    let (mut source, adapter, indexed_limit, incomplete_tail) =
        open_and_detect_new_source(canonical, options, &mut worker.decompress_buffer)?;
    let prepare_duration = prepare_started.elapsed();

    let mut hasher = prefix_hasher(indexed_limit);
    let mut reader = source
        .bounded_reader(indexed_limit)
        .with_context(|| format!("failed to rewind {}", canonical.display()))?;
    let mut projector = Projector::new(adapter, options.max_text_bytes, canonical)?;
    let mut projected_sessions = Vec::new();
    let mut records = Vec::new();
    let mut malformed = 0_u64;
    let mut oversized_count = 0_u64;
    let mut processed_bytes = 0_u64;
    let mut read_duration = Duration::ZERO;
    let mut project_duration = Duration::ZERO;
    let mut byte_offset = 0_u64;
    let mut seq = 0_u64;

    loop {
        let read_started = Instant::now();
        let read = read_bounded_record_into(
            &mut reader,
            options.max_record_bytes,
            Some(&mut hasher),
            true,
            &mut worker.record_buffer,
        )?;
        read_duration += read_started.elapsed();
        let (oversized, byte_length, consumed_bytes, raw_hash) = match read {
            RecordReadInto::Complete {
                oversized,
                byte_length,
                consumed_bytes,
                raw_hash,
            } => (oversized, byte_length, consumed_bytes, raw_hash),
            RecordReadInto::Incomplete | RecordReadInto::End => break,
        };

        let project_started = Instant::now();
        let parsed = (!oversized).then(|| serde_json::from_slice::<Value>(&worker.record_buffer));
        let facts = match &parsed {
            None => {
                oversized_count += 1;
                RecordFacts::oversized()
            }
            Some(Err(error)) => {
                malformed += 1;
                RecordFacts::malformed(error.to_string())
            }
            Some(Ok(value)) => record_facts(adapter, value),
        };
        if let Some(Ok(value)) = &parsed {
            projector.push(seq, value);
            projected_sessions.extend(projector.drain_completed());
        }
        project_duration += project_started.elapsed();

        records.push(PreparedRecord {
            seq,
            byte_offset,
            byte_length,
            raw_hash,
            oversized,
            facts,
        });
        processed_bytes += consumed_bytes;
        byte_offset += consumed_bytes;
        seq += 1;
    }

    let project_started = Instant::now();
    let session = finalize_projected_session(
        projector,
        projected_sessions,
        canonical,
        &mut worker.shell_parser,
        options.max_text_bytes,
    )?;
    project_duration += project_started.elapsed();

    let fingerprint = hasher.finalize().to_hex().to_string();
    Ok(PreparedNewSource {
        adapter,
        indexed_limit,
        incomplete_tail,
        fingerprint,
        records,
        session,
        malformed,
        oversized: oversized_count,
        processed_bytes,
        prepare_duration,
        read_duration,
        project_duration,
    })
}

fn persist_prepared_new_source(
    store: &mut Store,
    discovered: &DiscoveredFile,
    prepared: &PreparedNewSource,
    blobs: &mut BlobCache,
    telemetry: &mut IndexTelemetry,
) -> Result<SourceIndexResult> {
    telemetry.record_duration(IndexPhase::Prepare, prepared.prepare_duration);
    telemetry.record_duration(IndexPhase::Read, prepared.read_duration);
    telemetry.record_duration(IndexPhase::Project, prepared.project_duration);
    telemetry.add_processed_bytes(prepared.processed_bytes);

    let canonical = &discovered.path;
    let path_string = canonical.to_string_lossy().into_owned();
    let transaction = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("failed to start index transaction")?;
    let replaced_transcript = antigravity_replacement(&transaction, prepared.adapter, canonical)?;
    let source_id = ensure_source_row(&transaction, None, prepared.adapter, &path_string)?;

    let mut record_ids = HashMap::with_capacity(prepared.records.len());
    let persist_started = Instant::now();
    for record in &prepared.records {
        let started = Instant::now();
        let record_id = insert_record(
            &transaction,
            source_id,
            record.seq,
            record.byte_offset,
            record.byte_length,
            &record.raw_hash,
            record.oversized,
            &record.facts,
        )?;
        telemetry.record_persist(PersistTable::TraceRecords, started.elapsed());
        record_ids.insert(record.seq, record_id);
        telemetry.count_record();
    }
    telemetry.record_duration(IndexPhase::Persist, persist_started.elapsed());

    if let Some(previous) = replaced_transcript {
        clear_projection(&transaction, previous)?;
        transaction.execute("DELETE FROM trace_sources WHERE id = ?1", [previous])?;
    }
    let items = write_projected_session(
        &transaction,
        source_id,
        &prepared.session,
        &record_ids,
        blobs,
        telemetry,
    )?;

    let record_count = u64::try_from(prepared.records.len())?;
    transaction
        .execute(
            "UPDATE trace_sources
                SET adapter = ?2, file_size = ?3, modified_ns = ?4, indexed_bytes = ?5,
                    indexed_lines = ?6, indexed_fingerprint = ?7,
                    status = ?8, last_error = NULL, updated_at = CURRENT_TIMESTAMP
              WHERE id = ?1",
            params![
                source_id,
                prepared.adapter.name(),
                to_sql_i64(discovered.file_size, "file size")?,
                discovered.modified_ns,
                to_sql_i64(prepared.indexed_limit, "indexed bytes")?,
                to_sql_i64(record_count, "indexed lines")?,
                prepared.fingerprint,
                source_status(prepared.incomplete_tail),
            ],
        )
        .context("failed to update source state")?;

    let commit_started = Instant::now();
    transaction.commit().context("failed to commit index")?;
    telemetry.record_duration(IndexPhase::Commit, commit_started.elapsed());

    Ok(SourceIndexResult {
        error: None,
        path: path_string,
        adapter: prepared.adapter.name().to_owned(),
        action: "indexed".to_owned(),
        indexed_records: record_count,
        indexed_items: items,
        indexed_bytes: prepared.indexed_limit,
        file_size: discovered.file_size,
        malformed_records: prepared.malformed,
        oversized_records: prepared.oversized,
        incomplete_tail: prepared.incomplete_tail,
    })
}
