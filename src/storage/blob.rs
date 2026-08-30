//! Deduplicated storage for bounded text referenced by Semantic values.
//!
//! Item rows retain a Blob reference instead of repeating large text. The
//! public `blobs` relation resolves that reference; complete Runtime bytes
//! remain available only through the Item's Record evidence.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result};
use rusqlite::{Transaction, params};

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
}

impl BlobCache {
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
        let id = transaction
            .prepare_cached(
                "INSERT INTO content_blobs(hash, text, full_bytes, estimated_tokens)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(hash) DO UPDATE SET hash = hash
                 RETURNING id",
            )
            .context("failed to prepare Semantic Blob upsert")?
            .query_row(
                params![content.hash.as_slice(), text, full_bytes, estimated_tokens,],
                |row| row.get::<_, i64>(0),
            )
            .context("failed to store Semantic Blob")?;
        telemetry.record_persist(PersistTable::Items, started.elapsed());
        self.ids.insert(key, id);
        self.pending.push(key);
        Ok(id)
    }
}
