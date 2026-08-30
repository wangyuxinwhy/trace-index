//! Read-only summary of the currently published index.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;

use crate::domain::SEMANTIC_ROLES;
use crate::interface::output::{ObservedRole, StatusReport};
use crate::storage::db::{Store, display_database_path};
use crate::storage::schema::STORAGE_FORMAT;

/// Summarizes the current index without scanning trace files.
///
/// # Errors
///
/// Returns an error if stored counters are invalid or `SQLite` queries fail.
pub fn status(store: &Store, database_path: &Path) -> Result<StatusReport> {
    let (source_count, source_bytes, indexed_bytes, incomplete_sources): (i64, i64, i64, i64) =
        store.connection().query_row(
            "SELECT COUNT(*), COALESCE(SUM(file_size), 0),
                    COALESCE(SUM(indexed_bytes), 0),
                    COALESCE(SUM(status != 'complete'), 0)
               FROM trace_sources",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;

    Ok(StatusReport {
        database: display_database_path(database_path),
        storage_format: STORAGE_FORMAT,
        indexing_policy: store.indexing_policy()?,
        sources: usize::try_from(source_count)?,
        source_bytes: u64::try_from(source_bytes)?,
        indexed_bytes: u64::try_from(indexed_bytes)?,
        incomplete_sources: usize::try_from(incomplete_sources)?,
        observed_roles: observed_roles(store)?,
    })
}

/// Reports the semantic roles this index holds, against the compiled vocabulary.
///
/// The declaration states what the adapters can produce; only the corpus shows
/// what they did. A declared role that never appears is either an unreachable
/// rule or a shape this corpus lacks, and the two are worth telling apart —
/// which is exactly how a Pi branch that could never fire stayed invisible.
fn observed_roles(store: &Store) -> Result<Vec<ObservedRole>> {
    let mut statement = store.connection().prepare(
        "SELECT json_extract(i.semantic, '$.role'), s.adapter, COUNT(*)
           FROM domain_items i
           JOIN trace_sources s ON s.id = i.source_id
          GROUP BY 1, 2
          ORDER BY 1, 2",
    )?;
    let mut by_role: BTreeMap<String, (BTreeSet<String>, u64)> = BTreeMap::new();
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let role: String = row.get(0)?;
        let adapter: String = row.get(1)?;
        let count: i64 = row.get(2)?;
        let entry = by_role.entry(role).or_default();
        entry.0.insert(adapter);
        entry.1 += u64::try_from(count)?;
    }

    let observed = by_role
        .into_iter()
        .map(|(semantic_role, (adapters, items))| {
            let note = (!SEMANTIC_ROLES
                .iter()
                .any(|role| role.as_str() == semantic_role))
            .then(|| "not in the compiled vocabulary".to_owned());
            ObservedRole {
                semantic_role,
                adapters: adapters.into_iter().collect(),
                items,
                note,
            }
        })
        .collect::<Vec<_>>();

    Ok(observed)
}
