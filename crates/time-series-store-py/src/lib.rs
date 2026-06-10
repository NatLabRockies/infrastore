//! PyO3 bindings for `time-series-store`.
//!
//! Exposed module name: `time_series_store`. Top-level surface:
//!
//! ```python
//! from time_series_store import (
//!     TimeSeriesStore, SingleTimeSeries, NonSequentialTimeSeries, TimeSeriesKey,
//!     TimeSeriesType, OwnerCategory,
//!     TimeSeriesError, NotFoundError, DuplicateTimeSeriesError, InvalidParameterError,
//!     IntegrityError, ReadOnlyStoreError,
//! )
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, TimeZone, Utc};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBool, PyBytes, PyDelta, PyDict, PyFloat, PyInt, PyString};
use time_series_store_core as core_lib;

// ---- Exceptions -----------------------------------------------------------

create_exception!(time_series_store, TimeSeriesError, PyException);
create_exception!(time_series_store, NotFoundError, TimeSeriesError);
create_exception!(time_series_store, DuplicateTimeSeriesError, TimeSeriesError);
create_exception!(time_series_store, InvalidParameterError, TimeSeriesError);
create_exception!(time_series_store, IntegrityError, TimeSeriesError);
create_exception!(time_series_store, ReadOnlyStoreError, TimeSeriesError);

fn map_err(e: core_lib::TimeSeriesError) -> PyErr {
    use core_lib::TimeSeriesError as E;
    match e {
        E::NotFound => NotFoundError::new_err("time series not found"),
        E::DuplicateTimeSeries => {
            DuplicateTimeSeriesError::new_err("a time series with that key already exists")
        }
        E::InvalidParameter(m) => InvalidParameterError::new_err(m),
        E::IntegrityError(m) => IntegrityError::new_err(m),
        E::ReadOnlyStore => ReadOnlyStoreError::new_err("store is read-only"),
        E::ConnectionError(m) => TimeSeriesError::new_err(format!("connection: {m}")),
        E::IncompatibleForecast => TimeSeriesError::new_err("incompatible forecast"),
        E::Io(e) => TimeSeriesError::new_err(format!("io: {e}")),
        E::Sqlite(e) => TimeSeriesError::new_err(format!("sqlite: {e}")),
        E::Serde(e) => TimeSeriesError::new_err(format!("serde: {e}")),
    }
}

/// Translate the Python-facing compression arguments into a core
/// [`Compression`](core_lib::Compression). Level validation is left to the core
/// constructor so the error message stays in one place.
fn parse_compression(algorithm: &str, level: u8, shuffle: bool) -> PyResult<core_lib::Compression> {
    match algorithm {
        "none" => Ok(core_lib::Compression::None),
        "deflate" => Ok(core_lib::Compression::Deflate { level, shuffle }),
        other => Err(InvalidParameterError::new_err(format!(
            "unknown compression '{other}', expected 'deflate' or 'none'"
        ))),
    }
}

// ---- Enums ----------------------------------------------------------------

#[pyclass(
    eq,
    eq_int,
    name = "TimeSeriesType",
    module = "time_series_store",
    from_py_object
)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PyTimeSeriesType {
    SingleTimeSeries,
    NonSequentialTimeSeries,
    Deterministic,
    DeterministicSingleTimeSeries,
    Probabilistic,
    Scenarios,
}

impl From<PyTimeSeriesType> for core_lib::TimeSeriesType {
    fn from(v: PyTimeSeriesType) -> Self {
        match v {
            PyTimeSeriesType::SingleTimeSeries => core_lib::TimeSeriesType::SingleTimeSeries,
            PyTimeSeriesType::NonSequentialTimeSeries => {
                core_lib::TimeSeriesType::NonSequentialTimeSeries
            }
            PyTimeSeriesType::Deterministic => core_lib::TimeSeriesType::Deterministic,
            PyTimeSeriesType::DeterministicSingleTimeSeries => {
                core_lib::TimeSeriesType::DeterministicSingleTimeSeries
            }
            PyTimeSeriesType::Probabilistic => core_lib::TimeSeriesType::Probabilistic,
            PyTimeSeriesType::Scenarios => core_lib::TimeSeriesType::Scenarios,
        }
    }
}

impl From<core_lib::TimeSeriesType> for PyTimeSeriesType {
    fn from(v: core_lib::TimeSeriesType) -> Self {
        match v {
            core_lib::TimeSeriesType::SingleTimeSeries => PyTimeSeriesType::SingleTimeSeries,
            core_lib::TimeSeriesType::NonSequentialTimeSeries => {
                PyTimeSeriesType::NonSequentialTimeSeries
            }
            core_lib::TimeSeriesType::Deterministic => PyTimeSeriesType::Deterministic,
            core_lib::TimeSeriesType::DeterministicSingleTimeSeries => {
                PyTimeSeriesType::DeterministicSingleTimeSeries
            }
            core_lib::TimeSeriesType::Probabilistic => PyTimeSeriesType::Probabilistic,
            core_lib::TimeSeriesType::Scenarios => PyTimeSeriesType::Scenarios,
        }
    }
}

#[pyclass(
    eq,
    eq_int,
    name = "OwnerCategory",
    module = "time_series_store",
    from_py_object
)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PyOwnerCategory {
    Component,
    SupplementalAttribute,
}

impl From<PyOwnerCategory> for core_lib::OwnerCategory {
    fn from(v: PyOwnerCategory) -> Self {
        match v {
            PyOwnerCategory::Component => core_lib::OwnerCategory::Component,
            PyOwnerCategory::SupplementalAttribute => {
                core_lib::OwnerCategory::SupplementalAttribute
            }
        }
    }
}

// ---- Features -------------------------------------------------------------

/// Convert a Python dict { str: int|float|bool } into the core Features map.
fn features_from_dict(dict: Option<&Bound<'_, PyDict>>) -> PyResult<core_lib::Features> {
    let mut out: core_lib::Features = BTreeMap::new();
    let Some(dict) = dict else {
        return Ok(out);
    };
    for (k, v) in dict {
        let key: String = k.extract()?;
        let value = feature_value_from_py(&v)?;
        out.insert(key, value);
    }
    Ok(out)
}

fn feature_value_from_py(value: &Bound<'_, PyAny>) -> PyResult<core_lib::FeatureValue> {
    // Must check bool BEFORE int — bool is a subtype of int in Python.
    if value.is_instance_of::<PyBool>() {
        let b: bool = value.extract()?;
        Ok(core_lib::FeatureValue::Bool(b))
    } else if value.is_instance_of::<PyInt>() {
        let i: i64 = value.extract()?;
        Ok(core_lib::FeatureValue::Int(i))
    } else if value.is_instance_of::<PyFloat>() {
        let f: f64 = value.extract()?;
        Ok(core_lib::FeatureValue::Float(f))
    } else if value.is_instance_of::<PyString>() {
        let s: String = value.extract()?;
        Ok(core_lib::FeatureValue::Str(s))
    } else {
        Err(InvalidParameterError::new_err(format!(
            "feature values must be int, float, bool, or str; got {}",
            value.get_type().name()?
        )))
    }
}

fn features_to_dict<'py>(
    py: Python<'py>,
    features: &core_lib::Features,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    for (k, v) in features {
        match v {
            core_lib::FeatureValue::Int(i) => dict.set_item(k, *i)?,
            core_lib::FeatureValue::Float(f) => dict.set_item(k, *f)?,
            core_lib::FeatureValue::Bool(b) => dict.set_item(k, *b)?,
            core_lib::FeatureValue::Str(s) => dict.set_item(k, s)?,
        }
    }
    Ok(dict)
}

// ---- numpy dtype mapping --------------------------------------------------

fn dtype_from_numpy_name(name: &str) -> PyResult<core_lib::Dtype> {
    Ok(match name {
        "float64" => core_lib::Dtype::F64,
        "float32" => core_lib::Dtype::F32,
        "int64" => core_lib::Dtype::I64,
        "int32" => core_lib::Dtype::I32,
        "uint64" => core_lib::Dtype::U64,
        "bool" => core_lib::Dtype::Bool,
        other => {
            return Err(InvalidParameterError::new_err(format!(
                "unsupported numpy dtype '{other}' (expected float64/float32/int64/int32/uint64/bool)"
            )));
        }
    })
}

fn numpy_name(dtype: core_lib::Dtype) -> &'static str {
    match dtype {
        core_lib::Dtype::F64 => "float64",
        core_lib::Dtype::F32 => "float32",
        core_lib::Dtype::I64 => "int64",
        core_lib::Dtype::I32 => "int32",
        core_lib::Dtype::U64 => "uint64",
        core_lib::Dtype::Bool => "bool",
    }
}

/// Build a [`TypedArray`] from any numpy array: dtype from `.dtype.name`, shape
/// from `.shape`, and C-order (row-major) bytes from `.tobytes()`.
fn typed_array_from_numpy(data: &Bound<'_, PyAny>) -> PyResult<core_lib::TypedArray> {
    let shape: Vec<usize> = data.getattr("shape")?.extract()?;
    let dtype_name: String = data.getattr("dtype")?.getattr("name")?.extract()?;
    let dtype = dtype_from_numpy_name(&dtype_name)?;
    let bytes: Vec<u8> = data.call_method0("tobytes")?.extract()?;
    core_lib::TypedArray::new(dtype, shape, bytes).map_err(InvalidParameterError::new_err)
}

/// Reconstruct a numpy array (owned, writable) from a [`TypedArray`].
fn numpy_from_typed<'py>(
    py: Python<'py>,
    arr: &core_lib::TypedArray,
) -> PyResult<Bound<'py, PyAny>> {
    let np = py.import("numpy")?;
    let buf = PyBytes::new(py, &arr.bytes);
    let flat = np.call_method1("frombuffer", (buf, numpy_name(arr.dtype)))?;
    let shaped = flat.call_method1("reshape", (arr.shape.clone(),))?;
    // frombuffer is read-only; hand back a writable copy.
    shaped.call_method0("copy")
}

// ---- Deterministic --------------------------------------------------------

#[pyclass(name = "Deterministic", module = "time_series_store", from_py_object)]
#[derive(Clone)]
pub struct PyDeterministic {
    inner: core_lib::Deterministic,
    name: String,
}

#[pymethods]
impl PyDeterministic {
    /// Build a `Deterministic` forecast. `data` is a numpy array of shape
    /// `[H, count, *E]`. `name` is required.
    #[new]
    #[pyo3(signature = (
        initial_timestamp, resolution, horizon, interval, count, data, name
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: Bound<'_, PyDelta>,
        horizon: Bound<'_, PyDelta>,
        interval: Bound<'_, PyDelta>,
        count: usize,
        data: &Bound<'_, PyAny>,
        name: String,
    ) -> PyResult<Self> {
        let resolution = pydelta_to_chrono(&resolution)?;
        let horizon = pydelta_to_chrono(&horizon)?;
        let interval = pydelta_to_chrono(&interval)?;
        let typed = typed_array_from_numpy(data)?;
        let inner = core_lib::Deterministic::new(
            initial_timestamp,
            resolution,
            horizon,
            interval,
            count,
            typed,
        )
        .map_err(InvalidParameterError::new_err)?;
        Ok(Self { inner, name })
    }

    #[getter]
    fn name(&self) -> String {
        self.name.clone()
    }

    #[getter]
    fn initial_timestamp(&self) -> DateTime<Utc> {
        self.inner.initial_timestamp
    }

    #[getter]
    fn resolution<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDelta>> {
        chrono_to_pydelta(py, self.inner.resolution)
    }

    #[getter]
    fn horizon<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDelta>> {
        chrono_to_pydelta(py, self.inner.horizon)
    }

    #[getter]
    fn interval<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDelta>> {
        chrono_to_pydelta(py, self.inner.interval)
    }

    #[getter]
    fn count(&self) -> usize {
        self.inner.count
    }

    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        numpy_from_typed(py, &self.inner.data)
    }

    fn __repr__(&self) -> String {
        format!(
            "Deterministic(name={:?}, initial_timestamp={}, count={}, horizon={}s, interval={}s, resolution={}s, shape={:?})",
            self.name,
            self.inner.initial_timestamp,
            self.inner.count,
            self.inner.horizon.num_seconds(),
            self.inner.interval.num_seconds(),
            self.inner.resolution.num_seconds(),
            self.inner.data.shape,
        )
    }
}

// ---- Probabilistic --------------------------------------------------------

#[pyclass(name = "Probabilistic", module = "time_series_store", from_py_object)]
#[derive(Clone)]
pub struct PyProbabilistic {
    inner: core_lib::Probabilistic,
    name: String,
}

#[pymethods]
impl PyProbabilistic {
    /// Build a `Probabilistic` forecast. `data` is a numpy array of shape
    /// `[num_percentiles, H, count, *E]`. `name` is required.
    #[new]
    #[pyo3(signature = (
        initial_timestamp, resolution, horizon, interval, count, percentiles, data, name
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: Bound<'_, PyDelta>,
        horizon: Bound<'_, PyDelta>,
        interval: Bound<'_, PyDelta>,
        count: usize,
        percentiles: Vec<f64>,
        data: &Bound<'_, PyAny>,
        name: String,
    ) -> PyResult<Self> {
        let resolution = pydelta_to_chrono(&resolution)?;
        let horizon = pydelta_to_chrono(&horizon)?;
        let interval = pydelta_to_chrono(&interval)?;
        let typed = typed_array_from_numpy(data)?;
        let inner = core_lib::Probabilistic::new(
            initial_timestamp,
            resolution,
            horizon,
            interval,
            count,
            percentiles,
            typed,
        )
        .map_err(InvalidParameterError::new_err)?;
        Ok(Self { inner, name })
    }

    #[getter]
    fn name(&self) -> String {
        self.name.clone()
    }

    #[getter]
    fn initial_timestamp(&self) -> DateTime<Utc> {
        self.inner.initial_timestamp
    }

    #[getter]
    fn resolution<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDelta>> {
        chrono_to_pydelta(py, self.inner.resolution)
    }

    #[getter]
    fn horizon<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDelta>> {
        chrono_to_pydelta(py, self.inner.horizon)
    }

    #[getter]
    fn interval<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDelta>> {
        chrono_to_pydelta(py, self.inner.interval)
    }

    #[getter]
    fn count(&self) -> usize {
        self.inner.count
    }

    #[getter]
    fn percentiles(&self) -> Vec<f64> {
        self.inner.percentiles.clone()
    }

    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        numpy_from_typed(py, &self.inner.data)
    }

    fn __repr__(&self) -> String {
        format!(
            "Probabilistic(name={:?}, initial_timestamp={}, count={}, horizon={}s, interval={}s, resolution={}s, percentiles={:?}, shape={:?})",
            self.name,
            self.inner.initial_timestamp,
            self.inner.count,
            self.inner.horizon.num_seconds(),
            self.inner.interval.num_seconds(),
            self.inner.resolution.num_seconds(),
            self.inner.percentiles,
            self.inner.data.shape,
        )
    }
}

// ---- Scenarios ------------------------------------------------------------

#[pyclass(name = "Scenarios", module = "time_series_store", from_py_object)]
#[derive(Clone)]
pub struct PyScenarios {
    inner: core_lib::Scenarios,
    name: String,
}

#[pymethods]
impl PyScenarios {
    /// Build a `Scenarios` forecast. `data` is a numpy array of shape
    /// `[scenario_count, H, count, *E]`; `scenario_count` is taken from the
    /// leading axis. `name` is required.
    #[new]
    #[pyo3(signature = (
        initial_timestamp, resolution, horizon, interval, count, data, name
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: Bound<'_, PyDelta>,
        horizon: Bound<'_, PyDelta>,
        interval: Bound<'_, PyDelta>,
        count: usize,
        data: &Bound<'_, PyAny>,
        name: String,
    ) -> PyResult<Self> {
        let resolution = pydelta_to_chrono(&resolution)?;
        let horizon = pydelta_to_chrono(&horizon)?;
        let interval = pydelta_to_chrono(&interval)?;
        let typed = typed_array_from_numpy(data)?;
        let scenario_count = *typed.shape.first().ok_or_else(|| {
            InvalidParameterError::new_err("Scenarios: data must have at least one axis")
        })?;
        let inner = core_lib::Scenarios::new(
            initial_timestamp,
            resolution,
            horizon,
            interval,
            count,
            scenario_count,
            typed,
        )
        .map_err(InvalidParameterError::new_err)?;
        Ok(Self { inner, name })
    }

    #[getter]
    fn name(&self) -> String {
        self.name.clone()
    }

    #[getter]
    fn initial_timestamp(&self) -> DateTime<Utc> {
        self.inner.initial_timestamp
    }

    #[getter]
    fn resolution<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDelta>> {
        chrono_to_pydelta(py, self.inner.resolution)
    }

    #[getter]
    fn horizon<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDelta>> {
        chrono_to_pydelta(py, self.inner.horizon)
    }

    #[getter]
    fn interval<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDelta>> {
        chrono_to_pydelta(py, self.inner.interval)
    }

    #[getter]
    fn count(&self) -> usize {
        self.inner.count
    }

    #[getter]
    fn scenario_count(&self) -> usize {
        self.inner.scenario_count
    }

    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        numpy_from_typed(py, &self.inner.data)
    }

    fn __repr__(&self) -> String {
        format!(
            "Scenarios(name={:?}, initial_timestamp={}, count={}, horizon={}s, interval={}s, resolution={}s, scenario_count={}, shape={:?})",
            self.name,
            self.inner.initial_timestamp,
            self.inner.count,
            self.inner.horizon.num_seconds(),
            self.inner.interval.num_seconds(),
            self.inner.resolution.num_seconds(),
            self.inner.scenario_count,
            self.inner.data.shape,
        )
    }
}

// ---- SingleTimeSeries -----------------------------------------------------

#[pyclass(
    name = "SingleTimeSeries",
    module = "time_series_store",
    from_py_object
)]
#[derive(Clone)]
pub struct PySingleTimeSeries {
    inner: core_lib::SingleTimeSeries,
    name: String,
}

#[pymethods]
impl PySingleTimeSeries {
    /// `name` is required.
    #[new]
    #[pyo3(signature = (initial_timestamp, resolution, data, name))]
    fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: Bound<'_, PyDelta>,
        data: &Bound<'_, PyAny>,
        name: String,
    ) -> PyResult<Self> {
        let resolution = pydelta_to_chrono(&resolution)?;
        let typed = typed_array_from_numpy(data)?;
        Ok(Self {
            inner: core_lib::SingleTimeSeries::new(initial_timestamp, resolution, typed),
            name,
        })
    }

    #[getter]
    fn name(&self) -> String {
        self.name.clone()
    }

    #[getter]
    fn initial_timestamp(&self) -> DateTime<Utc> {
        self.inner.initial_timestamp
    }

    #[getter]
    fn length(&self) -> usize {
        self.inner.length
    }

    #[getter]
    fn resolution<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDelta>> {
        chrono_to_pydelta(py, self.inner.resolution)
    }

    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        numpy_from_typed(py, &self.inner.data)
    }

    fn __repr__(&self) -> String {
        format!(
            "SingleTimeSeries(name={:?}, initial_timestamp={}, length={}, resolution={}s, shape={:?})",
            self.name,
            self.inner.initial_timestamp,
            self.inner.length,
            self.inner.resolution.num_seconds(),
            self.inner.data.shape,
        )
    }
}

// ---- NonSequentialTimeSeries ----------------------------------------------

#[pyclass(
    name = "NonSequentialTimeSeries",
    module = "time_series_store",
    from_py_object
)]
#[derive(Clone)]
pub struct PyNonSequentialTimeSeries {
    inner: core_lib::NonSequentialTimeSeries,
    name: String,
}

#[pymethods]
impl PyNonSequentialTimeSeries {
    /// `name` is required.
    #[new]
    #[pyo3(signature = (timestamps, data, name))]
    fn new(
        timestamps: Vec<DateTime<Utc>>,
        data: &Bound<'_, PyAny>,
        name: String,
    ) -> PyResult<Self> {
        let typed = typed_array_from_numpy(data)?;
        let inner = core_lib::NonSequentialTimeSeries::new(timestamps, typed)
            .map_err(InvalidParameterError::new_err)?;
        Ok(Self { inner, name })
    }

    #[getter]
    fn name(&self) -> String {
        self.name.clone()
    }

    #[getter]
    fn timestamps(&self) -> Vec<DateTime<Utc>> {
        self.inner.timestamps.clone()
    }

    #[getter]
    fn length(&self) -> usize {
        self.inner.length
    }

    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        numpy_from_typed(py, &self.inner.data)
    }

    fn __repr__(&self) -> String {
        format!(
            "NonSequentialTimeSeries(length={}, shape={:?})",
            self.inner.length, self.inner.data.shape,
        )
    }
}

// ---- TimeSeriesKey --------------------------------------------------------

#[pyclass(name = "TimeSeriesKey", module = "time_series_store", from_py_object)]
#[derive(Clone)]
pub struct PyTimeSeriesKey {
    inner: core_lib::TimeSeriesKey,
}

#[pymethods]
impl PyTimeSeriesKey {
    #[getter]
    fn owner_uuid(&self) -> String {
        self.inner.owner_uuid.clone()
    }

    #[getter]
    fn time_series_type(&self) -> PyTimeSeriesType {
        self.inner.time_series_type.into()
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    #[getter]
    fn resolution<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDelta>>> {
        match self.inner.resolution {
            Some(d) => Ok(Some(chrono_to_pydelta(py, d)?)),
            None => Ok(None),
        }
    }

    #[getter]
    fn features<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        features_to_dict(py, &self.inner.features)
    }

    fn __repr__(&self) -> String {
        format!(
            "TimeSeriesKey(owner_uuid={:?}, time_series_type={:?}, name={:?}, features={:?})",
            self.inner.owner_uuid,
            self.inner.time_series_type.as_str(),
            self.inner.name,
            self.inner.features,
        )
    }
}

// ---- TimeSeriesStore ------------------------------------------------------

#[pyclass(name = "TimeSeriesStore", module = "time_series_store", unsendable)]
pub struct PyStore {
    inner: core_lib::Store,
}

#[pymethods]
impl PyStore {
    /// Create a new store. With `in_memory=True`, no filesystem I/O occurs;
    /// otherwise a NetCDF file is created at `path` and a catalog SQLite file
    /// at `<path>.sqlite` holds metadata.
    ///
    /// `compression` selects the NetCDF data-variable filter: `"deflate"`
    /// (default) applies DEFLATE at `compression_level` (0–9) with optional
    /// byte `shuffle`; `"none"` disables compression. The setting is ignored
    /// for in-memory stores and is persisted so later appends reuse it.
    #[classmethod]
    #[pyo3(signature = (path=None, in_memory=false, compression="deflate", compression_level=3, shuffle=true))]
    fn create(
        _cls: &Bound<'_, pyo3::types::PyType>,
        path: Option<PathBuf>,
        in_memory: bool,
        compression: &str,
        compression_level: u8,
        shuffle: bool,
    ) -> PyResult<Self> {
        let compression = parse_compression(compression, compression_level, shuffle)?;
        let store =
            core_lib::create_store_with_compression(path.as_deref(), in_memory, compression)
                .map_err(map_err)?;
        Ok(Self { inner: store })
    }

    /// Open an existing store from disk. `read_only=True` blocks all writes.
    #[classmethod]
    #[pyo3(signature = (path, read_only=false))]
    fn open(
        _cls: &Bound<'_, pyo3::types::PyType>,
        path: PathBuf,
        read_only: bool,
    ) -> PyResult<Self> {
        let store = core_lib::open_store(&path, read_only).map_err(map_err)?;
        Ok(Self { inner: store })
    }

    #[getter]
    fn read_only(&self) -> bool {
        self.inner.read_only()
    }

    /// Add a time series. The association `name` comes from the time series
    /// object (`time_series.name`).
    ///
    /// `features` is a `dict[str, int|float|bool|str]`. `units` is an optional
    /// string.
    #[pyo3(signature = (owner_uuid, owner_type, owner_category, time_series, features=None, units=None))]
    fn add_time_series(
        &mut self,
        owner_uuid: &str,
        owner_type: &str,
        owner_category: PyOwnerCategory,
        time_series: &Bound<'_, PyAny>,
        features: Option<&Bound<'_, PyDict>>,
        units: Option<String>,
    ) -> PyResult<PyTimeSeriesKey> {
        let features = features_from_dict(features)?;
        // `name` is read off the object.
        let (data, name) = if let Ok(single) = time_series.extract::<PySingleTimeSeries>() {
            (
                core_lib::TimeSeriesData::SingleTimeSeries(single.inner),
                single.name,
            )
        } else if let Ok(ns) = time_series.extract::<PyNonSequentialTimeSeries>() {
            (
                core_lib::TimeSeriesData::NonSequentialTimeSeries(ns.inner),
                ns.name,
            )
        } else if let Ok(det) = time_series.extract::<PyDeterministic>() {
            (core_lib::TimeSeriesData::Deterministic(det.inner), det.name)
        } else if let Ok(prob) = time_series.extract::<PyProbabilistic>() {
            (
                core_lib::TimeSeriesData::Probabilistic(prob.inner),
                prob.name,
            )
        } else if let Ok(scen) = time_series.extract::<PyScenarios>() {
            (core_lib::TimeSeriesData::Scenarios(scen.inner), scen.name)
        } else {
            return Err(InvalidParameterError::new_err(
                "time_series must be SingleTimeSeries, NonSequentialTimeSeries, \
                     Deterministic, Probabilistic, or Scenarios",
            ));
        };
        let key = self
            .inner
            .add_time_series(
                owner_uuid,
                owner_type,
                owner_category.into(),
                &name,
                data,
                features,
                units,
            )
            .map_err(map_err)?;
        Ok(PyTimeSeriesKey { inner: key })
    }

    /// Derive `DeterministicSingleTimeSeries` forecasts from the stored
    /// `SingleTimeSeries` associations (mirrors InfrastructureSystems.jl's
    /// `transform_single_time_series!`). Each `SingleTimeSeries` is re-described
    /// as a DST sharing the same underlying array; `count` is derived from each
    /// series' length. Returns the number of series transformed.
    fn transform_single_time_series(
        &mut self,
        horizon: Bound<'_, PyDelta>,
        interval: Bound<'_, PyDelta>,
    ) -> PyResult<usize> {
        let horizon = pydelta_to_chrono(&horizon)?;
        let interval = pydelta_to_chrono(&interval)?;
        self.inner
            .transform_single_time_series(horizon, interval, None, None)
            .map_err(map_err)
    }

    fn remove_time_series(&mut self, key: &PyTimeSeriesKey) -> PyResult<()> {
        self.inner.remove_time_series(&key.inner).map_err(map_err)
    }

    #[pyo3(signature = (owner_uuid=None))]
    fn clear_time_series(&mut self, owner_uuid: Option<String>) -> PyResult<usize> {
        self.inner
            .clear_time_series(owner_uuid.as_deref())
            .map_err(map_err)
    }

    /// Fetch a static time series by key. `time_range`, if given, is a tuple of
    /// `(start: datetime, end: datetime)` with end exclusive.
    #[pyo3(signature = (key, time_range=None))]
    fn get_time_series(
        &self,
        py: Python<'_>,
        key: &PyTimeSeriesKey,
        time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> PyResult<Py<PyAny>> {
        let data = self
            .inner
            .get_time_series(&key.inner, time_range)
            .map_err(map_err)?;
        // `name` is a per-association attribute, not part of the core data type
        // — resolve it from the metadata (consistent with the read, which
        // resolves the same key).
        let meta = self.inner.get_metadata(&key.inner).map_err(map_err)?;
        let name = meta.name;
        match data {
            core_lib::TimeSeriesData::SingleTimeSeries(s) => {
                Ok(Py::new(py, PySingleTimeSeries { inner: s, name })?.into_any())
            }
            core_lib::TimeSeriesData::NonSequentialTimeSeries(s) => {
                Ok(Py::new(py, PyNonSequentialTimeSeries { inner: s, name })?.into_any())
            }
            core_lib::TimeSeriesData::Deterministic(d) => {
                Ok(Py::new(py, PyDeterministic { inner: d, name })?.into_any())
            }
            core_lib::TimeSeriesData::Probabilistic(p) => {
                Ok(Py::new(py, PyProbabilistic { inner: p, name })?.into_any())
            }
            core_lib::TimeSeriesData::Scenarios(s) => {
                Ok(Py::new(py, PyScenarios { inner: s, name })?.into_any())
            }
        }
    }

    /// Return a list of metadata dicts matching the filter. Each dict has
    /// `owner_uuid`, `owner_type`, `time_series_type`, `name`, `length`,
    /// `resolution_seconds`, `features`, `units`.
    #[pyo3(signature = (
        owner_uuid=None, owner_type=None, time_series_type=None,
        name=None, resolution=None, features=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn list_time_series<'py>(
        &self,
        py: Python<'py>,
        owner_uuid: Option<String>,
        owner_type: Option<String>,
        time_series_type: Option<PyTimeSeriesType>,
        name: Option<String>,
        resolution: Option<Bound<'_, PyDelta>>,
        features: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let mut filter = core_lib::ListFilter::new();
        if let Some(uuid) = owner_uuid {
            filter = filter.owner_uuid(uuid);
        }
        if let Some(t) = owner_type {
            filter = filter.owner_type(t);
        }
        if let Some(t) = time_series_type {
            filter = filter.time_series_type(t.into());
        }
        if let Some(n) = name {
            filter = filter.name(n);
        }
        if let Some(r) = resolution {
            filter = filter.resolution(pydelta_to_chrono(&r)?);
        }
        if let Some(f) = features {
            filter = filter.features(features_from_dict(Some(f))?);
        }
        let metas = self.inner.list_time_series(filter).map_err(map_err)?;
        let mut out = Vec::with_capacity(metas.len());
        for m in &metas {
            let d = PyDict::new(py);
            d.set_item("owner_uuid", &m.owner_uuid)?;
            d.set_item("owner_type", &m.owner_type)?;
            d.set_item("owner_category", m.owner_category.as_str())?;
            d.set_item("time_series_type", m.time_series_type.as_str())?;
            d.set_item("name", &m.name)?;
            d.set_item("data_hash", core_lib::hash::hash_hex(&m.data_hash))?;
            d.set_item("length", m.length)?;
            d.set_item("resolution_seconds", m.resolution.map(|r| r.num_seconds()))?;
            d.set_item(
                "timestamps",
                m.timestamps.as_ref().map(|timestamps| {
                    timestamps
                        .iter()
                        .map(DateTime::to_rfc3339)
                        .collect::<Vec<_>>()
                }),
            )?;
            d.set_item("features", features_to_dict(py, &m.features)?)?;
            d.set_item("units", m.units.clone())?;
            out.push(d);
        }
        Ok(out)
    }

    fn get_time_series_keys(&self, owner_uuid: &str) -> PyResult<Vec<PyTimeSeriesKey>> {
        Ok(self
            .inner
            .get_time_series_keys(owner_uuid)
            .map_err(map_err)?
            .into_iter()
            .map(|k| PyTimeSeriesKey { inner: k })
            .collect())
    }

    fn has_time_series(&self, key: &PyTimeSeriesKey) -> PyResult<bool> {
        self.inner.has_time_series(&key.inner).map_err(map_err)
    }

    #[pyo3(signature = (time_series_type=None))]
    fn get_resolutions(
        &self,
        py: Python<'_>,
        time_series_type: Option<PyTimeSeriesType>,
    ) -> PyResult<Vec<Py<PyDelta>>> {
        let durations = self
            .inner
            .get_resolutions(time_series_type.map(Into::into))
            .map_err(map_err)?;
        let mut out = Vec::with_capacity(durations.len());
        for d in durations {
            out.push(chrono_to_pydelta(py, d)?.unbind());
        }
        Ok(out)
    }

    /// Return the store's forecast parameters as a dict with keys `horizon`,
    /// `interval` (timedeltas), `count` (int), and `resolution` (timedelta).
    /// Each value is `None` when the store holds no forecasts.
    fn get_forecast_parameters<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let p = self.inner.get_forecast_parameters().map_err(map_err)?;
        let d = PyDict::new(py);
        let dur = |py: Python<'py>, v: Option<chrono::Duration>| -> PyResult<Option<Py<PyDelta>>> {
            match v {
                Some(v) => Ok(Some(chrono_to_pydelta(py, v)?.unbind())),
                None => Ok(None),
            }
        };
        d.set_item("horizon", dur(py, p.horizon)?)?;
        d.set_item("interval", dur(py, p.interval)?)?;
        d.set_item("count", p.count)?;
        d.set_item("resolution", dur(py, p.resolution)?)?;
        Ok(d)
    }

    /// Return the store's compression policy as a dict with keys `compression`
    /// (`"deflate"` or `"none"`), `level` (int, 0-9), and `shuffle` (bool). For a
    /// store opened from disk this reflects the persisted policy; in-memory
    /// stores report `"none"`.
    fn get_compression<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        match self.inner.compression() {
            core_lib::Compression::None => {
                d.set_item("compression", "none")?;
                d.set_item("level", 0u8)?;
                d.set_item("shuffle", false)?;
            }
            core_lib::Compression::Deflate { level, shuffle } => {
                d.set_item("compression", "deflate")?;
                d.set_item("level", level)?;
                d.set_item("shuffle", shuffle)?;
            }
        }
        Ok(d)
    }

    fn get_time_series_counts<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let counts = self.inner.get_time_series_counts().map_err(map_err)?;
        let d = PyDict::new(py);
        d.set_item(
            "components_with_time_series",
            counts.components_with_time_series,
        )?;
        d.set_item("static_time_series", counts.static_time_series)?;
        d.set_item("forecasts", counts.forecasts)?;
        Ok(d)
    }

    fn compact<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let r = self.inner.compact().map_err(map_err)?;
        let d = PyDict::new(py);
        d.set_item("slots_reclaimed", r.slots_reclaimed)?;
        d.set_item("datasets_dropped", r.datasets_dropped)?;
        Ok(d)
    }

    fn verify_integrity(&self) -> PyResult<Vec<String>> {
        Ok(self.inner.verify_integrity().map_err(map_err)?.errors)
    }

    fn flush(&mut self) -> PyResult<()> {
        self.inner.flush().map_err(map_err)
    }
}

// ---- chrono ↔ pydelta helpers ---------------------------------------------

fn pydelta_to_chrono(delta: &Bound<'_, PyDelta>) -> PyResult<chrono::Duration> {
    // pyo3's `chrono` feature already implements TryFrom for Duration.
    delta.extract::<chrono::Duration>()
}

fn chrono_to_pydelta<'py>(py: Python<'py>, d: chrono::Duration) -> PyResult<Bound<'py, PyDelta>> {
    use pyo3::IntoPyObject;
    d.into_pyobject(py)
}

#[allow(dead_code)]
fn unused_tz_imports() {
    // Touch TimeZone so the import isn't pruned in case rustc trims earlier.
    let _ = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
}

// ---- Tracing ---------------------------------------------------------------

/// Initialize the Rust tracing subscriber.
///
/// `filter` is a [`tracing_subscriber::EnvFilter`] directive string, e.g.
/// `"debug"`, `"time_series_store_core=debug"`, or `"warn,time_series_store_core=trace"`.
///
/// The subscriber is initialized at most once per process. Calling this
/// function again after a successful first call is a no-op. If `RUST_LOG` is
/// set when the module is imported, a subscriber is initialized automatically
/// before this function is called; use this function when you need programmatic
/// control without relying on environment variables.
#[pyfunction]
fn init_tracing(filter: &str) -> PyResult<()> {
    use tracing_subscriber::EnvFilter;
    let env_filter = EnvFilter::try_new(filter)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .try_init();
    Ok(())
}

// ---- Module init ----------------------------------------------------------

#[pymodule]
fn time_series_store(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Auto-initialize from RUST_LOG if set. try_init() is a no-op when a
    // subscriber is already registered, so this is safe to call unconditionally.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    m.add_class::<PyStore>()?;
    m.add_class::<PySingleTimeSeries>()?;
    m.add_class::<PyNonSequentialTimeSeries>()?;
    m.add_class::<PyDeterministic>()?;
    m.add_class::<PyProbabilistic>()?;
    m.add_class::<PyScenarios>()?;
    m.add_class::<PyTimeSeriesKey>()?;
    m.add_class::<PyTimeSeriesType>()?;
    m.add_class::<PyOwnerCategory>()?;

    m.add("TimeSeriesError", py.get_type::<TimeSeriesError>())?;
    m.add("NotFoundError", py.get_type::<NotFoundError>())?;
    m.add(
        "DuplicateTimeSeriesError",
        py.get_type::<DuplicateTimeSeriesError>(),
    )?;
    m.add(
        "InvalidParameterError",
        py.get_type::<InvalidParameterError>(),
    )?;
    m.add("IntegrityError", py.get_type::<IntegrityError>())?;
    m.add("ReadOnlyStoreError", py.get_type::<ReadOnlyStoreError>())?;

    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_function(wrap_pyfunction!(init_tracing, m)?)?;
    Ok(())
}
