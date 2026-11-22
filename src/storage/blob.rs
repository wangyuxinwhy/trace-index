//! Deduplicated storage for bounded text referenced by Semantic values.
//!
//! Item rows retain a Blob reference instead of repeating large text. The
//! public `blobs` relation resolves that reference; complete Runtime bytes
//! remain available only through the Item's Record evidence.

use std::collections::HashMap;
use std::io::Read as _;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use rusqlite::types::{ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use super::db::to_sql_i64;
use crate::adapters::projection::{BoundedText, ContentHash};
use crate::indexing::telemetry::{IndexTelemetry, PersistTable};

/// Minimum UTF-8 byte length before attempting per-blob `zstd` compression.
const BLOB_COMPRESS_MIN_BYTES: usize = 128;
/// `zstd` compression level chosen from 190-file corpus benchmarks (2.58x
/// ratio, ~1.8s total across 93k blobs, ~7.6 us/blob decompression).
const BLOB_ZSTD_LEVEL: i32 = 3;
/// Upper safety cap on decompressed blob size so a damaged row cannot trigger
/// unbounded allocation.
const MAX_DECOMPRESSED_BLOB_BYTES: usize = 64 * 1024 * 1024;

/// Decodes a `zstd`-compressed UTF-8 blob stored in `content_blobs.text`.
pub(crate) fn decode_blob_text(compressed: &[u8]) -> Result<String> {
    let decoder = zstd::stream::read::Decoder::new(compressed)
        .context("failed to initialize zstd blob decoder")?;
    let mut output = Vec::new();
    decoder
        .take((MAX_DECOMPRESSED_BLOB_BYTES as u64) + 1)
        .read_to_end(&mut output)
        .context("failed to decompress Semantic Blob")?;
    ensure!(
        output.len() <= MAX_DECOMPRESSED_BLOB_BYTES,
        "decompressed Semantic Blob exceeds safety limit"
    );
    String::from_utf8(output).context("decompressed Semantic Blob is not valid UTF-8")
}

/// Deduplicates Semantic text across one indexing run.
///
/// Runtime instructions and replayed context repeat across Sources. Keeping
/// this cache at run scope avoids asking `SQLite` about the same digest for each
/// copy. Pending entries are removed after a Source transaction rolls back so
/// the cache never returns an id whose row no longer exists.
pub(crate) struct BlobCache {
    ids: HashMap<ContentHash, i64>,
    pending: Vec<ContentHash>,
    /// True when the database started empty for this run, so every stored
    /// `content_blobs` row was inserted through this cache. A cache miss is
    /// therefore guaranteed absent from `SQLite` and can skip the lookup probe.
    known_complete: bool,
    compressor: zstd::bulk::Compressor<'static>,
}

impl Default for BlobCache {
    fn default() -> Self {
        Self {
            ids: HashMap::new(),
            pending: Vec::new(),
            known_complete: false,
            compressor: zstd::bulk::Compressor::new(BLOB_ZSTD_LEVEL)
                .expect("zstd compressor initializes with constant level"),
        }
    }
}

impl BlobCache {
    /// Creates a run-scoped cache, enabling the direct-insert fast path when
    /// `content_blobs` is provably empty at the start of the run.
    pub(crate) fn for_connection(connection: &Connection) -> Result<Self> {
        let empty: bool = connection
            .query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM content_blobs)",
                [],
                |row| row.get(0),
            )
            .context("failed to inspect whether Semantic Blobs exist")?;
        Ok(Self {
            ids: HashMap::new(),
            pending: Vec::new(),
            known_complete: empty,
            compressor: zstd::bulk::Compressor::new(BLOB_ZSTD_LEVEL)
                .context("failed to initialize zstd blob compressor")?,
        })
    }

    pub(crate) fn begin_source(&mut self) {
        self.pending.clear();
    }

    pub(crate) fn commit_source(&mut self) {
        self.pending.clear();
    }

    pub(crate) fn rollback_source(&mut self) {
        for key in self.pending.drain(..) {
            self.ids.remove(&key);
        }
    }

    /// Looks up an existing Blob id without inserting or compressing on a miss.
    pub(crate) fn lookup_existing(
        &mut self,
        transaction: &Transaction<'_>,
        content: &BoundedText,
    ) -> Result<Option<i64>> {
        if let Some(&id) = self.ids.get(&content.hash) {
            return Ok(Some(id));
        }
        if self.known_complete {
            return Ok(None);
        }
        let Some(id) = lookup_blob_id(transaction, &content.hash)? else {
            return Ok(None);
        };
        self.ids.insert(content.hash, id);
        Ok(Some(id))
    }

    pub(crate) fn intern(
        &mut self,
        transaction: &Transaction<'_>,
        content: &BoundedText,
        telemetry: &mut IndexTelemetry,
    ) -> Result<i64> {
        let text = content
            .text
            .as_deref()
            .context("Semantic Blob has no published text")?;
        let key = content.hash;
        if let Some(id) = self.ids.get(&key) {
            return Ok(*id);
        }
        let full_bytes = to_sql_i64(content.full_bytes, "Semantic Blob byte length")?;
        let estimated_tokens =
            to_sql_i64(content.estimated_tokens, "Semantic Blob token estimate")?;
        let started = Instant::now();
        let (id, inserted) = if self.known_complete {
            let compressed = maybe_compress(&mut self.compressor, text);
            let stored = stored_blob_value(text, compressed.as_deref());
            if let Some(id) = insert_blob_if_absent(
                transaction,
                &content.hash,
                &stored,
                full_bytes,
                estimated_tokens,
            )? {
                (id, true)
            } else {
                // Another writer populated `content_blobs` after this run began.
                self.known_complete = false;
                let id = lookup_blob_id(transaction, &content.hash)?
                    .context("Semantic Blob missing after uniqueness conflict")?;
                (id, false)
            }
        } else if let Some(id) = lookup_blob_id(transaction, &content.hash)? {
            (id, false)
        } else {
            let compressed = maybe_compress(&mut self.compressor, text);
            let stored = stored_blob_value(text, compressed.as_deref());
            if let Some(id) = insert_blob_if_absent(
                transaction,
                &content.hash,
                &stored,
                full_bytes,
                estimated_tokens,
            )? {
                (id, true)
            } else {
                let id = lookup_blob_id(transaction, &content.hash)?
                    .context("Semantic Blob missing after uniqueness conflict")?;
                (id, false)
            }
        };
        telemetry.record_persist(PersistTable::Items, started.elapsed());
        self.ids.insert(key, id);
        if inserted {
            self.pending.push(key);
        }
        Ok(id)
    }
}

fn maybe_compress(compressor: &mut zstd::bulk::Compressor<'_>, text: &str) -> Option<Vec<u8>> {
    if text.len() < BLOB_COMPRESS_MIN_BYTES {
        return None;
    }
    let compressed = compressor.compress(text.as_bytes()).ok()?;
    (compressed.len() < text.len()).then_some(compressed)
}

struct StoredBlob<'a>(ValueRef<'a>);

impl ToSql for StoredBlob<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(self.0))
    }
}

fn stored_blob_value<'a>(text: &'a str, compressed: Option<&'a [u8]>) -> StoredBlob<'a> {
    match compressed {
        Some(bytes) => StoredBlob(ValueRef::Blob(bytes)),
        None => StoredBlob(ValueRef::Text(text.as_bytes())),
    }
}

fn lookup_blob_id(transaction: &Transaction<'_>, hash: &ContentHash) -> Result<Option<i64>> {
    transaction
        .prepare_cached("SELECT id FROM content_blobs WHERE hash = ?1")
        .context("failed to prepare Semantic Blob lookup")?
        .query_row([hash.as_slice()], |row| row.get::<_, i64>(0))
        .optional()
        .context("failed to look up Semantic Blob")
}

fn insert_blob_if_absent(
    transaction: &Transaction<'_>,
    hash: &ContentHash,
    stored_text: &StoredBlob<'_>,
    full_bytes: i64,
    estimated_tokens: i64,
) -> Result<Option<i64>> {
    transaction
        .prepare_cached(
            "INSERT INTO content_blobs(hash, text, full_bytes, estimated_tokens)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(hash) DO NOTHING
             RETURNING id",
        )
        .context("failed to prepare Semantic Blob insert")?
        .query_row(
            params![hash.as_slice(), stored_text, full_bytes, estimated_tokens],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .context("failed to store Semantic Blob")
}
