"""Native Trace Index: bounded SQL, explicit synchronization, source-backed evidence.

Start here (for Agents):
    import trace_index as ti
    print(ti.doc("start-here"))
    index = ti.Index.open()  # an existing index selected by configuration
    print(index.schema())
    print(index.schema("items"))

Help, bundled docs, and Schema discovery match this installed package. Discover
exact names and shapes before writing SQL; do not guess relations or JSON shapes.
Help and bundled docs work offline without an index or CLI executable. Opening
an existing index or reading its Schema requires the selected database to exist.
"""

from __future__ import annotations

import math
import os
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from types import MappingProxyType
from typing import Literal, TypeAlias

from . import _native
from ._native import (
    ClosedIndexError,
    ConfigurationError,
    IndexUnavailableError,
    ParameterError,
    QueryError,
    QueryRejectedError,
    QueryTimeoutError,
    RecordError,
    SyncError,
    TraceIndexError,
    UnknownRelationError,
    __version__,
)

# Compose the standard module docstring; no separate Help API or database access.
__doc__ += (
    "\n\n"
    + _native.__doc__
    + """

Python operations:
    index.query(sql, parameters=...) returns a bounded QueryResult, not JSON stdout.
    Inspect complete, cells_truncated, and hint, or call require_complete().
    Errors are Python exceptions; a bounded partial query is normally a result.
    index.sync(paths) explicitly writes; SyncFailedError retains its partial report.
    Index(db=...) selects without creating a database; sync() can create it.
    Use a context manager or close() to release a connection. Native I/O releases
    the GIL; one Index serializes operations and queries have independent snapshots.

Discovery:
    help(ti.Index)                    selection and connection lifetime
    help(ti.Index.query)              parameters, budgets, results, and exceptions
    help(ti.QueryBudget)              default budgets and allowed ranges
    help(ti.Index.sync)               explicit writes and partial failures
    help(ti.Record.inspect)           display-safe physical evidence
    ti.docs()                        bundled topic names and summaries
    ti.doc("how-to/query-evidence")   query workflows and shared SQL examples
    ti.doc("how-to/search-literals")  literal candidates and confirmation

Use help(object), object.__doc__, and Python signatures for local API details.
SQL blocks in bundled docs work with index.query(sql); CLI-specific process,
configuration-creation, export, and media-extraction instructions remain CLI-only.
"""
)

PathInput: TypeAlias = str | os.PathLike[str]
Parameter: TypeAlias = None | bool | int | float | str | bytes
Cell: TypeAlias = None | int | float | str | bytes
Row: TypeAlias = Mapping[str, Cell]
JSONValue: TypeAlias = (
    None | bool | int | float | str | list["JSONValue"] | dict[str, "JSONValue"]
)


def _integer(name: str, value: int, minimum: int, maximum: int) -> None:
    if type(value) is not int or not minimum <= value <= maximum:
        raise ValueError(f"{name} must be an integer in [{minimum}, {maximum}]")


@dataclass(frozen=True)
class QueryBudget:
    """Bounds match CLI rows/bytes. Output bytes count CLI-encoded rows, not Python RAM.

    Defaults are shown in the constructor signature. Allowed ranges: limit
    1..10000; max_cell_bytes 1..1048576; max_output_bytes 1024..16777216;
    timeout_seconds greater than zero and at most 300 (fractional allowed).
    Invalid construction raises ValueError. Lock waits and conversion are not
    included in the SQL execution timeout. No budget triggers automatic retry.
    """

    limit: int = 1000
    max_cell_bytes: int = 65536
    max_output_bytes: int = 65536
    timeout_seconds: float = 30.0

    def __post_init__(self) -> None:
        _integer("limit", self.limit, 1, 10000)
        _integer("max_cell_bytes", self.max_cell_bytes, 1, 1048576)
        _integer("max_output_bytes", self.max_output_bytes, 1024, 16777216)
        if (
            isinstance(self.timeout_seconds, bool)
            or not math.isfinite(self.timeout_seconds)
            or not 0 < self.timeout_seconds <= 300
        ):
            raise ValueError("timeout_seconds must be finite and in (0, 300]")


@dataclass(frozen=True)
class QueryResult:
    """Detached rows. complete covers the SQL row set, not source/index coverage.

    rows are read-only mappings; columns preserve SQL projection order.
    incomplete_reason is row_limit or output_budget when complete is false.
    cells_truncated counts prefix cells independently of complete; hint suggests
    a bounded next query. repr omits row bodies. No field reads more source data.
    require_complete() raises IncompleteResultError with this result if rows or
    cells were omitted by budgets, not for SQL's own LIMIT/substr or missing facts.

    TEXT stays str, including JSON-encoded public columns; BLOB becomes bytes.
    A prefix BLOB does not retain its original length in the cell: inspect
    cells_truncated or project length(...) explicitly when that length matters.
    """

    columns: tuple[str, ...]
    rows: tuple[Row, ...] = field(repr=False)
    complete: bool
    incomplete_reason: Literal["row_limit", "output_budget"] | None
    cells_truncated: int
    hint: str | None

    @property
    def returned(self) -> int:
        return len(self.rows)

    def require_complete(self) -> QueryResult:
        """Reject partial rows/cells; does not check Source or Runtime truncation."""
        if not self.complete or self.cells_truncated:
            raise IncompleteResultError(self)
        return self


class IncompleteResultError(TraceIndexError):
    """Explicit completeness check failed; result retains the bounded observation."""

    def __init__(self, result: QueryResult) -> None:
        super().__init__(result.hint or "query result is incomplete")
        self.result = result


@dataclass(frozen=True)
class Column:
    name: str
    data_type: str
    nullable: bool
    description: str | None = None


@dataclass(frozen=True)
class Relation:
    name: str
    columns: tuple[Column, ...]
    description: str | None = None


@dataclass(frozen=True)
class Schema:
    relations: tuple[Relation, ...]


@dataclass(frozen=True)
class DocTopic:
    name: str
    category: str
    title: str
    summary: str


def docs() -> tuple[DocTopic, ...]:
    """List this wheel's bundled docs without a database, credentials, or CLI.

    Example:
        >>> any(topic.name == "start-here" for topic in docs())
        True
    """
    return tuple(DocTopic(**topic) for topic in _native.docs()["topics"])


def doc(topic: str) -> str:
    """Read bundled Markdown without a database, credentials, or CLI.

    ValueError means an unknown topic; use docs() to discover the valid names.
    SQL and domain explanations are shared with the CLI, not translated copies.

    Example:
        >>> doc("start-here").lstrip().startswith("# Start Here")
        True
    """
    return _native.doc(topic)


@dataclass(frozen=True)
class IndexingPolicy:
    max_indexed_record_bytes: int
    max_published_text_bytes: int


@dataclass(frozen=True)
class SourceResult:
    path: str
    adapter: str
    action: str
    indexed_records: int
    indexed_items: int
    indexed_bytes: int
    file_size: int
    malformed_records: int
    oversized_records: int
    incomplete_tail: bool
    error: str | None = None


@dataclass(frozen=True)
class SyncReport:
    """All source results, including failures; not an all-or-nothing transaction.

    complete means no recognized source failed, not that every source record is
    well-formed, fully materialized, or complete. Inspect malformed_records,
    oversized_records, incomplete_files, skipped_files and source_results too.
    """

    adapters: tuple[str, ...]
    database: str
    indexing_policy: IndexingPolicy
    discovered_files: int
    indexed_files: int
    unchanged_files: int
    rebuilt_files: int
    indexed_records: int
    indexed_items: int
    malformed_records: int
    oversized_records: int
    incomplete_files: int
    skipped_files: int
    failed_files: int
    metrics: Mapping[str, JSONValue] = field(repr=False)
    source_results: tuple[SourceResult, ...] = field(repr=False)

    @property
    def complete(self) -> bool:
        return self.failed_files == 0


class SyncFailedError(SyncError):
    """Some recognized sources failed. Successful source writes remain committed."""

    def __init__(self, report: SyncReport) -> None:
        super().__init__(
            f"{report.failed_files} of {report.discovered_files} discovered files failed; inspect report.source_results"
        )
        self.report = report


@dataclass(frozen=True)
class ObservedRole:
    semantic_role: str
    adapters: tuple[str, ...]
    items: int
    note: str | None = None


@dataclass(frozen=True)
class StatusReport:
    database: str
    storage_format: int
    sources: int
    source_bytes: int
    indexed_bytes: int
    incomplete_sources: int
    indexing_policy: IndexingPolicy | None = None
    observed_roles: tuple[ObservedRole, ...] = ()


@dataclass(frozen=True)
class InspectionBudget:
    """Bound source materialization and display-safe representation.

    Defaults are shown in the constructor signature. max_record_bytes must be a
    positive signed 64-bit integer; max_value_bytes is 1..1048576 and
    max_output_bytes is 1024..16777216. Invalid construction raises ValueError.
    A structural reference may replace an over-budget value or whole Record;
    inspect raw_verified and externalized_values rather than assuming raw JSON.
    """

    max_record_bytes: int = 16777216
    max_value_bytes: int = 16384
    max_output_bytes: int = 65536

    def __post_init__(self) -> None:
        _integer("max_record_bytes", self.max_record_bytes, 1, 2**63 - 1)
        _integer("max_value_bytes", self.max_value_bytes, 1, 1048576)
        _integer("max_output_bytes", self.max_output_bytes, 1024, 16777216)


@dataclass(frozen=True)
class RecordSource:
    adapter: str
    uri: str


@dataclass(frozen=True)
class RecordParse:
    status: str
    error: str | None = None


@dataclass(frozen=True)
class RecordMetadata:
    id: int
    source: RecordSource
    seq: int
    byte_offset: int
    byte_length: int
    raw_hash: str
    parse: RecordParse
    oversized: bool


@dataclass(frozen=True)
class RecordVerification:
    record_id: int
    status: Literal["verified", "source_missing", "source_short", "hash_mismatch"]
    expected_hash: str
    expected_bytes: int
    actual_bytes: int
    actual_hash: str | None = None


@dataclass(frozen=True)
class RecordInspection:
    """Display-safe representation, not raw bytes; failed verification yields a reference."""

    record: RecordMetadata
    raw_verified: bool
    representation: JSONValue = field(repr=False)
    externalized_values: int
    representation_bytes: int


class Record:
    """An index-local Record handle. Construction performs no source I/O.

    Obtain one with index.record(record_id). Reads verify original source bytes
    on demand and fail with ClosedIndexError after the owning Index is closed.
    Invalid IDs raise ValueError; unknown Records or access errors raise
    RecordError. Integrity findings from verify() remain data, not exceptions.
    """

    def __init__(self, index: Index, record_id: int) -> None:
        _integer("record_id", record_id, 1, 2**63 - 1)
        self._index = index
        self._record_id = record_id

    @property
    def record_id(self) -> int:
        return self._record_id

    def verify(self) -> RecordVerification:
        """Check original source bytes without returning the Record body.

        Returns RecordVerification with status verified, source_missing,
        source_short, or hash_mismatch. See help(Record) for access errors.

        Example (record is a handle selected for physical evidence):
            >>> verification = record.verify()
            >>> verification.record_id == record.record_id
            True
        """
        return RecordVerification(**self._index._native.verify(self.record_id))

    def inspect(self, *, budget: InspectionBudget | None = None) -> RecordInspection:
        """Verify then return a bounded, display-safe RecordInspection.

        budget defaults to InspectionBudget(); use help(InspectionBudget) for
        limits. Inline media and over-budget values become structural references.
        Failed source verification also yields a reference, never unverified
        original content. Check raw_verified and externalized_values. This is
        not byte-exact JSON and performs no export. See help(Record) for errors.

        Example (record is a handle selected for physical evidence):
            >>> inspection = record.inspect()
            >>> inspection.record.id == record.record_id
            True
        """
        budget = budget or InspectionBudget()
        raw = self._index._native.inspect(
            self.record_id,
            budget.max_record_bytes,
            budget.max_value_bytes,
            budget.max_output_bytes,
        )
        metadata = raw["record"]
        raw["record"] = RecordMetadata(
            **{
                **metadata,
                "source": RecordSource(**metadata["source"]),
                "parse": RecordParse(**metadata["parse"]),
            }
        )
        return RecordInspection(**raw)


class Index:
    """Select a local index without creating it. Native connection opens lazily.

    Index.open() additionally verifies that an existing index can be read.
    Index(db=...).sync(paths) bootstraps a new database explicitly. config/db
    resolution matches CLI precedence and is captured at construction.
    Operations on one Index serialize; separate Index instances can run in
    parallel. Native I/O releases the GIL. Queries do not share a transaction.
    ConfigurationError means selection failed; IndexUnavailableError means an
    existing readable index could not be opened, including incompatible storage.
    close() is permanent and invalidates bound Record handles, not returned data.
    Sync releases the cached reader and uses a scoped writer; the next read
    reopens read-only. Do not share handles across a process fork. No Python
    callbacks run under a connection lock, and no cancellation guarantee or
    whole-sync rollback is implied by an interrupted operation.

    For the shared conceptual model and evidence boundaries use help(trace_index).
    For configuration precedence use doc("reference/configuration"). The Python
    API does not create configuration files; supply paths or use the CLI for that.

    Example (database is the selected existing index path):
        >>> with Index.open(db=database) as selected:
        ...     selected.query("SELECT 1 AS ready").rows[0]["ready"]
        1
    """

    def __init__(
        self, *, db: PathInput | None = None, config: PathInput | None = None
    ) -> None:
        self._native = _native.Index(db=db, config=config)

    @classmethod
    def open(
        cls, *, db: PathInput | None = None, config: PathInput | None = None
    ) -> Index:
        """Open an existing index read-only; never create or synchronize it.

        db and config use the same selection rules as Index(). Selection failures
        raise ConfigurationError; missing, unreadable, or incompatible databases
        raise IndexUnavailableError. Prefer a context manager for bounded use.

        Example (database is an existing index path):
            >>> with Index.open(db=database) as selected:
            ...     selected.database.is_absolute()
            True
        """
        index = cls(db=db, config=config)
        index._native.open()
        return index

    @property
    def database(self) -> Path:
        return self._native.database

    def close(self) -> None:
        """Permanently close this index and its Record handles; idempotent.

        Waits for an active operation; later I/O raises ClosedIndexError.
        Returned data remains usable. Closing does not synchronize or export.
        A context manager calls this method automatically.

        Example (database is an existing index path):
            >>> selected = Index.open(db=database)
            >>> selected.close()
            >>> selected.close()
        """
        self._native.close()

    def __enter__(self) -> Index:
        return self

    def __exit__(self, *args: object) -> None:
        self.close()

    def query(
        self,
        sql: str,
        *,
        parameters: Mapping[str, Parameter] | None = None,
        budget: QueryBudget | None = None,
    ) -> QueryResult:
        """Execute one bounded read-only SQL statement, without refreshing the index.

        Use :name placeholders with unprefixed mapping keys. Missing/extra keys
        fail; values bind in SQLite, never via string replacement. No list,
        datetime, identifier, or SQL-fragment coercion. TEXT remains str.
        Input values are None, bool (0/1), signed 64-bit int, finite float, str,
        or bytes. Unsupported types raise TypeError; missing/extra keys, invalid
        numbers, or non-:name placeholders raise ParameterError. Duplicate output
        names are rejected; use SQL aliases. SQL BLOB cells become bytes.

        budget defaults to QueryBudget(); use help(QueryBudget) for defaults and
        ranges. QueryError describes SQL/authorization failures; timeout raises
        QueryTimeoutError, reporting the configured execution limit including
        fractional seconds, not measured elapsed time. Opening failures raise
        IndexUnavailableError; a closed handle raises ClosedIndexError. A bounded
        partial result is returned normally. Use require_complete() when all SQL
        rows/cells are required;
        this does not prove index freshness, Source coverage, or Runtime completeness.
        No implicit synchronization, pagination, text expansion, or retry occurs.

        Example (index is open; use index.schema("sessions") to discover fields):
            >>> result = index.query(
            ...     "SELECT runtime, native_id FROM sessions WHERE working_directory = :directory",
            ...     parameters={"directory": "/path/to/project"},
            ... )
            >>> result.require_complete() is result
            True

        Conditional detail: doc("how-to/query-evidence") and
        doc("how-to/search-literals"). SQL remains the query entry point.
        """
        budget = budget or QueryBudget()
        raw = self._native.query(
            sql,
            dict(parameters) if parameters is not None else {},
            budget.limit,
            budget.max_cell_bytes,
            budget.max_output_bytes,
            budget.timeout_seconds,
        )
        return QueryResult(
            **{
                **raw,
                "columns": tuple(raw["columns"]),
                "rows": tuple(MappingProxyType(row) for row in raw["rows"]),
            }
        )

    def schema(self, relation: str | None = None) -> Schema:
        """Discover the selected existing index's public SQL contract.

        relation=None lists names and descriptions, without columns. Supplying
        one relation includes column names, SQL types, nullability and meanings.
        Does not synchronize. An unknown relation raises UnknownRelationError;
        use schema() without a name to discover valid names.
        Unavailable/incompatible indexes raise IndexUnavailableError, while
        closed handles raise ClosedIndexError.

        Example (index is open):
            >>> relation = index.schema("items").relations[0]
            >>> any(column.name == "semantic" for column in relation.columns)
            True
        """
        raw = self._native.schema(relation)
        return Schema(
            tuple(
                Relation(
                    name=obj["name"],
                    description=obj.get("description"),
                    columns=tuple(
                        Column(**column) for column in obj.get("columns", [])
                    ),
                )
                for obj in raw["objects"]
            )
        )

    def status(self) -> StatusReport:
        """Read stored policy and coverage as a StatusReport, without syncing.

        Opens an existing index; raises IndexUnavailableError for access/storage
        failures or ClosedIndexError for a closed handle. Stored coverage is not
        proof that sources have not changed since their last synchronization.

        Example (index is open):
            >>> index.status().sources >= 0
            True
        """
        raw = self._native.status()
        if raw.get("indexing_policy") is not None:
            raw["indexing_policy"] = IndexingPolicy(**raw["indexing_policy"])
        raw["observed_roles"] = tuple(
            ObservedRole(**{**role, "adapters": tuple(role["adapters"])})
            for role in raw.get("observed_roles", [])
        )
        return StatusReport(**raw)

    def sync(
        self,
        paths: Sequence[PathInput] = (),
        *,
        rebuild: bool = False,
        max_record_bytes: int | None = None,
        max_text_bytes: int | None = None,
    ) -> SyncReport:
        """Explicit write: incrementally index paths, or configured roots if empty.

        Creates the selected database when absent. Explicit paths replace roots,
        not append to them. Defaults preserve stored indexing policy; changing
        policy on a populated index needs a new database, even with rebuild=True.
        Rebuild replaces projections for selected Sources; local IDs may change.
        No automatic retry, progress listener, output, or source export.

        Raises SyncFailedError with .report when recognized sources fail;
        other SyncError failures may also follow partial committed writes.
        Skips, malformed records and incomplete tails remain report findings.
        Byte overrides must be positive signed 64-bit integers; invalid values
        raise ValueError. A single path instead of a sequence raises TypeError.
        A closed handle raises ClosedIndexError. Configured roots are captured
        at Index construction; explicit relative paths use the call's directory.

        Example (paths is a sequence of selected trace files or directories):
            >>> report = index.sync(paths)
            >>> report.failed_files
            0

        Use doc("how-to/configure-and-sync") for indexing policy and Source
        behavior; configuration creation and progress flags there are CLI-only.
        """
        if isinstance(paths, (str, bytes, os.PathLike)):
            raise TypeError("paths must be a sequence of paths, e.g. [path]")
        if type(rebuild) is not bool:
            raise TypeError("rebuild must be bool")
        for name, value in [
            ("max_record_bytes", max_record_bytes),
            ("max_text_bytes", max_text_bytes),
        ]:
            if value is not None:
                _integer(name, value, 1, 2**63 - 1)
        # Preserve symlink/.. semantics; native canonicalization owns resolution.
        base = Path.cwd()
        resolved = [base / path for path in paths]
        raw = self._native.sync(resolved, rebuild, max_record_bytes, max_text_bytes)
        report = SyncReport(
            **{
                **raw,
                "adapters": tuple(raw["adapters"]),
                "indexing_policy": IndexingPolicy(**raw["indexing_policy"]),
                "metrics": MappingProxyType(raw["metrics"]),
                "source_results": tuple(
                    SourceResult(**source) for source in raw.get("source_results", [])
                ),
            }
        )
        if not report.complete:
            raise SyncFailedError(report)
        return report

    def record(self, record_id: int) -> Record:
        """Bind an index-local Record ID; no evidence is read automatically.

        Use an Item witness or a Record selected for an explicit source audit.
        record_id must be a positive signed 64-bit integer (otherwise ValueError).
        The handle shares this Index's lifetime. See help(Record.verify) and
        help(Record.inspect) for source reads, findings and errors.

        Example (record_id was selected from this index):
            >>> index.record(record_id).record_id == record_id
            True
        """
        return Record(self, record_id)


__all__ = [
    "Index",
    "QueryBudget",
    "QueryResult",
    "Schema",
    "Relation",
    "Column",
    "docs",
    "doc",
    "DocTopic",
    "SyncReport",
    "SourceResult",
    "IndexingPolicy",
    "StatusReport",
    "ObservedRole",
    "Record",
    "InspectionBudget",
    "RecordSource",
    "RecordParse",
    "RecordMetadata",
    "RecordVerification",
    "RecordInspection",
    "TraceIndexError",
    "ConfigurationError",
    "IndexUnavailableError",
    "UnknownRelationError",
    "ClosedIndexError",
    "QueryError",
    "ParameterError",
    "QueryTimeoutError",
    "QueryRejectedError",
    "IncompleteResultError",
    "SyncError",
    "SyncFailedError",
    "RecordError",
    "PathInput",
    "Parameter",
    "Cell",
    "Row",
    "JSONValue",
    "__version__",
]
