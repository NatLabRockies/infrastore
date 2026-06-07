//! PyO3 bindings for `time-series-store`.
//!
//! Exposed module name: `time_series_store`. Top-level surface:
//!
//! ```python
//! from time_series_store import (
//!     TimeSeriesStore, SingleTimeSeries, TimeSeriesKey,
//!     TimeSeriesType, OwnerCategory,
//!     TimeSeriesError, NotFoundError, DuplicateTimeSeriesError, InvalidParameterError,
//!     IntegrityError, ReadOnlyStoreError,
//! )
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, TimeZone, Utc};
use ndarray::ArrayD;
use numpy::{PyArrayDyn, PyReadonlyArrayDyn};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBool, PyDict, PyDelta, PyFloat, PyInt, PyString};
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

// ---- Enums ----------------------------------------------------------------

#[pyclass(eq, eq_int, name = "TimeSeriesType", module = "time_series_store", from_py_object)]
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

#[pyclass(eq, eq_int, name = "OwnerCategory", module = "time_series_store", from_py_object)]
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

// ---- SingleTimeSeries -----------------------------------------------------

#[pyclass(name = "SingleTimeSeries", module = "time_series_store", from_py_object)]
#[derive(Clone)]
pub struct PySingleTimeSeries {
    inner: core_lib::SingleTimeSeries,
}

#[pymethods]
impl PySingleTimeSeries {
    #[new]
    fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: Bound<'_, PyDelta>,
        data: PyReadonlyArrayDyn<'_, f64>,
    ) -> PyResult<Self> {
        let resolution = pydelta_to_chrono(&resolution)?;
        let arr = data.as_array();
        let shape: Vec<usize> = arr.shape().to_vec();
        let values: Vec<f64> = arr.iter().copied().collect();
        let typed = core_lib::TypedArray::from_f64(shape, &values);
        Ok(Self {
            inner: core_lib::SingleTimeSeries::new(initial_timestamp, resolution, typed),
        })
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
    fn data<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyArrayDyn<f64>>> {
        let shape = self.inner.data.shape.clone();
        let values = self.inner.data.to_f64_vec().map_err(InvalidParameterError::new_err)?;
        let arr = ArrayD::from_shape_vec(shape, values)
            .map_err(|e| InvalidParameterError::new_err(e.to_string()))?;
        Ok(numpy::PyArray::from_array(py, &arr))
    }

    fn __repr__(&self) -> String {
        format!(
            "SingleTimeSeries(initial_timestamp={}, length={}, resolution={}s, shape={:?})",
            self.inner.initial_timestamp,
            self.inner.length,
            self.inner.resolution.num_seconds(),
            self.inner.data.shape,
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
    /// otherwise a NetCDF file is created at `path` and a sidecar SQLite file
    /// at `<path>.sqlite` holds metadata.
    #[classmethod]
    #[pyo3(signature = (path=None, in_memory=false))]
    fn create(
        _cls: &Bound<'_, pyo3::types::PyType>,
        path: Option<PathBuf>,
        in_memory: bool,
    ) -> PyResult<Self> {
        let store = core_lib::create_store(path.as_deref(), in_memory).map_err(map_err)?;
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

    /// Add a time series.
    ///
    /// `features` is a `dict[str, int|float|bool|str]`. `units` and
    /// `scaling_factor_multiplier` are optional strings.
    #[pyo3(signature = (
        owner_uuid, owner_type, owner_category, name, time_series,
        features=None, units=None, scaling_factor_multiplier=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn add_time_series(
        &mut self,
        owner_uuid: &str,
        owner_type: &str,
        owner_category: PyOwnerCategory,
        name: &str,
        time_series: PySingleTimeSeries,
        features: Option<&Bound<'_, PyDict>>,
        units: Option<String>,
        scaling_factor_multiplier: Option<String>,
    ) -> PyResult<PyTimeSeriesKey> {
        let features = features_from_dict(features)?;
        let key = self
            .inner
            .add_time_series(
                owner_uuid,
                owner_type,
                owner_category.into(),
                name,
                core_lib::TimeSeriesData::SingleTimeSeries(time_series.inner),
                features,
                units,
                scaling_factor_multiplier,
            )
            .map_err(map_err)?;
        Ok(PyTimeSeriesKey { inner: key })
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

    /// Fetch a SingleTimeSeries by key. `time_range`, if given, is a tuple of
    /// `(start: datetime, end: datetime)` with end exclusive.
    #[pyo3(signature = (key, time_range=None))]
    fn get_time_series(
        &self,
        key: &PyTimeSeriesKey,
        time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> PyResult<PySingleTimeSeries> {
        let data = self
            .inner
            .get_time_series(&key.inner, time_range)
            .map_err(map_err)?;
        let core_lib::TimeSeriesData::SingleTimeSeries(s) = data;
        Ok(PySingleTimeSeries { inner: s })
    }

    /// Return a list of metadata dicts matching the filter. Each dict has
    /// `owner_uuid`, `owner_type`, `time_series_type`, `name`, `length`,
    /// `resolution_seconds`, `features`, `units`, `scaling_factor_multiplier`.
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
            d.set_item(
                "data_hash",
                core_lib::hash::hash_hex(&m.data_hash),
            )?;
            d.set_item("length", m.length)?;
            d.set_item(
                "resolution_seconds",
                m.resolution.map(|r| r.num_seconds()),
            )?;
            d.set_item("features", features_to_dict(py, &m.features)?)?;
            d.set_item("units", m.units.clone())?;
            d.set_item(
                "scaling_factor_multiplier",
                m.scaling_factor_multiplier.clone(),
            )?;
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

// ---- Module init ----------------------------------------------------------

#[pymodule]
fn time_series_store(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyStore>()?;
    m.add_class::<PySingleTimeSeries>()?;
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
    Ok(())
}
