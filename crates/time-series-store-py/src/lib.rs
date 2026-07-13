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
use pyo3::types::{PyAny, PyBool, PyBytes, PyDict, PyFloat, PyInt, PyString};
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
        ref e @ E::IncompatibleFormat { .. } => TimeSeriesError::new_err(e.to_string()),
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

impl From<core_lib::OwnerCategory> for PyOwnerCategory {
    fn from(v: core_lib::OwnerCategory) -> Self {
        match v {
            core_lib::OwnerCategory::Component => PyOwnerCategory::Component,
            core_lib::OwnerCategory::SupplementalAttribute => {
                PyOwnerCategory::SupplementalAttribute
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
        resolution: Bound<'_, PyAny>,
        horizon: Bound<'_, PyAny>,
        interval: Bound<'_, PyAny>,
        count: usize,
        data: &Bound<'_, PyAny>,
        name: String,
    ) -> PyResult<Self> {
        let resolution = pyany_to_period(&resolution)?;
        let horizon = pyany_to_period(&horizon)?;
        let interval = pyany_to_period(&interval)?;
        let typed = typed_array_from_numpy(data)?;
        let inner = core_lib::Deterministic::new(
            initial_timestamp,
            resolution,
            horizon,
            interval,
            count,
            typed,
            name,
        )
        .map_err(InvalidParameterError::new_err)?;
        Ok(Self { inner })
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    #[getter]
    fn initial_timestamp(&self) -> DateTime<Utc> {
        self.inner.initial_timestamp
    }

    #[getter]
    fn resolution(&self) -> String {
        self.inner.resolution.to_iso8601()
    }

    #[getter]
    fn horizon(&self) -> String {
        self.inner.horizon.to_iso8601()
    }

    #[getter]
    fn interval(&self) -> String {
        self.inner.interval.to_iso8601()
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
            "Deterministic(name={:?}, initial_timestamp={}, count={}, horizon={}, interval={}, resolution={}, shape={:?})",
            self.inner.name,
            self.inner.initial_timestamp,
            self.inner.count,
            self.inner.horizon.to_iso8601(),
            self.inner.interval.to_iso8601(),
            self.inner.resolution.to_iso8601(),
            self.inner.data.shape,
        )
    }
}

// ---- Probabilistic --------------------------------------------------------

#[pyclass(name = "Probabilistic", module = "time_series_store", from_py_object)]
#[derive(Clone)]
pub struct PyProbabilistic {
    inner: core_lib::Probabilistic,
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
        resolution: Bound<'_, PyAny>,
        horizon: Bound<'_, PyAny>,
        interval: Bound<'_, PyAny>,
        count: usize,
        percentiles: Vec<f64>,
        data: &Bound<'_, PyAny>,
        name: String,
    ) -> PyResult<Self> {
        let resolution = pyany_to_period(&resolution)?;
        let horizon = pyany_to_period(&horizon)?;
        let interval = pyany_to_period(&interval)?;
        let typed = typed_array_from_numpy(data)?;
        let inner = core_lib::Probabilistic::new(
            initial_timestamp,
            resolution,
            horizon,
            interval,
            count,
            percentiles,
            typed,
            name,
        )
        .map_err(InvalidParameterError::new_err)?;
        Ok(Self { inner })
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    #[getter]
    fn initial_timestamp(&self) -> DateTime<Utc> {
        self.inner.initial_timestamp
    }

    #[getter]
    fn resolution(&self) -> String {
        self.inner.resolution.to_iso8601()
    }

    #[getter]
    fn horizon(&self) -> String {
        self.inner.horizon.to_iso8601()
    }

    #[getter]
    fn interval(&self) -> String {
        self.inner.interval.to_iso8601()
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
            "Probabilistic(name={:?}, initial_timestamp={}, count={}, horizon={}, interval={}, resolution={}, percentiles={:?}, shape={:?})",
            self.inner.name,
            self.inner.initial_timestamp,
            self.inner.count,
            self.inner.horizon.to_iso8601(),
            self.inner.interval.to_iso8601(),
            self.inner.resolution.to_iso8601(),
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
        resolution: Bound<'_, PyAny>,
        horizon: Bound<'_, PyAny>,
        interval: Bound<'_, PyAny>,
        count: usize,
        data: &Bound<'_, PyAny>,
        name: String,
    ) -> PyResult<Self> {
        let resolution = pyany_to_period(&resolution)?;
        let horizon = pyany_to_period(&horizon)?;
        let interval = pyany_to_period(&interval)?;
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
            name,
        )
        .map_err(InvalidParameterError::new_err)?;
        Ok(Self { inner })
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    #[getter]
    fn initial_timestamp(&self) -> DateTime<Utc> {
        self.inner.initial_timestamp
    }

    #[getter]
    fn resolution(&self) -> String {
        self.inner.resolution.to_iso8601()
    }

    #[getter]
    fn horizon(&self) -> String {
        self.inner.horizon.to_iso8601()
    }

    #[getter]
    fn interval(&self) -> String {
        self.inner.interval.to_iso8601()
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
            "Scenarios(name={:?}, initial_timestamp={}, count={}, horizon={}, interval={}, resolution={}, scenario_count={}, shape={:?})",
            self.inner.name,
            self.inner.initial_timestamp,
            self.inner.count,
            self.inner.horizon.to_iso8601(),
            self.inner.interval.to_iso8601(),
            self.inner.resolution.to_iso8601(),
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
}

#[pymethods]
impl PySingleTimeSeries {
    /// `name` is required.
    #[new]
    #[pyo3(signature = (initial_timestamp, resolution, data, name))]
    fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: Bound<'_, PyAny>,
        data: &Bound<'_, PyAny>,
        name: String,
    ) -> PyResult<Self> {
        let resolution = pyany_to_period(&resolution)?;
        let typed = typed_array_from_numpy(data)?;
        Ok(Self {
            inner: core_lib::SingleTimeSeries::new(initial_timestamp, resolution, typed, name),
        })
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
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
    fn resolution(&self) -> String {
        self.inner.resolution.to_iso8601()
    }

    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        numpy_from_typed(py, &self.inner.data)
    }

    fn __repr__(&self) -> String {
        format!(
            "SingleTimeSeries(name={:?}, initial_timestamp={}, length={}, resolution={}, shape={:?})",
            self.inner.name,
            self.inner.initial_timestamp,
            self.inner.length,
            self.inner.resolution.to_iso8601(),
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
        let inner = core_lib::NonSequentialTimeSeries::new(timestamps, typed, name)
            .map_err(InvalidParameterError::new_err)?;
        Ok(Self { inner })
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
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
    // Lookup handle: carries the identity tuple, not the descriptive window
    // fields (those are known only for a key returned from add/list).
    inner: core_lib::KeyIdentity,
}

#[pymethods]
impl PyTimeSeriesKey {
    #[getter]
    fn owner_id(&self) -> i64 {
        self.inner.owner_id
    }

    #[getter]
    fn owner_category(&self) -> PyOwnerCategory {
        self.inner.owner_category.into()
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
    fn resolution(&self) -> Option<String> {
        self.inner.resolution.map(|p| p.to_iso8601())
    }

    #[getter]
    fn interval(&self) -> Option<String> {
        self.inner.interval.map(|p| p.to_iso8601())
    }

    #[getter]
    fn features<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        features_to_dict(py, &self.inner.features)
    }

    fn __repr__(&self) -> String {
        format!(
            "TimeSeriesKey(owner_id={:?}, owner_category={:?}, time_series_type={:?}, name={:?}, features={:?})",
            self.inner.owner_id,
            self.inner.owner_category.as_str(),
            self.inner.time_series_type.as_str(),
            self.inner.name,
            self.inner.features,
        )
    }
}

/// Extract a required key from a bulk-add item dict, with a uniform error.
fn required_item<'py, T: pyo3::conversion::FromPyObjectOwned<'py>>(
    dict: &Bound<'py, PyDict>,
    key: &str,
) -> PyResult<T> {
    let value = dict.get_item(key)?.ok_or_else(|| {
        InvalidParameterError::new_err(format!("bulk add item is missing '{key}'"))
    })?;
    match value.extract::<T>() {
        Ok(v) => Ok(v),
        Err(e) => {
            let e: PyErr = e.into();
            Err(InvalidParameterError::new_err(format!(
                "bulk add item key '{key}' is invalid: {e}"
            )))
        }
    }
}

/// Pull the core data off a Python time-series object (`SingleTimeSeries`,
/// `NonSequentialTimeSeries`, `Deterministic`, `Probabilistic`, or `Scenarios`).
fn extract_time_series_data(time_series: &Bound<'_, PyAny>) -> PyResult<core_lib::TimeSeriesData> {
    if let Ok(single) = time_series.extract::<PySingleTimeSeries>() {
        Ok(core_lib::TimeSeriesData::SingleTimeSeries(single.inner))
    } else if let Ok(ns) = time_series.extract::<PyNonSequentialTimeSeries>() {
        Ok(core_lib::TimeSeriesData::NonSequentialTimeSeries(ns.inner))
    } else if let Ok(det) = time_series.extract::<PyDeterministic>() {
        Ok(core_lib::TimeSeriesData::Deterministic(det.inner))
    } else if let Ok(prob) = time_series.extract::<PyProbabilistic>() {
        Ok(core_lib::TimeSeriesData::Probabilistic(prob.inner))
    } else if let Ok(scen) = time_series.extract::<PyScenarios>() {
        Ok(core_lib::TimeSeriesData::Scenarios(scen.inner))
    } else {
        Err(InvalidParameterError::new_err(
            "time_series must be SingleTimeSeries, NonSequentialTimeSeries, \
                 Deterministic, Probabilistic, or Scenarios",
        ))
    }
}

// ---- TimeSeriesStore ------------------------------------------------------

/// Wrap a reconstructed [`core_lib::TimeSeriesData`] in its matching Python class.
fn time_series_data_to_py(py: Python<'_>, data: core_lib::TimeSeriesData) -> PyResult<Py<PyAny>> {
    match data {
        core_lib::TimeSeriesData::SingleTimeSeries(s) => {
            Ok(Py::new(py, PySingleTimeSeries { inner: s })?.into_any())
        }
        core_lib::TimeSeriesData::NonSequentialTimeSeries(s) => {
            Ok(Py::new(py, PyNonSequentialTimeSeries { inner: s })?.into_any())
        }
        core_lib::TimeSeriesData::Deterministic(d) => {
            Ok(Py::new(py, PyDeterministic { inner: d })?.into_any())
        }
        core_lib::TimeSeriesData::Probabilistic(p) => {
            Ok(Py::new(py, PyProbabilistic { inner: p })?.into_any())
        }
        core_lib::TimeSeriesData::Scenarios(s) => {
            Ok(Py::new(py, PyScenarios { inner: s })?.into_any())
        }
    }
}

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
    #[pyo3(signature = (owner_id, owner_type, owner_category, time_series, features=None, units=None))]
    fn add_time_series(
        &mut self,
        owner_id: i64,
        owner_type: &str,
        owner_category: PyOwnerCategory,
        time_series: &Bound<'_, PyAny>,
        features: Option<&Bound<'_, PyDict>>,
        units: Option<String>,
    ) -> PyResult<PyTimeSeriesKey> {
        let features = features_from_dict(features)?;
        let data = extract_time_series_data(time_series)?;
        let key = self
            .inner
            .add_time_series(
                owner_id,
                owner_type,
                owner_category.into(),
                data,
                features,
                units,
            )
            .map_err(map_err)?;
        Ok(PyTimeSeriesKey {
            inner: key.identity().clone(),
        })
    }

    /// Add many time series in one call, committing the metadata catalog once
    /// for the whole batch. This is much faster than calling
    /// `add_time_series` in a loop, which pays one SQLite transaction per
    /// series.
    ///
    /// `items` is a list of dicts whose keys mirror `add_time_series`'s
    /// parameters: `owner_id`, `owner_type`, `owner_category`,
    /// `time_series`, and optionally `features` and `units`.
    ///
    /// All-or-nothing: if any item fails, the entire batch is rolled back.
    /// Returns the new keys in input order.
    fn add_time_series_bulk(
        &mut self,
        items: Vec<Bound<'_, PyDict>>,
    ) -> PyResult<Vec<PyTimeSeriesKey>> {
        let mut requests = Vec::with_capacity(items.len());
        for item in &items {
            let owner_id: i64 = required_item(item, "owner_id")?;
            let owner_type: String = required_item(item, "owner_type")?;
            let owner_category: PyOwnerCategory = required_item(item, "owner_category")?;
            let time_series = item.get_item("time_series")?.ok_or_else(|| {
                InvalidParameterError::new_err("bulk add item is missing 'time_series'")
            })?;
            let features = match item.get_item("features")? {
                Some(f) if !f.is_none() => {
                    let dict = f
                        .cast_into::<PyDict>()
                        .map_err(|_| InvalidParameterError::new_err("'features' must be a dict"))?;
                    features_from_dict(Some(&dict))?
                }
                _ => features_from_dict(None)?,
            };
            let units: Option<String> = match item.get_item("units")? {
                Some(u) if !u.is_none() => Some(u.extract()?),
                _ => None,
            };
            let data = extract_time_series_data(&time_series)?;
            requests.push(core_lib::AddRequest {
                owner_id,
                owner_type,
                owner_category: owner_category.into(),
                data,
                features,
                units,

                logical_type: None,
            });
        }
        let keys = self.inner.add_time_series_bulk(requests).map_err(map_err)?;
        Ok(keys
            .into_iter()
            .map(|k| PyTimeSeriesKey {
                inner: k.identity().clone(),
            })
            .collect())
    }

    /// Derive `DeterministicSingleTimeSeries` forecasts from the stored
    /// `SingleTimeSeries` associations (mirrors InfrastructureSystems.jl's
    /// `transform_single_time_series!`). Each `SingleTimeSeries` is re-described
    /// as a DST sharing the same underlying array; `count` is derived from each
    /// series' length. Returns the number of series transformed.
    fn transform_single_time_series(
        &mut self,
        horizon: Bound<'_, PyAny>,
        interval: Bound<'_, PyAny>,
    ) -> PyResult<usize> {
        let horizon = pyany_to_period(&horizon)?;
        let interval = pyany_to_period(&interval)?;
        self.inner
            .transform_single_time_series(horizon, interval, None, None)
            .map_err(map_err)
    }

    fn remove_time_series(&mut self, key: &PyTimeSeriesKey) -> PyResult<()> {
        self.inner.remove_time_series(&key.inner).map_err(map_err)
    }

    /// Remove every time series for the owner `(owner_id, owner_category)`, or
    /// every time series in the store when neither is given. Both must be
    /// supplied together or neither.
    #[pyo3(signature = (owner_id=None, owner_category=None))]
    fn clear_time_series(
        &mut self,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
    ) -> PyResult<usize> {
        let owner = match (owner_id, owner_category) {
            (Some(id), Some(cat)) => Some((id, cat.into())),
            (None, None) => None,
            _ => {
                return Err(InvalidParameterError::new_err(
                    "clear_time_series requires both owner_id and owner_category, or neither",
                ));
            }
        };
        self.inner.clear_time_series(owner).map_err(map_err)
    }

    /// Reassign every time series owned by `(old_owner, owner_category)` to
    /// `(new_owner, owner_category)`. Returns the number of associations moved.
    fn replace_owner(
        &mut self,
        old_owner: i64,
        new_owner: i64,
        owner_category: PyOwnerCategory,
    ) -> PyResult<usize> {
        self.inner
            .replace_owner(old_owner, new_owner, owner_category.into())
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
        time_series_data_to_py(py, data)
    }

    /// Read many full series at once, returning a list of typed objects in the
    /// same order as `keys`. Packed `SingleTimeSeries` are read in one
    /// decompress-once pass per dataset (the bulk counterpart to
    /// `get_time_series`); other types reuse the per-key path. No time-range
    /// slicing — each series is returned in full.
    fn bulk_read(&self, py: Python<'_>, keys: Vec<PyTimeSeriesKey>) -> PyResult<Vec<Py<PyAny>>> {
        let identities: Vec<&core_lib::KeyIdentity> = keys.iter().map(|k| &k.inner).collect();
        let datas = self.inner.bulk_read(&identities).map_err(map_err)?;
        datas
            .into_iter()
            .map(|d| time_series_data_to_py(py, d))
            .collect()
    }

    /// Return a list of metadata dicts matching the filter. Each dict has
    /// `owner_id`, `owner_type`, `time_series_type`, `name`, `length`,
    /// `resolution` (ISO 8601 duration string, e.g. `PT1H`), `features`, `units`.
    #[pyo3(signature = (
        owner_id=None, owner_category=None, owner_type=None, time_series_type=None,
        name=None, resolution=None, features=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn list_time_series<'py>(
        &self,
        py: Python<'py>,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
        owner_type: Option<String>,
        time_series_type: Option<PyTimeSeriesType>,
        name: Option<String>,
        resolution: Option<Bound<'_, PyAny>>,
        features: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let mut filter = core_lib::ListFilter::new();
        if let Some(id) = owner_id {
            filter = filter.owner_id(id);
        }
        if let Some(c) = owner_category {
            filter = filter.owner_category(c.into());
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
            filter = filter.resolution(pyany_to_period(&r)?);
        }
        if let Some(f) = features {
            filter = filter.features(features_from_dict(Some(f))?);
        }
        let metas = self.inner.list_time_series(filter).map_err(map_err)?;
        let mut out = Vec::with_capacity(metas.len());
        for m in &metas {
            let d = PyDict::new(py);
            d.set_item("owner_id", m.owner_id)?;
            d.set_item("owner_type", &m.owner_type)?;
            d.set_item("owner_category", m.owner_category.as_str())?;
            d.set_item("time_series_type", m.time_series_type.as_str())?;
            d.set_item("name", &m.name)?;
            d.set_item("data_hash", core_lib::hash::hash_hex(&m.data_hash))?;
            d.set_item("length", m.length)?;
            d.set_item("resolution", m.resolution.map(|p| p.to_iso8601()))?;
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

    /// Group time series by their underlying stored array. Returns one dict per
    /// unique content hash, each with `data_hash` (hex str) and `keys` (the list
    /// of `TimeSeriesKey`s that resolve to that array). Keys sharing one dict
    /// share one deduplicated array. Accepts the same filters as
    /// `list_time_series`. Wraps the core `list_keys_with_hash`.
    #[pyo3(signature = (
        owner_id=None, owner_category=None, owner_type=None, time_series_type=None,
        name=None, resolution=None, features=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn list_array_groups<'py>(
        &self,
        py: Python<'py>,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
        owner_type: Option<String>,
        time_series_type: Option<PyTimeSeriesType>,
        name: Option<String>,
        resolution: Option<Bound<'_, PyAny>>,
        features: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let mut filter = core_lib::ListFilter::new();
        if let Some(id) = owner_id {
            filter = filter.owner_id(id);
        }
        if let Some(c) = owner_category {
            filter = filter.owner_category(c.into());
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
            filter = filter.resolution(pyany_to_period(&r)?);
        }
        if let Some(f) = features {
            filter = filter.features(features_from_dict(Some(f))?);
        }
        let rows = self.inner.list_keys_with_hash(filter).map_err(map_err)?;
        let mut groups: BTreeMap<[u8; 32], Vec<Py<PyTimeSeriesKey>>> = BTreeMap::new();
        for (key, hash) in rows {
            let pk = Py::new(
                py,
                PyTimeSeriesKey {
                    inner: key.identity().clone(),
                },
            )?;
            groups.entry(hash).or_default().push(pk);
        }
        let mut out = Vec::with_capacity(groups.len());
        for (hash, keys) in groups {
            let d = PyDict::new(py);
            d.set_item("data_hash", core_lib::hash::hash_hex(&hash))?;
            d.set_item("keys", keys)?;
            out.push(d);
        }
        Ok(out)
    }

    fn get_time_series_keys(
        &self,
        owner_id: i64,
        owner_category: PyOwnerCategory,
    ) -> PyResult<Vec<PyTimeSeriesKey>> {
        Ok(self
            .inner
            .get_time_series_keys(owner_id, owner_category.into())
            .map_err(map_err)?
            .into_iter()
            .map(|k| PyTimeSeriesKey {
                inner: k.identity().clone(),
            })
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
    ) -> PyResult<Vec<String>> {
        let _ = py;
        Ok(self
            .inner
            .get_resolutions(time_series_type.map(Into::into))
            .map_err(map_err)?
            .into_iter()
            .map(|p| p.to_iso8601())
            .collect())
    }

    /// Return the store's forecast parameters as a dict with keys `horizon`,
    /// `interval` (ISO 8601 duration strings, e.g. `PT1H`), `count` (int), and
    /// `resolution` (ISO 8601 duration string). Each value is `None` when the
    /// store holds no forecasts.
    fn get_forecast_parameters<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let p = self
            .inner
            .get_forecast_parameters(None, None)
            .map_err(map_err)?;
        let d = PyDict::new(py);
        let iso = |v: Option<core_lib::Period>| v.map(|p| p.to_iso8601());
        d.set_item("horizon", iso(p.horizon))?;
        d.set_item("interval", iso(p.interval))?;
        d.set_item("count", p.count)?;
        d.set_item("resolution", iso(p.resolution))?;
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
        d.set_item("feature_sets_reclaimed", r.feature_sets_reclaimed)?;
        Ok(d)
    }

    fn verify_integrity(&self) -> PyResult<Vec<String>> {
        Ok(self.inner.verify_integrity().map_err(map_err)?.errors)
    }

    fn flush(&mut self) -> PyResult<()> {
        self.inner.flush().map_err(map_err)
    }
}

// ---- period helpers -------------------------------------------------------

/// Accept a period as either a `datetime.timedelta` (fixed span) or an ISO-8601
/// duration `str` (e.g. "PT1H", "P1M", "P1Y"); the latter is required for
/// calendar (irregular) periods.
fn pyany_to_period(v: &Bound<'_, PyAny>) -> PyResult<core_lib::Period> {
    if let Ok(s) = v.extract::<String>() {
        core_lib::Period::from_iso8601(&s)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
    } else if let Ok(d) = v.extract::<chrono::Duration>() {
        Ok(core_lib::Period::Fixed(d))
    } else {
        Err(pyo3::exceptions::PyTypeError::new_err(
            "period must be a datetime.timedelta or an ISO-8601 duration string",
        ))
    }
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
