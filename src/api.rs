//! Shared native application boundary. Reads never create or synchronize an index.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};

use crate::indexing::indexer::IndexOptions;
use crate::indexing::sync::{self, SyncRoot};
use crate::indexing::telemetry::NoopProgress;
use crate::interface::config::{self, EffectiveIndexingConfig, LoadedConfig};
use crate::storage::db::Store;

pub use crate::interface::docs::{TopicList, get as doc, list as docs};
pub use crate::interface::help::agent_context;
pub use crate::interface::output::*;
pub use crate::interface::query::{NativeCell, QueryFailure, UnknownRelation};
pub use crate::storage::db::IndexingPolicy;
pub use rusqlite::types::Value as Parameter;

/// A selected index with a reusable read-only connection, opened on demand.
pub struct Index {
    loaded: LoadedConfig,
    reader: Option<Store>,
    closed: bool,
}

/// Attempt to use a closed native handle.
#[derive(Debug)]
pub struct ClosedIndex;
impl std::fmt::Display for ClosedIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("index is closed")
    }
}
impl std::error::Error for ClosedIndex {}

impl Index {
    /// Captures configuration without opening or creating the database.
    ///
    /// # Errors
    /// Returns configuration or path resolution errors.
    pub fn new(config: Option<&Path>, db: Option<&Path>) -> Result<Self> {
        Ok(Self {
            loaded: config::load(config, db)?,
            reader: None,
            closed: false,
        })
    }

    /// Selected absolute database path.
    #[must_use]
    pub fn database(&self) -> &Path {
        &self.loaded.effective.database.path
    }

    /// Opens the selected existing database for reading.
    ///
    /// # Errors
    /// Returns closed-handle, storage-format, or I/O errors.
    pub fn open(&mut self) -> Result<()> {
        self.reader().map(|_| ())
    }

    /// Releases the connection permanently. Independent result values remain valid.
    pub fn close(&mut self) {
        self.reader = None;
        self.closed = true;
    }

    fn check_open(&self) -> Result<()> {
        if self.closed {
            return Err(ClosedIndex.into());
        }
        Ok(())
    }

    fn reader(&mut self) -> Result<&Store> {
        self.check_open()?;
        if self.reader.is_none() {
            self.reader = Some(Store::open_read_only(self.database())?);
        }
        Ok(self.reader.as_ref().expect("reader initialized"))
    }

    /// Executes one bounded statement using true `SQLite` parameter binding.
    ///
    /// # Errors
    /// Returns budget, parameter, SQL, storage, or closed-handle errors.
    pub fn query(
        &mut self,
        sql: &str,
        parameters: &BTreeMap<String, Parameter>,
        budget: &QueryBudget,
    ) -> Result<SqlQueryReport<NativeCell>> {
        budget.validate()?;
        if sql.len() > 1024 * 1024 {
            return Err(QueryFailure::Rejected("SQL exceeds 1 MiB".into()).into());
        }
        crate::interface::query::query_sql_native(
            self.reader()?,
            sql,
            parameters,
            budget.limit,
            budget.max_cell_bytes,
            budget.max_output_bytes,
            budget.timeout,
        )
    }

    /// Discovers the current database's public schema.
    ///
    /// # Errors
    /// Returns unknown-relation, storage, or closed-handle errors.
    pub fn schema(&mut self, relation: Option<&str>) -> Result<SchemaReport> {
        crate::interface::query::public_schema(self.reader()?, relation, true)
    }

    /// Reads index coverage without synchronizing.
    ///
    /// # Errors
    /// Returns storage or closed-handle errors.
    pub fn status(&mut self) -> Result<StatusReport> {
        let path = self.database().to_owned();
        crate::indexing::status::status(self.reader()?, &path)
    }

    /// Explicitly synchronizes sources, creating the database when absent.
    ///
    /// Empty paths select configured roots. Per-source failures remain in the
    /// returned report; writes are per-source, not an all-or-nothing transaction.
    /// No callbacks, stdout, progress threads, or automatic retries are used.
    ///
    /// # Errors
    /// Returns configuration, indexing-policy, I/O, or closed-handle errors.
    /// An error does not imply that previous source writes were rolled back.
    pub fn sync(&mut self, paths: Vec<PathBuf>, options: &SyncOptions) -> Result<IndexReport> {
        self.check_open()?;
        if options.max_record_bytes == Some(0) || options.max_text_bytes == Some(0) {
            bail!("synchronization byte limits must be positive");
        }
        let roots = explicit_roots(paths, &self.loaded)?;
        // Validate roots before creating a database for a misspelled path.
        for root in &roots {
            root.path.canonicalize()?;
        }
        self.reader = None;
        let mut store = Store::open(self.database())?;
        let policy = resolve_indexing_policy(
            &self.loaded.effective.indexing,
            store.indexing_policy()?,
            store.is_empty()?,
            options.max_record_bytes,
            options.max_text_bytes,
        );
        sync::execute_observed(
            &mut store,
            &roots,
            &IndexOptions {
                rebuild: options.rebuild,
                max_record_bytes: policy.max_indexed_record_bytes,
                max_text_bytes: policy.max_published_text_bytes,
            },
            &mut NoopProgress,
        )
    }

    /// Verifies an indexed record against its source bytes.
    ///
    /// # Errors
    /// Returns unknown-record, I/O, or closed-handle errors.
    pub fn verify(&mut self, record_id: i64) -> Result<RecordVerification> {
        crate::ingest::record::verify_record(self.reader()?, record_id)
    }

    /// Reads a bounded display-safe representation, never byte-exact raw data.
    ///
    /// # Errors
    /// Returns unknown-record, I/O, or closed-handle errors.
    pub fn inspect(
        &mut self,
        record_id: i64,
        max_record_bytes: u64,
        max_value_bytes: usize,
        max_output_bytes: usize,
    ) -> Result<RecordResponse> {
        crate::ingest::record::inspect_record(
            self.reader()?,
            record_id,
            max_record_bytes,
            max_value_bytes,
            max_output_bytes,
        )
    }
}

/// SQL execution and observation budgets, independent of Python object overhead.
#[derive(Debug, Clone)]
pub struct QueryBudget {
    pub limit: usize,
    pub max_cell_bytes: usize,
    pub max_output_bytes: usize,
    pub timeout: Duration,
}
impl Default for QueryBudget {
    fn default() -> Self {
        Self {
            limit: 1000,
            max_cell_bytes: 65536,
            max_output_bytes: 65536,
            timeout: Duration::from_secs(30),
        }
    }
}
impl QueryBudget {
    /// Validates the same row and byte ranges as the CLI; fractional timeouts are allowed.
    ///
    /// # Errors
    /// Returns a rejected-query error for invalid budgets.
    pub fn validate(&self) -> Result<()> {
        if !(1..=10_000).contains(&self.limit)
            || !(1..=1024 * 1024).contains(&self.max_cell_bytes)
            || !(1024..=16 * 1024 * 1024).contains(&self.max_output_bytes)
            || self.timeout.is_zero()
            || self.timeout > Duration::from_mins(5)
        {
            return Err(QueryFailure::Rejected("invalid query budget: limit 1..10000, cell bytes 1..1048576, output bytes 1024..16777216, timeout (0,300] seconds".into()).into());
        }
        Ok(())
    }
}

/// Explicit synchronization overrides; absent byte limits reuse stored policy.
#[derive(Debug, Default)]
pub struct SyncOptions {
    pub rebuild: bool,
    pub max_record_bytes: Option<usize>,
    pub max_text_bytes: Option<usize>,
}

fn configured_roots(loaded: &LoadedConfig) -> Vec<SyncRoot> {
    loaded
        .effective
        .roots
        .iter()
        .map(|root| SyncRoot {
            label: root.label.clone(),
            path: root.path.clone(),
        })
        .collect()
}

pub(crate) fn explicit_roots(paths: Vec<PathBuf>, loaded: &LoadedConfig) -> Result<Vec<SyncRoot>> {
    if paths.is_empty() {
        let roots = configured_roots(loaded);
        if roots.is_empty() {
            bail!("index sync requires paths or at least one configured [[roots]] entry");
        }
        return Ok(roots);
    }
    Ok(paths
        .into_iter()
        .enumerate()
        .map(|(index, path)| SyncRoot {
            label: Some(format!("argument-{}", index + 1)),
            path,
        })
        .collect())
}

/// Resolves the policy requested by one synchronization at the point where
/// CLI, configuration, and storage state meet.
pub(crate) fn resolve_indexing_policy(
    configured: &EffectiveIndexingConfig,
    stored: Option<IndexingPolicy>,
    index_is_empty: bool,
    cli_max_record_bytes: Option<usize>,
    cli_max_text_bytes: Option<usize>,
) -> IndexingPolicy {
    let baseline = if index_is_empty {
        IndexingPolicy {
            max_indexed_record_bytes: configured.max_indexed_record_bytes.value,
            max_published_text_bytes: configured.max_published_text_bytes.value,
        }
    } else {
        stored.unwrap_or(IndexingPolicy {
            max_indexed_record_bytes: configured.max_indexed_record_bytes.value,
            max_published_text_bytes: configured.max_published_text_bytes.value,
        })
    };
    IndexingPolicy {
        max_indexed_record_bytes: cli_max_record_bytes.unwrap_or(baseline.max_indexed_record_bytes),
        max_published_text_bytes: cli_max_text_bytes.unwrap_or(baseline.max_published_text_bytes),
    }
}
