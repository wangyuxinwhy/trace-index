use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::public_schema::{recreate_public_views, validate_public_relations};
use super::schema::{
    CREATE_SCHEMA, CREATE_SECONDARY_INDEXES, DROP_SECONDARY_INDEXES, ENSURE_REQUIRED_INDEXES,
    STORAGE_FORMAT,
};

const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

/// Page size for databases this build creates. It was selected from corpus
/// benchmarks that balanced large JSON values against repeated scans. Changing
/// it requires a new storage format and a fresh benchmark.
const DEFAULT_PAGE_SIZE: u32 = 16_384;

/// Page cache per connection, as negative KiB.
///
/// Stated in KiB rather than pages because `cache_size` counts *pages*: the
/// default 2,000 meant 8 MB while pages were 4K and would have silently become
/// 32 MB when [`DEFAULT_PAGE_SIZE`] changed. The budget is deliberately the
/// same 8 MB it has always effectively been. Raising it was measured and
/// rejected — 8 MB and 256 MB ran within noise on every read shape tried,
/// because a `query run` process starts with an empty cache and exits before
/// filling it, while the OS page cache is what actually persists between
/// invocations and is far larger. The write path no longer wants a big cache
/// either: the random-order index insert that did want one was removed when
/// Secondary indexes are deferred during a cold bulk build.
const PAGE_CACHE_KIB: u32 = 8_192;

pub struct Store {
    connection: Connection,
    path: PathBuf,
}

/// Incremental-indexing bookkeeping for one source file.
///
/// This is synchronization state, not the public Source domain object.
#[derive(Debug, Clone)]
pub(crate) struct SourceState {
    pub id: i64,
    pub adapter: String,
    pub file_size: u64,
    pub modified_ns: i64,
    pub indexed_bytes: u64,
    pub indexed_lines: u64,
    pub indexed_fingerprint: String,
}

impl Store {
    /// Opens an index database and initializes its schema when necessary.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent directory or database cannot be opened,
    /// `SQLite` configuration fails, or the on-disk storage format does not match.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create database directory {}", parent.display())
            })?;
        }

        let is_new = !path.exists();
        let connection = Connection::open(path)
            .with_context(|| format!("failed to open SQLite database {}", path.display()))?;
        connection
            .busy_timeout(Duration::from_secs(30))
            .context("failed to set SQLite busy timeout")?;
        // `page_size` must precede both the write-ahead log and the first table:
        // SQLite ignores it once a page has been written, and in WAL mode it
        // cannot change at all. It therefore only ever takes effect on a
        // database this build creates, which is the whole contract — a page size
        // is chosen once and lives with the file. Changing it later would mean
        // reindexing, the same answer `initialize_schema` already gives when the
        // storage format moves.
        connection
            .execute_batch(&format!(
                "PRAGMA page_size = {DEFAULT_PAGE_SIZE};
                 PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA cache_size = -{PAGE_CACHE_KIB};"
            ))
            .context("failed to configure SQLite")?;
        register_query_functions(&connection)?;

        let mut store = Self {
            connection,
            path: PathBuf::from(path),
        };
        store.initialize_schema()?;
        if is_new {
            set_owner_only_permissions(path)?;
        }
        Ok(store)
    }

    /// Opens an existing index without requesting write access.
    ///
    /// Reading while `index sync` writes is supported and is the reason the
    /// index runs in WAL mode: a reader sees the snapshot that existed when it
    /// started, and neither side blocks the other. An earlier revision refused
    /// to open whenever the write-ahead log was non-empty, which is precisely
    /// when a writer is active, so it discarded the one property WAL was chosen
    /// for and left a long index run unobservable through this tool.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be opened read-only, its
    /// configuration cannot be applied, or its storage format does not match.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize database {}", path.display()))?;
        let uri = read_only_database_uri(&canonical)?;
        let connection = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| explain_read_only_open_failure(&canonical))?;
        connection
            .busy_timeout(Duration::from_secs(30))
            .context("failed to set SQLite busy timeout")?;
        connection
            .execute_batch(&format!(
                "PRAGMA foreign_keys = ON;
                 PRAGMA query_only = ON;
                 PRAGMA temp_store = MEMORY;
                 PRAGMA cache_size = -{PAGE_CACHE_KIB};"
            ))
            .context("failed to configure read-only SQLite connection")?;
        register_query_functions(&connection)?;

        let store = Self {
            connection,
            path: canonical,
        };
        store.validate_schema()?;
        store.connect_public_virtual_tables()?;
        Ok(store)
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    pub(crate) fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn source_state(&self, path: &str) -> Result<Option<SourceState>> {
        source_state(&self.connection, path)
    }

    pub(crate) fn is_empty(&self) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM trace_sources)",
                [],
                |row| row.get(0),
            )
            .context("failed to inspect whether the index is empty")
    }

    pub(crate) fn drop_secondary_indexes(&mut self) -> Result<()> {
        self.connection
            .execute_batch(DROP_SECONDARY_INDEXES)
            .context("failed to defer secondary indexes")
    }

    pub(crate) fn create_secondary_indexes(&mut self) -> Result<()> {
        self.connection
            .execute_batch(CREATE_SECONDARY_INDEXES)
            .context("failed to create secondary indexes")
    }

    fn ensure_required_indexes(&mut self) -> Result<()> {
        self.connection
            .execute_batch(ENSURE_REQUIRED_INDEXES)
            .context("failed to ensure required indexes")
    }

    fn initialize_schema(&mut self) -> Result<()> {
        let version: u32 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .context("failed to read storage format")?;

        if version != 0 && version != STORAGE_FORMAT {
            bail!(
                "storage format {version} does not match current format {STORAGE_FORMAT}; \
                 create a new index database and reindex the source JSONL files"
            );
        }

        if version == STORAGE_FORMAT {
            self.validate_schema()?;
            self.ensure_required_indexes()?;
            // A cold build defers query-only indexes. If the process is killed,
            // the next write-mode open restores them before synchronization.
            // Every statement is idempotent and no correctness constraint is
            // part of the deferred set.
            self.create_secondary_indexes()?;
            self.connect_public_virtual_tables()?;
            return Ok(());
        }

        let transaction = self
            .connection
            .transaction()
            .context("failed to start schema transaction")?;

        transaction
            .execute_batch(CREATE_SCHEMA)
            .context("failed to initialize database schema")?;

        recreate_public_views(&transaction)?;

        transaction
            .execute_batch(CREATE_SECONDARY_INDEXES)
            .context("failed to initialize secondary indexes")?;

        transaction
            .pragma_update(None, "user_version", STORAGE_FORMAT)
            .context("failed to persist storage format")?;
        transaction
            .commit()
            .context("failed to commit schema transaction")?;
        Ok(())
    }

    fn validate_schema(&self) -> Result<()> {
        let version: u32 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .context("failed to read storage format")?;
        if version != STORAGE_FORMAT {
            bail!(
                "storage format {version} does not match current format {STORAGE_FORMAT}; \
                create a new index database and reindex the source JSONL files"
            );
        }

        validate_public_relations(&self.connection)?;
        Ok(())
    }

    fn connect_public_virtual_tables(&self) -> Result<()> {
        self.connection
            .execute_batch("SELECT rowid FROM item_search WHERE 0;")
            .context("failed to connect public virtual tables")
    }
}

fn register_query_functions(connection: &Connection) -> Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8
        | FunctionFlags::SQLITE_DETERMINISTIC
        | FunctionFlags::SQLITE_INNOCUOUS;
    connection
        .create_scalar_function("trigram_query", 1, flags, |context| {
            let literal = context.get::<String>(0)?;
            make_trigram_query(&literal).map_err(|message| {
                rusqlite::Error::UserFunctionError(
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, message).into(),
                )
            })
        })
        .context("failed to register trigram_query SQL function")
}

fn make_trigram_query(literal: &str) -> std::result::Result<String, &'static str> {
    let characters = literal.chars().collect::<Vec<_>>();
    if characters.len() < 3 {
        return Err("trigram_query requires at least 3 Unicode characters; \
             narrow by source, project, actor, or time and use instr() instead");
    }

    Ok(characters
        .windows(3)
        .map(|window| {
            let token = window.iter().collect::<String>().replace('"', "\"\"");
            format!("\"{token}\"")
        })
        .collect::<Vec<_>>()
        .join(" AND "))
}

pub(crate) fn source_state(connection: &Connection, path: &str) -> Result<Option<SourceState>> {
    connection
        .query_row(
            "SELECT id, adapter, file_size, modified_ns, indexed_bytes, indexed_lines,
                    indexed_fingerprint
               FROM trace_sources
              WHERE path = ?1",
            [path],
            |row| {
                Ok(SourceState {
                    id: row.get(0)?,
                    adapter: row.get(1)?,
                    file_size: from_sql_u64(row.get(2)?)?,
                    modified_ns: row.get(3)?,
                    indexed_bytes: from_sql_u64(row.get(4)?)?,
                    indexed_lines: from_sql_u64(row.get(5)?)?,
                    indexed_fingerprint: row.get(6)?,
                })
            },
        )
        .optional()
        .context("failed to read source state")
}

pub(crate) fn to_sql_i64(value: u64, field: &str) -> Result<i64> {
    i64::try_from(value).with_context(|| format!("{field} exceeds SQLite INTEGER range"))
}

fn from_sql_u64(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

#[cfg(unix)]
pub(crate) fn set_owner_only_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set owner-only permissions on {}", path.display()))
}

#[cfg(not(unix))]
pub(crate) fn set_owner_only_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[must_use]
pub fn display_database_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
        .to_string_lossy()
        .into_owned()
}

/// Explains a read-only open failure, naming the one WAL state that causes it.
///
/// A read-only connection cannot recover a write-ahead log by itself, and it
/// locates pages through the `-shm` index it is not allowed to create. Both
/// exist while a writer is attached, so an active `index sync` reads fine; the
/// failing case is a non-empty WAL left behind by a writer that died.
fn explain_read_only_open_failure(path: &Path) -> String {
    let mut message = format!(
        "failed to open SQLite database read-only {}",
        path.display()
    );
    let mut wal_path = path.as_os_str().to_owned();
    wal_path.push("-wal");
    let wal_bytes = fs::metadata(PathBuf::from(wal_path))
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    if wal_bytes != 0 {
        let mut shm_path = path.as_os_str().to_owned();
        shm_path.push("-shm");
        if !PathBuf::from(shm_path).exists() {
            let _ = write!(
                message,
                "\nHint: the write-ahead log holds {wal_bytes} bytes but its -shm index is gone, \
                 so a writer exited without checkpointing. Open the database once for writing, \
                 for example with `trace-index index sync`, to recover it"
            );
        }
    }
    message
}

pub(crate) fn read_only_database_uri(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .with_context(|| format!("database path is not valid UTF-8: {}", path.display()))?;
    let mut uri = String::with_capacity(path.len() + 32);
    uri.push_str("file:");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'.' | b'_' | b'~') {
            uri.push(char::from(byte));
        } else {
            uri.push('%');
            uri.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
            uri.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
        }
    }
    uri.push_str("?mode=ro");
    Ok(uri)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{Store, make_trigram_query};

    #[test]
    fn builds_escaped_overlapping_trigram_queries() {
        assert_eq!(
            make_trigram_query("智能体使用").expect("trigram query"),
            "\"智能体\" AND \"能体使\" AND \"体使用\""
        );
        assert_eq!(
            make_trigram_query("a\"bc").expect("escaped trigram query"),
            "\"a\"\"b\" AND \"\"\"bc\""
        );
        assert!(make_trigram_query("中文").is_err());
    }

    #[test]
    fn opens_an_existing_index_read_only() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        drop(Store::open(&database).expect("create index"));

        let store = Store::open_read_only(&database).expect("open index read-only");
        let version: u32 = store
            .connection()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read storage format");
        assert_eq!(version, super::STORAGE_FORMAT);
    }

    #[test]
    fn reopens_an_existing_index_for_incremental_writes() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        drop(Store::open(&database).expect("create index"));

        let reopened = Store::open(&database).expect("reopen index for writes");
        reopened
            .connection()
            .execute(
                "INSERT INTO trace_sources(adapter, path) VALUES ('codex', '/tmp/trace.jsonl')",
                [],
            )
            .expect("write through reopened index");
    }

    #[test]
    fn write_open_repairs_indexes_missing_after_an_interrupted_cold_build() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        drop(Store::open(&database).expect("create index"));

        let connection = Connection::open(&database).expect("open database directly");
        connection
            .execute_batch(
                "DROP INDEX content_blobs_hash;
                 DROP INDEX items_role;",
            )
            .expect("simulate interrupted index build");
        drop(connection);

        let repaired = Store::open(&database).expect("repair indexes on write open");
        for name in ["content_blobs_hash", "items_role"] {
            let count: i64 = repaired
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'index' AND name = ?1",
                    [name],
                    |row| row.get(0),
                )
                .expect("inspect repaired index");
            assert_eq!(count, 1, "index {name} was not repaired");
        }
    }

    #[test]
    fn read_only_open_uses_sqlite_snapshot_locking_during_a_concurrent_write() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        drop(Store::open(&database).expect("create index"));
        let reader = Store::open_read_only(&database).expect("open index read-only");

        reader
            .connection()
            .execute_batch("BEGIN")
            .expect("begin read transaction");
        let initial_sources: i64 = reader
            .connection()
            .query_row("SELECT COUNT(*) FROM trace_sources", [], |row| row.get(0))
            .expect("establish read snapshot");
        assert_eq!(initial_sources, 0);

        let writer = Connection::open(&database).expect("open concurrent writer");
        writer
            .execute(
                "INSERT INTO trace_sources(adapter, path) VALUES ('codex', '/tmp/trace.jsonl')",
                [],
            )
            .expect("commit concurrent write");

        let snapshot_sources: i64 = reader
            .connection()
            .query_row("SELECT COUNT(*) FROM trace_sources", [], |row| row.get(0))
            .expect("read original snapshot");
        assert_eq!(snapshot_sources, 0);
        reader
            .connection()
            .execute_batch("COMMIT")
            .expect("finish read transaction");

        let refreshed_sources: i64 = reader
            .connection()
            .query_row("SELECT COUNT(*) FROM trace_sources", [], |row| row.get(0))
            .expect("read refreshed snapshot");
        assert_eq!(refreshed_sources, 1);
    }

    #[test]
    fn opens_read_only_while_a_writer_holds_an_uncheckpointed_wal() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        // The writer stays open for the whole test, which is what an `index
        // sync` looks like from a reader's side.
        let writer = Store::open(&database).expect("create index");
        writer
            .connection()
            .execute_batch("PRAGMA wal_autocheckpoint = 0")
            .expect("hold the write-ahead log");
        for ordinal in 0..64 {
            writer
                .connection()
                .execute(
                    "INSERT INTO trace_sources(adapter, path) VALUES ('codex', ?1)",
                    [format!("/tmp/trace-{ordinal}.jsonl")],
                )
                .expect("write while holding the log");
        }

        let mut wal_path = database.clone().into_os_string();
        wal_path.push("-wal");
        let wal_bytes = fs::metadata(PathBuf::from(wal_path))
            .expect("write-ahead log exists")
            .len();
        assert!(wal_bytes > 0, "test needs an uncheckpointed log");

        // An earlier revision refused here purely because the log was
        // non-empty, making a long index run unobservable through this tool.
        let reader = Store::open_read_only(&database).expect("read during an active write");
        let sources: i64 = reader
            .connection()
            .query_row("SELECT COUNT(*) FROM trace_sources", [], |row| row.get(0))
            .expect("count sources mid-write");
        assert_eq!(sources, 64);
    }

    #[test]
    fn has_no_indexes_redundant_with_unique_constraints() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("create index");
        let duplicates: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*)
                   FROM sqlite_schema
                  WHERE type = 'index'
                    AND name IN (
                        'trace_records_source',
                        'session_sources_source',
                        'loops_session',
                        'loops_source'
                    )",
                [],
                |row| row.get(0),
            )
            .expect("inspect indexes");
        assert_eq!(duplicates, 0);
    }

    #[test]
    fn stores_item_text_only_in_primary_contents() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("index.sqlite");
        let store = Store::open(&database).expect("create index");
        let duplicate_columns: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*)
                   FROM pragma_table_info('trace_events')
                  WHERE name IN ('searchable_text', 'text_truncated')",
                [],
                |row| row.get(0),
            )
            .expect("inspect event columns");
        assert_eq!(duplicate_columns, 0);
    }

    #[test]
    fn read_only_open_supports_uri_reserved_path_characters() {
        let directory = tempdir().expect("temp directory");
        let nested = directory.path().join("index space # percent %");
        fs::create_dir(&nested).expect("create nested directory");
        let database = nested.join("index.sqlite");
        drop(Store::open(&database).expect("create index"));

        Store::open_read_only(&database).expect("open escaped path read-only");
    }

    #[test]
    fn read_only_open_does_not_create_a_missing_database() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("missing.sqlite");

        assert!(Store::open_read_only(&database).is_err());
        assert!(!database.exists());
    }

    #[test]
    fn rejects_a_different_storage_format_without_mutating_it() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("other.sqlite");
        let connection = Connection::open(&database).expect("open database");
        // Relative to the current format rather than a literal, which this test
        // used to be: it hard-coded a number that happened to differ, and said
        // nothing when the format later became that number.
        let other_format = super::STORAGE_FORMAT + 1;
        connection
            .execute_batch(&format!(
                "CREATE TABLE sentinel(value TEXT);
                 PRAGMA user_version = {other_format};"
            ))
            .expect("create different format");
        drop(connection);

        let error = Store::open(&database)
            .err()
            .expect("reject different storage format");
        assert!(error.to_string().contains("reindex the source JSONL files"));

        let connection = Connection::open(&database).expect("reopen database");
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read unchanged format");
        assert_eq!(version, other_format);
        let new_tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                  WHERE type = 'table' AND name LIKE 'trace_%'",
                [],
                |row| row.get(0),
            )
            .expect("inspect unchanged database");
        assert_eq!(new_tables, 0);
    }
}
