//! Deduplicated storage for bounded text referenced by Semantic values.
//!
//! Item rows retain a Blob reference instead of repeating large text. The
//! public `blobs` relation resolves that reference; complete Runtime bytes
//! remain available only through the Item's Record evidence.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use super::db::to_sql_i64;
use crate::adapters::projection::{BoundedText, ContentHash};
use crate::indexing::telemetry::{IndexTelemetry, PersistTable};

/// Deduplicates Semantic text across one indexing run.
///
/// Runtime instructions and replayed context repeat across Sources. Keeping
/// this cache at run scope avoids asking `SQLite` about the same digest for each
/// copy. Pending entries are removed after a Source transaction rolls back so
/// the cache never returns an id whose row no longer exists.
#[derive(Debug, Default)]
pub(crate) struct BlobCache {
    ids: HashMap<ContentHash, i64>,
    pending: Vec<ContentHash>,
    /// True when the database started empty for this run, so every stored
    /// `content_blobs` row was inserted through this cache. A cache miss is
    /// therefore guaranteed absent from `SQLite` and can skip the lookup probe.
    known_complete: bool,
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
            if let Some(id) = insert_blob_if_absent(
                transaction,
                &content.hash,
                text,
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
        } else if let Some(id) = insert_blob_if_absent(
            transaction,
            &content.hash,
            text,
            full_bytes,
            estimated_tokens,
        )? {
            (id, true)
        } else {
            let id = lookup_blob_id(transaction, &content.hash)?
                .context("Semantic Blob missing after uniqueness conflict")?;
            (id, false)
        };
        telemetry.record_persist(PersistTable::Items, started.elapsed());
        self.ids.insert(key, id);
        if inserted {
            self.pending.push(key);
        }
        Ok(id)
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
    text: &str,
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
            params![hash.as_slice(), text, full_bytes, estimated_tokens],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .context("failed to store Semantic Blob")
}
