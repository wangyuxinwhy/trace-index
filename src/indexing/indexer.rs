//! Source-to-domain indexing pipeline.
//!
//! Physical Record ingestion stays byte-incremental: an append-only trace does
//! not need to insert the same Records twice. Domain projection is different:
//! Loop boundaries, dual-track pairing, and request-versus-steering all depend
//! on surrounding Records. Whenever a Source changes, its Loops and Items are
//! atomically replaced from the complete indexed Record prefix.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rusqlite::{Transaction, TransactionBehavior, params};
use serde_json::Value;

use super::telemetry::{IndexPhase, IndexTelemetry, PersistTable, ProgressObserver};
#[cfg(test)]
use super::telemetry::{NoopProgress, database_storage_bytes};
use crate::adapters::adapter::{AdapterKind, Projector, detect, record_facts};
use crate::adapters::projection::{RecordFacts, SessionProjection};
use crate::ingest::source::{
    READ_BUFFER_BYTES, RecordRead, complete_prefix_length, discover_jsonl_files,
    fingerprint_prefix_into, modified_ns, prefix_hasher, read_bounded_record,
    read_bounded_record_hashed,
};
use crate::interface::output::{IndexMetrics, IndexReport, SourceIndexResult};
use crate::shell::syntax;
use crate::storage::blob::BlobCache;
use crate::storage::db::{IndexingPolicy, SourceState, Store, display_database_path, to_sql_i64};
use crate::storage::persist::{clear_projection, write_session};

const MAX_ADAPTER_DETECTION_BYTES: usize = 1024 * 1024;

#[cfg(test)]
#[path = "contract_tests.rs"]
mod contract_tests;

#[derive(Debug)]
pub struct IndexOptions {
    pub rebuild: bool,
    pub max_record_bytes: usize,
    pub max_text_bytes: usize,
}

/// Indexes every `.jsonl` file found at the supplied paths.
///
/// # Errors
///
/// Returns an error if discovery, source validation, record reading, or an
/// atomic `SQLite` update fails. Files committed before a later source fails
/// remain valid and queryable.
#[cfg(test)]
fn index_paths(
    store: &mut Store,
    paths: &[PathBuf],
    options: &IndexOptions,
) -> Result<IndexReport> {
    let database_bytes_before = database_storage_bytes(store.path());
    let mut telemetry = IndexTelemetry::new();
    let mut progress = NoopProgress;
    // Tests build into a store that already carries its schema and indexes, so
    // the upsert path is the correct one.
    let mut report = index_paths_internal(store, paths, options, &mut telemetry, &mut progress)?;
    report.metrics = telemetry.finish(database_bytes_before, database_storage_bytes(store.path()));
    Ok(report)
}

pub(crate) fn sync_paths_observed(
    store: &mut Store,
    paths: &[PathBuf],
    options: &IndexOptions,
    telemetry: &mut IndexTelemetry,
    progress: &mut dyn ProgressObserver,
) -> Result<IndexReport> {
    index_paths_internal(store, paths, options, telemetry, progress)
}

fn index_paths_internal(
    store: &mut Store,
    paths: &[PathBuf],
    options: &IndexOptions,
    telemetry: &mut IndexTelemetry,
    progress: &mut dyn ProgressObserver,
) -> Result<IndexReport> {
    store.ensure_indexing_policy(IndexingPolicy {
        max_indexed_record_bytes: options.max_record_bytes,
        max_published_text_bytes: options.max_text_bytes,
    })?;
    let discover_started = Instant::now();
    let files = discover_jsonl_files(paths)?;
    let source_bytes = files.iter().try_fold(0_u64, |total, path| {
        let bytes = path
            .metadata()
            .with_context(|| format!("failed to stat discovered source {}", path.display()))?
            .len();
        Ok::<_, anyhow::Error>(total.saturating_add(bytes))
    })?;
    telemetry.record_duration(IndexPhase::Discover, discover_started.elapsed());
    telemetry.set_corpus(files.len(), source_bytes);
    progress.update(telemetry.progress(IndexPhase::Prepare, None), true);
    if files.is_empty() {
        bail!("no .jsonl files found in the supplied paths");
    }
    let mut report = IndexReport {
        adapters: Vec::new(),
        database: display_database_path(store.path()),
        indexing_policy: store
            .indexing_policy()?
            .context("indexing policy must be established before synchronization")?,
        discovered_files: files.len(),
        indexed_files: 0,
        unchanged_files: 0,
        rebuilt_files: 0,
        indexed_records: 0,
        indexed_items: 0,
        malformed_records: 0,
        oversized_records: 0,
        incomplete_files: 0,
        skipped_files: 0,
        failed_files: 0,
        metrics: IndexMetrics::default(),
        source_results: Vec::with_capacity(files.len()),
    };

    let mut adapters = BTreeSet::new();
    let mut blobs = BlobCache::default();
    for file in &files {
        // One unreadable file must not decide the fate of the other 1,528. A
        // stray or truncated JSONL under a trace root is a local fact; aborting
        // here left every later file unindexed while the database still looked
        // complete. Record-level tolerance already exists one layer down.
        blobs.begin_source();
        let result = match index_source(store, file, options, &mut blobs, telemetry, progress) {
            Ok(result) => {
                blobs.commit_source();
                result
            }
            Err(error) => {
                blobs.rollback_source();
                let action = if is_unrecognized_source(&error) {
                    report.skipped_files += 1;
                    "skipped"
                } else {
                    report.failed_files += 1;
                    "failed"
                };
                report.source_results.push(SourceIndexResult {
                    path: file.display().to_string(),
                    action: action.to_owned(),
                    error: Some(format!("{error:#}")),
                    ..SourceIndexResult::default()
                });
                telemetry.complete_file();
                progress.update(telemetry.progress(IndexPhase::Commit, Some(file)), false);
                continue;
            }
        };
        adapters.insert(result.adapter.clone());
        match result.action.as_str() {
            "unchanged" => report.unchanged_files += 1,
            "rebuilt" => {
                report.indexed_files += 1;
                report.rebuilt_files += 1;
            }
            _ => report.indexed_files += 1,
        }
        report.indexed_items += result.indexed_items;
        report.indexed_records += result.indexed_records;
        report.malformed_records += result.malformed_records;
        report.oversized_records += result.oversized_records;
        if result.incomplete_tail {
            report.incomplete_files += 1;
        }
        report.source_results.push(result);
        telemetry.complete_file();
        progress.update(telemetry.progress(IndexPhase::Commit, Some(file)), false);
    }
    report.adapters = adapters.into_iter().collect();

    Ok(report)
}

/// Per-source scan outcome, kept separate from the reporting struct so the
/// scan loop stays readable.
#[derive(Default)]
struct ScanCounts {
    new_records: u64,
    items: u64,
    malformed: u64,
    oversized: u64,
}

#[allow(
    clippy::too_many_lines,
    reason = "the source scan and its bookkeeping deliberately share one SQLite transaction"
)]
fn index_source(
    store: &mut Store,
    path: &Path,
    options: &IndexOptions,
    blobs: &mut BlobCache,
    telemetry: &mut IndexTelemetry,
    progress: &mut dyn ProgressObserver,
) -> Result<SourceIndexResult> {
    let prepare_started = Instant::now();
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))?;
    let path_string = canonical.to_string_lossy().into_owned();
    let metadata = canonical
        .metadata()
        .with_context(|| format!("failed to stat {}", canonical.display()))?;
    let file_size = metadata.len();
    let file_modified_ns = modified_ns(&metadata);
    let state = store.source_state(&path_string)?;

    // Decided before the file is opened. An unchanged Source needs nothing out
    // of it, and detecting the adapter costs one open and one record read per
    // file in the corpus -- five seconds across 1,933 of them -- to arrive at
    // what the stored row already says. Size and modification time settle
    // whether anything happened; the format cannot have changed if the bytes
    // did not. `--rebuild` re-reads and re-detects, which is what to use after
    // a change to detection itself.
    if let Some(existing) = &state
        && !options.rebuild
        && existing.file_size == file_size
        && existing.indexed_bytes == file_size
        && existing.modified_ns == file_modified_ns
    {
        telemetry.record_duration(IndexPhase::Prepare, prepare_started.elapsed());
        return Ok(SourceIndexResult {
            error: None,
            path: path_string,
            adapter: existing.adapter.clone(),
            action: "unchanged".to_owned(),
            indexed_records: 0,
            indexed_items: 0,
            indexed_bytes: existing.indexed_bytes,
            file_size,
            malformed_records: 0,
            oversized_records: 0,
            incomplete_tail: false,
        });
    }

    let adapter = detect_adapter(
        &canonical,
        options.max_record_bytes.max(MAX_ADAPTER_DETECTION_BYTES),
    )?;

    let mut file = File::open(&canonical)
        .with_context(|| format!("failed to open {}", canonical.display()))?;
    let indexed_limit = complete_prefix_length(&mut file, file_size)?;
    let incomplete_tail = indexed_limit < file_size;
    telemetry.record_duration(IndexPhase::Prepare, prepare_started.elapsed());

    let reset = decide_reset(
        &mut file,
        state.as_ref(),
        adapter,
        indexed_limit,
        options,
        telemetry,
    )?;
    let resume_line = match (&state, reset) {
        (Some(existing), false) => existing.indexed_lines,
        _ => 0,
    };
    let action = match (&state, reset, resume_line) {
        (Some(_), true, _) => "rebuilt",
        (None, _, _) | (Some(_), false, 0) => "indexed",
        (Some(_), false, _) => "appended",
    }
    .to_owned();

    let transaction = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("failed to start index transaction")?;

    let source_id = ensure_source_row(&transaction, state.as_ref(), adapter, &path_string)?;

    // A changed Source republishes its complete domain projection. Physical
    // Records remain incremental; Loops and Items depend on surrounding
    // Records and are therefore replaced atomically. A new Source has no old
    // projection to clear. This distinction is essential during a cold bulk
    // build, where secondary indexes are deferred: clearing every new Source
    // would scan the growing Item table once per Source and make the build
    // superlinear.
    let clear_started = Instant::now();
    if state.is_some() {
        clear_projection(&transaction, source_id)?;
    }
    if reset {
        transaction
            .execute(
                "DELETE FROM trace_records WHERE source_id = ?1",
                [source_id],
            )
            .context("failed to clear records for rebuild")?;
    }

    let mut record_ids = if resume_line > 0 {
        load_record_ids(&transaction, source_id)?
    } else {
        HashMap::new()
    };
    telemetry.record_duration(IndexPhase::Clear, clear_started.elapsed());

    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind {}", canonical.display()))?;
    let mut hasher = prefix_hasher(indexed_limit);
    let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, file.take(indexed_limit));

    let mut projector = Projector::new(adapter, options.max_text_bytes);
    let mut projected_sessions = Vec::new();
    let mut counts = ScanCounts::default();
    let mut byte_offset = 0_u64;
    let mut seq = 0_u64;

    loop {
        let read_started = Instant::now();
        let read =
            read_bounded_record_hashed(&mut reader, options.max_record_bytes, Some(&mut hasher))?;
        telemetry.record_duration(IndexPhase::Read, read_started.elapsed());
        let record = match read {
            RecordRead::Complete(record) => record,
            RecordRead::Incomplete | RecordRead::End => break,
        };

        let project_started = Instant::now();
        let parsed = record.bytes.as_deref().map(serde_json::from_slice::<Value>);
        let facts = match &parsed {
            None => {
                counts.oversized += 1;
                RecordFacts::oversized()
            }
            Some(Err(error)) => {
                counts.malformed += 1;
                RecordFacts::malformed(error.to_string())
            }
            Some(Ok(value)) => record_facts(adapter, value),
        };
        telemetry.record_duration(IndexPhase::Project, project_started.elapsed());

        let persist_started = Instant::now();
        if seq >= resume_line {
            let started = Instant::now();
            let record_id =
                insert_record(&transaction, source_id, seq, byte_offset, &record, &facts)?;
            telemetry.record_persist(PersistTable::TraceRecords, started.elapsed());
            record_ids.insert(seq, record_id);
            counts.new_records += 1;
            telemetry.count_record();
        }
        telemetry.record_duration(IndexPhase::Persist, persist_started.elapsed());

        if let Some(Ok(value)) = &parsed {
            let project_started = Instant::now();
            projector.push(seq, value);
            telemetry.record_duration(IndexPhase::Project, project_started.elapsed());

            projected_sessions.extend(projector.drain_completed());
        }

        telemetry.add_processed_bytes(record.consumed_bytes);
        byte_offset += record.consumed_bytes;
        seq += 1;
        if seq.is_multiple_of(2048) {
            progress.update(
                telemetry.progress(IndexPhase::Persist, Some(&canonical)),
                false,
            );
        }
    }

    projected_sessions.extend(projector.finish());
    if projected_sessions.len() != 1 {
        return Err(anyhow::Error::new(UnrecognizedSource(format!(
            "{} must establish exactly one Session; Adapter projected {}",
            canonical.display(),
            projected_sessions.len(),
        ))));
    }
    let mut session = projected_sessions.pop().expect("length checked above");
    if !session.identity_confirmed {
        return Err(anyhow::Error::new(UnrecognizedSource(format!(
            "{} does not contain a structurally confirmed Session identity",
            canonical.display(),
        ))));
    }
    syntax::parse_declared_commands(&mut session, options.max_text_bytes);
    counts.items += write_projected_session(
        &transaction,
        source_id,
        &session,
        &record_ids,
        blobs,
        telemetry,
    )?;

    let fingerprint = hasher.finalize().to_hex().to_string();
    transaction
        .execute(
            "UPDATE trace_sources
                SET adapter = ?2, file_size = ?3, modified_ns = ?4, indexed_bytes = ?5,
                    indexed_lines = ?6, indexed_fingerprint = ?7,
                    status = ?8, last_error = NULL, updated_at = CURRENT_TIMESTAMP
              WHERE id = ?1",
            params![
                source_id,
                adapter.name(),
                to_sql_i64(file_size, "file size")?,
                file_modified_ns,
                to_sql_i64(indexed_limit, "indexed bytes")?,
                to_sql_i64(seq, "indexed lines")?,
                fingerprint,
                if incomplete_tail {
                    "partial"
                } else {
                    "complete"
                },
            ],
        )
        .context("failed to update source state")?;

    let commit_started = Instant::now();
    transaction.commit().context("failed to commit index")?;
    telemetry.record_duration(IndexPhase::Commit, commit_started.elapsed());

    Ok(SourceIndexResult {
        error: None,
        path: path_string,
        adapter: adapter.name().to_owned(),
        action,
        indexed_records: counts.new_records,
        indexed_items: counts.items,
        indexed_bytes: indexed_limit,
        file_size,
        malformed_records: counts.malformed,
        oversized_records: counts.oversized,
        incomplete_tail,
    })
}

fn write_projected_session(
    transaction: &Transaction<'_>,
    source_id: i64,
    session: &SessionProjection,
    record_ids: &HashMap<u64, i64>,
    blobs: &mut BlobCache,
    telemetry: &mut IndexTelemetry,
) -> Result<u64> {
    let persist_started = Instant::now();
    let counts = write_session(
        transaction,
        source_id,
        session,
        record_ids,
        blobs,
        telemetry,
    )?;
    telemetry.record_duration(IndexPhase::Persist, persist_started.elapsed());
    Ok(counts.items)
}

/// Decides whether the stored prefix is still a prefix of the current file.
///
/// A rollout file only ever grows, so a changed prefix means it was rewritten
/// and every derived row for it is stale.
fn decide_reset(
    file: &mut File,
    state: Option<&SourceState>,
    adapter: AdapterKind,
    indexed_limit: u64,
    options: &IndexOptions,
    telemetry: &mut IndexTelemetry,
) -> Result<bool> {
    let Some(existing) = state else {
        return Ok(true);
    };
    if options.rebuild
        || existing.adapter != adapter.name()
        || existing.indexed_bytes > indexed_limit
    {
        return Ok(true);
    }
    if existing.indexed_bytes == 0 {
        return Ok(true);
    }

    let fingerprint_started = Instant::now();
    let mut scratch = prefix_hasher(existing.indexed_bytes);
    let current = fingerprint_prefix_into(file, existing.indexed_bytes, &mut scratch)?;
    telemetry.record_duration(IndexPhase::Fingerprint, fingerprint_started.elapsed());
    telemetry.add_fingerprint_bytes(existing.indexed_bytes);
    Ok(current != existing.indexed_fingerprint)
}

fn ensure_source_row(
    transaction: &Transaction<'_>,
    state: Option<&SourceState>,
    adapter: AdapterKind,
    path: &str,
) -> Result<i64> {
    if let Some(existing) = state {
        transaction
            .execute(
                "UPDATE trace_sources SET adapter = ?2, status = 'indexing', last_error = NULL
                  WHERE id = ?1",
                params![existing.id, adapter.name()],
            )
            .context("failed to mark source as indexing")?;
        return Ok(existing.id);
    }
    transaction
        .execute(
            "INSERT INTO trace_sources(adapter, path, status) VALUES (?1, ?2, 'indexing')",
            params![adapter.name(), path],
        )
        .context("failed to register source")?;
    Ok(transaction.last_insert_rowid())
}

fn load_record_ids(transaction: &Transaction<'_>, source_id: i64) -> Result<HashMap<u64, i64>> {
    let mut statement = transaction
        .prepare("SELECT seq, id FROM trace_records WHERE source_id = ?1")
        .context("failed to prepare record lookup")?;
    let rows = statement
        .query_map([source_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .context("failed to load existing records")?;
    let mut ids = HashMap::new();
    for row in rows {
        let (seq, id) = row.context("failed to read existing record")?;
        ids.insert(
            u64::try_from(seq).context("stored sequence is negative")?,
            id,
        );
    }
    Ok(ids)
}

fn insert_record(
    transaction: &Transaction<'_>,
    source_id: i64,
    seq: u64,
    byte_offset: u64,
    record: &crate::ingest::source::BoundedRecord,
    facts: &RecordFacts,
) -> Result<i64> {
    transaction
        .prepare_cached(
            "INSERT INTO trace_records(
                 source_id, seq, byte_offset, byte_length, raw_hash, ts_ms, native_type,
                 parse_status, parse_error, oversized)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .context("failed to prepare record insert")?
        .execute(params![
            source_id,
            to_sql_i64(seq, "record sequence")?,
            to_sql_i64(byte_offset, "record byte offset")?,
            to_sql_i64(record.byte_length, "record byte length")?,
            record.raw_hash,
            facts.ts_ms,
            facts.native_type,
            facts.parse_status,
            facts.parse_error,
            i64::from(record.bytes.is_none()),
        ])
        .context("failed to store record")?;
    Ok(transaction.last_insert_rowid())
}

/// A `.jsonl` file under a trace root that is not a trace this tool reads.
///
/// This is not an indexing failure. A trace root routinely holds files written
/// by something else, and a file whose first Record has not finished being
/// written is simply not readable yet. Both are reported as skipped, leaving
/// `failed` to mean a trace this tool could not index.
#[derive(Debug)]
pub(crate) struct UnrecognizedSource(String);

impl std::fmt::Display for UnrecognizedSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for UnrecognizedSource {}

pub(crate) fn is_unrecognized_source(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(<dyn std::error::Error>::is::<UnrecognizedSource>)
}

fn detect_adapter(path: &Path, max_record_bytes: usize) -> Result<AdapterKind> {
    // An I/O failure here is a real failure: the file may well be a trace we
    // simply could not read. Only the format verdict decides "not ours".
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, file);
    match read_bounded_record(&mut reader, max_record_bytes)? {
        RecordRead::Complete(record) => {
            let bytes = record.bytes.as_deref().unwrap_or_default();
            detect(bytes).map_err(|error| {
                anyhow::Error::new(UnrecognizedSource(format!("{}: {error:#}", path.display())))
            })
        }
        RecordRead::Incomplete | RecordRead::End => Err(anyhow::Error::new(UnrecognizedSource(
            format!("{} has no complete first record", path.display()),
        ))),
    }
}
