//! Source-to-domain indexing pipeline.
//!
//! Physical Record ingestion stays byte-incremental: an append-only trace does
//! not need to insert the same Records twice. Domain projection is recomputed
//! over the complete indexed Record prefix because Loop boundaries, dual-track
//! pairing, and request-versus-steering depend on surrounding Records; for
//! append-only updates `sync_session` diffs that projection in place to avoid
//! rewriting unchanged prefix Loops, Items, and FTS5 rows.

use std::collections::{BTreeSet, HashMap};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension as _, Transaction, TransactionBehavior, params};
use serde_json::Value;

use super::pipeline::{
    MIN_PIPELINE_FILES, index_files_pipelined, is_pipeline_eligible, pipeline_worker_count,
};
use super::telemetry::{IndexPhase, IndexTelemetry, PersistTable, ProgressObserver};
#[cfg(test)]
use super::telemetry::{NoopProgress, database_storage_bytes};
use crate::adapters::adapter::{AdapterKind, Projector, detect_at, record_facts};
use crate::adapters::projection::{RecordFacts, SessionProjection};
use crate::adapters::runtime::antigravity;
use crate::ingest::source::{
    IndexingSource, PrefixVerify, READ_BUFFER_BYTES, RecordRead, RecordReadInto,
    alternate_encoding, discover_jsonl_files, is_compressed, modified_ns, open_for_indexing,
    prefix_hasher, read_bounded_record, read_bounded_record_into, read_bounded_record_with_verify,
};
use crate::interface::output::{IndexMetrics, IndexReport, SourceIndexResult};
use crate::shell::syntax;
use crate::storage::blob::BlobCache;
use crate::storage::db::{
    IndexingPolicy, SourceState, Store, display_database_path, source_state, to_sql_i64,
};
use crate::storage::persist::{clear_projection, write_session};
use crate::storage::persist_diff::{IncrementalSyncOptions, append_antigravity_tail, sync_session};

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
    let existing_sources = if files.len() >= MIN_PIPELINE_FILES && !options.rebuild {
        let preload_started = Instant::now();
        let states = store.load_all_source_states()?;
        telemetry.record_duration(IndexPhase::Prepare, preload_started.elapsed());
        Some(states)
    } else {
        None
    };
    let parallel_candidates = if worker_count > 1
        && let Some(existing_sources) = existing_sources.as_ref()
    {
        files
            .iter()
            .filter(|file| is_pipeline_eligible(file, existing_sources))
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
    if parallel_candidates >= MIN_PIPELINE_FILES
        && let Some(existing_sources) = existing_sources.as_ref()
    {
        index_files_pipelined(
            store,
            &files,
            existing_sources,
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
                existing_sources.as_ref(),
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
    projected_sessions: Vec<SessionProjection>,
    canonical: &Path,
    shell_parser: &mut syntax::ShellParser,
    max_text_bytes: usize,
) -> Result<SessionProjection> {
    let mut session =
        finalize_projection_without_shell_parse(projector, projected_sessions, canonical)?;
    shell_parser.parse_declared_commands(&mut session, max_text_bytes);
    Ok(session)
}

fn finalize_projection_without_shell_parse(
    projector: Projector,
    mut projected_sessions: Vec<SessionProjection>,
    canonical: &Path,
) -> Result<SessionProjection> {
    projected_sessions.extend(projector.finish());
    if projected_sessions.len() != 1 {
        return Err(anyhow::Error::new(UnrecognizedSource(format!(
            "{} must establish exactly one Session; Adapter projected {}",
            canonical.display(),
            projected_sessions.len(),
        ))));
    }
    let session = projected_sessions.pop().expect("length checked above");
    if !session.identity_confirmed {
        return Err(anyhow::Error::new(UnrecognizedSource(format!(
            "{} does not contain a structurally confirmed Session identity",
            canonical.display(),
        ))));
    }
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
    existing_sources: Option<&HashMap<String, SourceState>>,
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
    let owned_existing;
    let existing_before: Option<&SourceState> = if let Some(states) = existing_sources {
        states.get(&path_string)
    } else {
        owned_existing = store.source_state(&path_string)?;
        owned_existing.as_ref()
    };
    if let Some(existing) = existing_before
        && !options.rebuild
        && existing.file_size == file_size
        && existing.complete
        && existing.modified_ns == file_modified_ns
    {
        telemetry.record_duration(IndexPhase::Prepare, prepare_started.elapsed());
        return Ok(unchanged_result(path_string, existing, file_size, false));
    }

    let SyncScratch {
        blobs,
        shell_parser,
        record_buffer,
        decompress_buffer,
    } = scratch;
    let mut opened = open_source_for_sync(canonical, options, decompress_buffer)?;
    telemetry.record_duration(IndexPhase::Prepare, prepare_started.elapsed());

    // Fast path for append-only updates to an already-indexed Source:
    // verify the existing prefix and scan the file in one sequential pass
    // outside any SQLite write lock, deferring shell parsing and content_blobs
    // probes for verified-prefix Items. Any prefix rewrite or concurrent state
    // change falls through cleanly to the standard transactional path below.
    if !options.rebuild
        && let Some(existing) = existing_before
        && existing.adapter == opened.adapter.name()
        && existing.indexed_bytes > 0
        && existing.indexed_bytes <= opened.indexed_limit
        && let Some(result) = try_index_append_only_source(
            store,
            discovered,
            &path_string,
            existing,
            &mut opened,
            options,
            blobs,
            shell_parser,
            record_buffer,
            telemetry,
            progress,
        )?
    {
        return Ok(result);
    }

    index_source_transactional(
        store,
        discovered,
        &path_string,
        &mut opened,
        options,
        blobs,
        shell_parser,
        record_buffer,
        telemetry,
        progress,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "transactional fallback threads the caller's scratch buffers and telemetry without reallocating"
)]
fn index_source_transactional(
    store: &mut Store,
    discovered: &DiscoveredFile,
    path_string: &str,
    opened: &mut OpenedSource<'_>,
    options: &IndexOptions,
    blobs: &mut BlobCache,
    shell_parser: &mut syntax::ShellParser,
    record_buffer: &mut Vec<u8>,
    telemetry: &mut IndexTelemetry,
    progress: &mut dyn ProgressObserver,
) -> Result<SourceIndexResult> {
    let transaction = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("failed to start index transaction")?;
    let update = match plan_source_update(
        &transaction,
        discovered,
        path_string,
        opened,
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
        canonical: &discovered.path,
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
        file_size: discovered.file_size,
        file_modified_ns: discovered.modified_ns,
        indexed_limit: opened.indexed_limit,
        incomplete_tail: opened.incomplete_tail,
        incremental_projection: update.incremental_projection,
        trusted_prefix_limit: update.incremental_projection.then_some(update.resume_line),
        replaced_transcript: update.replaced_transcript,
    };
    persist_and_commit_source(
        transaction,
        commit,
        &mut scanned,
        &record_ids,
        blobs,
        shell_parser,
        options.max_text_bytes,
        telemetry,
    )?;

    Ok(SourceIndexResult {
        error: None,
        path: path_string.to_owned(),
        adapter: opened.adapter.name().to_owned(),
        action: update.action,
        indexed_records: scanned.counts.new_records,
        indexed_items: scanned.counts.items,
        indexed_bytes: opened.indexed_limit,
        file_size: discovered.file_size,
        malformed_records: scanned.counts.malformed,
        oversized_records: scanned.counts.oversized,
        incomplete_tail: opened.incomplete_tail,
    })
}

struct PreparedAppendRecord {
    seq: u64,
    byte_offset: u64,
    byte_length: u64,
    raw_hash: String,
    oversized: bool,
    facts: RecordFacts,
}

struct PreparedAppendScan {
    scanned: ScannedSource,
    new_records: Vec<PreparedAppendRecord>,
}

fn source_state_matches_snapshot(current: Option<&SourceState>, expected: &SourceState) -> bool {
    current.is_some_and(|state| {
        state.id == expected.id
            && state.adapter == expected.adapter
            && state.indexed_bytes == expected.indexed_bytes
            && state.indexed_lines == expected.indexed_lines
            && state.indexed_fingerprint == expected.indexed_fingerprint
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "off-lock append-only fast path threads the caller's scratch buffers and telemetry without reallocating"
)]
fn try_index_append_only_source(
    store: &mut Store,
    discovered: &DiscoveredFile,
    path_string: &str,
    existing: &SourceState,
    opened: &mut OpenedSource<'_>,
    options: &IndexOptions,
    blobs: &mut BlobCache,
    shell_parser: &mut syntax::ShellParser,
    record_buffer: &mut Vec<u8>,
    telemetry: &mut IndexTelemetry,
    progress: &mut dyn ProgressObserver,
) -> Result<Option<SourceIndexResult>> {
    if opened.indexed_limit == existing.indexed_bytes {
        return try_update_unchanged_tail(
            store,
            discovered,
            path_string,
            existing,
            opened,
            telemetry,
        );
    }

    if opened.adapter == AdapterKind::Antigravity && existing.indexed_lines > 0 {
        return try_index_antigravity_tail(
            store,
            discovered,
            path_string,
            existing,
            opened,
            options,
            blobs,
            shell_parser,
            record_buffer,
            telemetry,
            progress,
        );
    }

    let Some(prepared) = scan_append_only_off_lock(
        &discovered.path,
        opened,
        existing,
        options,
        record_buffer,
        telemetry,
        progress,
    )?
    else {
        return Ok(None);
    };

    commit_append_only_scan(
        store,
        discovered,
        path_string,
        existing,
        opened,
        *prepared,
        blobs,
        shell_parser,
        options.max_text_bytes,
        telemetry,
    )
}

fn try_update_unchanged_tail(
    store: &mut Store,
    discovered: &DiscoveredFile,
    path_string: &str,
    existing: &SourceState,
    opened: &mut OpenedSource<'_>,
    telemetry: &mut IndexTelemetry,
) -> Result<Option<SourceIndexResult>> {
    let fingerprint_started = Instant::now();
    let current = opened.source.fingerprint_prefix(existing.indexed_bytes)?;
    telemetry.record_duration(IndexPhase::Fingerprint, fingerprint_started.elapsed());
    telemetry.add_fingerprint_bytes(existing.indexed_bytes);
    if current != existing.indexed_fingerprint {
        return Ok(None);
    }
    let transaction = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("failed to start index transaction")?;
    if !source_state_matches_snapshot(source_state(&transaction, path_string)?.as_ref(), existing)
        || antigravity_replacement(&transaction, opened.adapter, &discovered.path)?.is_some()
    {
        return Ok(None);
    }
    update_unchanged_source_locator(
        &transaction,
        existing.id,
        path_string,
        discovered.file_size,
        discovered.modified_ns,
        opened.incomplete_tail,
    )?;
    let commit_started = Instant::now();
    transaction.commit().context("failed to commit index")?;
    telemetry.record_duration(IndexPhase::Commit, commit_started.elapsed());
    Ok(Some(unchanged_result(
        path_string.to_owned(),
        existing,
        discovered.file_size,
        opened.incomplete_tail,
    )))
}

struct PreparedAntigravityTail {
    session_uuid: String,
    delta: antigravity::AntigravityTailDelta,
    new_records: Vec<PreparedAppendRecord>,
    fingerprint: String,
    line_count: u64,
    counts: ScanCounts,
}

#[allow(
    clippy::too_many_arguments,
    reason = "O(tail) Antigravity append path threads caller scratch buffers and telemetry without reallocating"
)]
fn try_index_antigravity_tail(
    store: &mut Store,
    discovered: &DiscoveredFile,
    path_string: &str,
    existing: &SourceState,
    opened: &mut OpenedSource<'_>,
    options: &IndexOptions,
    blobs: &mut BlobCache,
    shell_parser: &mut syntax::ShellParser,
    record_buffer: &mut Vec<u8>,
    telemetry: &mut IndexTelemetry,
    progress: &mut dyn ProgressObserver,
) -> Result<Option<SourceIndexResult>> {
    let Some(prepared) = scan_antigravity_tail_off_lock(
        &discovered.path,
        opened,
        existing,
        options,
        record_buffer,
        telemetry,
        progress,
    )?
    else {
        return Ok(None);
    };

    commit_antigravity_tail(
        store,
        discovered,
        path_string,
        existing,
        opened,
        *prepared,
        blobs,
        shell_parser,
        options.max_text_bytes,
        telemetry,
    )
}

fn scan_antigravity_tail_off_lock(
    canonical: &Path,
    opened: &mut OpenedSource<'_>,
    existing: &SourceState,
    options: &IndexOptions,
    record_buffer: &mut Vec<u8>,
    telemetry: &mut IndexTelemetry,
    progress: &mut dyn ProgressObserver,
) -> Result<Option<Box<PreparedAntigravityTail>>> {
    let Some((session_uuid, _full)) = antigravity::transcript(canonical) else {
        return Ok(None);
    };
    let fingerprint_started = Instant::now();
    let Some(mut full_hasher) = opened.source.verify_and_seed_prefix(
        existing.indexed_bytes,
        &existing.indexed_fingerprint,
        opened.indexed_limit,
    )?
    else {
        telemetry.record_duration(IndexPhase::Fingerprint, fingerprint_started.elapsed());
        telemetry.add_fingerprint_bytes(existing.indexed_bytes);
        return Ok(None);
    };
    telemetry.record_duration(IndexPhase::Fingerprint, fingerprint_started.elapsed());
    telemetry.add_fingerprint_bytes(existing.indexed_bytes);

    let mut reader = opened
        .source
        .bounded_reader_from(existing.indexed_bytes, opened.indexed_limit)
        .with_context(|| format!("failed to seek tail of {}", canonical.display()))?;
    let mut projector = antigravity::AntigravityTailProjector::new(options.max_text_bytes);
    let mut new_records = Vec::new();
    let mut counts = ScanCounts::default();
    let mut byte_offset = existing.indexed_bytes;
    let mut seq = existing.indexed_lines;

    loop {
        let read_started = Instant::now();
        let read = read_bounded_record_into(
            &mut reader,
            options.max_record_bytes,
            Some(&mut full_hasher),
            true,
            record_buffer,
        )?;
        telemetry.record_duration(IndexPhase::Read, read_started.elapsed());
        let RecordReadInto::Complete {
            oversized,
            byte_length,
            consumed_bytes,
            raw_hash,
        } = read
        else {
            break;
        };

        let project_started = Instant::now();
        let parsed = (!oversized).then(|| serde_json::from_slice::<Value>(record_buffer));
        let facts = classify_record(AdapterKind::Antigravity, parsed.as_ref(), true, &mut counts);
        if let Some(Ok(value)) = &parsed {
            projector.push(seq, value);
        }
        if let Some(facts) = facts {
            new_records.push(PreparedAppendRecord {
                seq,
                byte_offset,
                byte_length,
                raw_hash,
                oversized,
                facts,
            });
            counts.new_records += 1;
        }
        telemetry.record_duration(IndexPhase::Project, project_started.elapsed());

        telemetry.add_processed_bytes(consumed_bytes);
        byte_offset += consumed_bytes;
        seq += 1;
        if seq.is_multiple_of(2048) {
            progress.update(
                telemetry.progress(IndexPhase::Project, Some(canonical)),
                false,
            );
        }
    }

    if byte_offset != opened.indexed_limit {
        return Ok(None);
    }

    let fingerprint = full_hasher.finalize().to_hex().to_string();
    Ok(Some(Box::new(PreparedAntigravityTail {
        session_uuid,
        delta: projector.finish(),
        new_records,
        fingerprint,
        line_count: seq,
        counts,
    })))
}

#[allow(
    clippy::too_many_arguments,
    reason = "tail commit receives the off-lock delta alongside store caches and telemetry"
)]
fn commit_antigravity_tail(
    store: &mut Store,
    discovered: &DiscoveredFile,
    path_string: &str,
    existing: &SourceState,
    opened: &OpenedSource<'_>,
    mut prepared: PreparedAntigravityTail,
    blobs: &mut BlobCache,
    shell_parser: &mut syntax::ShellParser,
    max_text_bytes: usize,
    telemetry: &mut IndexTelemetry,
) -> Result<Option<SourceIndexResult>> {
    let transaction = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("failed to start index transaction")?;
    if !source_state_matches_snapshot(source_state(&transaction, path_string)?.as_ref(), existing)
        || antigravity_replacement(&transaction, opened.adapter, &discovered.path)?.is_some()
    {
        return Ok(None);
    }

    let persist_started = Instant::now();
    let mut new_record_ids = HashMap::with_capacity(prepared.new_records.len());
    for record in &prepared.new_records {
        let started = Instant::now();
        let record_id = insert_record(
            &transaction,
            existing.id,
            record.seq,
            record.byte_offset,
            record.byte_length,
            &record.raw_hash,
            record.oversized,
            &record.facts,
        )?;
        telemetry.record_persist(PersistTable::TraceRecords, started.elapsed());
        new_record_ids.insert(record.seq, record_id);
        telemetry.count_record();
    }
    let Some(write_counts) = append_antigravity_tail(
        &transaction,
        existing.id,
        &prepared.session_uuid,
        &mut prepared.delta,
        &new_record_ids,
        blobs,
        shell_parser,
        max_text_bytes,
        telemetry,
    )?
    else {
        return Ok(None);
    };
    telemetry.record_duration(IndexPhase::Persist, persist_started.elapsed());

    finalize_source_row(
        &transaction,
        existing.id,
        opened.adapter,
        discovered.file_size,
        discovered.modified_ns,
        opened.indexed_limit,
        prepared.line_count,
        &prepared.fingerprint,
        opened.incomplete_tail,
    )?;
    let commit_started = Instant::now();
    transaction.commit().context("failed to commit index")?;
    telemetry.record_duration(IndexPhase::Commit, commit_started.elapsed());

    Ok(Some(SourceIndexResult {
        error: None,
        path: path_string.to_owned(),
        adapter: opened.adapter.name().to_owned(),
        action: "appended".to_owned(),
        indexed_records: prepared.counts.new_records,
        indexed_items: write_counts.items,
        indexed_bytes: opened.indexed_limit,
        file_size: discovered.file_size,
        malformed_records: prepared.counts.malformed,
        oversized_records: prepared.counts.oversized,
        incomplete_tail: opened.incomplete_tail,
    }))
}

#[allow(
    clippy::too_many_arguments,
    reason = "commit phase receives the off-lock scan output alongside store caches and telemetry"
)]
fn commit_append_only_scan(
    store: &mut Store,
    discovered: &DiscoveredFile,
    path_string: &str,
    existing: &SourceState,
    opened: &OpenedSource<'_>,
    mut prepared: PreparedAppendScan,
    blobs: &mut BlobCache,
    shell_parser: &mut syntax::ShellParser,
    max_text_bytes: usize,
    telemetry: &mut IndexTelemetry,
) -> Result<Option<SourceIndexResult>> {
    let transaction = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("failed to start index transaction")?;
    if !source_state_matches_snapshot(source_state(&transaction, path_string)?.as_ref(), existing)
        || antigravity_replacement(&transaction, opened.adapter, &discovered.path)?.is_some()
    {
        return Ok(None);
    }

    let clear_started = Instant::now();
    let mut record_ids = if existing.indexed_lines > 0 {
        load_record_ids(&transaction, existing.id)?
    } else {
        HashMap::new()
    };
    telemetry.record_duration(IndexPhase::Clear, clear_started.elapsed());

    let persist_started = Instant::now();
    for record in &prepared.new_records {
        let started = Instant::now();
        let record_id = insert_record(
            &transaction,
            existing.id,
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

    let commit = CommitSourceParams {
        source_id: existing.id,
        adapter: opened.adapter,
        file_size: discovered.file_size,
        file_modified_ns: discovered.modified_ns,
        indexed_limit: opened.indexed_limit,
        incomplete_tail: opened.incomplete_tail,
        incremental_projection: true,
        trusted_prefix_limit: Some(existing.indexed_lines),
        replaced_transcript: None,
    };
    persist_and_commit_source(
        transaction,
        commit,
        &mut prepared.scanned,
        &record_ids,
        blobs,
        shell_parser,
        max_text_bytes,
        telemetry,
    )?;

    let action = if existing.indexed_lines == 0 {
        "indexed"
    } else {
        "appended"
    };
    Ok(Some(SourceIndexResult {
        error: None,
        path: path_string.to_owned(),
        adapter: opened.adapter.name().to_owned(),
        action: action.to_owned(),
        indexed_records: prepared.scanned.counts.new_records,
        indexed_items: prepared.scanned.counts.items,
        indexed_bytes: opened.indexed_limit,
        file_size: discovered.file_size,
        malformed_records: prepared.scanned.counts.malformed,
        oversized_records: prepared.scanned.counts.oversized,
        incomplete_tail: opened.incomplete_tail,
    }))
}

fn scan_append_only_off_lock(
    canonical: &Path,
    opened: &mut OpenedSource<'_>,
    existing: &SourceState,
    options: &IndexOptions,
    record_buffer: &mut Vec<u8>,
    telemetry: &mut IndexTelemetry,
    progress: &mut dyn ProgressObserver,
) -> Result<Option<Box<PreparedAppendScan>>> {
    let mut full_hasher = prefix_hasher(opened.indexed_limit);
    let mut verify_hasher = prefix_hasher(existing.indexed_bytes);
    let mut verify_remaining = existing.indexed_bytes;
    let mut prefix_verified = false;
    let mut reader = opened
        .source
        .bounded_reader(opened.indexed_limit)
        .with_context(|| format!("failed to rewind {}", canonical.display()))?;

    let mut projector = Projector::new(opened.adapter, options.max_text_bytes, canonical)?;
    let mut projected_sessions = Vec::new();
    let mut new_records = Vec::new();
    let mut counts = ScanCounts::default();
    let mut byte_offset = 0_u64;
    let mut seq = 0_u64;

    loop {
        let persist_record = seq >= existing.indexed_lines;
        let read_started = Instant::now();
        let read = read_next_append_record(
            &mut reader,
            options.max_record_bytes,
            &mut full_hasher,
            &mut verify_hasher,
            &mut verify_remaining,
            persist_record,
            record_buffer,
        )?;
        telemetry.record_duration(IndexPhase::Read, read_started.elapsed());
        let RecordReadInto::Complete {
            oversized,
            byte_length,
            consumed_bytes,
            raw_hash,
        } = read
        else {
            break;
        };

        let next_byte_offset = byte_offset + consumed_bytes;
        let next_seq = seq + 1;
        if !prefix_verified && verify_remaining == 0 {
            telemetry.add_fingerprint_bytes(existing.indexed_bytes);
            if next_byte_offset != existing.indexed_bytes
                || next_seq != existing.indexed_lines
                || verify_hasher.finalize().to_hex().as_str() != existing.indexed_fingerprint
            {
                return Ok(None);
            }
            prefix_verified = true;
        }

        let project_started = Instant::now();
        project_append_record(
            opened.adapter,
            &mut projector,
            &mut projected_sessions,
            record_buffer,
            persist_record,
            seq,
            byte_offset,
            byte_length,
            raw_hash,
            oversized,
            &mut counts,
            &mut new_records,
        );
        telemetry.record_duration(IndexPhase::Project, project_started.elapsed());

        telemetry.add_processed_bytes(consumed_bytes);
        byte_offset = next_byte_offset;
        seq = next_seq;
        if seq.is_multiple_of(2048) {
            progress.update(
                telemetry.progress(IndexPhase::Project, Some(canonical)),
                false,
            );
        }
    }

    if !prefix_verified {
        return Ok(None);
    }

    let project_started = Instant::now();
    let session =
        finalize_projection_without_shell_parse(projector, projected_sessions, canonical)?;
    telemetry.record_duration(IndexPhase::Project, project_started.elapsed());
    let fingerprint = full_hasher.finalize().to_hex().to_string();
    Ok(Some(Box::new(PreparedAppendScan {
        scanned: ScannedSource {
            session,
            fingerprint,
            line_count: seq,
            counts,
        },
        new_records,
    })))
}

fn read_next_append_record(
    reader: &mut impl std::io::BufRead,
    max_record_bytes: usize,
    full_hasher: &mut blake3::Hasher,
    verify_hasher: &mut blake3::Hasher,
    verify_remaining: &mut u64,
    persist_record: bool,
    record_buffer: &mut Vec<u8>,
) -> Result<RecordReadInto> {
    if *verify_remaining > 0 {
        let mut verify = PrefixVerify {
            hasher: verify_hasher,
            remaining: *verify_remaining,
        };
        let result = read_bounded_record_with_verify(
            reader,
            max_record_bytes,
            Some(full_hasher),
            Some(&mut verify),
            persist_record,
            record_buffer,
        )?;
        *verify_remaining = verify.remaining;
        Ok(result)
    } else {
        read_bounded_record_into(
            reader,
            max_record_bytes,
            Some(full_hasher),
            persist_record,
            record_buffer,
        )
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "per-record helper separates prefix-skip vs new-record projection cleanly"
)]
fn project_append_record(
    adapter: AdapterKind,
    projector: &mut Projector,
    projected_sessions: &mut Vec<SessionProjection>,
    record_buffer: &[u8],
    persist_record: bool,
    seq: u64,
    byte_offset: u64,
    byte_length: u64,
    raw_hash: String,
    oversized: bool,
    counts: &mut ScanCounts,
    new_records: &mut Vec<PreparedAppendRecord>,
) {
    if persist_record {
        let parsed = (!oversized).then(|| serde_json::from_slice::<Value>(record_buffer));
        let facts = classify_record(adapter, parsed.as_ref(), true, counts);
        if let Some(Ok(value)) = &parsed {
            projector.push(seq, value);
            projected_sessions.extend(projector.drain_completed());
        }
        if let Some(facts) = facts {
            new_records.push(PreparedAppendRecord {
                seq,
                byte_offset,
                byte_length,
                raw_hash,
                oversized,
                facts,
            });
            counts.new_records += 1;
        }
    } else if oversized {
        counts.oversized += 1;
    } else if !projector.can_skip_prefix_record(record_buffer) {
        match serde_json::from_slice::<Value>(record_buffer) {
            Ok(value) => {
                projector.push(seq, &value);
                projected_sessions.extend(projector.drain_completed());
            }
            Err(_) => counts.malformed += 1,
        }
    }
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
    trusted_prefix_limit: Option<u64>,
    replaced_transcript: Option<i64>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "commit helper threads the transaction, scratch caches, and shell parser together"
)]
fn persist_and_commit_source(
    transaction: Transaction<'_>,
    meta: CommitSourceParams,
    scanned: &mut ScannedSource,
    record_ids: &HashMap<u64, i64>,
    blobs: &mut BlobCache,
    shell_parser: &mut syntax::ShellParser,
    max_text_bytes: usize,
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
            &mut scanned.session,
            record_ids,
            blobs,
            &mut IncrementalSyncOptions {
                trusted_prefix_limit: meta.trusted_prefix_limit,
                shell_parser,
                max_text_bytes,
            },
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

    let project_started = Instant::now();
    let session = finalize_projected_session(
        projector,
        projected_sessions,
        ctx.canonical,
        ctx.shell_parser,
        ctx.options.max_text_bytes,
    )?;
    ctx.telemetry
        .record_duration(IndexPhase::Project, project_started.elapsed());
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
    session: &mut SessionProjection,
    record_ids: &HashMap<u64, i64>,
    blobs: &mut BlobCache,
    options: &mut IncrementalSyncOptions<'_>,
    telemetry: &mut IndexTelemetry,
) -> Result<u64> {
    let persist_started = Instant::now();
    let counts = sync_session(
        transaction,
        source_id,
        session,
        record_ids,
        blobs,
        options,
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
