from pathlib import Path
from typing import Any, final
from os import PathLike

__version__: str
__all__ = [
    "Index",
    "docs",
    "doc",
    "__version__",
    "TraceIndexError",
    "ConfigurationError",
    "IndexUnavailableError",
    "ClosedIndexError",
    "QueryError",
    "ParameterError",
    "QueryTimeoutError",
    "QueryRejectedError",
    "SyncError",
    "RecordError",
]

class TraceIndexError(Exception): ...
class ConfigurationError(TraceIndexError): ...
class IndexUnavailableError(TraceIndexError): ...
class ClosedIndexError(TraceIndexError): ...
class QueryError(TraceIndexError): ...
class ParameterError(QueryError): ...
class QueryTimeoutError(QueryError): ...
class QueryRejectedError(QueryError): ...
class SyncError(TraceIndexError): ...
class RecordError(TraceIndexError): ...

@final
class Index:
    def __new__(
        cls,
        *,
        config: str | PathLike[str] | None = None,
        db: str | PathLike[str] | None = None,
    ) -> Index: ...
    @property
    def database(self) -> Path: ...
    def open(self) -> None: ...
    def close(self) -> None: ...
    def query(
        self,
        sql: str,
        parameters: dict[str, Any],
        limit: int,
        max_cell_bytes: int,
        max_output_bytes: int,
        timeout_seconds: float,
    ) -> dict[str, Any]: ...
    def schema(self, relation: str | None = None) -> dict[str, Any]: ...
    def status(self) -> dict[str, Any]: ...
    def sync(
        self,
        paths: list[Path],
        rebuild: bool = False,
        max_record_bytes: int | None = None,
        max_text_bytes: int | None = None,
    ) -> dict[str, Any]: ...
    def verify(self, record_id: int) -> dict[str, Any]: ...
    def inspect(
        self,
        record_id: int,
        max_record_bytes: int,
        max_value_bytes: int,
        max_output_bytes: int,
    ) -> dict[str, Any]: ...

def docs() -> dict[str, Any]: ...
def doc(topic: str) -> str: ...
