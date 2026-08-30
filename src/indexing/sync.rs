//! Explicit synchronization orchestration for configured roots.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};

use super::indexer::{IndexOptions, sync_paths_observed};
#[cfg(test)]
use super::telemetry::NoopProgress;
use super::telemetry::{IndexPhase, IndexTelemetry, ProgressObserver, database_storage_bytes};
use crate::interface::output::IndexReport;
use crate::storage::db::Store;

#[derive(Debug, Clone)]
pub struct SyncRoot {
    pub label: Option<String>,
    pub path: PathBuf,
}

/// Explicitly synchronizes configured trace roots.
///
/// # Errors
///
/// Returns an error when a root cannot be canonicalized, indexing fails, or
/// deferred indexes cannot be restored.
#[cfg(test)]
fn execute(store: &mut Store, roots: &[SyncRoot], options: &IndexOptions) -> Result<IndexReport> {
    let mut progress = NoopProgress;
    execute_observed(store, roots, options, &mut progress)
}

/// Synchronizes while emitting observational progress to the supplied observer.
///
/// # Errors
///
/// Returns an error when a root cannot be canonicalized, indexing fails, or
/// deferred indexes cannot be restored.
pub fn execute_observed(
    store: &mut Store,
    roots: &[SyncRoot],
    options: &IndexOptions,
    progress: &mut dyn ProgressObserver,
) -> Result<IndexReport> {
    if roots.is_empty() {
        bail!("no trace roots configured or supplied");
    }

    let database_bytes_before = database_storage_bytes(store.path());
    let mut telemetry = IndexTelemetry::new();
    let prepare_started = Instant::now();
    let paths = canonical_paths(roots)?;
    let bulk_load = store.is_empty()?;
    telemetry.record_duration(IndexPhase::Prepare, prepare_started.elapsed());

    if bulk_load {
        let started = Instant::now();
        if let Err(error) = store.drop_secondary_indexes() {
            let _ = store.create_secondary_indexes();
            progress.finish();
            return Err(error);
        }
        telemetry.record_duration(IndexPhase::BuildIndexes, started.elapsed());
    }

    match sync_paths_observed(store, &paths, options, &mut telemetry, progress) {
        Ok(mut report) => {
            restore_secondary_indexes(store, bulk_load, &mut telemetry, progress)?;
            progress.finish();
            report.metrics =
                telemetry.finish(database_bytes_before, database_storage_bytes(store.path()));
            Ok(report)
        }
        Err(error) => {
            let error = match restore_secondary_indexes(store, bulk_load, &mut telemetry, progress)
            {
                Ok(()) => error,
                Err(restore_error) => anyhow!(
                    "indexing failed: {error:#}; rebuilding deferred indexes also failed: {restore_error:#}"
                ),
            };
            progress.finish();
            Err(error)
        }
    }
}

fn restore_secondary_indexes(
    store: &mut Store,
    deferred: bool,
    telemetry: &mut IndexTelemetry,
    progress: &mut dyn ProgressObserver,
) -> Result<()> {
    if !deferred {
        return Ok(());
    }
    progress.update(telemetry.progress(IndexPhase::BuildIndexes, None), true);
    let started = Instant::now();
    let result = store.create_secondary_indexes();
    telemetry.record_duration(IndexPhase::BuildIndexes, started.elapsed());
    result
}

fn canonical_paths(roots: &[SyncRoot]) -> Result<Vec<PathBuf>> {
    roots
        .iter()
        .map(|root| {
            root.path.canonicalize().with_context(|| {
                format!(
                    "failed to canonicalize trace root {}: {}",
                    root.label.as_deref().unwrap_or("<unlabelled>"),
                    root.path.display()
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{SyncRoot, execute};
    use crate::indexing::indexer::IndexOptions;
    use crate::storage::db::Store;

    #[test]
    fn synchronizes_configured_roots() {
        let directory = tempdir().expect("temporary directory");
        let trace = directory.path().join("trace.jsonl");
        fs::write(
            &trace,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"sync-test\",\"cwd\":\"/tmp\"}}\n",
                "{\"type\":\"turn_context\",\"payload\":{\"turn_id\":\"loop-1\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hi\"}}\n"
            ),
        )
        .expect("write trace");
        let mut store = Store::open(&directory.path().join("index.sqlite")).expect("open store");
        let report = execute(
            &mut store,
            &[SyncRoot {
                label: Some("test".to_owned()),
                path: trace,
            }],
            &IndexOptions {
                rebuild: false,
                max_record_bytes: 1024 * 1024,
                max_text_bytes: 64 * 1024,
            },
        )
        .expect("sync traces");

        assert_eq!(report.indexed_files, 1);
        assert_secondary_indexes_exist(&store);
    }

    #[test]
    fn restores_deferred_indexes_when_a_cold_sync_indexes_nothing() {
        let directory = tempdir().expect("temporary directory");
        let trace = directory.path().join("unsupported.jsonl");
        fs::write(&trace, "{\"type\":\"unsupported\"}\n").expect("write trace");
        let mut store = Store::open(&directory.path().join("index.sqlite")).expect("open store");
        let report = execute(
            &mut store,
            &[SyncRoot {
                label: Some("test".to_owned()),
                path: trace,
            }],
            &IndexOptions {
                rebuild: false,
                max_record_bytes: 1024 * 1024,
                max_text_bytes: 64 * 1024,
            },
        )
        .expect("an unrecognized file is skipped, not a run failure");
        assert_eq!(report.skipped_files, 1);
        assert_eq!(report.failed_files, 0);
        assert_eq!(report.indexed_files, 0);
        assert_eq!(report.source_results[0].action, "skipped");
        assert!(report.source_results[0].error.is_some());
        // Deferred indexes must come back even when the cold sync indexed
        // nothing, or the next read would run without them.
        assert_secondary_indexes_exist(&store);
    }

    fn assert_secondary_indexes_exist(store: &Store) {
        let indexes: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                  WHERE type = 'index'
                    AND name IN (
                        'items_role',
                        'items_loop',
                        'loops_start_record'
                    )",
                [],
                |row| row.get(0),
            )
            .expect("inspect indexes");
        assert_eq!(indexes, 3);
    }
}
