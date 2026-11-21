//! Read-only summary of the currently published index.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Result, bail};
use rusqlite::types::ValueRef;

use crate::domain::SEMANTIC_ROLES;
use crate::interface::output::{ObservedRole, StatusReport};
use crate::storage::db::{Store, display_database_path};

/// Summarizes the current index without scanning trace files.
///
/// # Errors
///
/// Returns an error if stored counters are invalid or `SQLite` queries fail.
pub fn status(store: &Store, database_path: &Path) -> Result<StatusReport> {
    // Hold one deferred read transaction across every status query so source
    // totals, adapter mappings, and Item role counts observe a single WAL
    // snapshot even while a concurrent `index sync` commits new Sources.
    let snapshot = store.connection().unchecked_transaction()?;
    let (source_count, source_bytes, indexed_bytes, incomplete_sources): (i64, i64, i64, i64) =
        snapshot.query_row(
            "SELECT COUNT(*), COALESCE(SUM(file_size), 0),
                    COALESCE(SUM(indexed_bytes), 0),
                    COALESCE(SUM(status != 'complete'), 0)
               FROM trace_sources",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;

    let report = StatusReport {
        database: display_database_path(database_path),
        storage_format: store.storage_format()?,
        indexing_policy: store.indexing_policy()?,
        sources: usize::try_from(source_count)?,
        source_bytes: u64::try_from(source_bytes)?,
        indexed_bytes: u64::try_from(indexed_bytes)?,
        incomplete_sources: usize::try_from(incomplete_sources)?,
        observed_roles: observed_roles(&snapshot)?,
    };
    snapshot.finish()?;
    Ok(report)
}

/// Reports the semantic roles this index holds, against the compiled vocabulary.
///
/// The declaration states what the adapters can produce; only the corpus shows
/// what they did. A declared role that never appears is either an unreachable
/// rule or a shape this corpus lacks, and the two are worth telling apart —
/// which is exactly how a Pi branch that could never fire stayed invisible.
fn observed_roles(connection: &rusqlite::Connection) -> Result<Vec<ObservedRole>> {
    // Load the small source-to-adapter map once inside the shared read snapshot
    // instead of joining and sorting every Item row inside SQLite. Scanning
    // `(role, source_id)` directly lets SQLite walk `items_role_occurred` as a
    // covering index with no table lookups, no JSON parsing, and no temp B-tree.
    let mut source_stmt = connection.prepare("SELECT id, adapter FROM trace_sources")?;
    let mut adapters_by_source: HashMap<i64, String> = HashMap::new();
    let mut source_rows = source_stmt.query([])?;
    while let Some(row) = source_rows.next()? {
        adapters_by_source.insert(row.get(0)?, row.get(1)?);
    }

    let mut statement = connection
        .prepare("SELECT json_extract(semantic, '$.role'), source_id FROM domain_items")?;
    let mut by_role: BTreeMap<String, (BTreeSet<String>, u64)> = BTreeMap::new();
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let role = match row.get_ref(0)? {
            ValueRef::Text(bytes) => std::str::from_utf8(bytes)?,
            other => bail!(
                "stored Item semantic role must be text, found {:?}",
                other.data_type()
            ),
        };
        let source_id: i64 = row.get(1)?;
        let Some(adapter) = adapters_by_source.get(&source_id) else {
            bail!("stored Item references missing Source id {source_id}");
        };
        if let Some((adapters, count)) = by_role.get_mut(role) {
            if !adapters.contains(adapter.as_str()) {
                adapters.insert(adapter.clone());
            }
            *count += 1;
        } else {
            let mut adapters = BTreeSet::new();
            adapters.insert(adapter.clone());
            by_role.insert(role.to_owned(), (adapters, 1));
        }
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
