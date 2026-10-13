//! Caller-owned Session metadata, independent of the rebuildable trace index.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::value::RawValue;

const FORMAT: i64 = 1;
const TABLE: &str = "CREATE TABLE session_documents (
    runtime TEXT NOT NULL, native_id TEXT NOT NULL,
    metadata TEXT CHECK(metadata IS NULL OR json_valid(metadata)),
    version INTEGER NOT NULL CHECK(version > 0), updated_at INTEGER NOT NULL,
    PRIMARY KEY(runtime, native_id)
); PRAGMA user_version = 1;";

/// One document or its retained deletion version. JSON null is not absence.
#[derive(Serialize)]
pub(crate) struct Document {
    pub runtime: String,
    pub native_id: String,
    pub found: bool,
    pub version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Box<RawValue>>,
}

/// Receipt emitted only after the metadata transaction commits.
#[derive(Serialize)]
pub(crate) struct Receipt {
    pub runtime: String,
    pub native_id: String,
    pub version: i64,
    pub changed: bool,
    pub metadata_database: PathBuf,
}

fn validate(connection: &Connection) -> Result<()> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        version == FORMAT,
        "unsupported metadata storage format {version}; expected {FORMAT}; preserve this user-owned database"
    );
    let columns = connection
        .prepare("SELECT name FROM pragma_table_info('session_documents') ORDER BY cid")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        columns == ["runtime", "native_id", "metadata", "version", "updated_at"],
        "metadata schema is invalid; preserve this user-owned database"
    );
    connection.prepare(
        "SELECT runtime, native_id, metadata, version, updated_at FROM session_documents LIMIT 0",
    )?;
    Ok(())
}

/// Opens an existing metadata database without creating files.
pub(crate) fn open_read(path: &Path) -> Result<Option<Connection>> {
    match fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        other => {
            other
                .with_context(|| format!("cannot inspect metadata database {}", path.display()))?;
        }
    }
    let uri = super::db::read_only_database_uri(&path.canonicalize()?)?;
    let connection = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    connection.busy_timeout(Duration::from_secs(30))?;
    validate(&connection)?;
    Ok(Some(connection))
}

/// Reads one document; a missing database or key is version zero, without writes.
pub(crate) fn get(path: &Path, runtime: &str, native_id: &str) -> Result<Document> {
    validate_target(runtime, native_id)?;
    let row =
        open_read(path)?
            .map(|c| {
                c.query_row(
        "SELECT metadata, version FROM session_documents WHERE runtime=?1 AND native_id=?2",
        params![runtime, native_id], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?))
    ).optional()
            })
            .transpose()?
            .flatten();
    let (text, version) = row.unwrap_or((None, 0));
    Ok(Document {
        runtime: runtime.to_owned(),
        native_id: native_id.to_owned(),
        found: text.is_some(),
        version,
        metadata: text.map(RawValue::from_string).transpose()?,
    })
}

fn validate_target(runtime: &str, native_id: &str) -> Result<()> {
    ensure!(
        matches!(runtime, "codex" | "pi" | "claude"),
        "unknown Runtime {runtime:?}; expected codex, pi or claude"
    );
    ensure!(!native_id.is_empty(), "native_id must not be empty");
    Ok(())
}

/// Replaces or deletes one document using an atomic optional version check.
pub(crate) fn write(
    path: &Path,
    runtime: &str,
    native_id: &str,
    text: Option<&str>,
    expected: Option<i64>,
) -> Result<Receipt> {
    validate_target(runtime, native_id)?;
    if let Some(text) = &text {
        let _: Box<RawValue> =
            serde_json::from_str(text).context("metadata must be one complete JSON value")?;
    }
    ensure!(
        expected.is_none_or(|v| v >= 0),
        "if-version must be nonnegative"
    );
    // Deletion of a missing store is a no-op, including its version check.
    if text.is_none() && open_read(path)?.is_none() {
        ensure!(
            expected.is_none_or(|v| v == 0),
            "metadata version conflict: expected {}, current 0",
            expected.unwrap_or_default()
        );
        return Ok(Receipt {
            runtime: runtime.to_owned(),
            native_id: native_id.to_owned(),
            version: 0,
            changed: false,
            metadata_database: path.to_owned(),
        });
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // create_new sets permissions before SQLite sees the file and handles concurrent first writers.
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let mut c = Connection::open(path)?;
    c.busy_timeout(Duration::from_secs(30))?;
    let t = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let format: i64 = t.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if format == 0 {
        let tables: i64 = t.query_row("SELECT COUNT(*) FROM sqlite_schema", [], |r| r.get(0))?;
        ensure!(
            tables == 0,
            "refusing to initialize an unrelated database as metadata"
        );
        // An empty file can be left by an interrupted first writer; initialize it transactionally.
        t.execute_batch(TABLE)?;
    } else {
        validate(&t)?;
    }
    t.commit()?;
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
    let t = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = t
        .query_row(
            "SELECT metadata, version FROM session_documents WHERE runtime=?1 AND native_id=?2",
            params![runtime, native_id],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    let (previous, version) = current.unwrap_or((None, 0));
    if expected.is_some_and(|v| v != version) {
        bail!(
            "metadata version conflict: expected {}, current {version}; get the document again before writing",
            expected.unwrap_or_default()
        );
    }
    let changed = previous.as_deref() != text;
    let version = if changed {
        version
            .checked_add(1)
            .context("metadata version exhausted")?
    } else {
        version
    };
    if changed {
        let timestamp = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
        t.execute("INSERT INTO session_documents(runtime,native_id,metadata,version,updated_at) VALUES(?1,?2,?3,?4,?5)
            ON CONFLICT(runtime,native_id) DO UPDATE SET metadata=excluded.metadata, version=excluded.version, updated_at=excluded.updated_at",
            params![runtime,native_id,text,version,timestamp])?;
    }
    t.commit()?;
    Ok(Receipt {
        runtime: runtime.to_owned(),
        native_id: native_id.to_owned(),
        version,
        changed,
        metadata_database: path.to_owned(),
    })
}

/// Installs a read-only connection-local relation. User SQL cannot attach databases.
pub(crate) fn attach_query(c: &Connection, path: Option<&Path>) -> Result<()> {
    let query_only: bool = c.query_row("PRAGMA query_only", [], |r| r.get(0))?;
    // Read-only file open flags still protect both databases while the tool
    // installs its in-memory view. query_only also forbids TEMP schema writes.
    if query_only {
        c.execute_batch("PRAGMA query_only=OFF;")?;
    }
    let result = attach_query_inner(c, path).and_then(|()| {
        let names = c
            .prepare("SELECT name FROM pragma_table_info('session_metadata') ORDER BY cid")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let expected = super::public_schema::SESSION_METADATA
            .columns
            .iter()
            .map(|column| column.name)
            .collect::<Vec<_>>();
        ensure!(
            names == expected,
            "Session metadata query view does not match its public declaration"
        );
        Ok(())
    });
    let restored = if query_only {
        c.execute_batch("PRAGMA query_only=ON;")
            .map_err(anyhow::Error::from)
    } else {
        Ok(())
    };
    result.and(restored)
}

fn attach_query_inner(c: &Connection, path: Option<&Path>) -> Result<()> {
    c.execute_batch("DROP VIEW IF EXISTS temp.session_metadata;")?;
    if c.query_row(
        "SELECT COUNT(*) FROM pragma_database_list WHERE name='caller_metadata'",
        [],
        |r| r.get::<_, i64>(0),
    )? != 0
    {
        c.execute_batch("DETACH DATABASE caller_metadata;")?;
    }
    if let Some(path) = path
        && open_read(path)?.is_some()
    {
        let uri = super::db::read_only_database_uri(&path.canonicalize()?)?;
        c.execute("ATTACH DATABASE ?1 AS caller_metadata", [uri])?;
        c.execute_batch("CREATE TEMP VIEW session_metadata AS SELECT s.id AS session_id,m.runtime,m.native_id,m.metadata,m.version,m.updated_at
            FROM caller_metadata.session_documents m LEFT JOIN main.domain_sessions s ON s.runtime=m.runtime AND s.native_id=m.native_id WHERE m.metadata IS NOT NULL;")?;
    } else {
        c.execute_batch("CREATE TEMP VIEW session_metadata AS SELECT CAST(NULL AS INTEGER) AS session_id,CAST(NULL AS TEXT) AS runtime,
            CAST(NULL AS TEXT) AS native_id,CAST(NULL AS TEXT) AS metadata,CAST(NULL AS INTEGER) AS version,CAST(NULL AS INTEGER) AS updated_at WHERE 0;")?;
    }
    Ok(())
}
