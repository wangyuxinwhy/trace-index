//! Source-to-domain indexing pipeline.
//!
//! Physical Record ingestion stays byte-incremental: an append-only trace does
//! not need to insert the same Records twice. Domain projection is recomputed
//! over the complete indexed Record prefix because Loop boundaries, dual-track
//! pairing, and request-versus-steering depend on surrounding Records; for
//! append-only updates `sync_session` diffs that projection in place to avoid
//! rewriting unchanged prefix Loops, Items, and FTS5 rows.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension as _, Transaction, TransactionBehavior, params};
use serde_json::Value;

use super::pipeline::{
    MIN_PIPELINE_FILES, index_files_pipelined, is_pipeline_eligible, load_existing_source_paths,
    pipeline_worker_count,
};
use super::telemetry::{IndexPhase, IndexTelemetry, PersistTable, ProgressObserver};
#[cfg(test)]
use super::telemetry::{NoopProgress, database_storage_bytes};
use crate::adapters::adapter::{AdapterKind, Projector, detect_at, record_facts};
use crate::adapters::projection::{RecordFacts, SessionProjection};
use crate::adapters::runtime::antigravity;
use crate::ingest::source::{
    IndexingSource, READ_BUFFER_BYTES, RecordRead, RecordReadInto, alternate_encoding,
    discover_jsonl_files, is_compressed, modified_ns, open_for_indexing, prefix_hasher,
    read_bounded_record, read_bounded_record_into,
};
use crate::interface::output::{IndexMetrics, IndexReport, SourceIndexResult};
use crate::shell::syntax;
use crate::storage::blob::BlobCache;
use crate::storage::db::{
    IndexingPolicy, SourceState, Store, display_database_path, source_state, to_sql_i64,
};
use crate::storage::persist::{clear_projection, write_session};
use crate::storage::persist_diff::sync_session;

pub(super) const MAX_ADAPTER_DETECTION_BYTES: usize = 1024 * 1024;

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

pub(super) struct RunSink<'a> {
    pub(super) telemetry: &'a mut IndexTelemetry,
    pub(super) progress: &'a mut dyn ProgressObserver,
    pub(super) adapters: &'a mut BTreeSet<String>,
    pub(super) report: &'a mut IndexReport,
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
    let (files, source_bytes) = discover_sources(paths)?;
    telemetry.record_duration(IndexPhase::Discover, discover_started.elapsed());
    telemetry.set_corpus(files.len(), source_bytes);
    progress.update(telemetry.progress(IndexPhase::Prepare, None), true);
    if files.is_empty() {
        bail!("no trace files found in the supplied paths");
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
    let mut scratch = SyncScratch::new(store)?;
    let worker_count = pipeline_worker_count(files.len());
    let existing_paths = if worker_count > 1 && !options.rebuild {
        load_existing_source_paths(store)?
    } else {
        HashSet::new()
    };
    let parallel_candidates = if worker_count > 1 && !options.rebuild {
        files
            .iter()
            .filter(|file| is_pipeline_eligible(file, &existing_paths))
            .count()
    } else {
        0
    };

    let mut sink = RunSink {
        telemetry,
        progress,
        adapters: &mut adapters,
        report: &mut report,
    };
    if parallel_candidates >= MIN_PIPELINE_FILES {
        index_files_pipelined(
            store,
            &files,
            &existing_paths,
            worker_count,
            options,
            &mut scratch,
            &mut sink,
        );
    } else {
        for file in &files {
            // One unreadable file must not decide the fate of the other 1,528. A
            // stray or truncated JSONL under a trace root is a local fact; aborting
            // here left every later file unindexed while the database still looked
            // complete. Record-level tolerance already exists one layer down.
            scratch.blobs.begin_source();
            let outcome = index_source(
                store,
                file,
                options,
                &mut scratch,
                sink.telemetry,
                sink.progress,
            );
            record_source_outcome(file, outcome, &mut scratch.blobs, &mut sink);
        }
    }
    report.adapters = adapters.into_iter().collect();

    Ok(report)
}

pub(super) fn record_source_outcome(
    file: &DiscoveredFile,
    outcome: Result<SourceIndexResult>,
    blobs: &mut BlobCache,
    sink: &mut RunSink<'_>,
) {
    let result = match outcome {
        Ok(result) => {
            blobs.commit_source();
            result
        }
        Err(error) => {
            blobs.rollback_source();
            let action = if is_unrecognized_source(&error) {
                sink.report.skipped_files += 1;
                "skipped"
            } else {
                sink.report.failed_files += 1;
                "failed"
            };
            sink.report.source_results.push(SourceIndexResult {
                path: file.path.display().to_string(),
                action: action.to_owned(),
                error: Some(format!("{error:#}")),
                ..SourceIndexResult::default()
            });
            sink.telemetry.complete_file();
            sink.progress.update(
                sink.telemetry
                    .progress(IndexPhase::Commit, Some(&file.path)),
                false,
            );
            return;
        }
    };
    sink.adapters.insert(result.adapter.clone());
    match result.action.as_str() {
        "unchanged" => sink.report.unchanged_files += 1,
        "rebuilt" => {
            sink.report.indexed_files += 1;
            sink.report.rebuilt_files += 1;
        }
        _ => sink.report.indexed_files += 1,
    }
    sink.report.indexed_items += result.indexed_items;
    sink.report.indexed_records += result.indexed_records;
    sink.report.malformed_records += result.malformed_records;
    sink.report.oversized_records += result.oversized_records;
    if result.incomplete_tail {
        sink.report.incomplete_files += 1;
    }
    sink.report.source_results.push(result);
    sink.telemetry.complete_file();
    sink.progress.update(
        sink.telemetry
            .progress(IndexPhase::Commit, Some(&file.path)),
        false,
    );
}

pub(super) fn finalize_projected_session(
    projector: Projector,
    mut projected_sessions: Vec<SessionProjection>,
    canonical: &Path,
    shell_parser: &mut syntax::ShellParser,
    max_text_bytes: usize,
) -> Result<SessionProjection> {
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
    shell_parser.parse_declared_commands(&mut session, max_text_bytes);
    Ok(session)
}

/// Canonical discovered Source paired with the `stat` metadata already read
/// during discovery so `index_source` avoids duplicate `realpath`/`stat` calls.
pub(super) struct DiscoveredFile {
    pub(super) path: PathBuf,
    pub(super) file_size: u64,
    pub(super) modified_ns: i64,
}

fn discover_sources(paths: &[PathBuf]) -> Result<(Vec<DiscoveredFile>, u64)> {
    let discovered_paths = discover_jsonl_files(paths)?;
    let mut files = Vec::with_capacity(discovered_paths.len());
    let mut source_bytes = 0_u64;
    for path in discovered_paths {
        let metadata = path
            .metadata()
            .with_context(|| format!("failed to stat discovered source {}", path.display()))?;
        let file_size = metadata.len();
        source_bytes = source_bytes.saturating_add(file_size);
        files.push(DiscoveredFile {
            path,
            file_size,
            modified_ns: modified_ns(&metadata),
        });
    }
    Ok((files, source_bytes))
}

/// Run-scoped reusable state shared across Sources in one sync.
pub(super) struct SyncScratch {
    pub(super) blobs: BlobCache,
    shell_parser: syntax::ShellParser,
    record_buffer: Vec<u8>,
    decompress_buffer: Vec<u8>,
}

impl SyncScratch {
    fn new(store: &Store) -> Result<Self> {
        Ok(Self {
            blobs: BlobCache::for_connection(store.connection())?,
            shell_parser: syntax::ShellParser::new(),
            record_buffer: Vec::with_capacity(READ_BUFFER_BYTES),
            decompress_buffer: Vec::new(),
        })
    }
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

struct ScannedSource {
    session: SessionProjection,
    fingerprint: String,
    line_count: u64,
    counts: ScanCounts,
}

struct OpenedSource<'a> {
    source: IndexingSource<'a>,
    adapter: AdapterKind,
    indexed_limit: u64,
    incomplete_tail: bool,
}

fn open_source_for_sync<'a>(
    canonical: &Path,
    options: &IndexOptions,
    decompress_buffer: &'a mut Vec<u8>,
) -> Result<OpenedSource<'a>> {
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
    Ok(OpenedSource {
        source,
        adapter,
        indexed_limit,
        incomplete_tail,
    })
}

fn prepare_source_for_scan(
    transaction: &Transaction<'_>,
    source_id: i64,
    has_existing_state: bool,
    reset: bool,
    incremental_projection: bool,
    resume_line: u64,
    telemetry: &mut IndexTelemetry,
) -> Result<HashMap<u64, i64>> {
    // A new or reset Source writes its full projection from scratch. An
    // append-only update to an existing Source keeps its existing Loops and
    // Items in place until `sync_session` diffs the prefix and rewrites only
    // the changed tail, avoiding multi-second FTS5/index churn on hot sessions.
    let clear_started = Instant::now();
    if has_existing_state && !incremental_projection {
        clear_projection(transaction, source_id)?;
    }
    if reset {
        transaction
            .execute(
                "DELETE FROM trace_records WHERE source_id = ?1",
                [source_id],
            )
            .context("failed to clear records for rebuild")?;
    }
    let record_ids = if resume_line > 0 {
        load_record_ids(transaction, source_id)?
    } else {
        HashMap::new()
    };
    telemetry.record_duration(IndexPhase::Clear, clear_started.elapsed());
    Ok(record_ids)
}

struct SourceUpdate {
    source_id: i64,
    has_existing_state: bool,
    reset: bool,
    resume_line: u64,
    action: String,
    incremental_projection: bool,
    replaced_transcript: Option<i64>,
}

enum SourceUpdatePlan {
    Unchanged(SourceIndexResult),
    Update(SourceUpdate),
}

fn plan_source_update(
    transaction: &Transaction<'_>,
    discovered: &DiscoveredFile,
    path_string: &str,
    opened: &mut OpenedSource<'_>,
    options: &IndexOptions,
    telemetry: &mut IndexTelemetry,
) -> Result<SourceUpdatePlan> {
    let canonical = &discovered.path;
    let (state, encoding_changed) =
        encoding_source_state(transaction, opened.adapter, canonical, path_string)?;

    let reset = decide_reset(
        &mut opened.source,
        state.as_ref(),
        opened.adapter,
        opened.indexed_limit,
        options,
        telemetry,
    )?;
    if encoding_changed && reset && !options.rebuild {
        return Err(UnrecognizedSource(format!(
            "{} does not preserve the indexed prefix of its sibling encoding; pass --rebuild to replace it",
            canonical.display()
        ))
        .into());
    }
    let replaced_transcript = antigravity_replacement(transaction, opened.adapter, canonical)?;
    if let Some(existing) = state.as_ref()
        && !reset
        && replaced_transcript.is_none()
        && opened.indexed_limit == existing.indexed_bytes
    {
        update_unchanged_source_locator(
            transaction,
            existing.id,
            path_string,
            discovered.file_size,
            discovered.modified_ns,
            opened.incomplete_tail,
        )?;
        return Ok(SourceUpdatePlan::Unchanged(unchanged_result(
            path_string.to_owned(),
            existing,
            discovered.file_size,
            opened.incomplete_tail,
        )));
    }
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
    let has_existing_state = state.is_some();
    let source_id = ensure_source_row(transaction, state.as_ref(), opened.adapter, path_string)?;
    let incremental_projection = !reset && has_existing_state && replaced_transcript.is_none();
    Ok(SourceUpdatePlan::Update(SourceUpdate {
        source_id,
        has_existing_state,
        reset,
        resume_line,
        action,
        incremental_projection,
        replaced_transcript,
    }))
}

pub(super) fn index_source(
    store: &mut Store,
    discovered: &DiscoveredFile,
    options: &IndexOptions,
    scratch: &mut SyncScratch,
    telemetry: &mut IndexTelemetry,
    progress: &mut dyn ProgressObserver,
) -> Result<SourceIndexResult> {
    let prepare_started = Instant::now();
    let canonical = &discovered.path;
    let path_string = canonical.to_string_lossy().into_owned();
    let file_size = discovered.file_size;
    let file_modified_ns = discovered.modified_ns;

    // Decided before the file is opened. Size and modification time settle
    // whether anything happened; `--rebuild` re-reads and re-detects.
    if let Some(existing) = store.source_state(&path_string)?
        && !options.rebuild
        && existing.file_size == file_size
        && existing.complete
        && existing.modified_ns == file_modified_ns
    {
        telemetry.record_duration(IndexPhase::Prepare, prepare_started.elapsed());
        return Ok(unchanged_result(path_string, &existing, file_size, false));
    }

    let SyncScratch {
        blobs,
        shell_parser,
        record_buffer,
        decompress_buffer,
    } = scratch;
    let mut opened = open_source_for_sync(canonical, options, decompress_buffer)?;
    telemetry.record_duration(IndexPhase::Prepare, prepare_started.elapsed());

    let transaction = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("failed to start index transaction")?;
    let update = match plan_source_update(
        &transaction,
        discovered,
        &path_string,
        &mut opened,
        options,
        telemetry,
    )? {
        SourceUpdatePlan::Unchanged(result) => {
            transaction.commit()?;
            return Ok(result);
        }
        SourceUpdatePlan::Update(update) => update,
    };

    let mut record_ids = prepare_source_for_scan(
        &transaction,
        update.source_id,
        update.has_existing_state,
        update.reset,
        update.incremental_projection,
        update.resume_line,
        telemetry,
    )?;

    let mut scanned = scan_and_project_source(&mut ScanContext {
        transaction: &transaction,
        source_id: update.source_id,
        canonical,
        adapter: opened.adapter,
        source: &mut opened.source,
        indexed_limit: opened.indexed_limit,
        resume_line: update.resume_line,
        options,
        record_ids: &mut record_ids,
        record_buffer,
        shell_parser,
        telemetry,
        progress,
    })?;
    let commit = CommitSourceParams {
        source_id: update.source_id,
        adapter: opened.adapter,
        file_size,
        file_modified_ns,
        indexed_limit: opened.indexed_limit,
        incomplete_tail: opened.incomplete_tail,
        incremental_projection: update.incremental_projection,
        replaced_transcript: update.replaced_transcript,
    };
    persist_and_commit_source(
        transaction,
        commit,
        &mut scanned,
        &record_ids,
        blobs,
        telemetry,
    )?;

    Ok(SourceIndexResult {
        error: None,
        path: path_string,
        adapter: opened.adapter.name().to_owned(),
        action: update.action,
        indexed_records: scanned.counts.new_records,
        indexed_items: scanned.counts.items,
        indexed_bytes: opened.indexed_limit,
        file_size,
        malformed_records: scanned.counts.malformed,
        oversized_records: scanned.counts.oversized,
        incomplete_tail: opened.incomplete_tail,
    })
}

#[derive(Clone, Copy)]
struct CommitSourceParams {
    source_id: i64,
    adapter: AdapterKind,
    file_size: u64,
    file_modified_ns: i64,
    indexed_limit: u64,
    incomplete_tail: bool,
    incremental_projection: bool,
    replaced_transcript: Option<i64>,
}

fn persist_and_commit_source(
    transaction: Transaction<'_>,
    meta: CommitSourceParams,
    scanned: &mut ScannedSource,
    record_ids: &HashMap<u64, i64>,
    blobs: &mut BlobCache,
    telemetry: &mut IndexTelemetry,
) -> Result<()> {
    if let Some(previous) = meta.replaced_transcript {
        // Replace the old projection only after establishing the new Source.
        // Failure rolls both operations back together.
        clear_projection(&transaction, previous)?;
        transaction.execute("DELETE FROM trace_sources WHERE id = ?1", [previous])?;
    }
    scanned.counts.items += if meta.incremental_projection {
        sync_projected_session(
            &transaction,
            meta.source_id,
            &scanned.session,
            record_ids,
            blobs,
            telemetry,
        )?
    } else {
        write_projected_session(
            &transaction,
            meta.source_id,
            &scanned.session,
            record_ids,
            blobs,
            telemetry,
        )?
    };
    finalize_source_row(
        &transaction,
        meta.source_id,
        meta.adapter,
        meta.file_size,
        meta.file_modified_ns,
        meta.indexed_limit,
        scanned.line_count,
        &scanned.fingerprint,
        meta.incomplete_tail,
    )?;
    let commit_started = Instant::now();
    transaction.commit().context("failed to commit index")?;
    telemetry.record_duration(IndexPhase::Commit, commit_started.elapsed());
    Ok(())
}

struct ScanContext<'a, 'buf, 'connection> {
    transaction: &'a Transaction<'connection>,
    source_id: i64,
    canonical: &'a Path,
    adapter: AdapterKind,
    source: &'a mut IndexingSource<'buf>,
    indexed_limit: u64,
    resume_line: u64,
    options: &'a IndexOptions,
    record_ids: &'a mut HashMap<u64, i64>,
    record_buffer: &'a mut Vec<u8>,
    shell_parser: &'a mut syntax::ShellParser,
    telemetry: &'a mut IndexTelemetry,
    progress: &'a mut dyn ProgressObserver,
}

fn classify_record(
    adapter: AdapterKind,
    parsed: Option<&serde_json::Result<Value>>,
    persist_record: bool,
    counts: &mut ScanCounts,
) -> Option<RecordFacts> {
    match parsed {
        None => {
            counts.oversized += 1;
            persist_record.then(RecordFacts::oversized)
        }
        Some(Err(error)) => {
            counts.malformed += 1;
            persist_record.then(|| RecordFacts::malformed(error.to_string()))
        }
        Some(Ok(value)) => persist_record.then(|| record_facts(adapter, value)),
    }
}

fn scan_and_project_source(ctx: &mut ScanContext<'_, '_, '_>) -> Result<ScannedSource> {
    let mut hasher = prefix_hasher(ctx.indexed_limit);
    let mut reader = ctx
        .source
        .bounded_reader(ctx.indexed_limit)
        .with_context(|| format!("failed to rewind {}", ctx.canonical.display()))?;

    let mut projector = Projector::new(ctx.adapter, ctx.options.max_text_bytes, ctx.canonical)?;
    let mut projected_sessions = Vec::new();
    let mut counts = ScanCounts::default();
    let mut byte_offset = 0_u64;
    let mut seq = 0_u64;

    loop {
        let persist_record = seq >= ctx.resume_line;
        let read_started = Instant::now();
        let read = read_bounded_record_into(
            &mut reader,
            ctx.options.max_record_bytes,
            Some(&mut hasher),
            persist_record,
            ctx.record_buffer,
        )?;
        ctx.telemetry
            .record_duration(IndexPhase::Read, read_started.elapsed());
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
        let parsed = (!oversized).then(|| serde_json::from_slice::<Value>(ctx.record_buffer));
        let facts = classify_record(ctx.adapter, parsed.as_ref(), persist_record, &mut counts);
        ctx.telemetry
            .record_duration(IndexPhase::Project, project_started.elapsed());

        let persist_started = Instant::now();
        if let Some(facts) = facts.as_ref() {
            let started = Instant::now();
            let record_id = insert_record(
                ctx.transaction,
                ctx.source_id,
                seq,
                byte_offset,
                byte_length,
                &raw_hash,
                oversized,
                facts,
            )?;
            ctx.telemetry
                .record_persist(PersistTable::TraceRecords, started.elapsed());
            ctx.record_ids.insert(seq, record_id);
            counts.new_records += 1;
            ctx.telemetry.count_record();
        }
        ctx.telemetry
            .record_duration(IndexPhase::Persist, persist_started.elapsed());

        if let Some(Ok(value)) = &parsed {
            let project_started = Instant::now();
            projector.push(seq, value);
            ctx.telemetry
                .record_duration(IndexPhase::Project, project_started.elapsed());
            projected_sessions.extend(projector.drain_completed());
        }

        ctx.telemetry.add_processed_bytes(consumed_bytes);
        byte_offset += consumed_bytes;
        seq += 1;
        if seq.is_multiple_of(2048) {
            ctx.progress.update(
                ctx.telemetry
                    .progress(IndexPhase::Persist, Some(ctx.canonical)),
                false,
            );
        }
    }

    let session = finalize_projected_session(
        projector,
        projected_sessions,
        ctx.canonical,
        ctx.shell_parser,
        ctx.options.max_text_bytes,
    )?;
    let fingerprint = hasher.finalize().to_hex().to_string();
    Ok(ScannedSource {
        session,
        fingerprint,
        line_count: seq,
        counts,
    })
}

fn update_unchanged_source_locator(
    transaction: &Transaction<'_>,
    source_id: i64,
    path_string: &str,
    file_size: u64,
    file_modified_ns: i64,
    incomplete_tail: bool,
) -> Result<()> {
    // A metadata change or encoding switch can leave every published byte
    // identical. Update the physical locator without rebuilding evidence.
    transaction.execute(
        "UPDATE trace_sources
            SET path = ?2, file_size = ?3, modified_ns = ?4, status = ?5,
                last_error = NULL, updated_at = CURRENT_TIMESTAMP
          WHERE id = ?1",
        params![
            source_id,
            path_string,
            to_sql_i64(file_size, "file size")?,
            file_modified_ns,
            source_status(incomplete_tail),
        ],
    )?;
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "checkpoint columns map one-to-one to the trace_sources update statement"
)]
fn finalize_source_row(
    transaction: &Transaction<'_>,
    source_id: i64,
    adapter: AdapterKind,
    file_size: u64,
    file_modified_ns: i64,
    indexed_limit: u64,
    line_count: u64,
    fingerprint: &str,
    incomplete_tail: bool,
) -> Result<()> {
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
                to_sql_i64(line_count, "indexed lines")?,
                fingerprint,
                source_status(incomplete_tail),
            ],
        )
        .context("failed to update source state")?;
    Ok(())
}

fn unchanged_result(
    path: String,
    existing: &SourceState,
    file_size: u64,
    incomplete_tail: bool,
) -> SourceIndexResult {
    SourceIndexResult {
        path,
        adapter: existing.adapter.clone(),
        action: "unchanged".to_owned(),
        indexed_bytes: existing.indexed_bytes,
        file_size,
        incomplete_tail,
        ..SourceIndexResult::default()
    }
}

pub(super) fn source_status(incomplete_tail: bool) -> &'static str {
    if incomplete_tail {
        "partial"
    } else {
        "complete"
    }
}

pub(super) fn antigravity_replacement(
    transaction: &Transaction<'_>,
    adapter: AdapterKind,
    path: &Path,
) -> Result<Option<i64>> {
    if adapter != AdapterKind::Antigravity {
        return Ok(None);
    }
    let (uuid, full) = antigravity::transcript(path).context("invalid Antigravity locator")?;
    let previous = transaction
        .query_row(
            "SELECT src.id, src.path
               FROM domain_sessions s
               JOIN session_sources ss ON ss.session_id = s.id
               JOIN trace_sources src ON src.id = ss.source_id
              WHERE s.runtime = ?1 AND s.native_id = ?2",
            params![adapter.name(), uuid],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((source_id, previous)) = previous else {
        return Ok(None);
    };
    let previous = Path::new(&previous);
    if previous == path {
        return Ok(None);
    }
    if full && antigravity::transcript(previous).is_some_and(|(_, full)| !full) {
        return Ok(Some(source_id));
    }
    Err(anyhow::Error::new(UnrecognizedSource(format!(
        "Antigravity conversation {uuid} is already indexed from {}",
        previous.display()
    ))))
}

pub(super) fn write_projected_session(
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

fn sync_projected_session(
    transaction: &Transaction<'_>,
    source_id: i64,
    session: &SessionProjection,
    record_ids: &HashMap<u64, i64>,
    blobs: &mut BlobCache,
    telemetry: &mut IndexTelemetry,
) -> Result<u64> {
    let persist_started = Instant::now();
    let counts = sync_session(
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
    source: &mut IndexingSource<'_>,
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
    let current = source.fingerprint_prefix(existing.indexed_bytes)?;
    telemetry.record_duration(IndexPhase::Fingerprint, fingerprint_started.elapsed());
    telemetry.add_fingerprint_bytes(existing.indexed_bytes);
    Ok(current != existing.indexed_fingerprint)
}

fn encoding_source_state(
    transaction: &Transaction<'_>,
    adapter: AdapterKind,
    path: &Path,
    locator: &str,
) -> Result<(Option<SourceState>, bool)> {
    let state = source_state(transaction, locator)?;
    if state.is_some() || adapter != AdapterKind::Codex {
        return Ok((state, false));
    }
    let alternate = source_state(transaction, &alternate_encoding(path).to_string_lossy())?
        .filter(|existing| existing.adapter == adapter.name());
    let encoding_changed = alternate.is_some();
    Ok((alternate, encoding_changed))
}

pub(super) fn ensure_source_row(
    transaction: &Transaction<'_>,
    state: Option<&SourceState>,
    adapter: AdapterKind,
    path: &str,
) -> Result<i64> {
    if let Some(existing) = state {
        transaction
            .execute(
                "UPDATE trace_sources SET adapter = ?2, path = ?3, status = 'indexing', last_error = NULL
                  WHERE id = ?1",
                params![existing.id, adapter.name(), path],
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

#[allow(
    clippy::too_many_arguments,
    reason = "physical Record columns map one-to-one to the insert statement"
)]
pub(super) fn insert_record(
    transaction: &Transaction<'_>,
    source_id: i64,
    seq: u64,
    byte_offset: u64,
    byte_length: u64,
    raw_hash: &str,
    oversized: bool,
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
            to_sql_i64(byte_length, "record byte length")?,
            raw_hash,
            facts.ts_ms,
            facts.native_type,
            facts.parse_status,
            facts.parse_error,
            i64::from(oversized),
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
pub(crate) struct UnrecognizedSource(pub(super) String);

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

pub(super) fn detect_adapter(
    source: &mut IndexingSource<'_>,
    path: &Path,
    max_record_bytes: usize,
) -> Result<AdapterKind> {
    // An I/O failure here is a real failure: the file may well be a trace we
    // simply could not read. Only the format verdict decides "not ours".
    let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, source);
    match read_bounded_record(&mut reader, max_record_bytes)? {
        RecordRead::Complete(bytes) => {
            let bytes = bytes.as_deref().unwrap_or_default();
            detect_at(bytes, Some(path)).map_err(|error| {
                anyhow::Error::new(UnrecognizedSource(format!("{}: {error:#}", path.display())))
            })
        }
        RecordRead::Incomplete | RecordRead::End => Err(anyhow::Error::new(UnrecognizedSource(
            format!("{} has no complete first record", path.display()),
        ))),
    }
}
