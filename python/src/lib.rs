//! PyO3 boundary: convert values only while attached; perform I/O while detached.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyString};
use trace_index::api::{self, NativeCell, Parameter, QueryFailure};

create_exception!(_native, TraceIndexError, PyException);
create_exception!(_native, ConfigurationError, TraceIndexError);
create_exception!(_native, IndexUnavailableError, TraceIndexError);
create_exception!(_native, ClosedIndexError, TraceIndexError);
create_exception!(_native, QueryError, TraceIndexError);
create_exception!(_native, ParameterError, QueryError);
create_exception!(_native, QueryTimeoutError, QueryError);
create_exception!(_native, QueryRejectedError, QueryError);
create_exception!(_native, SyncError, TraceIndexError);
create_exception!(_native, RecordError, TraceIndexError);

fn error(error: anyhow::Error, fallback: fn(String) -> PyErr) -> PyErr {
    let message = format!("{error:#}");
    if error.downcast_ref::<api::ClosedIndex>().is_some() {
        return ClosedIndexError::new_err(message);
    }
    if let Some(kind) = error.downcast_ref::<QueryFailure>() {
        return match kind {
            QueryFailure::Parameter(_) => ParameterError::new_err(message),
            QueryFailure::Rejected(_) => QueryRejectedError::new_err(message),
            QueryFailure::Timeout => QueryTimeoutError::new_err(message),
        };
    }
    fallback(message)
}

#[pyclass(name = "Index", module = "trace_index._native", frozen)]
struct NativeIndex {
    inner: Mutex<api::Index>,
    database: PathBuf,
}

impl NativeIndex {
    fn access<T>(&self, operation: impl FnOnce(&mut api::Index) -> PyResult<T>) -> PyResult<T> {
        let mut index = self
            .inner
            .lock()
            .map_err(|_| TraceIndexError::new_err("native index lock poisoned"))?;
        operation(&mut index)
    }
}

#[pymethods]
impl NativeIndex {
    #[new]
    #[pyo3(signature = (*, config=None, db=None))]
    fn new(py: Python<'_>, config: Option<PathBuf>, db: Option<PathBuf>) -> PyResult<Self> {
        let index = py
            .detach(|| api::Index::new(config.as_deref(), db.as_deref()))
            .map_err(|e| error(e, ConfigurationError::new_err))?;
        let database = index.database().to_owned();
        Ok(Self {
            inner: Mutex::new(index),
            database,
        })
    }

    #[getter]
    fn database(&self) -> PathBuf {
        self.database.clone()
    }

    fn open(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            self.access(|index| {
                index
                    .open()
                    .map_err(|e| error(e, IndexUnavailableError::new_err))
            })
        })
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            self.access(|index| {
                index.close();
                Ok(())
            })
        })
    }

    #[pyo3(signature = (sql, parameters, limit, max_cell_bytes, max_output_bytes, timeout_seconds))]
    #[allow(clippy::too_many_arguments)]
    fn query<'py>(
        &self,
        py: Python<'py>,
        sql: String,
        parameters: &Bound<'py, PyDict>,
        limit: usize,
        max_cell_bytes: usize,
        max_output_bytes: usize,
        timeout_seconds: f64,
    ) -> PyResult<Bound<'py, PyDict>> {
        let parameters = parameters
            .iter()
            .map(|(key, value)| {
                let name = key
                    .extract::<String>()
                    .map_err(|_| ParameterError::new_err("parameter names must be strings"))?;
                Ok((name, parameter(&value)?))
            })
            .collect::<PyResult<BTreeMap<_, _>>>()?;
        if !timeout_seconds.is_finite() || timeout_seconds <= 0.0 || timeout_seconds > 300.0 {
            return Err(QueryRejectedError::new_err(
                "timeout_seconds must be finite and in (0, 300]",
            ));
        }
        let budget = api::QueryBudget {
            limit,
            max_cell_bytes,
            max_output_bytes,
            timeout: Duration::from_secs_f64(timeout_seconds),
        };
        let report = py.detach(|| {
            self.access(|index| {
                index
                    .open()
                    .map_err(|e| error(e, IndexUnavailableError::new_err))?;
                index
                    .query(&sql, &parameters, &budget)
                    .map_err(|e| error(e, QueryError::new_err))
            })
        })?;
        let result = PyDict::new(py);
        result.set_item("columns", report.columns)?;
        result.set_item("complete", report.complete)?;
        result.set_item("incomplete_reason", report.incomplete_reason)?;
        result.set_item("cells_truncated", report.cells_truncated)?;
        result.set_item("hint", report.hint)?;
        let rows = report
            .rows
            .into_iter()
            .map(|row| {
                let object = PyDict::new(py);
                for (key, value) in row {
                    match value {
                        NativeCell::Null => object.set_item(key, py.None())?,
                        NativeCell::Integer(v) => object.set_item(key, v)?,
                        NativeCell::Real(v) => object.set_item(key, v)?,
                        NativeCell::Text(v) => object.set_item(key, v)?,
                        NativeCell::Blob { bytes, .. } => {
                            object.set_item(key, PyBytes::new(py, &bytes))?
                        }
                    }
                }
                Ok(object)
            })
            .collect::<PyResult<Vec<_>>>()?;
        result.set_item("rows", rows)?;
        Ok(result)
    }

    #[pyo3(signature = (relation=None))]
    fn schema<'py>(
        &self,
        py: Python<'py>,
        relation: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let report = py.detach(|| {
            self.access(|index| {
                index.schema(relation.as_deref()).map_err(|e| {
                    if e.is::<api::UnknownRelation>() {
                        IndexUnavailableError::new_err(format!(
                            "{e}\nHint: use index.schema() to discover valid public relation names"
                        ))
                    } else {
                        error(e, IndexUnavailableError::new_err)
                    }
                })
            })
        })?;
        Ok(pythonize::pythonize(py, &report)?)
    }

    fn status<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let report = py.detach(|| {
            self.access(|index| {
                index
                    .status()
                    .map_err(|e| error(e, IndexUnavailableError::new_err))
            })
        })?;
        Ok(pythonize::pythonize(py, &report)?)
    }

    #[pyo3(signature = (paths, rebuild=false, max_record_bytes=None, max_text_bytes=None))]
    fn sync<'py>(
        &self,
        py: Python<'py>,
        paths: Vec<PathBuf>,
        rebuild: bool,
        max_record_bytes: Option<usize>,
        max_text_bytes: Option<usize>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let options = api::SyncOptions {
            rebuild,
            max_record_bytes,
            max_text_bytes,
        };
        let report = py.detach(|| {
            self.access(|index| {
                index
                    .sync(paths, &options)
                    .map_err(|e| error(e, SyncError::new_err))
            })
        })?;
        Ok(pythonize::pythonize(py, &report)?)
    }

    fn verify<'py>(&self, py: Python<'py>, record_id: i64) -> PyResult<Bound<'py, PyAny>> {
        let report = py.detach(|| {
            self.access(|index| {
                index
                    .verify(record_id)
                    .map_err(|e| error(e, RecordError::new_err))
            })
        })?;
        Ok(pythonize::pythonize(py, &report)?)
    }

    fn inspect<'py>(
        &self,
        py: Python<'py>,
        record_id: i64,
        max_record_bytes: u64,
        max_value_bytes: usize,
        max_output_bytes: usize,
    ) -> PyResult<Bound<'py, PyAny>> {
        let report = py.detach(|| {
            self.access(|index| {
                index
                    .inspect(
                        record_id,
                        max_record_bytes,
                        max_value_bytes,
                        max_output_bytes,
                    )
                    .map_err(|e| error(e, RecordError::new_err))
            })
        })?;
        Ok(pythonize::pythonize(py, &report)?)
    }
}

fn parameter(value: &Bound<'_, PyAny>) -> PyResult<Parameter> {
    if value.is_none() {
        return Ok(Parameter::Null);
    }
    if value.is_instance_of::<PyBool>() {
        return Ok(Parameter::Integer(i64::from(value.extract::<bool>()?)));
    }
    if value.is_instance_of::<PyInt>() {
        return value
            .extract::<i64>()
            .map(Parameter::Integer)
            .map_err(|_| ParameterError::new_err("integer parameter exceeds signed 64-bit range"));
    }
    if value.is_instance_of::<PyFloat>() {
        let number = value.extract::<f64>()?;
        if !number.is_finite() {
            return Err(ParameterError::new_err("float parameter must be finite"));
        }
        return Ok(Parameter::Real(number));
    }
    if value.is_instance_of::<PyString>() {
        return Ok(Parameter::Text(value.extract()?));
    }
    if let Ok(bytes) = value.cast::<PyBytes>() {
        return Ok(Parameter::Blob(bytes.as_bytes().to_vec()));
    }
    Err(PyTypeError::new_err(
        "parameters must be None, bool, int, float, str, or bytes",
    ))
}

#[pyfunction]
fn docs(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    Ok(pythonize::pythonize(py, &api::docs())?)
}

#[pyfunction]
fn doc(topic: &str) -> PyResult<String> {
    api::doc(topic)
        .map(|doc| doc.content.into_owned())
        .map_err(|e| {
            PyValueError::new_err(format!(
                "{e}\nHint: use docs() to discover valid topic names"
            ))
        })
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.setattr("__doc__", api::agent_context())?;
    module.add_class::<NativeIndex>()?;
    module.add_function(wrap_pyfunction!(docs, module)?)?;
    module.add_function(wrap_pyfunction!(doc, module)?)?;
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    let py = module.py();
    macro_rules! exception { ($($name:ident),* $(,)?) => { $(module.add(stringify!($name), py.get_type::<$name>())?;)* }; }
    exception!(
        TraceIndexError,
        ConfigurationError,
        IndexUnavailableError,
        ClosedIndexError,
        QueryError,
        ParameterError,
        QueryTimeoutError,
        QueryRejectedError,
        SyncError,
        RecordError
    );
    Ok(())
}
