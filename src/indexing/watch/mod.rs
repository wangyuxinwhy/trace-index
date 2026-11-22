//! Event-driven incremental watcher for Agent trace roots.
//!
//! Watches configured (or explicit) trace roots via OS filesystem notifications
//! (`FSEvents` on macOS, `inotify` on Linux), coalesces bursts per file with a
//! bounded debounce + max-wait window, and runs targeted incremental `sync` on
//! only the modified trace files. Between batches the `SQLite` connection is
//! dropped so idle memory stays minimal and WAL checkpoints are never pinned.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use notify::{Event, EventKind, RecursiveMode, Watcher as _};
use serde::{Deserialize, Serialize};

use super::indexer::IndexOptions;
use super::sync::{self, SyncRoot};
use super::telemetry::NoopProgress;
use crate::adapters::runtime::antigravity;
use crate::storage::db::{Store, display_database_path};

pub(crate) mod service;

/// Default quiet period after the last write event to a file before syncing it.
pub(crate) const DEFAULT_DEBOUNCE_MS: u64 = 3_000;
/// Maximum delay before flushing a continuously streaming trace file.
pub(crate) const DEFAULT_MAX_WAIT_MS: u64 = 15_000;
/// Default low-frequency safety-net incremental reconcile interval in seconds (`0` disables).
///
/// OS notification subsystems (`FSEvents`, `inotify`) can lag or drop events under
/// heavy load or queue overflow; periodic incremental reconciliation (which skips
/// unchanged complete files by size and modification time) catches any missed writes
/// without holding idle database handles.
pub(crate) const DEFAULT_RECONCILE_SECS: u64 = 300;

/// Live watcher and OS-service status reported by `index status` and `index watch status`.
#[derive(Debug, Clone, Serialize)]
pub struct WatchStatus {
    pub active: bool,
    pub service_installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synced_batches: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synced_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debounce_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_wait_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconcile_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Summary emitted on stdout when a foreground `index watch run` exits cleanly.
#[derive(Debug, Serialize)]
pub struct WatchRunSummary {
    pub database: String,
    pub watched_roots: Vec<String>,
    pub synced_batches: u64,
    pub synced_files: u64,
    pub indexed_records: u64,
    pub indexed_items: u64,
}

/// Configuration for one `index watch run` invocation.
pub(crate) struct WatchRunConfig {
    pub(crate) database_path: PathBuf,
    pub(crate) roots: Vec<SyncRoot>,
    pub(crate) max_record_bytes: usize,
    pub(crate) max_text_bytes: usize,
    pub(crate) debounce: Duration,
    pub(crate) max_wait: Duration,
    pub(crate) reconcile_interval: Option<Duration>,
    pub(crate) initial_sync: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WatchStateFile {
    pid: u32,
    started_at_ms: i64,
    #[serde(default)]
    last_sync_at_ms: Option<i64>,
    #[serde(default)]
    synced_batches: u64,
    #[serde(default)]
    synced_files: u64,
    debounce_ms: u64,
    max_wait_ms: u64,
    #[serde(default)]
    reconcile_secs: u64,
    #[serde(default)]
    last_error: Option<String>,
}

/// Derives the advisory lockfile path for one database without altering the `SQLite` path.
pub(crate) fn watch_lock_path(database_path: &Path) -> PathBuf {
    let mut os = database_path.as_os_str().to_os_string();
    os.push(".watch.lock");
    PathBuf::from(os)
}

/// Derives the runtime state JSON path for one database.
pub(crate) fn watch_state_path(database_path: &Path) -> PathBuf {
    let mut os = database_path.as_os_str().to_os_string();
    os.push(".watch.json");
    PathBuf::from(os)
}

/// Strictly read-only inspection of live watcher activity and OS service status.
///
/// Uses a non-blocking shared OS advisory lock (`File::try_lock_shared`) on an
/// existing read-only file descriptor so crashed watchers never leave false
/// positives and read-only sandboxes never fail.
pub(crate) fn inspect_watch_status(database_path: &Path) -> WatchStatus {
    let active = is_watch_lock_held(&watch_lock_path(database_path));
    let state = if active {
        read_state_file(&watch_state_path(database_path))
    } else {
        None
    };
    let svc = service::inspect_service(database_path);
    let service_installed = svc.as_ref().is_some_and(|info| info.installed);
    let service_id = svc
        .as_ref()
        .filter(|info| info.installed)
        .map(|info| info.service_id.clone());
    let service_path = svc
        .as_ref()
        .filter(|info| info.installed)
        .map(|info| display_database_path(&info.service_path));

    WatchStatus {
        active,
        service_installed,
        pid: state.as_ref().map(|s| s.pid),
        started_at_ms: state.as_ref().map(|s| s.started_at_ms),
        last_sync_at_ms: state.as_ref().and_then(|s| s.last_sync_at_ms),
        synced_batches: state.as_ref().map(|s| s.synced_batches),
        synced_files: state.as_ref().map(|s| s.synced_files),
        debounce_ms: state.as_ref().map(|s| s.debounce_ms),
        max_wait_ms: state.as_ref().map(|s| s.max_wait_ms),
        reconcile_secs: state.as_ref().map(|s| s.reconcile_secs),
        service_id,
        service_path,
        last_error: state.and_then(|s| s.last_error),
    }
}

fn is_watch_lock_held(lock_path: &Path) -> bool {
    let Ok(file) = File::open(lock_path) else {
        return false;
    };
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    }
}

fn read_state_file(state_path: &Path) -> Option<WatchStateFile> {
    let bytes = fs::read(state_path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_state_file(state_path: &Path, state: &WatchStateFile) -> Result<()> {
    let mut tmp = state_path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp_path = PathBuf::from(tmp);
    let bytes = serde_json::to_vec(state).context("failed to serialize watch state")?;
    fs::write(&tmp_path, bytes)
        .with_context(|| format!("failed to write watch state {}", tmp_path.display()))?;
    fs::rename(&tmp_path, state_path)
        .with_context(|| format!("failed to publish watch state {}", state_path.display()))?;
    Ok(())
}

struct WatchLockGuard {
    file: File,
    state_path: PathBuf,
}

impl WatchLockGuard {
    fn acquire(database_path: &Path) -> Result<Self> {
        let lock_path = watch_lock_path(database_path);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create directory for watch lock {}",
                    parent.display()
                )
            })?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open watch lock {}", lock_path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                file,
                state_path: watch_state_path(database_path),
            }),
            Err(TryLockError::WouldBlock) => bail!(
                "another `trace-index index watch` process is already active for {}",
                database_path.display()
            ),
            Err(TryLockError::Error(error)) => Err(error)
                .with_context(|| format!("failed to acquire watch lock {}", lock_path.display())),
        }
    }
}

impl Drop for WatchLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.state_path);
        let _ = self.file.unlock();
    }
}

/// Runs the foreground event-driven watcher until `SIGINT`/`SIGTERM` is received.
pub(crate) fn run_watch(config: &WatchRunConfig) -> Result<WatchRunSummary> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        stop_flag.store(true, Ordering::SeqCst);
    })
    .context("failed to register watch shutdown signal handler")?;
    run_watch_until(config, &stop)
}

/// Testable core loop that registers the OS filesystem watcher and runs until `stop` becomes `true`.
pub(crate) fn run_watch_until(
    config: &WatchRunConfig,
    stop: &AtomicBool,
) -> Result<WatchRunSummary> {
    if config.roots.is_empty() {
        bail!("index watch requires explicit paths or at least one configured [[roots]] entry");
    }
    let canonical_roots = canonicalize_roots(&config.roots)?;
    let watch_plan = build_watch_plan(&canonical_roots)?;
    // Acquire the exclusive per-database lock BEFORE allocating OS watcher
    // resources so duplicate watchers fail immediately.
    let lock = WatchLockGuard::acquire(&config.database_path)?;

    // Register the OS watcher BEFORE the initial catch-up sync so no write
    // occurring during startup is missed between the initial scan and watch.
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    })
    .context("failed to initialize filesystem watcher")?;
    for (dir, mode) in &watch_plan.directories {
        watcher
            .watch(dir, *mode)
            .with_context(|| format!("failed to watch {}", dir.display()))?;
    }

    let summary = run_watch_with_lock(config, &canonical_roots, &watch_plan, &rx, stop, lock);
    drop(watcher);
    summary
}

#[cfg(test)]
fn run_watch_with_receiver(
    config: &WatchRunConfig,
    canonical_roots: &[SyncRoot],
    watch_plan: &WatchPlan,
    rx: &mpsc::Receiver<notify::Result<Event>>,
    stop: &AtomicBool,
) -> Result<WatchRunSummary> {
    let lock = WatchLockGuard::acquire(&config.database_path)?;
    run_watch_with_lock(config, canonical_roots, watch_plan, rx, stop, lock)
}

fn run_watch_with_lock(
    config: &WatchRunConfig,
    canonical_roots: &[SyncRoot],
    watch_plan: &WatchPlan,
    rx: &mpsc::Receiver<notify::Result<Event>>,
    stop: &AtomicBool,
    _lock: WatchLockGuard,
) -> Result<WatchRunSummary> {
    let mut state = WatchStateFile {
        pid: std::process::id(),
        started_at_ms: now_unix_ms(),
        last_sync_at_ms: None,
        synced_batches: 0,
        synced_files: 0,
        debounce_ms: u64::try_from(config.debounce.as_millis()).unwrap_or(DEFAULT_DEBOUNCE_MS),
        max_wait_ms: u64::try_from(config.max_wait.as_millis()).unwrap_or(DEFAULT_MAX_WAIT_MS),
        reconcile_secs: config
            .reconcile_interval
            .map_or(0, |interval| interval.as_secs()),
        last_error: None,
    };
    let state_path = watch_state_path(&config.database_path);
    write_state_file(&state_path, &state)?;

    emit_watch_log(
        "started",
        &config.database_path,
        canonical_roots.len(),
        0,
        0,
        None,
    );

    let mut runtime = WatchRuntime {
        config,
        canonical_roots,
        state: &mut state,
        state_path: &state_path,
        totals: SyncTotals::default(),
    };
    if config.initial_sync && !stop.load(Ordering::SeqCst) {
        runtime.sync_batch(canonical_roots, "initial_sync");
    }

    let filter = PathFilter::new(
        &config.database_path,
        &watch_plan.directory_roots,
        &watch_plan.exact_files,
    );
    runtime.run_event_loop(rx, &filter, stop);

    let totals = runtime.totals;
    emit_watch_log(
        "stopped",
        &config.database_path,
        canonical_roots.len(),
        state.synced_batches,
        state.synced_files,
        None,
    );

    Ok(WatchRunSummary {
        database: display_database_path(&config.database_path),
        watched_roots: canonical_roots
            .iter()
            .map(|root| display_database_path(&root.path))
            .collect(),
        synced_batches: state.synced_batches,
        synced_files: state.synced_files,
        indexed_records: totals.records,
        indexed_items: totals.items,
    })
}

#[derive(Default)]
struct SyncTotals {
    records: u64,
    items: u64,
}

struct WatchRuntime<'a> {
    config: &'a WatchRunConfig,
    canonical_roots: &'a [SyncRoot],
    state: &'a mut WatchStateFile,
    state_path: &'a Path,
    totals: SyncTotals,
}

impl WatchRuntime<'_> {
    fn run_event_loop(
        &mut self,
        rx: &mpsc::Receiver<notify::Result<Event>>,
        filter: &PathFilter,
        stop: &AtomicBool,
    ) {
        let mut scheduler = DebounceScheduler::new(self.config.debounce, self.config.max_wait);
        let mut next_reconcile = self
            .config
            .reconcile_interval
            .map(|interval| Instant::now() + interval);

        while !stop.load(Ordering::SeqCst) {
            let now = Instant::now();
            let mut timeout = scheduler.next_timeout(now).min(Duration::from_millis(250));
            if let Some(due) = next_reconcile {
                timeout = timeout.min(due.saturating_duration_since(now));
            }

            match rx.recv_timeout(timeout) {
                Ok(Ok(event)) => {
                    handle_notify_event(&event, filter, &mut scheduler, Instant::now());
                }
                Ok(Err(error)) => {
                    self.state.last_error = Some(format!("watch error: {error}"));
                    let _ = write_state_file(self.state_path, self.state);
                    emit_watch_log(
                        "watch_error",
                        &self.config.database_path,
                        0,
                        0,
                        0,
                        Some(&error.to_string()),
                    );
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            if stop.load(Ordering::SeqCst) {
                break;
            }
            let now = Instant::now();
            let batch = scheduler.take_due(now);
            if batch.full_rescan {
                self.sync_batch(self.canonical_roots, "rescan");
                next_reconcile = self
                    .config
                    .reconcile_interval
                    .map(|interval| Instant::now() + interval);
            } else if !batch.paths.is_empty() {
                let existing: Vec<SyncRoot> = batch
                    .paths
                    .into_iter()
                    .filter(|path| path.is_file())
                    .map(|path| SyncRoot { label: None, path })
                    .collect();
                if !existing.is_empty() {
                    self.sync_batch(&existing, "event_sync");
                }
            } else if let Some(due) = next_reconcile
                && now >= due
            {
                self.sync_batch(self.canonical_roots, "reconcile");
                next_reconcile = self
                    .config
                    .reconcile_interval
                    .map(|interval| Instant::now() + interval);
            }
        }
    }

    fn sync_batch(&mut self, roots: &[SyncRoot], kind: &str) {
        if !roots_have_any_trace_file(roots) {
            self.state.last_error = None;
            let _ = write_state_file(self.state_path, self.state);
            return;
        }
        // Open the database only for the duration of one batch so idle watchers
        // hold zero SQLite handles, zero WAL snapshots, and minimal memory.
        // Once a database contains Sources, its stored indexing policy is
        // authoritative and must be reused even if config limits later changed.
        let result = Store::open(&self.config.database_path).and_then(|mut store| {
            let (max_record_bytes, max_text_bytes) = if store.is_empty()? {
                (self.config.max_record_bytes, self.config.max_text_bytes)
            } else if let Some(stored) = store.indexing_policy()? {
                (
                    stored.max_indexed_record_bytes,
                    stored.max_published_text_bytes,
                )
            } else {
                (self.config.max_record_bytes, self.config.max_text_bytes)
            };
            let options = IndexOptions {
                rebuild: false,
                max_record_bytes,
                max_text_bytes,
            };
            let mut progress = NoopProgress;
            sync::execute_observed(&mut store, roots, &options, &mut progress)
        });

        match result {
            Ok(report) => {
                self.state.last_sync_at_ms = Some(now_unix_ms());
                self.state.synced_batches = self.state.synced_batches.saturating_add(1);
                self.state.synced_files = self
                    .state
                    .synced_files
                    .saturating_add(u64::try_from(report.indexed_files).unwrap_or(0));
                self.state.last_error = if report.failed_files > 0 {
                    Some(format!(
                        "{} files failed during {kind}",
                        report.failed_files
                    ))
                } else {
                    None
                };
                self.totals.records = self.totals.records.saturating_add(report.indexed_records);
                self.totals.items = self.totals.items.saturating_add(report.indexed_items);
                let _ = write_state_file(self.state_path, self.state);
                if report.indexed_files > 0 || report.failed_files > 0 || kind == "initial_sync" {
                    emit_watch_log(
                        kind,
                        &self.config.database_path,
                        report.indexed_files,
                        report.indexed_records,
                        report.metrics.elapsed_ms,
                        self.state.last_error.as_deref(),
                    );
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                self.state.last_error = Some(message.clone());
                let _ = write_state_file(self.state_path, self.state);
                emit_watch_log(kind, &self.config.database_path, 0, 0, 0, Some(&message));
            }
        }
    }
}

fn emit_watch_log(
    event: &str,
    database: &Path,
    files: usize,
    records_or_batches: u64,
    elapsed_ms: u64,
    error: Option<&str>,
) {
    #[derive(Serialize)]
    struct WatchLog<'a> {
        kind: &'static str,
        event: &'a str,
        at_ms: i64,
        database: String,
        files: usize,
        count: u64,
        elapsed_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'a str>,
    }

    let entry = WatchLog {
        kind: "watch",
        event,
        at_ms: now_unix_ms(),
        database: display_database_path(database),
        files,
        count: records_or_batches,
        elapsed_ms,
        error,
    };
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    if serde_json::to_writer(&mut lock, &entry).is_ok() {
        let _ = writeln!(lock);
    }
}

struct WatchPlan {
    directories: Vec<(PathBuf, RecursiveMode)>,
    directory_roots: Vec<PathBuf>,
    exact_files: BTreeSet<PathBuf>,
}

fn canonicalize_roots(roots: &[SyncRoot]) -> Result<Vec<SyncRoot>> {
    roots
        .iter()
        .map(|root| {
            let canonical = root.path.canonicalize().with_context(|| {
                format!(
                    "failed to canonicalize watch root {}: {}",
                    root.label.as_deref().unwrap_or("<unlabelled>"),
                    root.path.display()
                )
            })?;
            Ok(SyncRoot {
                label: root.label.clone(),
                path: canonical,
            })
        })
        .collect()
}

fn build_watch_plan(roots: &[SyncRoot]) -> Result<WatchPlan> {
    let mut dirs: HashMap<PathBuf, RecursiveMode> = HashMap::new();
    let mut directory_roots = Vec::new();
    let mut exact_files = BTreeSet::new();
    for root in roots {
        if root.path.is_dir() {
            dirs.insert(root.path.clone(), RecursiveMode::Recursive);
            directory_roots.push(root.path.clone());
        } else if root.path.is_file() {
            let parent = root
                .path
                .parent()
                .context("watched file has no parent directory")?
                .to_path_buf();
            dirs.entry(parent).or_insert(RecursiveMode::NonRecursive);
            exact_files.insert(root.path.clone());
        } else {
            bail!(
                "watch root {} is neither a file nor a directory",
                root.path.display()
            );
        }
    }
    let mut directories: Vec<_> = dirs.into_iter().collect();
    directories.sort_by(|a, b| a.0.cmp(&b.0));
    directory_roots.sort();
    directory_roots.dedup();
    Ok(WatchPlan {
        directories,
        directory_roots,
        exact_files,
    })
}

struct PathFilter {
    excluded: BTreeSet<PathBuf>,
    directory_roots: Vec<PathBuf>,
    exact_files: BTreeSet<PathBuf>,
}

impl PathFilter {
    fn new(
        database_path: &Path,
        directory_roots: &[PathBuf],
        exact_files: &BTreeSet<PathBuf>,
    ) -> Self {
        let mut excluded = BTreeSet::new();
        let canonical_db = database_path
            .canonicalize()
            .unwrap_or_else(|_| database_path.to_path_buf());
        for suffix in [
            "",
            "-wal",
            "-shm",
            ".watch.lock",
            ".watch.json",
            ".watch.json.tmp",
        ] {
            let mut os = canonical_db.as_os_str().to_os_string();
            os.push(suffix);
            excluded.insert(PathBuf::from(os));
        }
        Self {
            excluded,
            directory_roots: directory_roots.to_vec(),
            exact_files: exact_files.clone(),
        }
    }

    fn accepts(&self, path: &Path) -> bool {
        if self.excluded.contains(path) {
            return false;
        }
        if !is_trace_extension(path) {
            return false;
        }
        if !self.exact_files.is_empty() {
            let matches_exact = self.exact_files.contains(path)
                || path
                    .canonicalize()
                    .ok()
                    .is_some_and(|canonical| self.exact_files.contains(&canonical));
            if matches_exact {
                return true;
            }
            if self.directory_roots.is_empty()
                || !self
                    .directory_roots
                    .iter()
                    .any(|root| path.starts_with(root))
            {
                return false;
            }
        }
        // Inside an Antigravity conversation tree, ignore thousands of non-transcript
        // files under `.system_generated/steps/` and artifacts.
        if is_inside_antigravity_brain(path) {
            return antigravity::transcript(path).is_some();
        }
        true
    }
}

fn roots_have_any_trace_file(roots: &[SyncRoot]) -> bool {
    roots.iter().any(|root| path_has_any_trace_file(&root.path))
}

fn path_has_any_trace_file(path: &Path) -> bool {
    if path.is_file() {
        return is_trace_extension(path);
    }
    if !path.is_dir() {
        return false;
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry_path);
            } else if file_type.is_file() && is_trace_extension(&entry_path) {
                return true;
            }
        }
    }
    false
}

fn is_trace_extension(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    ends_with_ignore_ascii_case(name, ".jsonl") || ends_with_ignore_ascii_case(name, ".jsonl.zst")
}

fn ends_with_ignore_ascii_case(haystack: &str, suffix: &str) -> bool {
    haystack.len() >= suffix.len()
        && haystack.is_char_boundary(haystack.len() - suffix.len())
        && haystack[haystack.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

fn is_inside_antigravity_brain(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == ".system_generated")
}

fn handle_notify_event(
    event: &Event,
    filter: &PathFilter,
    scheduler: &mut DebounceScheduler,
    now: Instant,
) {
    if event.need_rescan() {
        scheduler.schedule_rescan(now);
        return;
    }
    if matches!(event.kind, EventKind::Access(_) | EventKind::Remove(_)) {
        return;
    }
    for path in &event.paths {
        if filter.accepts(path) {
            scheduler.record_path(path.clone(), now);
        }
    }
}

struct PendingEntry {
    first_seen: Instant,
    last_seen: Instant,
}

struct DueBatch {
    full_rescan: bool,
    paths: Vec<PathBuf>,
}

struct DebounceScheduler {
    debounce: Duration,
    max_wait: Duration,
    pending: HashMap<PathBuf, PendingEntry>,
    rescan_due: Option<Instant>,
}

impl DebounceScheduler {
    fn new(debounce: Duration, max_wait: Duration) -> Self {
        Self {
            debounce,
            max_wait,
            pending: HashMap::new(),
            rescan_due: None,
        }
    }

    fn record_path(&mut self, path: PathBuf, now: Instant) {
        self.pending
            .entry(path)
            .and_modify(|entry| entry.last_seen = now)
            .or_insert(PendingEntry {
                first_seen: now,
                last_seen: now,
            });
    }

    fn schedule_rescan(&mut self, now: Instant) {
        self.rescan_due = Some(now + self.debounce);
    }

    fn next_timeout(&self, now: Instant) -> Duration {
        let mut earliest: Option<Instant> = self.rescan_due;
        for entry in self.pending.values() {
            let due = (entry.last_seen + self.debounce).min(entry.first_seen + self.max_wait);
            earliest = Some(match earliest {
                Some(current) => current.min(due),
                None => due,
            });
        }
        earliest.map_or(Duration::from_secs(1), |due| {
            due.saturating_duration_since(now)
        })
    }

    fn take_due(&mut self, now: Instant) -> DueBatch {
        if let Some(due) = self.rescan_due
            && now >= due
        {
            self.rescan_due = None;
            self.pending.clear();
            return DueBatch {
                full_rescan: true,
                paths: Vec::new(),
            };
        }
        let mut paths = Vec::new();
        self.pending.retain(|path, entry| {
            let due = (entry.last_seen + self.debounce).min(entry.first_seen + self.max_wait);
            if now >= due {
                paths.push(path.clone());
                false
            } else {
                true
            }
        });
        paths.sort();
        DueBatch {
            full_rescan: false,
            paths,
        }
    }
}

fn now_unix_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use notify::EventKind;
    use notify::event::{DataChange, ModifyKind};
    use tempfile::tempdir;

    use super::{
        DebounceScheduler, PathFilter, WatchLockGuard, WatchRunConfig, build_watch_plan,
        canonicalize_roots, inspect_watch_status, run_watch_until, run_watch_with_receiver,
    };
    use crate::indexing::sync::SyncRoot;
    use crate::storage::db::Store;

    #[test]
    fn path_filter_rejects_database_artifacts_and_non_trace_files() {
        let db = PathBuf::from("/tmp/trace-index/index.sqlite");
        let filter = PathFilter::new(&db, &[], &BTreeSet::new());

        assert!(!filter.accepts(&db));
        assert!(!filter.accepts(&PathBuf::from("/tmp/trace-index/index.sqlite-wal")));
        assert!(!filter.accepts(&PathBuf::from("/tmp/trace-index/index.sqlite.watch.lock")));
        assert!(!filter.accepts(&PathBuf::from("/tmp/trace-index/index.sqlite.watch.json")));
        assert!(!filter.accepts(&PathBuf::from("/tmp/trace-index/notes.md")));
        assert!(filter.accepts(&PathBuf::from("/tmp/sessions/rollout.jsonl")));
        assert!(filter.accepts(&PathBuf::from("/tmp/sessions/rollout.jsonl.zst")));
        assert!(!filter.accepts(&PathBuf::from(
            "/tmp/brain/12345678-1234-1234-1234-123456789abc/.system_generated/steps/1/out.jsonl"
        )));
        assert!(filter.accepts(&PathBuf::from(
            "/tmp/brain/12345678-1234-1234-1234-123456789abc/.system_generated/logs/transcript.jsonl"
        )));
    }

    #[test]
    fn debounce_scheduler_coalesces_bursts_and_honors_max_wait() {
        let t0 = Instant::now();
        let mut sched = DebounceScheduler::new(Duration::from_secs(3), Duration::from_secs(10));
        let path = PathBuf::from("/tmp/a.jsonl");

        sched.record_path(path.clone(), t0);
        assert!(sched.take_due(t0 + Duration::from_secs(2)).paths.is_empty());

        // Continuous writes reset debounce, but max_wait still forces flush at t0 + 10s.
        sched.record_path(path.clone(), t0 + Duration::from_secs(2));
        sched.record_path(path.clone(), t0 + Duration::from_secs(4));
        sched.record_path(path.clone(), t0 + Duration::from_secs(8));
        assert!(sched.take_due(t0 + Duration::from_secs(9)).paths.is_empty());
        let batch = sched.take_due(t0 + Duration::from_secs(10));
        assert!(!batch.full_rescan);
        assert_eq!(batch.paths, vec![path]);
    }

    #[test]
    fn watch_lock_reports_active_exclusively_and_releases_on_drop() {
        let dir = tempdir().expect("tempdir");
        let db = dir.path().join("index.sqlite");

        assert!(!inspect_watch_status(&db).active);
        {
            let _guard = WatchLockGuard::acquire(&db).expect("acquire first lock");
            assert!(inspect_watch_status(&db).active);
            assert!(
                WatchLockGuard::acquire(&db).is_err(),
                "second watcher for same database must be rejected"
            );
        }
        assert!(!inspect_watch_status(&db).active);
    }

    fn write_initial_trace(trace: &std::path::Path) {
        std::fs::write(
            trace,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"watch-sess\",\"cwd\":\"/tmp\"}}\n",
                "{\"type\":\"turn_context\",\"payload\":{\"turn_id\":\"loop-1\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"initial hello\"}}\n"
            ),
        )
        .expect("write initial trace");
    }

    fn append_second_loop(trace: &std::path::Path) {
        let mut f = OpenOptions::new()
            .append(true)
            .open(trace)
            .expect("open append");
        writeln!(
            f,
            "{{\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"loop-2\"}}}}"
        )
        .expect("write turn");
        writeln!(
            f,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"event triggered follow up\"}}}}"
        )
        .expect("write msg");
        f.sync_all().expect("fsync");
    }

    fn wait_for_loops(db: &std::path::Path, target: i64, timeout: Duration) -> i64 {
        let deadline = Instant::now() + timeout;
        let mut count = 0_i64;
        while Instant::now() < deadline {
            if let Ok(store) = Store::open_read_only_without_metadata(db)
                && let Ok(cnt) =
                    store
                        .connection()
                        .query_row("SELECT COUNT(*) FROM loops", [], |r| r.get::<_, i64>(0))
            {
                count = cnt;
                if count >= target {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(30));
        }
        count
    }

    #[test]
    fn watch_event_loop_syncs_appended_lines_and_reports_live_status() {
        let dir = tempdir().expect("tempdir");
        let traces_dir = dir.path().join("traces");
        std::fs::create_dir_all(&traces_dir).expect("create traces dir");
        let trace = traces_dir.join("session.jsonl");
        write_initial_trace(&trace);
        let canonical_trace = trace.canonicalize().expect("canonical trace");
        let db = dir.path().join("index.sqlite");

        let config = WatchRunConfig {
            database_path: db.clone(),
            roots: vec![SyncRoot {
                label: Some("test".to_owned()),
                path: traces_dir,
            }],
            max_record_bytes: 1024 * 1024,
            max_text_bytes: 64 * 1024,
            debounce: Duration::from_millis(80),
            max_wait: Duration::from_millis(300),
            reconcile_interval: None,
            initial_sync: true,
        };
        let canonical_roots = canonicalize_roots(&config.roots).expect("canonical roots");
        let watch_plan = build_watch_plan(&canonical_roots).expect("watch plan");

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            run_watch_with_receiver(&config, &canonical_roots, &watch_plan, &rx, &stop_bg)
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let st = inspect_watch_status(&db);
            if st.active && st.synced_batches.unwrap_or(0) >= 1 {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let live_status = inspect_watch_status(&db);
        assert!(
            live_status.active,
            "watcher must report active while running"
        );
        assert!(live_status.synced_batches.unwrap_or(0) >= 1);

        append_second_loop(&trace);
        let modify_event =
            notify::Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
                .add_path(canonical_trace);
        tx.send(Ok(modify_event)).expect("send notify event");

        let loop_count = wait_for_loops(&db, 2, Duration::from_secs(5));
        stop.store(true, Ordering::SeqCst);
        let summary = handle.join().expect("join watcher").expect("watch ok");
        assert_eq!(
            loop_count, 2,
            "watcher must sync appended Loop on notify event"
        );
        assert!(summary.synced_batches >= 2);
        assert!(!inspect_watch_status(&db).active);
    }

    #[test]
    fn watch_reconcile_catches_missed_writes_without_os_events() {
        let dir = tempdir().expect("tempdir");
        let traces_dir = dir.path().join("traces");
        std::fs::create_dir_all(&traces_dir).expect("create traces dir");
        let trace = traces_dir.join("session.jsonl");
        write_initial_trace(&trace);
        let db = dir.path().join("index.sqlite");

        let config = WatchRunConfig {
            database_path: db.clone(),
            roots: vec![SyncRoot {
                label: Some("test".to_owned()),
                path: traces_dir,
            }],
            max_record_bytes: 1024 * 1024,
            max_text_bytes: 64 * 1024,
            debounce: Duration::from_millis(80),
            max_wait: Duration::from_millis(300),
            reconcile_interval: Some(Duration::from_millis(200)),
            initial_sync: true,
        };
        let canonical_roots = canonicalize_roots(&config.roots).expect("canonical roots");
        let watch_plan = build_watch_plan(&canonical_roots).expect("watch plan");

        let (_tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            run_watch_with_receiver(&config, &canonical_roots, &watch_plan, &rx, &stop_bg)
        });

        assert_eq!(wait_for_loops(&db, 1, Duration::from_secs(5)), 1);
        // Append without sending any OS notify event; safety-net reconcile must catch it.
        append_second_loop(&trace);

        let loop_count = wait_for_loops(&db, 2, Duration::from_secs(5));
        stop.store(true, Ordering::SeqCst);
        let summary = handle.join().expect("join watcher").expect("watch ok");
        assert_eq!(
            loop_count, 2,
            "periodic reconcile must catch missed OS events"
        );
        assert!(summary.synced_batches >= 2);
    }

    #[test]
    fn watch_startup_registers_os_watcher_runs_initial_sync_and_exits_cleanly() {
        let dir = tempdir().expect("tempdir");
        let traces_dir = dir.path().join("traces");
        std::fs::create_dir_all(&traces_dir).expect("create traces dir");
        let trace = traces_dir.join("session.jsonl");
        write_initial_trace(&trace);
        let db = dir.path().join("index.sqlite");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);
        let db_bg = db.clone();
        let traces_bg = traces_dir;

        let handle = thread::spawn(move || {
            run_watch_until(
                &WatchRunConfig {
                    database_path: db_bg,
                    roots: vec![SyncRoot {
                        label: Some("test".to_owned()),
                        path: traces_bg,
                    }],
                    max_record_bytes: 1024 * 1024,
                    max_text_bytes: 64 * 1024,
                    debounce: Duration::from_millis(150),
                    max_wait: Duration::from_millis(500),
                    reconcile_interval: Some(Duration::from_secs(super::DEFAULT_RECONCILE_SECS)),
                    initial_sync: true,
                },
                &stop_bg,
            )
        });

        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let st = inspect_watch_status(&db);
            if st.active && st.synced_batches.unwrap_or(0) >= 1 {
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }
        let live_status = inspect_watch_status(&db);
        assert!(live_status.active);
        assert_eq!(live_status.reconcile_secs, Some(300));
        assert_eq!(wait_for_loops(&db, 1, Duration::from_secs(5)), 1);

        stop.store(true, Ordering::SeqCst);
        let summary = handle.join().expect("join watcher").expect("watch ok");
        assert!(summary.synced_batches >= 1);
        assert!(!inspect_watch_status(&db).active);
    }

    #[test]
    fn watch_sync_batch_reuses_stored_indexing_policy_on_non_empty_database() {
        let dir = tempdir().expect("tempdir");
        let traces_dir = dir.path().join("traces");
        std::fs::create_dir_all(&traces_dir).expect("create traces dir");
        let trace = traces_dir.join("session.jsonl");
        write_initial_trace(&trace);
        let canonical_trace = trace.canonicalize().expect("canonical trace");
        let db = dir.path().join("index.sqlite");

        // Pre-populate the database with a custom indexing policy.
        {
            let mut store = Store::open(&db).expect("open store");
            let opts = crate::indexing::indexer::IndexOptions {
                rebuild: false,
                max_record_bytes: 2 * 1024 * 1024,
                max_text_bytes: 32 * 1024,
            };
            let mut progress = crate::indexing::telemetry::NoopProgress;
            crate::indexing::sync::execute_observed(
                &mut store,
                &[SyncRoot {
                    label: Some("seed".to_owned()),
                    path: traces_dir.clone(),
                }],
                &opts,
                &mut progress,
            )
            .expect("initial seed sync");
        }

        // Start watch with DIFFERENT configured limits; it must reuse the stored DB policy.
        let config = WatchRunConfig {
            database_path: db.clone(),
            roots: vec![SyncRoot {
                label: Some("test".to_owned()),
                path: traces_dir,
            }],
            max_record_bytes: 16 * 1024 * 1024,
            max_text_bytes: 64 * 1024,
            debounce: Duration::from_millis(80),
            max_wait: Duration::from_millis(300),
            reconcile_interval: None,
            initial_sync: true,
        };
        let canonical_roots = canonicalize_roots(&config.roots).expect("canonical roots");
        let watch_plan = build_watch_plan(&canonical_roots).expect("watch plan");
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            run_watch_with_receiver(&config, &canonical_roots, &watch_plan, &rx, &stop_bg)
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let st = inspect_watch_status(&db);
            if st.active && st.synced_batches.unwrap_or(0) >= 1 {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        assert!(
            inspect_watch_status(&db).last_error.is_none(),
            "watch must not fail with policy mismatch on non-empty DB"
        );

        append_second_loop(&trace);
        tx.send(Ok(notify::Event::new(EventKind::Modify(ModifyKind::Data(
            DataChange::Content,
        )))
        .add_path(canonical_trace)))
            .expect("send notify event");

        assert_eq!(wait_for_loops(&db, 2, Duration::from_secs(5)), 2);
        assert!(inspect_watch_status(&db).last_error.is_none());
        stop.store(true, Ordering::SeqCst);
        let _ = handle.join().expect("join watcher").expect("watch ok");
    }

    #[test]
    fn watch_sync_batch_ignores_empty_watched_root_without_error() {
        let dir = tempdir().expect("tempdir");
        let empty_dir = dir.path().join("empty-traces");
        std::fs::create_dir_all(&empty_dir).expect("create empty dir");
        let db = dir.path().join("index.sqlite");

        let config = WatchRunConfig {
            database_path: db.clone(),
            roots: vec![SyncRoot {
                label: Some("empty".to_owned()),
                path: empty_dir,
            }],
            max_record_bytes: 1024 * 1024,
            max_text_bytes: 64 * 1024,
            debounce: Duration::from_millis(80),
            max_wait: Duration::from_millis(300),
            reconcile_interval: None,
            initial_sync: true,
        };
        let canonical_roots = canonicalize_roots(&config.roots).expect("canonical roots");
        let watch_plan = build_watch_plan(&canonical_roots).expect("watch plan");
        let (_tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            run_watch_with_receiver(&config, &canonical_roots, &watch_plan, &rx, &stop_bg)
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if inspect_watch_status(&db).active {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let st = inspect_watch_status(&db);
        assert!(st.active);
        assert!(
            st.last_error.is_none(),
            "empty watched root must not record an error"
        );
        assert!(
            !db.exists(),
            "empty watched root should not create empty DB"
        );

        stop.store(true, Ordering::SeqCst);
        let _ = handle.join().expect("join watcher").expect("watch ok");
    }
}
