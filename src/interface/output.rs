//! Stable machine-readable response objects emitted by the CLI.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::storage::db::IndexingPolicy;

#[derive(Debug, Clone, Serialize)]
pub struct RecordParse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub id: i64,
    pub source: RecordSource,
    pub seq: u64,
    pub byte_offset: u64,
    pub byte_length: u64,
    pub raw_hash: String,
    pub parse: RecordParse,
    pub oversized: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordSource {
    pub adapter: String,
    pub uri: String,
}

#[derive(Debug, Serialize)]
pub struct RecordResponse {
    pub record: Record,
    pub raw_verified: bool,
    pub representation: serde_json::Value,
    pub externalized_values: usize,
    pub representation_bytes: usize,
}

#[derive(Debug, Serialize)]
pub struct RecordVerification {
    pub record_id: i64,
    pub status: String,
    pub expected_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_hash: Option<String>,
    pub expected_bytes: u64,
    pub actual_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct RecordExportReport {
    pub record_id: i64,
    pub output: String,
    pub byte_length: u64,
    pub raw_hash: String,
}

#[derive(Debug, Serialize)]
pub struct AssetDescriptor {
    pub reference: String,
    pub record_id: i64,
    pub json_pointer: String,
    pub media_type: String,
    pub encoding: String,
    pub encoded_byte_length: usize,
}

#[derive(Debug, Serialize)]
pub struct AssetExtractReport {
    pub asset: AssetDescriptor,
    pub output: String,
    pub byte_length: usize,
    pub content_hash: String,
}

#[derive(Debug, Serialize)]
pub struct IndexReport {
    pub adapters: Vec<String>,
    pub database: String,
    pub indexing_policy: IndexingPolicy,
    pub discovered_files: usize,
    pub indexed_files: usize,
    pub unchanged_files: usize,
    pub rebuilt_files: usize,
    pub indexed_records: u64,
    pub indexed_items: u64,
    pub malformed_records: u64,
    pub oversized_records: u64,
    pub incomplete_files: usize,
    /// Discovered `.jsonl` files that are not traces this tool reads, or whose
    /// first Record is not yet complete. A trace root routinely holds them, so
    /// this is an ordinary outcome and does not affect the exit status.
    pub skipped_files: usize,
    /// Traces that could not be indexed. Each keeps a `source_results` entry
    /// naming the failure, and the command exits non-zero, because the index
    /// that resulted is short of what the caller asked for.
    pub failed_files: usize,
    pub metrics: IndexMetrics,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_results: Vec<SourceIndexResult>,
}

#[derive(Debug, Default, Serialize)]
pub struct IndexMetrics {
    pub elapsed_ms: u64,
    pub source_bytes: u64,
    pub processed_bytes: u64,
    pub fingerprint_bytes: u64,
    pub database_bytes_before: u64,
    pub database_bytes_after: u64,
    pub phases_ms: IndexPhaseMetrics,
    pub persist_ms: IndexPersistMetrics,
    pub writes: IndexWriteMetrics,
}

#[derive(Debug, Default, Serialize)]
pub struct IndexPhaseMetrics {
    pub discover: u64,
    pub prepare: u64,
    /// Discarding the previous projection before rebuilding it.
    ///
    /// Split out of `unattributed` because a changed Source drops all of its
    /// domain rows through one cascading delete, and the cost of that cascade is
    /// not visible in the row counts `writes` reports.
    pub clear: u64,
    pub read: u64,
    pub project: u64,
    pub persist: u64,
    pub fingerprint: u64,
    pub commit: u64,
    pub build_indexes: u64,
    pub unattributed: u64,
}

/// Where the `persist` phase spent its time, per statement target.
///
/// A partition of `phases_ms.persist`, in the same sense that `phases_ms` is a
/// partition of `elapsed_ms`: every write site inside the phase is timed, and
/// `other` is what is left once they are subtracted. Naming only some of them
/// would let the share of whichever one was measured be read against a
/// denominator that excludes its rivals.
#[derive(Debug, Default, Serialize)]
pub struct IndexPersistMetrics {
    pub trace_records: u64,
    pub sessions: u64,
    pub loops: u64,
    pub items: u64,
    pub item_search: u64,
    /// Marshalling, hashing and correlation between the writes above.
    pub other: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct IndexWriteMetrics {
    pub records: u64,
    pub sessions: u64,
    pub loops: u64,
    pub items: u64,
}

#[derive(Debug, Serialize)]
pub struct ResponseEnvelope<T: Serialize> {
    pub data: T,
}

#[derive(Debug, Serialize)]
pub struct SqlQueryReport {
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    pub returned: usize,
    pub cells_truncated: usize,
    pub columns: Vec<String>,
    pub rows: Vec<BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Serialize)]
pub struct SchemaReport {
    pub objects: Vec<SchemaObject>,
}

#[derive(Debug, Serialize)]
pub struct SchemaObject {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<SchemaColumn>,
}

#[derive(Debug, Serialize)]
pub struct SchemaColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct SourceIndexResult {
    pub path: String,
    pub adapter: String,
    pub action: String,
    pub indexed_records: u64,
    pub indexed_items: u64,
    pub indexed_bytes: u64,
    pub file_size: u64,
    pub malformed_records: u64,
    pub oversized_records: u64,
    pub incomplete_tail: bool,
    /// Why this Source was skipped, when `action` is `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub database: String,
    pub storage_format: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexing_policy: Option<IndexingPolicy>,
    pub sources: usize,
    pub source_bytes: u64,
    pub indexed_bytes: u64,
    pub incomplete_sources: usize,
    /// Which semantic roles this index actually holds, and from which
    /// adapters. The compiled vocabulary says what the code can produce; this
    /// says what the corpus did produce, and the two disagreeing is how an
    /// unreachable rule or an unadapted record type becomes visible.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub observed_roles: Vec<ObservedRole>,
}

#[derive(Debug, Serialize)]
pub struct ObservedRole {
    pub semantic_role: String,
    pub adapters: Vec<String>,
    pub items: u64,
    /// The vocabulary declares this role but no adapter here produced it, or
    /// it declares support this index does not show.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
