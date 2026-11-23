//! Read-only summary of the currently published index.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Result, bail};
use rusqlite::types::ValueRef;

use super::watch::inspect_watch_status;
use crate::domain::SEMANTIC_ROLES;
use crate::interface::output::{ObservedRole, StatusReport};
use crate::storage::db::{Store, display_database_path};

/// Upper bound on `source_id` for the flat L1-cache lookup table in
/// [`observed_roles`]; sparse databases with larger ids fall back to a map.
const MAX_DENSE_SOURCE_ID: usize = 1 << 20;

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
        watch: inspect_watch_status(database_path),
        observed_roles: observed_roles(&snapshot)?,
    };
    snapshot.finish()?;
    Ok(report)
}

/// Compact source-id to adapter-index lookup table for the tight `domain_items`
/// covering-index scan in [`observed_roles`].
struct SourceAdapterLookup {
    adapter_names: Vec<String>,
    dense: Vec<Option<u16>>,
    sparse: HashMap<i64, u16>,
}

impl SourceAdapterLookup {
    fn load(connection: &rusqlite::Connection) -> Result<Self> {
        let mut statement = connection.prepare("SELECT id, adapter FROM trace_sources")?;
        let mut rows = statement.query([])?;
        let mut entries: Vec<(i64, String)> = Vec::new();
        let mut unique_adapters: BTreeSet<String> = BTreeSet::new();
        let mut max_dense_id = 0_usize;
        while let Some(row) = rows.next()? {
            let source_id: i64 = row.get(0)?;
            let adapter: String = row.get(1)?;
            if let Ok(index) = usize::try_from(source_id)
                && index <= MAX_DENSE_SOURCE_ID
            {
                max_dense_id = max_dense_id.max(index);
            }
            unique_adapters.insert(adapter.clone());
            entries.push((source_id, adapter));
        }

        let adapter_names: Vec<String> = unique_adapters.into_iter().collect();
        let adapter_indices: HashMap<&str, u16> = adapter_names
            .iter()
            .enumerate()
            .map(|(idx, name)| {
                let idx = u16::try_from(idx).expect("adapter count fits in u16");
                (name.as_str(), idx)
            })
            .collect();

        let mut dense = if entries.is_empty() {
            Vec::new()
        } else {
            vec![None; max_dense_id + 1]
        };
        let mut sparse = HashMap::new();
        for (source_id, adapter) in &entries {
            let adapter_idx = adapter_indices[adapter.as_str()];
            if let Ok(index) = usize::try_from(*source_id)
                && index < dense.len()
            {
                dense[index] = Some(adapter_idx);
            } else {
                sparse.insert(*source_id, adapter_idx);
            }
        }
        Ok(Self {
            adapter_names,
            dense,
            sparse,
        })
    }

    fn adapter_index(&self, source_id: i64) -> Option<u16> {
        if let Ok(index) = usize::try_from(source_id)
            && let Some(entry) = self.dense.get(index)
        {
            return *entry;
        }
        self.sparse.get(&source_id).copied()
    }
}

#[derive(Default)]
struct RoleTally {
    adapter_bits: u64,
    extra_adapters: BTreeSet<u16>,
    items: u64,
}

impl RoleTally {
    fn record(&mut self, adapter_idx: u16) {
        if let Some(bit) = 1_u64.checked_shl(u32::from(adapter_idx)) {
            self.adapter_bits |= bit;
        } else {
            self.extra_adapters.insert(adapter_idx);
        }
        self.items += 1;
    }

    fn merge_from(&mut self, other: Self) {
        self.adapter_bits |= other.adapter_bits;
        self.extra_adapters.extend(other.extra_adapters);
        self.items += other.items;
    }

    fn adapter_names(&self, names: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        let mut bits = self.adapter_bits;
        while bits != 0 {
            let idx = bits.trailing_zeros() as usize;
            if let Some(name) = names.get(idx) {
                out.push(name.clone());
            }
            bits &= bits - 1;
        }
        for &idx in &self.extra_adapters {
            if let Some(name) = names.get(usize::from(idx)) {
                out.push(name.clone());
            }
        }
        out
    }
}

/// Reports the semantic roles this index holds, against the compiled vocabulary.
///
/// The declaration states what the adapters can produce; only the corpus shows
/// what they did. A declared role that never appears is either an unreachable
/// rule or a shape this corpus lacks, and the two are worth telling apart —
/// which is exactly how a Pi branch that could never fire stayed invisible.
fn observed_roles(connection: &rusqlite::Connection) -> Result<Vec<ObservedRole>> {
    // Load the small source-to-adapter table once inside the shared read
    // snapshot instead of joining and sorting every Item row inside SQLite.
    // Walking `items_role_occurred` yields contiguous runs per semantic role
    // with no table lookups, no JSON parsing, and no temp B-tree; caching the
    // active role avoids millions of BTreeMap/BTreeSet lookups on large corpora.
    let lookup = SourceAdapterLookup::load(connection)?;
    let mut statement = connection
        .prepare("SELECT json_extract(semantic, '$.role'), source_id FROM domain_items")?;
    let mut by_role: BTreeMap<String, RoleTally> = BTreeMap::new();
    let mut current_role = String::new();
    let mut current_tally = RoleTally::default();
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let role_bytes = match row.get_ref(0)? {
            ValueRef::Text(bytes) => bytes,
            other => bail!(
                "stored Item semantic role must be text, found {:?}",
                other.data_type()
            ),
        };
        let source_id: i64 = row.get(1)?;
        let Some(adapter_idx) = lookup.adapter_index(source_id) else {
            bail!("stored Item references missing Source id {source_id}");
        };
        if role_bytes == current_role.as_bytes() && current_tally.items > 0 {
            current_tally.record(adapter_idx);
            continue;
        }
        if current_tally.items > 0 {
            by_role
                .entry(std::mem::take(&mut current_role))
                .or_default()
                .merge_from(std::mem::take(&mut current_tally));
        }
        let role = std::str::from_utf8(role_bytes)?;
        current_role.push_str(role);
        current_tally.record(adapter_idx);
    }
    if current_tally.items > 0 {
        by_role
            .entry(current_role)
            .or_default()
            .merge_from(current_tally);
    }

    let observed = by_role
        .into_iter()
        .map(|(semantic_role, tally)| {
            let note = (!SEMANTIC_ROLES
                .iter()
                .any(|role| role.as_str() == semantic_role))
            .then(|| "not in the compiled vocabulary".to_owned());
            ObservedRole {
                semantic_role,
                adapters: tally.adapter_names(&lookup.adapter_names),
                items: tally.items,
                note,
            }
        })
        .collect::<Vec<_>>();

    Ok(observed)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::status;
    use crate::storage::db::Store;

    #[test]
    fn status_aggregates_observed_roles_across_dense_and_sparse_sources() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("status.sqlite");
        let store = Store::open(&db_path).expect("open store");
        let conn = store.connection();
        conn.execute_batch(
            "INSERT INTO trace_sources(id, adapter, path, file_size, indexed_bytes, status)
               VALUES (1, 'codex', '/tmp/a.jsonl', 100, 100, 'complete'),
                      (2000000, 'pi', '/tmp/b.jsonl', 200, 150, 'partial');
             INSERT INTO domain_sessions(id, runtime, native_id)
               VALUES (1, 'codex', 's1'), (2, 'pi', 's2');
             INSERT INTO domain_items(source_id, session_id, occurred_at, semantic)
               VALUES (1, 1, 10, '{\"role\":\"human.request\",\"value\":{\"has_images\":false},\"evidence_strength\":\"structural\"}'),
                      (1, 1, 20, '{\"role\":\"human.request\",\"value\":{\"has_images\":false},\"evidence_strength\":\"structural\"}'),
                      (2000000, 2, 30, '{\"role\":\"human.request\",\"value\":{\"has_images\":false},\"evidence_strength\":\"structural\"}'),
                      (2000000, 2, 40, '{\"role\":\"custom.experimental\",\"value\":{},\"evidence_strength\":\"heuristic\"}');",
        )
        .expect("seed status rows");

        let report = status(&store, &db_path).expect("compute status");
        assert_eq!(report.sources, 2);
        assert_eq!(report.incomplete_sources, 1);
        assert_eq!(report.source_bytes, 300);
        assert_eq!(report.indexed_bytes, 250);
        assert_eq!(report.observed_roles.len(), 2);

        assert_eq!(
            report.observed_roles[0].semantic_role,
            "custom.experimental"
        );
        assert_eq!(report.observed_roles[0].adapters, vec!["pi"]);
        assert_eq!(report.observed_roles[0].items, 1);
        assert_eq!(
            report.observed_roles[0].note.as_deref(),
            Some("not in the compiled vocabulary")
        );

        assert_eq!(report.observed_roles[1].semantic_role, "human.request");
        assert_eq!(report.observed_roles[1].adapters, vec!["codex", "pi"]);
        assert_eq!(report.observed_roles[1].items, 3);
        assert!(report.observed_roles[1].note.is_none());
    }
}
