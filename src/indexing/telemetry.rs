//! Indexing progress and performance measurements.

use std::fs;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::interface::output::{
    IndexMetrics, IndexPersistMetrics, IndexPhaseMetrics, IndexWriteMetrics,
};

const AUTO_START_DELAY: Duration = Duration::from_secs(2);
const NDJSON_INTERVAL: Duration = Duration::from_secs(30);
const HUMAN_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexPhase {
    Discover,
    Prepare,
    Clear,
    Read,
    Project,
    Persist,
    Fingerprint,
    Commit,
    BuildIndexes,
}

/// A write site inside the `persist` phase.
///
/// Every one of them, so the shares are read against a denominator that holds
/// all of them rather than one chosen after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistTable {
    /// Physical Record rows, written by ingestion rather than by domain
    /// writer. Enumerating the write sites file by file missed it, and it is
    /// the largest of them.
    TraceRecords,
    Sessions,
    Loops,
    Items,
    ItemSearch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressMode {
    Auto,
    Human,
    Ndjson,
    Off,
}

#[derive(Debug)]
pub struct IndexTelemetry {
    started: Instant,
    phase_nanos: PhaseNanos,
    persist_nanos: PersistNanos,
    source_bytes: u64,
    processed_bytes: u64,
    fingerprint_bytes: u64,
    completed_files: usize,
    total_files: usize,
    records: u64,
    items: u64,
    writes: IndexWriteMetrics,
}

impl IndexTelemetry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            phase_nanos: PhaseNanos::default(),
            persist_nanos: PersistNanos::default(),
            source_bytes: 0,
            processed_bytes: 0,
            fingerprint_bytes: 0,
            completed_files: 0,
            total_files: 0,
            records: 0,
            items: 0,
            writes: IndexWriteMetrics::default(),
        }
    }

    pub fn record_duration(&mut self, phase: IndexPhase, duration: Duration) {
        self.phase_nanos.add(phase, duration);
    }

    pub fn record_persist(&mut self, table: PersistTable, duration: Duration) {
        self.persist_nanos.add(table, duration);
    }

    pub fn set_corpus(&mut self, total_files: usize, source_bytes: u64) {
        self.total_files = total_files;
        self.source_bytes = source_bytes;
    }

    pub fn add_processed_bytes(&mut self, bytes: u64) {
        self.processed_bytes = self.processed_bytes.saturating_add(bytes);
    }

    pub fn add_fingerprint_bytes(&mut self, bytes: u64) {
        self.fingerprint_bytes = self.fingerprint_bytes.saturating_add(bytes);
    }

    pub fn complete_file(&mut self) {
        self.completed_files = self.completed_files.saturating_add(1);
    }

    pub fn count_record(&mut self) {
        self.records = self.records.saturating_add(1);
        self.writes.records = self.writes.records.saturating_add(1);
    }

    pub fn count_item(&mut self) {
        self.items = self.items.saturating_add(1);
        self.writes.items = self.writes.items.saturating_add(1);
    }

    pub fn count_session(&mut self) {
        self.writes.sessions = self.writes.sessions.saturating_add(1);
    }

    pub fn count_loop(&mut self) {
        self.writes.loops = self.writes.loops.saturating_add(1);
    }

    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    #[must_use]
    pub fn progress(&self, phase: IndexPhase, current_source: Option<&Path>) -> ProgressUpdate {
        ProgressUpdate {
            kind: "progress",
            phase,
            elapsed_ms: duration_ms(self.elapsed()),
            completed_files: self.completed_files,
            total_files: self.total_files,
            processed_bytes: self.processed_bytes,
            source_bytes: self.source_bytes,
            records: self.records,
            items: self.items,
            current_source: current_source.map(|path| path.to_string_lossy().into_owned()),
        }
    }

    #[must_use]
    pub fn finish(self, database_bytes_before: u64, database_bytes_after: u64) -> IndexMetrics {
        let elapsed_ms = duration_ms(self.started.elapsed());
        let phases_ms = self.phase_nanos.finish(elapsed_ms);
        let persist_ms = self.persist_nanos.finish(phases_ms.persist);
        IndexMetrics {
            elapsed_ms,
            source_bytes: self.source_bytes,
            processed_bytes: self.processed_bytes,
            fingerprint_bytes: self.fingerprint_bytes,
            database_bytes_before,
            database_bytes_after,
            phases_ms,
            persist_ms,
            writes: self.writes,
        }
    }
}

impl Default for IndexTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
struct PhaseNanos {
    discover: u128,
    prepare: u128,
    clear: u128,
    read: u128,
    project: u128,
    persist: u128,
    fingerprint: u128,
    commit: u128,
    build_indexes: u128,
}

impl PhaseNanos {
    fn add(&mut self, phase: IndexPhase, duration: Duration) {
        let target = match phase {
            IndexPhase::Discover => &mut self.discover,
            IndexPhase::Prepare => &mut self.prepare,
            IndexPhase::Clear => &mut self.clear,
            IndexPhase::Read => &mut self.read,
            IndexPhase::Project => &mut self.project,
            IndexPhase::Persist => &mut self.persist,
            IndexPhase::Fingerprint => &mut self.fingerprint,
            IndexPhase::Commit => &mut self.commit,
            IndexPhase::BuildIndexes => &mut self.build_indexes,
        };
        *target = target.saturating_add(duration.as_nanos());
    }

    fn finish(self, elapsed_ms: u64) -> IndexPhaseMetrics {
        let mut phases = IndexPhaseMetrics {
            discover: nanos_ms(self.discover),
            prepare: nanos_ms(self.prepare),
            clear: nanos_ms(self.clear),
            read: nanos_ms(self.read),
            project: nanos_ms(self.project),
            persist: nanos_ms(self.persist),
            fingerprint: nanos_ms(self.fingerprint),
            commit: nanos_ms(self.commit),
            build_indexes: nanos_ms(self.build_indexes),
            unattributed: 0,
        };
        let attributed = phases
            .discover
            .saturating_add(phases.prepare)
            .saturating_add(phases.clear)
            .saturating_add(phases.read)
            .saturating_add(phases.project)
            .saturating_add(phases.persist)
            .saturating_add(phases.fingerprint)
            .saturating_add(phases.commit)
            .saturating_add(phases.build_indexes);
        phases.unattributed = elapsed_ms.saturating_sub(attributed);
        phases
    }
}

#[derive(Debug, Default)]
struct PersistNanos {
    trace_records: u128,
    sessions: u128,
    loops: u128,
    items: u128,
    item_search: u128,
}

impl PersistNanos {
    fn add(&mut self, table: PersistTable, duration: Duration) {
        let target = match table {
            PersistTable::TraceRecords => &mut self.trace_records,
            PersistTable::Sessions => &mut self.sessions,
            PersistTable::Loops => &mut self.loops,
            PersistTable::Items => &mut self.items,
            PersistTable::ItemSearch => &mut self.item_search,
        };
        *target = target.saturating_add(duration.as_nanos());
    }

    fn finish(self, persist_ms: u64) -> IndexPersistMetrics {
        let mut tables = IndexPersistMetrics {
            trace_records: nanos_ms(self.trace_records),
            sessions: nanos_ms(self.sessions),
            loops: nanos_ms(self.loops),
            items: nanos_ms(self.items),
            item_search: nanos_ms(self.item_search),
            other: 0,
        };
        let attributed = [
            tables.trace_records,
            tables.sessions,
            tables.loops,
            tables.items,
            tables.item_search,
        ]
        .into_iter()
        .fold(0_u64, u64::saturating_add);
        tables.other = persist_ms.saturating_sub(attributed);
        tables
    }
}

#[derive(Debug, Serialize)]
pub struct ProgressUpdate {
    #[serde(rename = "type")]
    kind: &'static str,
    phase: IndexPhase,
    elapsed_ms: u64,
    completed_files: usize,
    total_files: usize,
    processed_bytes: u64,
    source_bytes: u64,
    records: u64,
    items: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_source: Option<String>,
}

pub trait ProgressObserver {
    fn update(&mut self, update: ProgressUpdate, force: bool);
    fn finish(&mut self);
}

#[derive(Debug, Default)]
#[cfg(test)]
pub struct NoopProgress;

#[cfg(test)]
impl ProgressObserver for NoopProgress {
    fn update(&mut self, _update: ProgressUpdate, _force: bool) {}
    fn finish(&mut self) {}
}

#[derive(Debug)]
pub struct StderrProgress {
    mode: ProgressMode,
    started: Instant,
    last_write: Option<Instant>,
    rendered_human: bool,
    auto_delay: bool,
}

impl StderrProgress {
    #[must_use]
    pub fn new(requested: ProgressMode) -> Self {
        let stderr_is_terminal = std::io::stderr().is_terminal();
        let (mode, auto_delay) = match requested {
            ProgressMode::Auto if stderr_is_terminal => (ProgressMode::Human, true),
            ProgressMode::Auto => (ProgressMode::Ndjson, true),
            mode => (mode, false),
        };
        Self {
            mode,
            started: Instant::now(),
            last_write: None,
            rendered_human: false,
            auto_delay,
        }
    }

    fn should_write(&self, now: Instant, force: bool) -> bool {
        if self.mode == ProgressMode::Off {
            return false;
        }
        if self.auto_delay && now.duration_since(self.started) < AUTO_START_DELAY {
            return false;
        }
        if force && !self.auto_delay {
            return true;
        }
        let interval = match self.mode {
            ProgressMode::Human => HUMAN_INTERVAL,
            ProgressMode::Ndjson => NDJSON_INTERVAL,
            ProgressMode::Auto | ProgressMode::Off => return false,
        };
        self.last_write
            .is_none_or(|last| now.duration_since(last) >= interval)
    }
}

impl ProgressObserver for StderrProgress {
    fn update(&mut self, update: ProgressUpdate, force: bool) {
        let now = Instant::now();
        if !self.should_write(now, force) {
            return;
        }
        let mut stderr = std::io::stderr().lock();
        match self.mode {
            ProgressMode::Human => {
                let _ = write!(
                    stderr,
                    "\rphase={:?} files={}/{} bytes={}/{} records={} items={}",
                    update.phase,
                    update.completed_files,
                    update.total_files,
                    update.processed_bytes,
                    update.source_bytes,
                    update.records,
                    update.items
                );
                let _ = stderr.flush();
                self.rendered_human = true;
            }
            ProgressMode::Ndjson => {
                let _ = serde_json::to_writer(&mut stderr, &update);
                let _ = writeln!(stderr);
            }
            ProgressMode::Auto | ProgressMode::Off => {}
        }
        self.last_write = Some(now);
    }

    fn finish(&mut self) {
        if self.rendered_human {
            let _ = writeln!(std::io::stderr());
            self.rendered_human = false;
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn nanos_ms(nanos: u128) -> u64 {
    u64::try_from(nanos / 1_000_000).unwrap_or(u64::MAX)
}

#[must_use]
pub fn database_storage_bytes(path: &Path) -> u64 {
    let mut total = fs::metadata(path).map_or(0, |metadata| metadata.len());
    let path_text = path.as_os_str().to_string_lossy();
    for suffix in ["-wal", "-shm"] {
        let sidecar = format!("{path_text}{suffix}");
        total = total.saturating_add(fs::metadata(sidecar).map_or(0, |metadata| metadata.len()));
    }
    total
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{IndexPhase, IndexTelemetry};

    #[test]
    fn reports_unattributed_time_without_underflow() {
        let mut telemetry = IndexTelemetry::new();
        telemetry.record_duration(IndexPhase::Read, Duration::from_mins(1));
        let metrics = telemetry.finish(0, 0);
        assert_eq!(metrics.phases_ms.unattributed, 0);
    }
}
