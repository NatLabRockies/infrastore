//! PyO3 bindings for `infrastore`.
//!
//! Exposed module name: `infrastore`. Top-level surface:
//!
//! ```python
//! from infrastore import (
//!     Store, SingleTimeSeries, NonSequentialTimeSeries,
//!     TimeSeriesType, OwnerCategory,
//!     SupplementalAttributeAssociation, ParentChildAssociation,
//!     TimeSeriesError, NotFoundError, OwnerMismatchError, DuplicateTimeSeriesError,
//!     InvalidParameterError,
//!     IntegrityError, ReadOnlyStoreError, IoError, ConnectionError,
//!     IncompatibleFormatError, IncompatibleForecastError, StorageError,
//!     DuplicateAssociationError,
//!     DuplicateAssociationIdError,
//! )
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, TimeZone, Utc};
use infrastore_core as core_lib;
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{
    PyAny, PyBool, PyBytes, PyDateTime, PyDict, PyFloat, PyInt, PyList, PyString, PyTzInfo,
};

// ---- Exceptions -----------------------------------------------------------

create_exception!(infrastore, TimeSeriesError, PyException);
create_exception!(infrastore, NotFoundError, TimeSeriesError);
create_exception!(infrastore, OwnerMismatchError, TimeSeriesError);
create_exception!(infrastore, DuplicateTimeSeriesError, TimeSeriesError);
create_exception!(infrastore, DuplicateAssociationError, TimeSeriesError);
create_exception!(infrastore, DuplicateAssociationIdError, TimeSeriesError);
create_exception!(infrastore, InvalidParameterError, TimeSeriesError);
create_exception!(infrastore, IntegrityError, TimeSeriesError);
create_exception!(infrastore, ReadOnlyStoreError, TimeSeriesError);
create_exception!(infrastore, IoError, TimeSeriesError);
create_exception!(infrastore, ConnectionError, TimeSeriesError);
create_exception!(infrastore, IncompatibleFormatError, TimeSeriesError);
create_exception!(infrastore, IncompatibleForecastError, TimeSeriesError);
create_exception!(infrastore, StorageError, TimeSeriesError);
create_exception!(infrastore, StoreExistsError, TimeSeriesError);
create_exception!(infrastore, MismatchedArtifactError, TimeSeriesError);
create_exception!(infrastore, CatalogMigrationRequiredError, TimeSeriesError);
create_exception!(infrastore, CatalogTooNewError, TimeSeriesError);

fn map_err(e: core_lib::TimeSeriesError) -> PyErr {
    use core_lib::TimeSeriesError as E;
    match e {
        E::NotFound => NotFoundError::new_err("time series not found"),
        // Distinct from `NotFoundError`: the row is there, and it is the
        // caller's belief about who owns it that is stale.
        ref e @ E::OwnerMismatch { .. } => OwnerMismatchError::new_err(e.to_string()),
        E::DuplicateTimeSeries => {
            DuplicateTimeSeriesError::new_err("a time series with that key already exists")
        }
        E::DuplicateAssociation(m) => DuplicateAssociationError::new_err(m),
        ref e @ E::DuplicateAssociationId(_) => DuplicateAssociationIdError::new_err(e.to_string()),
        E::InvalidParameter(m) => InvalidParameterError::new_err(m),
        E::IntegrityError(m) => IntegrityError::new_err(m),
        E::ReadOnlyStore => ReadOnlyStoreError::new_err("store is read-only"),
        E::ConnectionError(m) => ConnectionError::new_err(m),
        E::IncompatibleForecast => IncompatibleForecastError::new_err(
            "forecast parameters are incompatible with existing forecasts",
        ),
        ref e @ E::IncompatibleFormat { .. } => IncompatibleFormatError::new_err(e.to_string()),
        ref e @ E::StoreExists { .. } => StoreExistsError::new_err(e.to_string()),
        ref e @ E::MismatchedArtifact { .. } => MismatchedArtifactError::new_err(e.to_string()),
        ref e @ E::CatalogMigrationRequired { .. } => {
            CatalogMigrationRequiredError::new_err(e.to_string())
        }
        ref e @ E::CatalogTooNew { .. } => CatalogTooNewError::new_err(e.to_string()),
        E::Io(e) => IoError::new_err(e.to_string()),
        E::Sqlite(e) => StorageError::new_err(format!("sqlite: {e}")),
        E::Serde(e) => StorageError::new_err(format!("serde: {e}")),
        // `TimeSeriesError` is non_exhaustive; map future variants to the base
        // exception rather than failing to compile against a newer core.
        e => TimeSeriesError::new_err(e.to_string()),
    }
}

/// A timestamp taken from Python: the instant it names, plus how it was spelled.
///
/// PyO3's own `DateTime<Utc>` extraction requires the `tzinfo` to *be*
/// `datetime.timezone.utc`, and its `DateTime<FixedOffset>` extraction asks the
/// `tzinfo` for `utcoffset(None)`, which every non-fixed zone answers with
/// `None`. Between them they reject instants that are perfectly well defined:
/// `ZoneInfo("UTC")` is not `timezone.utc`, and a correct
/// `ZoneInfo("America/Denver")` timestamp is not a fixed offset. The failure
/// arrived as a bare `TypeError`/`ValueError` from outside this package's
/// exception hierarchy, naming pyo3's expectation rather than the store's rule.
///
/// The conversion belongs to Python, which is the only party that knows what a
/// named zone's offset is at a given instant: an aware datetime is converted
/// with `astimezone(timezone.utc)` and extracted from there.
///
/// A **naive** datetime is now accepted rather than refused, and recorded as
/// [`TimeReference::Zoneless`] — a wall clock naming no instant, held as if UTC.
/// Accepting it is only defensible because the read path can hand the same
/// spelling back ([`spell_instant`]): naive and aware datetimes are not equal in
/// Python and are not even comparable, so a store that took one and returned the
/// other would be worse than one that refused.
///
/// The conversion for a naive value never goes through `astimezone`, which
/// assumes *system local time* — the same script would otherwise write a
/// different instant on a laptop in Denver than in CI on UTC. The fields are
/// read as they stand, which is `replace(tzinfo=utc)` semantics.
#[derive(Clone, Debug)]
struct PyInstant {
    instant: DateTime<Utc>,
    /// The spelling this datetime arrived in. Always concrete: naive is
    /// `Zoneless`, and every aware value falls into one of the three zoned
    /// spellings.
    reference: core_lib::TimeReference,
}

impl PyInstant {
    fn is_zoneless(&self) -> bool {
        self.reference.is_zoneless()
    }
}

impl<'py> FromPyObject<'_, 'py> for PyInstant {
    type Error = PyErr;

    fn extract(obj: pyo3::Borrowed<'_, 'py, PyAny>) -> PyResult<Self> {
        let dt = obj.cast::<PyDateTime>().map_err(|_| {
            InvalidParameterError::new_err(format!(
                "expected a datetime, got {}",
                obj.get_type()
                    .name()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|_| "?".into())
            ))
        })?;
        let py = obj.py();
        let utc = PyTzInfo::utc(py)?;
        // `utcoffset()` is None for a naive datetime *and* for an aware one
        // whose tzinfo declines to place it. The two are the same thing here:
        // neither names an instant, so both are read as wall clocks.
        let offset = dt.call_method0("utcoffset")?;
        if offset.is_none() {
            // Not `astimezone`: that would apply the machine's local zone. The
            // wall clock is taken as it stands, which is what `Zoneless` means.
            let kwargs = PyDict::new(py);
            kwargs.set_item("tzinfo", utc)?;
            let as_utc = dt.call_method("replace", (), Some(&kwargs))?;
            return Ok(PyInstant {
                instant: as_utc.extract::<DateTime<Utc>>()?,
                reference: core_lib::TimeReference::Zoneless,
            });
        }
        let tzinfo = dt.getattr("tzinfo")?;
        let reference = infer_reference(&tzinfo, &offset)?;
        // Already `datetime.timezone.utc`: the wall clock is the instant, so
        // skip the round trip through Python's conversion.
        //
        // Identity, not `==`. A `tzinfo` decides its own equality, so an object
        // whose `__eq__` claims UTC while its `utcoffset` says otherwise would
        // take this branch and be read at its wall clock -- a silently wrong
        // instant, in the one place whose whole job is to pin one down. The
        // singleton cannot lie about its own offset, and it is what
        // `timezone(timedelta(0))` returns, so this still covers the common
        // case; every other zone, UTC-equivalent or not, goes through
        // `astimezone`, which asks `utcoffset` rather than taking its word.
        let in_utc = if tzinfo.is(utc) {
            dt.as_any().clone()
        } else {
            dt.call_method1("astimezone", (&utc,))?
        };
        Ok(PyInstant {
            instant: in_utc.extract::<DateTime<Utc>>()?,
            reference,
        })
    }
}

/// Which spelling an *aware* datetime's `tzinfo` records.
///
/// The `key` case is tested first on purpose: `ZoneInfo("UTC")` records
/// `Zone("UTC")`, not `Utc`. The two render identically forever, so the
/// distinction shows up only in what the catalog reports back — which is the
/// point of recording a spelling at all.
fn infer_reference(
    tzinfo: &Bound<'_, PyAny>,
    offset: &Bound<'_, PyAny>,
) -> PyResult<core_lib::TimeReference> {
    // `zoneinfo.ZoneInfo` exposes the IANA name as `key`; so does `pytz`'s
    // `zone`-carrying tzinfo under a different name, which is deliberately not
    // probed -- one attribute, one contract.
    if let Ok(key) = tzinfo.getattr("key")
        && let Ok(name) = key.extract::<String>()
    {
        let zone = core_lib::TimeReference::Zone(name);
        // Shape only. A tzinfo that carries a `key` the core cannot spell (an
        // exotic custom class) falls back to its offset rather than failing the
        // write.
        if zone.validate().is_ok() {
            return Ok(zone);
        }
    }
    if tzinfo.is(PyTzInfo::utc(tzinfo.py())?) {
        return Ok(core_lib::TimeReference::Utc);
    }
    // Judged as the float Python hands over, *before* any narrowing. `datetime`
    // has allowed sub-minute offsets since 3.7 and carries them to the
    // microsecond, so `60.5` truncated to `60` first would satisfy the
    // whole-minutes check below and be recorded as `+00:01` -- storing the
    // instant correctly while moving the wall clock 500 ms, silently, which is
    // the one failure this whole feature exists to prevent.
    let seconds = offset.call_method0("total_seconds")?.extract::<f64>()?;
    if !seconds.is_finite() || seconds.fract() != 0.0 || seconds % 60.0 != 0.0 {
        return Err(InvalidParameterError::new_err(format!(
            "the timestamp's UTC offset is {seconds} seconds, which is not a whole number of \
             minutes; the store records an offset in minutes, so this spelling cannot be \
             stored faithfully"
        )));
    }
    // Python bounds an offset to strictly within a day, so this always fits;
    // the guard is here so a future `datetime` that did not could never reach
    // the cast.
    let minutes = seconds / 60.0;
    if minutes.abs() >= 24.0 * 60.0 {
        return Err(InvalidParameterError::new_err(format!(
            "the timestamp's UTC offset is {seconds} seconds, which is not a real UTC offset; \
             it must be strictly within a day of UTC"
        )));
    }
    Ok(core_lib::TimeReference::FixedOffset(minutes as i32))
}

/// Render `instant` in the spelling `reference` names.
///
/// The read-side inverse of [`PyInstant`], and the reason accepting a naive
/// datetime is defensible at all. `None` and `Utc` both give an aware UTC
/// datetime — an unspecified reference is not a claim, but it is also not a
/// reason to hand back a naive value the caller cannot compare against one.
fn spell_instant<'py>(
    py: Python<'py>,
    instant: DateTime<Utc>,
    reference: Option<&core_lib::TimeReference>,
) -> PyResult<Bound<'py, PyAny>> {
    use pyo3::IntoPyObject;
    let utc_obj = instant.into_pyobject(py)?.into_any();
    match reference {
        None | Some(core_lib::TimeReference::Utc) => Ok(utc_obj),
        Some(core_lib::TimeReference::Zoneless) => {
            // A wall clock: the stored instant's UTC fields, unlabelled.
            let kwargs = PyDict::new(py);
            kwargs.set_item("tzinfo", py.None())?;
            Ok(utc_obj.call_method("replace", (), Some(&kwargs))?)
        }
        Some(core_lib::TimeReference::FixedOffset(minutes)) => {
            let offset = chrono::FixedOffset::east_opt(minutes * 60).ok_or_else(|| {
                InvalidParameterError::new_err(format!("unrepresentable UTC offset {minutes}"))
            })?;
            Ok(instant.with_timezone(&offset).into_pyobject(py)?.into_any())
        }
        Some(core_lib::TimeReference::Zone(name)) => {
            // instant -> local is total and single-valued, so this direction
            // never has to choose between two candidates. It needs a tz
            // database, which is why it lives here and not in the core.
            match zone_info(py, name) {
                Ok(zone) => Ok(utc_obj.call_method1("astimezone", (zone,))?),
                Err(_) => {
                    // The instant is still right; only the label cannot be
                    // resolved by *this* interpreter's database. Reporting UTC
                    // beats failing a read of data that is perfectly intact.
                    warn_unknown_zone(py, name)?;
                    Ok(utc_obj)
                }
            }
        }
    }
}

/// How a reference reads in a `__repr__`. `"unspecified"` rather than `None` so
/// the field always says something, matching the other descriptors' spelling.
fn reference_label(reference: Option<&core_lib::TimeReference>) -> String {
    reference
        .map(core_lib::TimeReference::as_storage_string)
        .unwrap_or_else(|| "unspecified".to_string())
}

/// `spell_instant` over a vector, for the timestamp axes.
fn spell_instants<'py>(
    py: Python<'py>,
    instants: &[DateTime<Utc>],
    reference: Option<&core_lib::TimeReference>,
) -> PyResult<Vec<Bound<'py, PyAny>>> {
    instants
        .iter()
        .map(|t| spell_instant(py, *t, reference))
        .collect()
}

/// `zoneinfo.ZoneInfo(name)`, or the interpreter's own error if it has no such
/// zone.
fn zone_info<'py>(py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyAny>> {
    py.import("zoneinfo")?.getattr("ZoneInfo")?.call1((name,))
}

/// Warn that this interpreter's tz database does not know `name`.
///
/// A warning, never an error: the store does not gate on zone existence (see
/// `TimeReference::validate`), because doing so would couple legitimate data to
/// the release cadence of whichever database happened to be asked. Every layer
/// that *has* a database audits instead.
fn warn_unknown_zone(py: Python<'_>, name: &str) -> PyResult<()> {
    let message = format!(
        "time_reference names the IANA zone {name:?}, which this interpreter's tz database \
         does not have; the instants are stored either way, but rendering them in that zone \
         needs a database that knows it (try installing or updating the tzdata package)"
    );
    let message = std::ffi::CString::new(message)
        .map_err(|_| InvalidParameterError::new_err("zone name contains an interior NUL"))?;
    let category = py.get_type::<pyo3::exceptions::PyUserWarning>();
    PyErr::warn(py, &category, &message, 2)
}

/// The one spelling a vector of timestamps carries.
///
/// A series has one reference, so its timestamps have to agree on one. Mixing
/// naive and aware values in a single vector is already an error in Python the
/// moment anything compares them, so this reports it at the door instead.
fn vector_reference(timestamps: &[PyInstant]) -> PyResult<Option<core_lib::TimeReference>> {
    let Some(first) = timestamps.first() else {
        return Ok(None);
    };
    for (index, t) in timestamps.iter().enumerate() {
        if t.reference != first.reference {
            return Err(InvalidParameterError::new_err(format!(
                "timestamps disagree about how they are spelled: element 0 is {} but element \
                 {index} is {}; one series records one spelling",
                first.reference, t.reference
            )));
        }
    }
    Ok(Some(first.reference.clone()))
}

/// [`PyInstant`] over a sequence, for the timestamp vectors.
fn instants_to_utc(v: &[PyInstant]) -> Vec<DateTime<Utc>> {
    v.iter().map(|i| i.instant).collect()
}

/// [`PyInstant`] over the optional `(start, end)` pairs, carrying the spelling
/// through so the core can apply the bound rule.
fn range_to_core(r: Option<(PyInstant, PyInstant)>) -> PyResult<Option<core_lib::TimeRange>> {
    let Some((a, b)) = r else { return Ok(None) };
    if a.is_zoneless() != b.is_zoneless() {
        return Err(InvalidParameterError::new_err(
            "the two time_range bounds are spelled differently: one is timezone-aware and the \
             other is naive. A range is one request; spell both bounds the way the series is.",
        ));
    }
    Ok(Some(core_lib::TimeRange::spelled(
        a.instant,
        b.instant,
        a.is_zoneless(),
    )))
}

/// Refuse a point-read instant whose spelling the reader's axis cannot answer.
///
/// The ranged reads get this for free: they hand a `TimeRange` to the core,
/// which applies the rule once. A point read passed only the instant, so the
/// check was simply skipped -- a naive wall clock could query an
/// instant-bearing reader (and an aware datetime a zoneless one), be
/// reinterpreted as UTC, and return a *row* rather than the category-mismatch
/// error the same mismatch earns on a ranged read.
///
/// A point is a degenerate range, so it goes through the very same check rather
/// than a second copy of the rule, and reports it in the same words.
fn check_point_spelling(
    when: &PyInstant,
    axis: Option<&core_lib::TimeReference>,
    what: &str,
) -> PyResult<()> {
    core_lib::TimeRange::spelled(when.instant, when.instant, when.is_zoneless())
        .check_against(axis, what)
        .map_err(map_err)
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

/// Translate the Python-facing `catalog` argument into a core
/// [`CatalogMode`](core_lib::CatalogMode).
///
/// `None` means "whatever matches the backend", which is what these constructors
/// did before the argument existed: an in-memory store has no file for a catalog
/// to sit beside, and an on-disk one has always kept its catalog in
/// `<path>.sqlite`. Passing it explicitly is what unlocks the new combination —
/// arrays in an HDF5 file, catalog in RAM.
fn parse_catalog(catalog: Option<&str>, in_memory: bool) -> PyResult<core_lib::CatalogMode> {
    match catalog {
        None if in_memory => Ok(core_lib::CatalogMode::InMemory),
        None => Ok(core_lib::CatalogMode::Attached),
        Some("attached") => Ok(core_lib::CatalogMode::Attached),
        Some("memory") => Ok(core_lib::CatalogMode::InMemory),
        Some(other) => Err(InvalidParameterError::new_err(format!(
            "unknown catalog '{other}', expected 'attached' or 'memory'"
        ))),
    }
}

/// The `catalog` spelling for a core [`CatalogMode`](core_lib::CatalogMode).
fn catalog_name(catalog: core_lib::CatalogMode) -> &'static str {
    match catalog {
        core_lib::CatalogMode::Attached => "attached",
        core_lib::CatalogMode::InMemory => "memory",
    }
}

// ---- Enums ----------------------------------------------------------------

#[pyclass(
    eq,
    eq_int,
    name = "TimeSeriesType",
    module = "infrastore",
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
    module = "infrastore",
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

/// Parse an `element_type` in its canonical string form.
fn parse_element_type(s: &str) -> PyResult<core_lib::ElementType> {
    s.parse::<core_lib::ElementType>()
        .map_err(|e| InvalidParameterError::new_err(e.to_string()))
}

/// Parse an explicit `time_reference` argument, auditing a zone name against
/// this interpreter's tz database.
///
/// The audit is a warning, never a gate: the store does not check zone
/// existence, because gating would refuse legitimate data whenever IANA's
/// database moves ahead of whichever copy happened to be asked. A typo still
/// gets said out loud at the moment it is written, which is the only moment a
/// caller can fix it cheaply.
fn parse_time_reference(py: Python<'_>, spelling: &str) -> PyResult<core_lib::TimeReference> {
    let reference = core_lib::TimeReference::parse(spelling).map_err(map_err)?;
    if let core_lib::TimeReference::Zone(name) = &reference
        && zone_info(py, name).is_err()
    {
        warn_unknown_zone(py, name)?;
    }
    Ok(reference)
}

/// `None` (the argument was omitted) means *unspecified*, which is deliberately
/// not the same as `"natural_units"`. An unrecognized spelling raises rather
/// than degrading to unspecified: silently dropping a declared basis would make
/// per-unit values indistinguishable from values whose basis nobody stated.
fn parse_unit_system(s: Option<&str>) -> PyResult<Option<core_lib::UnitSystem>> {
    match s {
        None => Ok(None),
        Some(s) => core_lib::UnitSystem::parse(s).map(Some).ok_or_else(|| {
            InvalidParameterError::new_err(format!(
                "invalid unit_system {s:?}; expected 'natural_units' or 'component_base'"
            ))
        }),
    }
}

fn dtype_from_numpy_name(name: &str) -> PyResult<core_lib::Dtype> {
    Ok(match name {
        "float64" => core_lib::Dtype::F64,
        "float32" => core_lib::Dtype::F32,
        "int64" => core_lib::Dtype::I64,
        "int32" => core_lib::Dtype::I32,
        "int16" => core_lib::Dtype::I16,
        "int8" => core_lib::Dtype::I8,
        "uint64" => core_lib::Dtype::U64,
        "uint32" => core_lib::Dtype::U32,
        "uint16" => core_lib::Dtype::U16,
        "uint8" => core_lib::Dtype::U8,
        "bool" => core_lib::Dtype::Bool,
        other => {
            return Err(InvalidParameterError::new_err(format!(
                "unsupported numpy dtype '{other}' (expected float64/float32/\
                 int64/int32/int16/int8/uint64/uint32/uint16/uint8/bool)"
            )));
        }
    })
}

/// The numpy type descriptor for a dtype, with byte order stated explicitly:
/// `"<f8"`, `"<i4"`, and `"|u1"` for the single-byte types where order does not
/// apply.
///
/// Spelled this way rather than as a plain name (`"float64"`), which numpy
/// resolves to the *host's* byte order. A `TypedArray`'s bytes are always
/// little-endian — that is the documented on-disk layout — so decoding them
/// under the native order would read them backwards on a big-endian host.
fn numpy_le_descr(dtype: core_lib::Dtype) -> &'static str {
    match dtype {
        core_lib::Dtype::F64 => "<f8",
        core_lib::Dtype::F32 => "<f4",
        core_lib::Dtype::I64 => "<i8",
        core_lib::Dtype::I32 => "<i4",
        core_lib::Dtype::I16 => "<i2",
        core_lib::Dtype::I8 => "|i1",
        core_lib::Dtype::U64 => "<u8",
        core_lib::Dtype::U32 => "<u4",
        core_lib::Dtype::U16 => "<u2",
        core_lib::Dtype::U8 => "|u1",
        core_lib::Dtype::Bool => "|b1",
    }
}

/// Build a [`TypedArray`] from any numpy array: dtype from `.dtype.name`, shape
/// from `.shape`, and little-endian C-order (row-major) bytes.
///
/// The array is normalized to little-endian first. `.dtype.name` drops byte
/// order (`np.dtype(">f8").name == "float64"`) while `.tobytes()` preserves it,
/// so without the conversion a big-endian array's bytes would be stored under a
/// little-endian label and read back byte-reversed — silently, since every value
/// is still a legal one. Converting rather than rejecting matches what this
/// function already does about memory layout, where `.tobytes()` re-orders a
/// non-contiguous array instead of refusing it: the caller's values are right,
/// only their representation differs from the store's. `copy=False` makes this a
/// no-op for an array that is already little-endian, which is every array on a
/// little-endian host.
fn typed_array_from_numpy(data: &Bound<'_, PyAny>) -> PyResult<core_lib::TypedArray> {
    let shape: Vec<usize> = data.getattr("shape")?.extract()?;
    let dtype_obj = data.getattr("dtype")?;
    let dtype_name: String = dtype_obj.getattr("name")?.extract()?;
    let dtype = dtype_from_numpy_name(&dtype_name)?;
    let little_endian = dtype_obj.call_method1("newbyteorder", ("<",))?;
    let kwargs = PyDict::new(data.py());
    kwargs.set_item("copy", false)?;
    let normalized = data.call_method("astype", (little_endian,), Some(&kwargs))?;
    let bytes: Vec<u8> = normalized.call_method0("tobytes")?.extract()?;
    core_lib::TypedArray::new(dtype, shape, bytes).map_err(InvalidParameterError::new_err)
}

/// Reconstruct a numpy array (owned, writable) from a [`TypedArray`].
fn numpy_from_typed<'py>(
    py: Python<'py>,
    arr: &core_lib::TypedArray,
) -> PyResult<Bound<'py, PyAny>> {
    let np = py.import("numpy")?;
    let buf = PyBytes::new(py, &arr.bytes);
    let flat = np.call_method1("frombuffer", (buf, numpy_le_descr(arr.dtype)))?;
    let shaped = flat.call_method1("reshape", (arr.shape.clone(),))?;
    // frombuffer is read-only; hand back a writable copy.
    shaped.call_method0("copy")
}

// ---- Deterministic --------------------------------------------------------

#[pyclass(name = "Deterministic", module = "infrastore", from_py_object)]
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
        initial_timestamp: PyInstant,
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
            initial_timestamp.instant,
            resolution,
            horizon,
            interval,
            count,
            typed,
            name,
        )
        .map_err(InvalidParameterError::new_err)?;
        // The spelling is inferred from the timestamp the caller handed us, not
        // asked for separately: the intent is in the object, and it is erased
        // the moment the instant reaches the core.
        let mut inner = inner;
        inner.time_reference = Some(initial_timestamp.reference);
        Ok(Self { inner })
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    /// The first window's timestamp, spelled the way it was written.
    #[getter]
    fn initial_timestamp<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        spell_instant(
            py,
            self.inner.initial_timestamp,
            self.inner.time_reference.as_ref(),
        )
    }

    /// How this series' timestamps were spelled: `"utc"`, `"zoneless"`, a fixed
    /// offset (`"-07:00"`), an IANA zone name, or `None` for unspecified.
    #[getter]
    fn time_reference(&self) -> Option<String> {
        self.inner
            .time_reference
            .as_ref()
            .map(core_lib::TimeReference::as_storage_string)
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

    /// Value equality: all fields including the data array (bitwise).
    fn __eq__(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    /// Number of forecast windows (`count`).
    fn __len__(&self) -> usize {
        self.inner.count
    }

    fn __repr__(&self) -> String {
        format!(
            "Deterministic(name={:?}, initial_timestamp={}, count={}, horizon={}, interval={}, resolution={}, shape={:?}, time_reference={})",
            self.inner.name,
            self.inner.initial_timestamp,
            self.inner.count,
            self.inner.horizon.to_iso8601(),
            self.inner.interval.to_iso8601(),
            self.inner.resolution.to_iso8601(),
            self.inner.data.shape,
            reference_label(self.inner.time_reference.as_ref()),
        )
    }
}

// ---- Probabilistic --------------------------------------------------------

#[pyclass(name = "Probabilistic", module = "infrastore", from_py_object)]
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
        initial_timestamp: PyInstant,
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
            initial_timestamp.instant,
            resolution,
            horizon,
            interval,
            count,
            percentiles,
            typed,
            name,
        )
        .map_err(InvalidParameterError::new_err)?;
        // The spelling is inferred from the timestamp the caller handed us, not
        // asked for separately: the intent is in the object, and it is erased
        // the moment the instant reaches the core.
        let mut inner = inner;
        inner.time_reference = Some(initial_timestamp.reference);
        Ok(Self { inner })
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    /// The first window's timestamp, spelled the way it was written.
    #[getter]
    fn initial_timestamp<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        spell_instant(
            py,
            self.inner.initial_timestamp,
            self.inner.time_reference.as_ref(),
        )
    }

    /// How this series' timestamps were spelled: `"utc"`, `"zoneless"`, a fixed
    /// offset (`"-07:00"`), an IANA zone name, or `None` for unspecified.
    #[getter]
    fn time_reference(&self) -> Option<String> {
        self.inner
            .time_reference
            .as_ref()
            .map(core_lib::TimeReference::as_storage_string)
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

    /// Value equality: all fields including the data array (bitwise).
    fn __eq__(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    /// Number of forecast windows (`count`).
    fn __len__(&self) -> usize {
        self.inner.count
    }

    fn __repr__(&self) -> String {
        format!(
            "Probabilistic(name={:?}, initial_timestamp={}, count={}, horizon={}, interval={}, resolution={}, percentiles={:?}, shape={:?}, time_reference={})",
            self.inner.name,
            self.inner.initial_timestamp,
            self.inner.count,
            self.inner.horizon.to_iso8601(),
            self.inner.interval.to_iso8601(),
            self.inner.resolution.to_iso8601(),
            self.inner.percentiles,
            self.inner.data.shape,
            reference_label(self.inner.time_reference.as_ref()),
        )
    }
}

// ---- Scenarios ------------------------------------------------------------

#[pyclass(name = "Scenarios", module = "infrastore", from_py_object)]
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
        initial_timestamp: PyInstant,
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
            initial_timestamp.instant,
            resolution,
            horizon,
            interval,
            count,
            scenario_count,
            typed,
            name,
        )
        .map_err(InvalidParameterError::new_err)?;
        // The spelling is inferred from the timestamp the caller handed us, not
        // asked for separately: the intent is in the object, and it is erased
        // the moment the instant reaches the core.
        let mut inner = inner;
        inner.time_reference = Some(initial_timestamp.reference);
        Ok(Self { inner })
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    /// The first window's timestamp, spelled the way it was written.
    #[getter]
    fn initial_timestamp<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        spell_instant(
            py,
            self.inner.initial_timestamp,
            self.inner.time_reference.as_ref(),
        )
    }

    /// How this series' timestamps were spelled: `"utc"`, `"zoneless"`, a fixed
    /// offset (`"-07:00"`), an IANA zone name, or `None` for unspecified.
    #[getter]
    fn time_reference(&self) -> Option<String> {
        self.inner
            .time_reference
            .as_ref()
            .map(core_lib::TimeReference::as_storage_string)
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

    /// Value equality: all fields including the data array (bitwise).
    fn __eq__(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    /// Number of forecast windows (`count`).
    fn __len__(&self) -> usize {
        self.inner.count
    }

    fn __repr__(&self) -> String {
        format!(
            "Scenarios(name={:?}, initial_timestamp={}, count={}, horizon={}, interval={}, resolution={}, scenario_count={}, shape={:?}, time_reference={})",
            self.inner.name,
            self.inner.initial_timestamp,
            self.inner.count,
            self.inner.horizon.to_iso8601(),
            self.inner.interval.to_iso8601(),
            self.inner.resolution.to_iso8601(),
            self.inner.scenario_count,
            self.inner.data.shape,
            reference_label(self.inner.time_reference.as_ref()),
        )
    }
}

// ---- SingleTimeSeries -----------------------------------------------------

#[pyclass(name = "SingleTimeSeries", module = "infrastore", from_py_object)]
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
        initial_timestamp: PyInstant,
        resolution: Bound<'_, PyAny>,
        data: &Bound<'_, PyAny>,
        name: String,
    ) -> PyResult<Self> {
        let resolution = pyany_to_period(&resolution)?;
        let typed = typed_array_from_numpy(data)?;
        let mut inner =
            core_lib::SingleTimeSeries::new(initial_timestamp.instant, resolution, typed, name);
        // See the forecast constructors: the spelling rides in on the timestamp.
        inner.time_reference = Some(initial_timestamp.reference);
        Ok(Self { inner })
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    /// The grid's first timestamp, spelled the way it was written.
    #[getter]
    fn initial_timestamp<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        spell_instant(
            py,
            self.inner.initial_timestamp,
            self.inner.time_reference.as_ref(),
        )
    }

    /// How this series' timestamps were spelled: `"utc"`, `"zoneless"`, a fixed
    /// offset (`"-07:00"`), an IANA zone name, or `None` for unspecified.
    #[getter]
    fn time_reference(&self) -> Option<String> {
        self.inner
            .time_reference
            .as_ref()
            .map(core_lib::TimeReference::as_storage_string)
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

    /// Value equality: all fields including the data array (bitwise).
    fn __eq__(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    /// Number of time steps (`length`).
    fn __len__(&self) -> usize {
        self.inner.length
    }

    fn __repr__(&self) -> String {
        format!(
            "SingleTimeSeries(name={:?}, initial_timestamp={}, length={}, resolution={}, shape={:?}, time_reference={})",
            self.inner.name,
            self.inner.initial_timestamp,
            self.inner.length,
            self.inner.resolution.to_iso8601(),
            self.inner.data.shape,
            reference_label(self.inner.time_reference.as_ref()),
        )
    }
}

// ---- NonSequentialTimeSeries ----------------------------------------------

#[pyclass(
    name = "NonSequentialTimeSeries",
    module = "infrastore",
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
    fn new(timestamps: Vec<PyInstant>, data: &Bound<'_, PyAny>, name: String) -> PyResult<Self> {
        let typed = typed_array_from_numpy(data)?;
        // One series records one spelling, so the vector has to agree on one.
        let reference = vector_reference(&timestamps)?;
        let mut inner =
            core_lib::NonSequentialTimeSeries::new(instants_to_utc(&timestamps), typed, name)
                .map_err(InvalidParameterError::new_err)?;
        inner.time_reference = reference;
        Ok(Self { inner })
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    /// The explicit timestamp vector, spelled the way it was written.
    #[getter]
    fn timestamps<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyAny>>> {
        spell_instants(
            py,
            &self.inner.timestamps,
            self.inner.time_reference.as_ref(),
        )
    }

    /// How this series' timestamps were spelled: `"utc"`, `"zoneless"`, a fixed
    /// offset (`"-07:00"`), an IANA zone name, or `None` for unspecified.
    #[getter]
    fn time_reference(&self) -> Option<String> {
        self.inner
            .time_reference
            .as_ref()
            .map(core_lib::TimeReference::as_storage_string)
    }

    #[getter]
    fn length(&self) -> usize {
        self.inner.length
    }

    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        numpy_from_typed(py, &self.inner.data)
    }

    /// Value equality: all fields including the data array (bitwise).
    fn __eq__(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    /// Number of time steps (`length`).
    fn __len__(&self) -> usize {
        self.inner.length
    }

    fn __repr__(&self) -> String {
        format!(
            "NonSequentialTimeSeries(name={:?}, length={}, shape={:?}, time_reference={})",
            self.inner.name,
            self.inner.length,
            self.inner.data.shape,
            reference_label(self.inner.time_reference.as_ref()),
        )
    }
}

// ---- Associations ---------------------------------------------------------

/// One attachment of a supplemental attribute to a component.
///
/// Identity is the `(component_id, attribute_id)` pair; the type names are
/// denormalized filtering aids, so re-attaching the same pair under different
/// type names is still a duplicate and the second `add` raises
/// `DuplicateAssociationError`.
#[pyclass(
    name = "SupplementalAttributeAssociation",
    module = "infrastore",
    from_py_object
)]
#[derive(Clone)]
pub struct PySupplementalAttributeAssociation {
    inner: core_lib::SupplementalAttributeAssociation,
}

#[pymethods]
impl PySupplementalAttributeAssociation {
    #[new]
    #[pyo3(signature = (component_id, component_type, attribute_id, attribute_type))]
    fn new(
        component_id: i64,
        component_type: String,
        attribute_id: i64,
        attribute_type: String,
    ) -> Self {
        Self {
            inner: core_lib::SupplementalAttributeAssociation {
                component_id,
                component_type,
                attribute_id,
                attribute_type,
                id: None,
            },
        }
    }

    #[getter]
    fn component_id(&self) -> i64 {
        self.inner.component_id
    }

    #[getter]
    fn component_type(&self) -> String {
        self.inner.component_type.clone()
    }

    #[getter]
    fn attribute_id(&self) -> i64 {
        self.inner.attribute_id
    }

    #[getter]
    fn attribute_type(&self) -> String {
        self.inner.attribute_type.clone()
    }

    /// The catalog row's id, or `None` for a value that has not been through
    /// the catalog. Outside equality and hashing: identity is the
    /// `(component_id, attribute_id)` pair, so a row read back compares equal
    /// to the value that wrote it.
    #[getter]
    fn id(&self) -> Option<i64> {
        self.inner.id
    }

    fn __repr__(&self) -> String {
        format!(
            "SupplementalAttributeAssociation(component_id={}, component_type={:?}, \
             attribute_id={}, attribute_type={:?})",
            self.inner.component_id,
            self.inner.component_type,
            self.inner.attribute_id,
            self.inner.attribute_type,
        )
    }

    /// Structural equality over all four fields — stricter than the table's
    /// notion of identity, so that a round-tripped row compares equal only when
    /// its type names survived too.
    fn __eq__(&self, other: &PySupplementalAttributeAssociation) -> bool {
        self.inner == other.inner
    }

    /// Consistent with `__eq__`, so attachments work in sets and as dict keys
    /// (bulk export/import comparisons rely on this).
    fn __hash__(&self) -> u64 {
        hash_of(&self.inner)
    }
}

/// One directed edge between two components — a generator (parent) connected to
/// a bus (child), say.
///
/// Identity is the ordered `(parent_id, child_id)` pair, so the reversed pair is
/// a different edge. As above, the type names are denormalized filtering aids
/// and do not enter identity.
#[pyclass(name = "ParentChildAssociation", module = "infrastore", from_py_object)]
#[derive(Clone)]
pub struct PyParentChildAssociation {
    inner: core_lib::ParentChildAssociation,
}

#[pymethods]
impl PyParentChildAssociation {
    #[new]
    #[pyo3(signature = (parent_id, parent_type, child_id, child_type))]
    fn new(parent_id: i64, parent_type: String, child_id: i64, child_type: String) -> Self {
        Self {
            inner: core_lib::ParentChildAssociation {
                parent_id,
                parent_type,
                child_id,
                child_type,
                id: None,
            },
        }
    }

    #[getter]
    fn parent_id(&self) -> i64 {
        self.inner.parent_id
    }

    #[getter]
    fn parent_type(&self) -> String {
        self.inner.parent_type.clone()
    }

    #[getter]
    fn child_id(&self) -> i64 {
        self.inner.child_id
    }

    #[getter]
    fn child_type(&self) -> String {
        self.inner.child_type.clone()
    }

    /// The catalog row's id, or `None`. See
    /// `SupplementalAttributeAssociation.id`.
    #[getter]
    fn id(&self) -> Option<i64> {
        self.inner.id
    }

    fn __repr__(&self) -> String {
        format!(
            "ParentChildAssociation(parent_id={}, parent_type={:?}, child_id={}, \
             child_type={:?})",
            self.inner.parent_id,
            self.inner.parent_type,
            self.inner.child_id,
            self.inner.child_type,
        )
    }

    /// Structural equality over all four fields; see
    /// [`PySupplementalAttributeAssociation::__eq__`].
    fn __eq__(&self, other: &PyParentChildAssociation) -> bool {
        self.inner == other.inner
    }

    /// Consistent with `__eq__`.
    fn __hash__(&self) -> u64 {
        hash_of(&self.inner)
    }
}

/// `__hash__` body shared by the two association pyclasses.
fn hash_of<T: std::hash::Hash>(value: &T) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
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

// ---- Store ------------------------------------------------------

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

/// Build a numpy array (owned, writable) from raw dtype/shape/bytes.
fn numpy_from_parts<'py>(
    py: Python<'py>,
    dtype: core_lib::Dtype,
    shape: Vec<usize>,
    bytes: Vec<u8>,
) -> PyResult<Bound<'py, PyAny>> {
    let arr =
        core_lib::TypedArray::new(dtype, shape, bytes).map_err(InvalidParameterError::new_err)?;
    numpy_from_typed(py, &arr)
}

// ---- StaticReader / ForecastReader ----------------------------------------

/// A prepared columnar reader over the static series sharing one timeline —
/// a grid of one resolution for `SingleTimeSeries`, or one explicit timestamp
/// vector for `NonSequentialTimeSeries`. Build with
/// `Store.build_static_reader`, drive with `Store.static_read`, then read a
/// group's buffer with `group_values`.
#[pyclass(name = "StaticReader", module = "infrastore", unsendable)]
pub struct PyStaticReader {
    inner: core_lib::StaticReader,
}

#[pymethods]
impl PyStaticReader {
    /// The reader's shared timeline: `{"time_series_type": str,
    /// "initial_timestamp": rfc3339 str, "resolution": ISO-8601 str | None,
    /// "length": int, "time_reference": str | None}`.
    ///
    /// `resolution` is `None` for a `NonSequentialTimeSeries` reader: an
    /// irregular timeline has no constant step, so walk `timestamps()` instead.
    ///
    /// `time_reference` is the one spelling the axis carries. A reader whose
    /// columns all agree reports their reference; one whose columns merely agree
    /// on naming instants reports `"utc"`. A cohort mixing zoneless with the
    /// rest never builds at all.
    fn grid<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("time_series_type", self.inner.time_series_type().as_str())?;
        d.set_item(
            "initial_timestamp",
            render_catalog_timestamp(self.inner.initial_timestamp(), self.inner.time_reference()),
        )?;
        d.set_item(
            "resolution",
            self.inner.resolution().map(|r| r.to_iso8601()),
        )?;
        d.set_item("length", self.inner.length())?;
        d.set_item(
            "time_reference",
            self.inner
                .time_reference()
                .map(core_lib::TimeReference::as_storage_string),
        )?;
        Ok(d)
    }

    /// One dict per columnar group: `{"dtype": str, "element_type": str, "element_shape": list[int],
    /// "ids": list[int]}` (column order matches `group_values`). Resolve an id
    /// with `get_metadata_by_id` to recover the series a column came from.
    fn groups<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        self.inner
            .groups()
            .iter()
            .map(|g| {
                let d = PyDict::new(py);
                d.set_item("dtype", g.dtype().as_str())?;
                d.set_item("element_type", g.element_type().to_string())?;
                d.set_item("element_shape", g.element_shape().to_vec())?;
                let ids: Vec<i64> = g.ids().iter().map(|id| id.get()).collect();
                d.set_item("ids", ids)?;
                Ok(d)
            })
            .collect()
    }

    /// Every timestamp on the reader's timeline, in order, spelled the way the
    /// cohort's own series are.
    fn timestamps<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyAny>>> {
        let axis: Vec<DateTime<Utc>> = self.inner.timestamps().collect();
        spell_instants(py, &axis, self.inner.time_reference())
    }

    fn __repr__(&self) -> String {
        format!(
            "StaticReader({}, initial_timestamp={}, resolution={}, length={}, groups={}, \
             columns={})",
            self.inner.time_series_type().as_str(),
            self.inner.initial_timestamp(),
            self.inner
                .resolution()
                .map_or_else(|| "None".to_string(), |r| r.to_iso8601()),
            self.inner.length(),
            self.inner.groups().len(),
            self.inner
                .groups()
                .iter()
                .map(|g| g.num_columns())
                .sum::<usize>(),
        )
    }

    /// The most-recent read of group `index` as a numpy array shaped
    /// `(num_columns, *element_shape)`. Empty until the first `static_read`.
    fn group_values<'py>(&self, py: Python<'py>, index: usize) -> PyResult<Bound<'py, PyAny>> {
        let group = self.inner.groups().get(index).ok_or_else(|| {
            InvalidParameterError::new_err(format!("group index {index} out of range"))
        })?;
        let mut shape = vec![group.num_columns()];
        shape.extend_from_slice(group.element_shape());
        numpy_from_parts(py, group.dtype(), shape, group.values().to_vec())
    }
}

/// A prepared per-entry window reader over dense forecasts of one type sharing
/// one window timeline. Build with `Store.build_forecast_reader`,
/// drive with `Store.forecast_read`, then read an entry's window with
/// `entry_values`.
#[pyclass(name = "ForecastReader", module = "infrastore", unsendable)]
pub struct PyForecastReader {
    inner: core_lib::ForecastReader,
}

#[pymethods]
impl PyForecastReader {
    /// The window timeline: `{"initial_timestamp": rfc3339 str, "resolution":
    /// ISO str, "interval": ISO str, "count": int, "time_series_type": str,
    /// "time_reference": str | None}`.
    fn timeline<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item(
            "initial_timestamp",
            render_catalog_timestamp(self.inner.initial_timestamp(), self.inner.time_reference()),
        )?;
        d.set_item("resolution", self.inner.resolution().to_iso8601())?;
        d.set_item("interval", self.inner.interval().to_iso8601())?;
        d.set_item("count", self.inner.count())?;
        d.set_item("time_series_type", self.inner.time_series_type().as_str())?;
        d.set_item(
            "time_reference",
            self.inner
                .time_reference()
                .map(core_lib::TimeReference::as_storage_string),
        )?;
        Ok(d)
    }

    /// The per-entry association ids, in order (parallel to `entry_values`).
    fn entries(&self) -> Vec<i64> {
        self.inner.entries().iter().map(|e| e.id().get()).collect()
    }

    /// The number of deduplicated window slots: one physical backend read per
    /// slot on each `forecast_read` (`<= len(entries())`). Entries that share a
    /// backing array and read plan collapse to one slot.
    fn num_slots(&self) -> usize {
        self.inner.slots().len()
    }

    /// The 0-based slot backing entry `index`. Entries reporting equal slots
    /// share one window, so group by this to materialize each unique window only
    /// once. Raises `InvalidParameterError` if `index` is out of range.
    fn entry_slot(&self, index: usize) -> PyResult<usize> {
        self.inner
            .entries()
            .get(index)
            .map(|e| e.slot())
            .ok_or_else(|| {
                InvalidParameterError::new_err(format!("entry index {index} out of range"))
            })
    }

    /// Every window start timestamp, in order.
    fn timestamps<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyAny>>> {
        let axis: Vec<DateTime<Utc>> = self.inner.timestamps().collect();
        spell_instants(py, &axis, self.inner.time_reference())
    }

    fn __repr__(&self) -> String {
        format!(
            "ForecastReader(time_series_type={}, initial_timestamp={}, resolution={}, interval={}, count={}, entries={})",
            self.inner.time_series_type().as_str(),
            self.inner.initial_timestamp(),
            self.inner.resolution().to_iso8601(),
            self.inner.interval().to_iso8601(),
            self.inner.count(),
            self.inner.entries().len(),
        )
    }

    /// The most-recent read of entry `index` as a numpy array shaped
    /// `(*window_shape)` (e.g. `(horizon, *element_shape)`). Empty until the
    /// first `forecast_read`.
    fn entry_values<'py>(&self, py: Python<'py>, index: usize) -> PyResult<Bound<'py, PyAny>> {
        if index >= self.inner.entries().len() {
            return Err(InvalidParameterError::new_err(format!(
                "entry index {index} out of range"
            )));
        }
        let slot = self.inner.entry_slot(index);
        numpy_from_parts(
            py,
            slot.dtype(),
            slot.window_shape().to_vec(),
            slot.window().to_vec(),
        )
    }
}

/// The context manager returned by `Store.transaction()`.
///
/// Holds a Python-level reference to its store rather than a Rust borrow, so the
/// transaction is store state for the block's duration and nothing has to be
/// borrowed across `__enter__`/`__exit__`.
#[pyclass(name = "Transaction", module = "infrastore", unsendable)]
pub struct PyTransaction {
    store: Py<PyStore>,
}

#[pymethods]
impl PyTransaction {
    /// Begin the transaction. Returns the store, so `as` binds something useful.
    fn __enter__(&self, py: Python<'_>) -> PyResult<Py<PyStore>> {
        self.store.borrow_mut(py).begin_transaction()?;
        Ok(self.store.clone_ref(py))
    }

    /// Commit on a clean exit, roll back otherwise. Never suppresses the
    /// exception that caused the unwind: if the rollback itself fails, that
    /// failure is reported as a warning so the original error still propagates.
    // `exc_value` and `traceback` are unused; see the note on `Store.__exit__`
    // for why they are still spelled without a leading underscore.
    #[pyo3(signature = (exc_type=None, exc_value=None, traceback=None))]
    #[allow(unused_variables)]
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: Option<Py<PyAny>>,
        exc_value: Option<Py<PyAny>>,
        traceback: Option<Py<PyAny>>,
    ) -> PyResult<bool> {
        let mut store = self.store.borrow_mut(py);
        if exc_type.is_none() {
            store.commit_transaction()?;
        } else if let Err(e) = store.rollback_transaction() {
            drop(store);
            PyErr::warn(
                py,
                &py.get_type::<pyo3::exceptions::PyRuntimeWarning>(),
                std::ffi::CString::new(format!(
                    "infrastore transaction rollback failed; the store may retain partial \
                     work from the transaction: {e}"
                ))?
                .as_c_str(),
                1,
            )?;
        }
        Ok(false)
    }
}

#[pyclass(name = "Store", module = "infrastore", unsendable)]
pub struct PyStore {
    /// `None` once `close()` (or `__exit__`) has dropped the store; every store
    /// operation then raises via [`PyStore::store`] / [`PyStore::store_mut`].
    inner: Option<core_lib::Store>,
    /// Whether the store was opened read-only (cached for `__repr__`/`read_only`
    /// so they work even after `close()`).
    read_only: bool,
    /// Human-readable source for `__repr__`: the path, or `"in-memory"`.
    descr: String,
}

impl PyStore {
    /// Borrow the live store, or raise if it has been closed.
    fn store(&self) -> PyResult<&core_lib::Store> {
        self.inner
            .as_ref()
            .ok_or_else(|| TimeSeriesError::new_err("store is closed"))
    }

    /// Mutably borrow the live store, or raise if it has been closed.
    fn store_mut(&mut self) -> PyResult<&mut core_lib::Store> {
        self.inner
            .as_mut()
            .ok_or_else(|| TimeSeriesError::new_err("store is closed"))
    }
}

#[pymethods]
impl PyStore {
    /// Create a new store. With `in_memory=True`, no filesystem I/O occurs;
    /// otherwise an HDF5 file is created at `path` and a catalog SQLite file
    /// at `<path>.sqlite` holds metadata.
    ///
    /// `compression` selects the HDF5 data-variable filter: `"deflate"`
    /// (default) applies DEFLATE at `compression_level` (0–9) with optional
    /// byte `shuffle`; `"none"` disables compression. The setting is ignored
    /// for in-memory stores and is persisted so later appends reuse it.
    ///
    /// `catalog` places the SQLite catalog: `"attached"` writes it to
    /// `<path>.sqlite`, where every commit is durable, and `"memory"` holds it
    /// in RAM so it reaches disk only through `persist_to()` or
    /// `persist_catalog()` — nothing survives a crash, which suits building a
    /// store in a scratch directory beside volatile state. Arrays stream to the
    /// HDF5 file either way. The default matches the backend: `"memory"` when
    /// `in_memory=True`, else `"attached"`.
    ///
    /// Raises `StoreExistsError` if `path` (or `<path>.sqlite`) already holds a
    /// store: creating there would discard its arrays while keeping its
    /// catalog, leaving a store that reopens cleanly with every array missing.
    /// Pass `overwrite=True` to discard the existing artifact on purpose, or use
    /// `Store.open()` to keep it.
    #[classmethod]
    #[pyo3(signature = (path=None, *, in_memory=false, compression="deflate", compression_level=3, shuffle=true, catalog=None, overwrite=false))]
    #[allow(clippy::too_many_arguments)]
    fn create(
        _cls: &Bound<'_, pyo3::types::PyType>,
        path: Option<PathBuf>,
        in_memory: bool,
        compression: &str,
        compression_level: u8,
        shuffle: bool,
        catalog: Option<&str>,
        overwrite: bool,
    ) -> PyResult<Self> {
        let compression = parse_compression(compression, compression_level, shuffle)?;
        let catalog = parse_catalog(catalog, in_memory)?;
        let descr = match &path {
            Some(p) if !in_memory => p.display().to_string(),
            _ => "in-memory".to_string(),
        };
        let store = match (overwrite, in_memory) {
            (true, true) => {
                return Err(InvalidParameterError::new_err(
                    "overwrite=True is meaningless for an in-memory store: there is no artifact to replace",
                ));
            }
            (true, false) => {
                let path = path.as_deref().ok_or_else(|| {
                    InvalidParameterError::new_err("path is required when in_memory=False")
                })?;
                core_lib::create_store_replacing(path, compression, catalog)
            }
            (false, _) => core_lib::create_store_with_catalog(
                path.as_deref(),
                in_memory,
                compression,
                catalog,
            ),
        }
        .map_err(map_err)?;
        Ok(Self {
            inner: Some(store),
            read_only: false,
            descr,
        })
    }

    /// Copy the store at `src` to `dest` and open the copy read-write.
    ///
    /// Both halves are copied, so `dest` is a complete, independent store, and
    /// `src` is never opened for writing.
    ///
    /// This is the safe way to load a store you care about and then change it.
    /// `Store.open(path)` defaults to read-write, and every mutation then lands
    /// in that file directly — HDF5 has no journal and no repair tool, so an
    /// interrupted write there is unrecoverable. Working on a copy and calling
    /// `persist_to(src)` leaves the original intact until one atomic rename
    /// replaces it.
    ///
    /// Raises `StoreExistsError` if `dest` already holds a store.
    #[classmethod]
    #[pyo3(signature = (src, dest, *, catalog="attached"))]
    fn open_copy(
        _cls: &Bound<'_, pyo3::types::PyType>,
        src: PathBuf,
        dest: PathBuf,
        catalog: &str,
    ) -> PyResult<Self> {
        let catalog = parse_catalog(Some(catalog), false)?;
        let descr = dest.display().to_string();
        let store = core_lib::open_store_copy(&src, &dest, catalog).map_err(map_err)?;
        Ok(Self {
            inner: Some(store),
            read_only: false,
            descr,
        })
    }

    /// Open an existing store from disk. `read_only=True` blocks all writes.
    ///
    /// `catalog="memory"` reads `<path>.sqlite` into RAM and leaves the file
    /// alone; later mutations reach disk only through `persist_to()`. The HDF5
    /// half is still opened in place, so a caller that means to leave the
    /// original untouched until an explicit save must open a copy.
    #[classmethod]
    #[pyo3(signature = (path, *, read_only=false, catalog="attached"))]
    fn open(
        _cls: &Bound<'_, pyo3::types::PyType>,
        path: PathBuf,
        read_only: bool,
        catalog: &str,
    ) -> PyResult<Self> {
        let catalog = parse_catalog(Some(catalog), false)?;
        let descr = path.display().to_string();
        let store =
            core_lib::open_store_with_catalog(&path, read_only, catalog).map_err(map_err)?;
        Ok(Self {
            inner: Some(store),
            read_only,
            descr,
        })
    }

    #[getter]
    fn read_only(&self) -> bool {
        self.read_only
    }

    /// Where this store's catalog lives: `"attached"` or `"memory"`.
    #[getter]
    fn catalog(&self) -> PyResult<&'static str> {
        Ok(catalog_name(self.store()?.catalog_mode()))
    }

    /// Close the store, dropping the underlying handle and flushing/releasing
    /// its files. Subsequent store operations raise `TimeSeriesError`. Idempotent
    /// (a second `close()` is a no-op).
    fn close(&mut self) {
        self.inner = None;
    }

    /// Context-manager entry: returns the store itself.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Context-manager exit: closes the store. Does not suppress exceptions.
    // The three arguments are unused, but they are named without a leading
    // underscore on purpose: PyO3 takes the Python keyword name from the Rust
    // one verbatim, so `_exc_type` made `store.__exit__(exc_type=None)` -- the
    // spelling the type stub and the protocol both use -- a TypeError.
    #[pyo3(signature = (exc_type=None, exc_value=None, traceback=None))]
    #[allow(unused_variables)]
    fn __exit__(
        &mut self,
        exc_type: Option<Py<PyAny>>,
        exc_value: Option<Py<PyAny>>,
        traceback: Option<Py<PyAny>>,
    ) -> bool {
        self.close();
        false
    }

    fn __repr__(&self) -> String {
        let state = if self.inner.is_none() { ", closed" } else { "" };
        let read_only = if self.read_only { "True" } else { "False" };
        format!("Store({}, read_only={}{})", self.descr, read_only, state)
    }

    /// Add a time series. The association `name` comes from the time series
    /// object (`time_series.name`).
    ///
    /// `features` is a `dict[str, int|float|bool|str]`. A feature name that
    /// shadows a time-series or key field (`name`, `resolution`, `owner_id`,
    /// …) is rejected with `InvalidParameterError`. `units`, `quantity_kind`,
    /// `component_field`, and `application_data` are optional strings
    /// (`application_data` is an opaque, package-owned payload — typically JSON
    /// — stored verbatim on the association). `quantity_kind` names what the
    /// values measure, e.g. `"ActivePower"`. `unit_system` is `"natural_units"`
    /// or `"component_base"`; omitting it leaves the basis unspecified, which is
    /// not the same as declaring natural units. `component_field` names the
    /// field on the owning component whose value these values are the
    /// time-varying form of, e.g. `"max_active_power"`; it is free-form and
    /// never interpreted by the store.
    ///
    /// `element_type` declares what the array's elements mean in the store's own
    /// vocabulary (`"tuple(3,f64)"`, `"piecewise_linear"`, …). Omit it for plain
    /// numbers, where it defaults to the array's own dtype spelling.
    #[pyo3(signature = (owner_id, owner_type, owner_category, time_series, *, features=None, units=None, element_type=None, application_data=None, quantity_kind=None, unit_system=None, time_reference=None, component_field=None))]
    #[allow(clippy::too_many_arguments)]
    fn add_time_series(
        &mut self,
        owner_id: i64,
        owner_type: &str,
        owner_category: PyOwnerCategory,
        time_series: &Bound<'_, PyAny>,
        features: Option<&Bound<'_, PyDict>>,
        units: Option<String>,
        element_type: Option<String>,
        application_data: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        time_reference: Option<String>,
        component_field: Option<String>,
    ) -> PyResult<i64> {
        let features = features_from_dict(features)?;
        let mut data = extract_time_series_data(time_series)?;
        // These describe the series, so they are set on it, not on the request —
        // and each is applied only when the caller actually supplied it, the way
        // `element_type` always has been. Setting them unconditionally from
        // arguments that default to `None` meant a read-then-re-add silently
        // dropped five of the six descriptors that `get_time_series` had just
        // populated, while keeping the sixth; the value classes expose no
        // properties for them, so the caller could neither notice nor re-supply
        // what was lost. Omitting one now keeps whatever the series carries.
        if let Some(units) = units {
            data.set_units(Some(units));
        }
        if let Some(application_data) = application_data {
            data.set_application_data(Some(application_data));
        }
        if let Some(quantity_kind) = quantity_kind {
            data.set_quantity_kind(Some(quantity_kind));
        }
        if let Some(unit_system) = unit_system {
            data.set_unit_system(parse_unit_system(Some(unit_system.as_str()))?);
        }
        if let Some(time_reference) = time_reference {
            data.set_time_reference(Some(parse_time_reference(
                time_series.py(),
                &time_reference,
            )?));
        }
        if let Some(component_field) = component_field {
            data.set_component_field(Some(component_field));
        }
        if let Some(et) = element_type {
            data.set_element_type(parse_element_type(&et)?);
        }
        let request = core_lib::AddRequest::new(owner_id, owner_type, owner_category.into(), data)
            .with_features(features);
        let added = self.store_mut()?.add(request).map_err(map_err)?;
        Ok(added.get())
    }

    /// Add many time series in one call, committing the metadata catalog once
    /// for the whole batch. This is much faster than calling
    /// `add_time_series` in a loop, which pays one SQLite transaction per
    /// series.
    ///
    /// `items` is a list of dicts whose keys mirror `add_time_series`'s
    /// parameters: `owner_id`, `owner_type`, `owner_category`,
    /// `time_series`, and optionally `features`, `units`, `element_type`,
    /// `application_data`, `quantity_kind`, `unit_system`, and
    /// `component_field`. Any other key raises, as the misspelled keyword it
    /// almost always is: `add_time_series` rejects one for free, and reading
    /// only the keys it knows made the bulk path silently drop `unit_sytem` and
    /// every other typo, along with whatever it was carrying.
    ///
    /// All-or-nothing: if any item fails, the entire batch is rolled back.
    /// Returns the catalog `id` of each new row, in input order.
    ///
    /// An item may carry `id` to file its association under a specific catalog
    /// id, for a writer replaying a document that recorded one. Either every
    /// item supplies an id or none does; a mixed batch is refused, because an
    /// assigned id is drawn from the same counter an explicit one advances, so
    /// whether the two collide would depend on the order the items happen to be
    /// in.
    fn add_time_series_bulk(&mut self, items: Vec<Bound<'_, PyDict>>) -> PyResult<Vec<i64>> {
        let mut requests = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            reject_unknown_item_keys(item, index)?;
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
            let application_data: Option<String> = match item.get_item("application_data")? {
                Some(l) if !l.is_none() => Some(l.extract()?),
                _ => None,
            };
            let quantity_kind: Option<String> = match item.get_item("quantity_kind")? {
                Some(q) if !q.is_none() => Some(q.extract()?),
                _ => None,
            };
            let unit_system: Option<String> = match item.get_item("unit_system")? {
                Some(u) if !u.is_none() => Some(u.extract()?),
                _ => None,
            };
            let time_reference: Option<String> = match item.get_item("time_reference")? {
                Some(t) if !t.is_none() => Some(t.extract()?),
                _ => None,
            };
            let component_field: Option<String> = match item.get_item("component_field")? {
                Some(c) if !c.is_none() => Some(c.extract()?),
                _ => None,
            };
            let element_type = match item.get_item("element_type")? {
                Some(e) if !e.is_none() => Some(parse_element_type(&e.extract::<String>()?)?),
                _ => None,
            };
            let mut data = extract_time_series_data(&time_series)?;
            // As in `add_time_series`: a key the item omits leaves the series'
            // own descriptor alone rather than clearing it.
            if let Some(units) = units {
                data.set_units(Some(units));
            }
            if let Some(application_data) = application_data {
                data.set_application_data(Some(application_data));
            }
            if let Some(quantity_kind) = quantity_kind {
                data.set_quantity_kind(Some(quantity_kind));
            }
            if let Some(unit_system) = unit_system {
                data.set_unit_system(parse_unit_system(Some(unit_system.as_str()))?);
            }
            if let Some(time_reference) = time_reference {
                data.set_time_reference(Some(parse_time_reference(item.py(), &time_reference)?));
            }
            if let Some(component_field) = component_field {
                data.set_component_field(Some(component_field));
            }
            if let Some(et) = element_type {
                data.set_element_type(et);
            }
            requests.push(core_lib::AddRequest {
                owner_id,
                owner_type,
                owner_category: owner_category.into(),
                data,
                features,
            });
        }
        Ok(self
            .store_mut()?
            .add_time_series_bulk(requests)
            .map_err(map_err)?
            .into_iter()
            .map(|id| id.get())
            .collect())
    }

    /// Derive `DeterministicSingleTimeSeries` forecasts from the stored
    /// `SingleTimeSeries` associations (mirrors InfrastructureSystems.jl's
    /// `transform_single_time_series!`). Each `SingleTimeSeries` is re-described
    /// as a DST sharing the same underlying array; `count` is derived from each
    /// series' length. Returns the number of series transformed.
    #[pyo3(signature = (horizon, interval, *, owner_category=None, resolution=None))]
    fn transform_single_time_series(
        &mut self,
        horizon: Bound<'_, PyAny>,
        interval: Bound<'_, PyAny>,
        owner_category: Option<PyOwnerCategory>,
        resolution: Option<Bound<'_, PyAny>>,
    ) -> PyResult<usize> {
        let horizon = pyany_to_period(&horizon)?;
        let interval = pyany_to_period(&interval)?;
        let resolution = match resolution {
            Some(r) => Some(pyany_to_period(&r)?),
            None => None,
        };
        self.store_mut()?
            .transform_single_time_series(
                horizon,
                interval,
                owner_category.map(Into::into),
                resolution,
                Default::default(),
            )
            .map(|outcome| outcome.transformed)
            .map_err(map_err)
    }

    /// Remove every time series for the owner `(owner_id, owner_category)`, or
    /// every time series in the store when neither is given. Both must be
    /// supplied together or neither.
    #[pyo3(signature = (*, owner_id=None, owner_category=None))]
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
        self.store_mut()?.clear_time_series(owner).map_err(map_err)
    }

    /// Reassign every time series owned by `(old_owner, owner_category)` to
    /// `(new_owner, owner_category)`. Returns the number of associations moved.
    fn replace_owner(
        &mut self,
        old_owner: i64,
        new_owner: i64,
        owner_category: PyOwnerCategory,
    ) -> PyResult<usize> {
        self.store_mut()?
            .replace_owner(old_owner, new_owner, owner_category.into())
            .map_err(map_err)
    }

    /// Rename the association filed under `id`.
    ///
    /// Only the catalog name changes, and the id is the same afterwards — a
    /// rename moves the name, not the reference, so anything holding the id
    /// keeps working.
    fn rename_time_series(&mut self, id: i64, new_name: &str) -> PyResult<()> {
        self.store_mut()?
            .rename_time_series(core_lib::TimeSeriesId(id), new_name)
            .map_err(map_err)?;
        Ok(())
    }

    /// Return a list of catalog metadata dicts matching the filter. Each dict
    /// has `id` — the association id that addresses the series — plus
    /// `owner_id`, `owner_type`, `owner_category`, `time_series_type`, `name`,
    /// `data_hash` (hex string), `length`, `resolution` (ISO 8601 duration
    /// string, e.g. `PT1H`, or `None`), `features`, `units`, and the rest of the
    /// row's descriptors.
    ///
    /// The listing that answers identity questions: which series exist, what
    /// each is, and the `id` to read or remove it by. `timestamps` is always
    /// `None` here — an irregular series' time axis is the one part of a row
    /// that costs a read per row, so a listing omits it; `read_by_id` returns
    /// the series with its axis.
    ///
    /// `name_glob` filters names by a SQLite `GLOB` pattern (case-sensitive,
    /// `*`/`?` wildcards); when both `name` and `name_glob` are given, both
    /// must match. `component_field` matches the owning component's field
    /// exactly and case-sensitively — "every series that varies this field";
    /// a row that declares none matches no value, so it cannot select the rows
    /// that left it unset. All filter arguments are keyword-only.
    ///
    /// `time_series_type` is a `TimeSeriesType`. `TimeSeriesType.Deterministic`
    /// also matches the `DeterministicSingleTimeSeries` rows that
    /// `transform_single_time_series` derives — each row still reports its own
    /// `time_series_type`, and passing
    /// `TimeSeriesType.DeterministicSingleTimeSeries` selects only those. Every
    /// method taking these filter kwargs reads the type the same way.
    #[pyo3(signature = (
        *, owner_id=None, owner_category=None, owner_type=None, time_series_type=None,
        name=None, name_glob=None, component_field=None, zoneless=None, resolution=None,
        interval=None, features=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn list_metadata<'py>(
        &self,
        py: Python<'py>,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
        owner_type: Option<String>,
        time_series_type: Option<Bound<'_, PyAny>>,
        name: Option<String>,
        name_glob: Option<String>,
        component_field: Option<String>,
        zoneless: Option<bool>,
        resolution: Option<Bound<'_, PyAny>>,
        interval: Option<Bound<'_, PyAny>>,
        features: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let filter = build_list_filter(
            owner_id,
            owner_category,
            owner_type,
            time_series_type.as_ref(),
            name,
            name_glob,
            component_field,
            zoneless,
            resolution,
            interval,
            features,
        )?;
        let metas = self.store()?.list_metadata(filter).map_err(map_err)?;
        let mut out = Vec::with_capacity(metas.len());
        for m in &metas {
            out.push(metadata_to_dict(py, m)?);
        }
        Ok(out)
    }

    /// Return True if at least one time series matches the filters — e.g.
    /// "does this owner have any time series (of type T)?" — without listing
    /// them. Accepts the same keyword-only filters as `list_metadata`, and
    /// answers from index probes that hydrate no rows — a `features` filter
    /// included — so it is safe to call in hot loops.
    #[pyo3(signature = (
        *, owner_id=None, owner_category=None, owner_type=None, time_series_type=None,
        name=None, name_glob=None, component_field=None, zoneless=None, resolution=None,
        interval=None, features=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn has_any_time_series(
        &self,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
        owner_type: Option<String>,
        time_series_type: Option<Bound<'_, PyAny>>,
        name: Option<String>,
        name_glob: Option<String>,
        component_field: Option<String>,
        zoneless: Option<bool>,
        resolution: Option<Bound<'_, PyAny>>,
        interval: Option<Bound<'_, PyAny>>,
        features: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<bool> {
        let filter = build_list_filter(
            owner_id,
            owner_category,
            owner_type,
            time_series_type.as_ref(),
            name,
            name_glob,
            component_field,
            zoneless,
            resolution,
            interval,
            features,
        )?;
        self.store()?.has_any_time_series(filter).map_err(map_err)
    }

    /// Return True if the store holds no persistent content of any kind — no
    /// time series, and no associations in any catalog.
    ///
    /// Answered by short-circuited existence probes, one per catalog table, so
    /// the cost does not grow with the store. Prefer it over a conjunction over
    /// the ``count_*`` methods: that runs a full aggregation, and it silently
    /// stops being correct when the catalog gains a table.
    fn is_empty(&self) -> PyResult<bool> {
        self.store()?.is_empty().map_err(map_err)
    }

    #[pyo3(signature = (time_series_type=None))]
    fn get_resolutions(
        &self,
        py: Python<'_>,
        time_series_type: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Vec<String>> {
        let _ = py;
        let requested = pyany_to_requested_type_opt(time_series_type.as_ref(), "time_series_type")?;
        Ok(self
            .store()?
            .get_resolutions(requested)
            .map_err(map_err)?
            .into_iter()
            .map(|p| p.to_iso8601())
            .collect())
    }

    /// Return the store's forecast parameters as a dict with keys `horizon`,
    /// `interval` (ISO 8601 duration strings, e.g. `PT1H`), `count` (int), and
    /// `resolution` (ISO 8601 duration string). Each value is `None` when the
    /// store holds no forecasts.
    #[pyo3(signature = (*, resolution=None, interval=None))]
    fn get_forecast_parameters<'py>(
        &self,
        py: Python<'py>,
        resolution: Option<Bound<'_, PyAny>>,
        interval: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let resolution = match resolution {
            Some(r) => Some(pyany_to_period(&r)?),
            None => None,
        };
        let interval = match interval {
            Some(i) => Some(pyany_to_period(&i)?),
            None => None,
        };
        let p = self
            .store()?
            .get_forecast_parameters(resolution, interval)
            .map_err(map_err)?;
        let d = PyDict::new(py);
        let iso = |v: Option<core_lib::Period>| v.map(|p| p.to_iso8601());
        d.set_item("horizon", iso(p.horizon))?;
        d.set_item("interval", iso(p.interval))?;
        d.set_item("count", p.count)?;
        d.set_item("resolution", iso(p.resolution))?;
        d.set_item(
            "initial_timestamp",
            p.initial_timestamp.map(|t| t.to_rfc3339()),
        )?;
        Ok(d)
    }

    /// Return the store's compression policy as a dict with keys `compression`
    /// (`"deflate"` or `"none"`), `level` (int, 0-9), and `shuffle` (bool). For a
    /// store opened from disk this reflects the persisted policy; in-memory
    /// stores report `"none"`.
    fn get_compression<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        match self.store()?.compression() {
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
        let counts = self.store()?.get_time_series_counts().map_err(map_err)?;
        let d = PyDict::new(py);
        d.set_item(
            "components_with_time_series",
            counts.components_with_time_series,
        )?;
        d.set_item("static_time_series", counts.static_time_series)?;
        d.set_item("forecasts", counts.forecasts)?;
        Ok(d)
    }

    /// Reclaim space in both halves of the store, returning a dict
    /// `{"slots_reclaimed": int, "datasets_dropped": int,
    /// "feature_sets_reclaimed": int, "timestamp_sets_reclaimed": int,
    /// "bytes_reclaimed": int}`.
    ///
    /// For an on-disk store this rewrites the HDF5 file from the catalog's live
    /// set and swaps the rewrite over the original — HDF5 cannot hand freed
    /// space back in place, so this is what makes a delete actually shrink the
    /// store. Assumes this process is the store's only user.
    fn compact<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let r = self.store_mut()?.compact().map_err(map_err)?;
        let d = PyDict::new(py);
        d.set_item("slots_reclaimed", r.slots_reclaimed)?;
        d.set_item("datasets_dropped", r.datasets_dropped)?;
        d.set_item("feature_sets_reclaimed", r.feature_sets_reclaimed)?;
        d.set_item("timestamp_sets_reclaimed", r.timestamp_sets_reclaimed)?;
        d.set_item("bytes_reclaimed", r.bytes_reclaimed)?;
        Ok(d)
    }

    /// Recompute each stored array's content hash and report the ones that
    /// disagree with the hash recorded alongside them, as a dict
    /// `{"ok": bool, "errors": list[str]}`.
    ///
    /// Checks the HDF5 half of the store only — the SQLite catalog is not
    /// inspected, so `ok` being True does not mean the store as a whole is sound.
    /// A catalog that is corrupted, truncated, or paired with the wrong `.h5`
    /// file still reports `ok`, while every read of the affected series raises.
    /// For catalog-side checks use `check_static_consistency` (per-resolution grid
    /// agreement) and `compact` (which reports the unreachable arrays and feature
    /// sets a delete left behind — an expected state, not corruption).
    fn verify_integrity<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let report = self.store()?.verify_integrity().map_err(map_err)?;
        let d = PyDict::new(py);
        d.set_item("ok", report.ok())?;
        d.set_item("errors", report.errors)?;
        Ok(d)
    }

    fn flush(&mut self) -> PyResult<()> {
        self.store_mut()?.flush().map_err(map_err)
    }

    // ---- Transactions -----------------------------------------------------

    /// Begin a transaction spanning subsequent operations, so that adds,
    /// removals, and transforms either all take effect or none do. Calls nest;
    /// only the outermost commit makes anything durable.
    ///
    /// Prefer the `transaction()` context manager, which cannot leak an open
    /// transaction. Removals are reversible only inside one.
    ///
    /// This holds the SQLite write lock until the outermost commit or rollback,
    /// so another writer on the same artifact will block and then fail on its
    /// busy timeout. Scope a transaction to the span that needs atomicity.
    ///
    /// Raises `ReadOnlyStoreError` if the store is read-only.
    fn begin_transaction(&mut self) -> PyResult<()> {
        self.store_mut()?.begin_transaction().map_err(map_err)
    }

    /// Commit the innermost open transaction. Raises `InvalidParameterError` if
    /// none is open.
    fn commit_transaction(&mut self) -> PyResult<()> {
        self.store_mut()?.commit_transaction().map_err(map_err)
    }

    /// Roll back the innermost open transaction, undoing every operation it
    /// covered. Raises `InvalidParameterError` if none is open.
    fn rollback_transaction(&mut self) -> PyResult<()> {
        self.store_mut()?.rollback_transaction().map_err(map_err)
    }

    /// Whether a transaction is currently open.
    #[getter]
    fn in_transaction(&self) -> PyResult<bool> {
        Ok(self.store()?.in_transaction())
    }

    /// A context manager that commits on a clean exit and rolls back if the
    /// block raises.
    ///
    /// ```python
    /// with store.transaction():
    ///     store.add_time_series(...)
    ///     store.remove_time_series(old_key)
    /// ```
    ///
    /// Both operations take effect or neither does — including the removal,
    /// which outside a transaction is irreversible. Blocks nest.
    fn transaction(slf: Py<Self>) -> PyTransaction {
        PyTransaction { store: slf }
    }

    // ---- Readers ----------------------------------------------------------

    /// Build a `StaticReader` over the static series matching the filter.
    ///
    /// For `SingleTimeSeries` (the default) a `resolution` is required — one
    /// resolution per reader — and all matched series must share one grid. For
    /// `time_series_type="NonSequentialTimeSeries"` pass no resolution (an
    /// irregular series has none): all matched series must instead lie on one
    /// timestamp vector, which is also what pools their arrays on disk. Drive it
    /// with `static_read`.
    #[pyo3(signature = (resolution=None, *, time_series_type=None, owner_id=None, owner_category=None, owner_type=None, name=None, name_glob=None, component_field=None, zoneless=None, features=None))]
    #[allow(clippy::too_many_arguments)]
    fn build_static_reader(
        &self,
        resolution: Option<Bound<'_, PyAny>>,
        time_series_type: Option<&Bound<'_, PyAny>>,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
        owner_type: Option<String>,
        name: Option<String>,
        name_glob: Option<String>,
        component_field: Option<String>,
        zoneless: Option<bool>,
        features: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<PyStaticReader> {
        let filter = build_list_filter(
            owner_id,
            owner_category,
            owner_type,
            time_series_type,
            name,
            name_glob,
            component_field,
            zoneless,
            resolution,
            None,
            features,
        )?;
        let reader = self.store()?.build_static_reader(filter).map_err(map_err)?;
        Ok(PyStaticReader { inner: reader })
    }

    /// Fill `reader`'s buffers with every column's value at `when` (off-grid
    /// raises). Afterwards read a group with `reader.group_values(i)`.
    fn static_read(&self, reader: &mut PyStaticReader, when: PyInstant) -> PyResult<()> {
        check_point_spelling(
            &when,
            reader.inner.time_reference(),
            "this reader's timeline",
        )?;
        self.store()?
            .static_read(&mut reader.inner, when.instant)
            .map_err(map_err)
    }

    /// Build a `ForecastReader` over the forecasts of `time_series_type` matching
    /// the filter. A `resolution` is required; a `Deterministic` reader also
    /// includes `DeterministicSingleTimeSeries`, matching the read request rule.
    /// Drive it with `forecast_read`.
    #[pyo3(signature = (time_series_type, resolution, *, owner_id=None, owner_category=None, owner_type=None, name=None, name_glob=None, component_field=None, zoneless=None, features=None))]
    #[allow(clippy::too_many_arguments)]
    fn build_forecast_reader(
        &self,
        time_series_type: &Bound<'_, PyAny>,
        resolution: Bound<'_, PyAny>,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
        owner_type: Option<String>,
        name: Option<String>,
        name_glob: Option<String>,
        component_field: Option<String>,
        zoneless: Option<bool>,
        features: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<PyForecastReader> {
        let filter = build_list_filter(
            owner_id,
            owner_category,
            owner_type,
            Some(time_series_type),
            name,
            name_glob,
            component_field,
            zoneless,
            Some(resolution),
            None,
            features,
        )?;
        let reader = self
            .store()?
            .build_forecast_reader(filter)
            .map_err(map_err)?;
        Ok(PyForecastReader { inner: reader })
    }

    /// Fill `reader`'s buffers with every entry's forecast window at `when`
    /// (off-grid raises). Afterwards read an entry with `reader.entry_values(i)`.
    fn forecast_read(&self, reader: &mut PyForecastReader, when: PyInstant) -> PyResult<()> {
        check_point_spelling(
            &when,
            reader.inner.time_reference(),
            "this reader's timeline",
        )?;
        self.store()?
            .forecast_read(&mut reader.inner, when.instant)
            .map_err(map_err)
    }

    // ---- Phase 3 additions ------------------------------------------------

    /// The metadata row filed under `id`, or `None` if the catalog holds no
    /// such row.
    ///
    /// The read direction of the id every write hands back: a caller that
    /// recorded ids in its own model resolves them here rather than keeping an
    /// id-to-key map beside the store. `None` rather than an exception, because
    /// a caller validating references it stored earlier is asking whether one
    /// still resolves, and a stale reference is an answer.
    fn get_metadata_by_id<'py>(
        &self,
        py: Python<'py>,
        id: i64,
    ) -> PyResult<Option<Bound<'py, PyDict>>> {
        match self
            .store()?
            .get_metadata_by_id(core_lib::TimeSeriesId(id))
            .map_err(map_err)?
        {
            Some(m) => Ok(Some(metadata_to_dict(py, &m)?)),
            None => Ok(None),
        }
    }

    /// The catalog metadata dicts named by `ids`, in the order the ids are
    /// given.
    ///
    /// `list_metadata` addressed by id instead of by attributes — the bulk
    /// companion to `get_metadata_by_id`, and what a consumer hydrating a model
    /// full of recorded ids wants: one catalog query for the whole set rather
    /// than one call per reference.
    ///
    /// Raises `NotFoundError` if any id names no row: a caller naming ids is
    /// asserting they exist, and a silently short list would let a stale
    /// reference pass as an absent match. Sift the set with
    /// `association_exists` first when some are expected to have gone. Repeats
    /// are returned once each, in place.
    fn list_metadata_by_ids<'py>(
        &self,
        py: Python<'py>,
        ids: Vec<i64>,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows = self
            .store()?
            .list_metadata_by_ids(&to_ids(&ids))
            .map_err(map_err)?;
        rows.iter().map(|m| metadata_to_dict(py, m)).collect()
    }

    /// Whether an association is filed under `id`.
    ///
    /// A primary-key probe that fetches no row, so a caller can check every
    /// reference in its model on load instead of discovering a dangling one
    /// mid-run.
    fn association_exists(&self, id: i64) -> PyResult<bool> {
        self.store()?
            .association_exists(core_lib::TimeSeriesId(id))
            .map_err(map_err)
    }

    /// Read many series by catalog id, in the order the ids are given.
    ///
    /// Repeats are allowed and returned once each, in place. Raises
    /// `NotFoundError` if any id names no row — unlike `association_exists`,
    /// this call is already committed to reading, so a stale reference is a
    /// failure rather than an answer.
    fn read_by_ids(&self, py: Python<'_>, ids: Vec<i64>) -> PyResult<Vec<Py<PyAny>>> {
        let ids = to_ids(&ids);
        self.store()?
            .read_by_ids(&ids, core_lib::ReadWindow::full())
            .map_err(map_err)?
            .into_iter()
            .map(|d| time_series_data_to_py(py, d))
            .collect()
    }

    /// Read many series by catalog id, each clipped to whatever lies within
    /// `time_range`.
    ///
    /// The *bounds* read beside `read_by_ids`' *window* read. A window says
    /// "these exact steps" and is checked; a range says "whatever falls between
    /// these instants" and clips to what is there — which is what an export
    /// naming a month of a store it did not write actually wants, since it does
    /// not know how many steps each series has in them.
    ///
    /// Both bounds must be spelled the way the series are — two aware
    /// datetimes, or two naive ones. A set mixing zoneless and instant-bearing
    /// series has no single valid spelling and raises `InvalidParameterError`
    /// rather than being resolved per series.
    fn read_by_ids_range(
        &self,
        py: Python<'_>,
        ids: Vec<i64>,
        time_range: (PyInstant, PyInstant),
    ) -> PyResult<Vec<Py<PyAny>>> {
        let range = range_to_core(Some(time_range))?.expect("a supplied range is always Some");
        self.store()?
            .read_by_ids_range(&to_ids(&ids), range)
            .map_err(map_err)?
            .into_iter()
            .map(|d| time_series_data_to_py(py, d))
            .collect()
    }

    /// Read one series by catalog id, or the window of it these arguments name,
    /// in a single call.
    ///
    /// The id is a primary-key lookup and its row carries the grid, so the store
    /// resolves the window itself: a caller holding an id needs no metadata read
    /// to ask for the second day of a series. With no keywords this is
    /// `read_by_ids` for one id.
    ///
    /// `start_time` is the first timestamp to read — a window boundary
    /// (`initial_timestamp + k * interval`) for a forecast — and must be spelled
    /// the way the series is, naive for a zoneless one and aware otherwise.
    /// `len` counts timesteps and applies to the static types; `count` counts
    /// windows and applies to the forecasts. Passing the one that does not apply
    /// raises `InvalidParameterError`.
    ///
    /// A window is checked, not clamped: a `start_time` off the series' grid, or
    /// a `len`/`count` running past its end, raises `InvalidParameterError` —
    /// where the `time_range` on `get_time_series` and `bulk_read` hands back
    /// the smaller answer that fits. Raises `NotFoundError` if the id names no
    /// row.
    ///
    /// Pass `owner_id` and `owner_category` together to hold the row to that
    /// owner, and get `OwnerMismatchError` when it belongs to someone else. The
    /// owner comes off the very row the values are materialized from, so the
    /// guarded read costs exactly what the unguarded one does — where confirming
    /// the owner in a call of its own would be a second round trip whose answer
    /// describes the row as it was rather than the row being read.
    #[pyo3(signature = (
        id, *, start_time=None, len=None, count=None, owner_id=None, owner_category=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn read_by_id(
        &self,
        py: Python<'_>,
        id: i64,
        start_time: Option<PyInstant>,
        len: Option<usize>,
        count: Option<usize>,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
    ) -> PyResult<Py<PyAny>> {
        let window = core_lib::ReadWindow {
            start: start_time.as_ref().map(|s| s.instant),
            zoneless: start_time.as_ref().is_some_and(|s| s.is_zoneless()),
            len,
            count,
        };
        let store = self.store()?;
        let id = core_lib::TimeSeriesId(id);
        let data = match owner_guard(owner_id, owner_category, "read_by_id")? {
            Some(owner) => store.read_by_id_for_owner(id, owner, window),
            None => store.read_by_id(id, window),
        }
        .map_err(map_err)?;
        time_series_data_to_py(py, data)
    }

    /// Remove many series by catalog id, in one all-or-nothing transaction.
    /// Returns the number removed.
    ///
    /// The removal direction of the id every write hands back: a caller that
    /// recorded ids in its own model retires one without rebuilding the key it
    /// was filed under, and an id names exactly one row where a key can match a
    /// whole forecast family. Raises `NotFoundError` if any id names no row,
    /// rolling the batch back — sift the set with `association_exists` first
    /// when some references are expected to have gone. A repeated id is removed
    /// once.
    ///
    /// Pass `owner_id` and `owner_category` together to remove only rows that
    /// belong to that owner; a single mismatch raises `OwnerMismatchError` and
    /// rolls the whole batch back. A caller that reasons about a series as one
    /// component's — "retire this component's series" — cannot assemble the guard
    /// out of the unguarded parts: an id survives `replace_owner`, so a
    /// `get_metadata_by_id` that confirms the owner and a `remove_by_ids` that
    /// then deletes leave a window in which a reassignment lands and the removal
    /// retires the *new* owner's series. Here the check and the delete are one
    /// transaction.
    #[pyo3(signature = (ids, *, owner_id=None, owner_category=None))]
    fn remove_by_ids(
        &mut self,
        ids: Vec<i64>,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
    ) -> PyResult<usize> {
        let owner = owner_guard(owner_id, owner_category, "remove_by_ids")?;
        let ids = to_ids(&ids);
        let store = self.store_mut()?;
        match owner {
            Some(owner) => store.remove_by_ids_for_owner(&ids, owner),
            None => store.remove_by_ids(&ids),
        }
        .map_err(map_err)
    }

    /// Distinct forecast intervals (ISO-8601 strings), optionally scoped to one
    /// time series type.
    #[pyo3(signature = (time_series_type=None))]
    fn get_intervals(&self, time_series_type: Option<Bound<'_, PyAny>>) -> PyResult<Vec<String>> {
        let requested = pyany_to_requested_type_opt(time_series_type.as_ref(), "time_series_type")?;
        Ok(self
            .store()?
            .get_intervals(requested)
            .map_err(map_err)?
            .into_iter()
            .map(|p| p.to_iso8601())
            .collect())
    }

    /// Distinct series names matching the filter, sorted.
    #[pyo3(signature = (
        *, owner_id=None, owner_category=None, owner_type=None, time_series_type=None,
        name=None, name_glob=None, component_field=None, zoneless=None, resolution=None,
        interval=None, features=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn list_names(
        &self,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
        owner_type: Option<String>,
        time_series_type: Option<Bound<'_, PyAny>>,
        name: Option<String>,
        name_glob: Option<String>,
        component_field: Option<String>,
        zoneless: Option<bool>,
        resolution: Option<Bound<'_, PyAny>>,
        interval: Option<Bound<'_, PyAny>>,
        features: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<String>> {
        let filter = build_list_filter(
            owner_id,
            owner_category,
            owner_type,
            time_series_type.as_ref(),
            name,
            name_glob,
            component_field,
            zoneless,
            resolution,
            interval,
            features,
        )?;
        self.store()?.list_names(filter).map_err(map_err)
    }

    /// Distinct owner types matching the filter, sorted.
    #[pyo3(signature = (
        *, owner_id=None, owner_category=None, owner_type=None, time_series_type=None,
        name=None, name_glob=None, component_field=None, zoneless=None, resolution=None,
        interval=None, features=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn list_owner_types(
        &self,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
        owner_type: Option<String>,
        time_series_type: Option<Bound<'_, PyAny>>,
        name: Option<String>,
        name_glob: Option<String>,
        component_field: Option<String>,
        zoneless: Option<bool>,
        resolution: Option<Bound<'_, PyAny>>,
        interval: Option<Bound<'_, PyAny>>,
        features: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<String>> {
        let filter = build_list_filter(
            owner_id,
            owner_category,
            owner_type,
            time_series_type.as_ref(),
            name,
            name_glob,
            component_field,
            zoneless,
            resolution,
            interval,
            features,
        )?;
        self.store()?.list_owner_types(filter).map_err(map_err)
    }

    /// Remove every series matching the filter in one all-or-nothing
    /// transaction. Returns the number of associations removed.
    #[pyo3(signature = (
        *, owner_id=None, owner_category=None, owner_type=None, time_series_type=None,
        name=None, name_glob=None, component_field=None, zoneless=None, resolution=None,
        interval=None, features=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn remove_by_filter(
        &mut self,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
        owner_type: Option<String>,
        time_series_type: Option<Bound<'_, PyAny>>,
        name: Option<String>,
        name_glob: Option<String>,
        component_field: Option<String>,
        zoneless: Option<bool>,
        resolution: Option<Bound<'_, PyAny>>,
        interval: Option<Bound<'_, PyAny>>,
        features: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<usize> {
        let filter = build_list_filter(
            owner_id,
            owner_category,
            owner_type,
            time_series_type.as_ref(),
            name,
            name_glob,
            component_field,
            zoneless,
            resolution,
            interval,
            features,
        )?;
        self.store_mut()?.remove_by_filter(filter).map_err(map_err)
    }

    /// Copy an association onto another owner, optionally renaming it. Shares the
    /// underlying array (no data is duplicated). Returns the new key.
    #[pyo3(signature = (src, dst_owner_id, dst_owner_type, *, new_name=None))]
    fn copy_time_series(
        &mut self,
        src: i64,
        dst_owner_id: i64,
        dst_owner_type: &str,
        new_name: Option<String>,
    ) -> PyResult<i64> {
        self.store_mut()?
            .copy_time_series(
                core_lib::TimeSeriesId(src),
                dst_owner_id,
                dst_owner_type,
                new_name.as_deref(),
            )
            .map_err(map_err)
            .map(|id| id.get())
    }

    /// Distinct owner ids of `owner_category` that have a time series, optionally
    /// restricted by type and/or resolution.
    #[pyo3(signature = (owner_category, *, time_series_type=None, resolution=None))]
    fn list_owner_ids(
        &self,
        owner_category: PyOwnerCategory,
        time_series_type: Option<Bound<'_, PyAny>>,
        resolution: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Vec<i64>> {
        let resolution = match resolution {
            Some(r) => Some(pyany_to_period(&r)?),
            None => None,
        };
        let requested = pyany_to_requested_type_opt(time_series_type.as_ref(), "time_series_type")?;
        self.store()?
            .list_owner_ids(owner_category.into(), requested, resolution)
            .map_err(map_err)
    }

    /// Grouped static-series summary: one dict per distinct owner/name/shape
    /// combination with the association `count`.
    fn static_summary<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let iso = |p: Option<core_lib::Period>| p.map(|x| x.to_iso8601());
        self.store()?
            .static_summary()
            .map_err(map_err)?
            .iter()
            .map(|r| {
                let d = PyDict::new(py);
                d.set_item("owner_type", &r.owner_type)?;
                d.set_item("owner_category", r.owner_category.as_str())?;
                d.set_item("time_series_type", r.time_series_type.as_str())?;
                d.set_item("name", &r.name)?;
                d.set_item(
                    "initial_timestamp",
                    r.initial_timestamp.map(|t| t.to_rfc3339()),
                )?;
                d.set_item("resolution", iso(r.resolution))?;
                d.set_item("time_step_count", r.time_step_count)?;
                d.set_item("count", r.count)?;
                Ok(d)
            })
            .collect()
    }

    /// Grouped forecast summary: one dict per distinct owner/name/window
    /// configuration with the association `count`.
    fn forecast_summary<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let iso = |p: Option<core_lib::Period>| p.map(|x| x.to_iso8601());
        self.store()?
            .forecast_summary()
            .map_err(map_err)?
            .iter()
            .map(|r| {
                let d = PyDict::new(py);
                d.set_item("owner_type", &r.owner_type)?;
                d.set_item("owner_category", r.owner_category.as_str())?;
                d.set_item("time_series_type", r.time_series_type.as_str())?;
                d.set_item("name", &r.name)?;
                d.set_item(
                    "initial_timestamp",
                    r.initial_timestamp.map(|t| t.to_rfc3339()),
                )?;
                d.set_item("resolution", iso(r.resolution))?;
                d.set_item("horizon", iso(r.horizon))?;
                d.set_item("interval", iso(r.interval))?;
                d.set_item("window_count", r.window_count)?;
                d.set_item("count", r.count)?;
                Ok(d)
            })
            .collect()
    }

    /// Association count grouped by time series type, as a `dict[str, int]`.
    fn counts_by_type<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for (t, n) in self.store()?.counts_by_type().map_err(map_err)? {
            d.set_item(t.as_str(), n)?;
        }
        Ok(d)
    }

    /// Number of distinct stored arrays (shared series count once).
    fn num_distinct_arrays(&self) -> PyResult<i64> {
        self.store()?.num_distinct_arrays().map_err(map_err)
    }

    /// Distinct owners per category and distinct stored arrays per kind, as a
    /// dict.
    fn time_series_counts_detailed<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let c = self
            .store()?
            .time_series_counts_detailed()
            .map_err(map_err)?;
        let d = PyDict::new(py);
        d.set_item("components_with_time_series", c.components_with_time_series)?;
        d.set_item(
            "supplemental_attributes_with_time_series",
            c.supplemental_attributes_with_time_series,
        )?;
        d.set_item("static_time_series_count", c.static_time_series_count)?;
        d.set_item("forecast_count", c.forecast_count)?;
        Ok(d)
    }

    /// Verify per-resolution static-grid consistency. Returns one dict
    /// (`resolution`, `initial_timestamp`, `length`) per resolution present;
    /// `resolution`, if given, scopes the check. Raises on divergence.
    #[pyo3(signature = (resolution=None))]
    fn check_static_consistency<'py>(
        &self,
        py: Python<'py>,
        resolution: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let resolution = match resolution {
            Some(r) => Some(pyany_to_period(&r)?),
            None => None,
        };
        self.store()?
            .check_static_consistency(resolution)
            .map_err(map_err)?
            .iter()
            .map(|c| {
                let d = PyDict::new(py);
                d.set_item("resolution", c.resolution.to_iso8601())?;
                d.set_item("initial_timestamp", c.initial_timestamp.to_rfc3339())?;
                d.set_item("length", c.length)?;
                Ok(d)
            })
            .collect()
    }

    /// Count the `SingleTimeSeries` and `DeterministicSingleTimeSeries`
    /// associations referencing the array `data_hash` (a 64-char hex string),
    /// as a dict `{"sts": int, "dst": int}`.
    fn count_array_references<'py>(
        &self,
        py: Python<'py>,
        data_hash: &str,
    ) -> PyResult<Bound<'py, PyDict>> {
        let hash = hash_from_hex(data_hash)?;
        let (sts, dst) = self
            .store()?
            .count_array_references(&hash)
            .map_err(map_err)?;
        let d = PyDict::new(py);
        d.set_item("sts", sts)?;
        d.set_item("dst", dst)?;
        Ok(d)
    }

    /// Fetch a stored array by its content hash (a 64-char hex string) as a numpy
    /// array in its native dtype and shape.
    fn get_array_by_hash<'py>(
        &self,
        py: Python<'py>,
        data_hash: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        let hash = hash_from_hex(data_hash)?;
        let arr = self.store()?.get_array_by_hash(&hash).map_err(map_err)?;
        numpy_from_typed(py, &arr)
    }

    /// Persist the store to a new HDF5 + SQLite artifact at `path`.
    fn persist_to(&mut self, path: PathBuf) -> PyResult<()> {
        self.store_mut()?.persist_to(&path).map_err(map_err)
    }

    /// Write an in-memory catalog to this store's own `<path>.sqlite`, pairing
    /// it with the HDF5 file already there.
    ///
    /// `persist_to()` aimed at another path copies the arrays; this writes only
    /// the catalog, because the arrays are already where they belong. That makes
    /// `catalog="memory"` usable for what it is good for — skipping per-commit
    /// journaling during a bulk load — without copying the array file to land
    /// the result.
    ///
    /// A checkpoint, not a mode switch: the catalog stays in memory, and later
    /// changes are again RAM-only until the next call. For `catalog="attached"`
    /// this is `flush()`.
    fn persist_catalog(&mut self) -> PyResult<()> {
        self.store_mut()?.persist_catalog().map_err(map_err)
    }

    // ---- Supplemental-attribute associations ------------------------------
    //
    // Which supplemental attributes are attached to which components. The store
    // holds the relationship only. Attachments are independent of time series in
    // both directions: removing a time series never removes an attachment, and
    // vice versa.

    /// Attach a supplemental attribute to a component. Raises
    /// `DuplicateAssociationError` if that component already carries that
    /// attribute, whatever type names are supplied.
    /// Attach a supplemental attribute to a component, returning the catalog id
    /// the attachment was filed under.
    fn add_supplemental_attribute_association(
        &mut self,
        association: &PySupplementalAttributeAssociation,
    ) -> PyResult<i64> {
        self.store_mut()?
            .add_supplemental_attribute_association(association.inner.clone())
            .map_err(map_err)
    }

    /// Attach many in one all-or-nothing transaction, returning the catalog id
    /// of each in input order. A duplicate anywhere in the batch rolls the batch
    /// back. This is the import half of the bulk round trip whose export is
    /// `list_supplemental_attribute_associations()` with no filter.
    ///
    /// The count is `len()` on the result.
    fn add_supplemental_attribute_associations(
        &mut self,
        associations: Vec<PySupplementalAttributeAssociation>,
    ) -> PyResult<Vec<i64>> {
        let assocs = associations.into_iter().map(|a| a.inner).collect();
        self.store_mut()?
            .add_supplemental_attribute_associations(assocs)
            .map_err(map_err)
    }

    /// Whether any attachment matches the filter.
    #[pyo3(signature = (*, component_id=None, component_types=None, attribute_id=None, attribute_types=None))]
    fn has_supplemental_attribute_association(
        &self,
        component_id: Option<i64>,
        component_types: Option<Vec<String>>,
        attribute_id: Option<i64>,
        attribute_types: Option<Vec<String>>,
    ) -> PyResult<bool> {
        let filter = build_supplemental_attribute_filter(
            component_id,
            component_types,
            attribute_id,
            attribute_types,
        );
        self.store()?
            .has_supplemental_attribute_association(&filter)
            .map_err(map_err)
    }

    /// Full attachment rows matching the filter, in insertion order. Passing no
    /// filter exports the whole table.
    #[pyo3(signature = (*, component_id=None, component_types=None, attribute_id=None, attribute_types=None))]
    fn list_supplemental_attribute_associations(
        &self,
        component_id: Option<i64>,
        component_types: Option<Vec<String>>,
        attribute_id: Option<i64>,
        attribute_types: Option<Vec<String>>,
    ) -> PyResult<Vec<PySupplementalAttributeAssociation>> {
        let filter = build_supplemental_attribute_filter(
            component_id,
            component_types,
            attribute_id,
            attribute_types,
        );
        Ok(self
            .store()?
            .list_supplemental_attribute_associations(&filter)
            .map_err(map_err)?
            .into_iter()
            .map(|inner| PySupplementalAttributeAssociation { inner })
            .collect())
    }

    /// Distinct attribute ids matching the filter, ascending — the attributes
    /// attached to one component when `component_id` is given.
    #[pyo3(signature = (*, component_id=None, component_types=None, attribute_id=None, attribute_types=None))]
    fn list_supplemental_attribute_ids(
        &self,
        component_id: Option<i64>,
        component_types: Option<Vec<String>>,
        attribute_id: Option<i64>,
        attribute_types: Option<Vec<String>>,
    ) -> PyResult<Vec<i64>> {
        let filter = build_supplemental_attribute_filter(
            component_id,
            component_types,
            attribute_id,
            attribute_types,
        );
        self.store()?
            .list_supplemental_attribute_ids(&filter)
            .map_err(map_err)
    }

    /// Distinct component ids matching the filter, ascending — the components
    /// carrying one attribute when `attribute_id` is given.
    #[pyo3(signature = (*, component_id=None, component_types=None, attribute_id=None, attribute_types=None))]
    fn list_components_with_attributes(
        &self,
        component_id: Option<i64>,
        component_types: Option<Vec<String>>,
        attribute_id: Option<i64>,
        attribute_types: Option<Vec<String>>,
    ) -> PyResult<Vec<i64>> {
        let filter = build_supplemental_attribute_filter(
            component_id,
            component_types,
            attribute_id,
            attribute_types,
        );
        self.store()?
            .list_components_with_attributes(&filter)
            .map_err(map_err)
    }

    /// Remove every attachment matching the filter, returning how many were
    /// removed. Matching nothing returns 0 rather than raising: only the caller
    /// knows whether a hit was expected.
    #[pyo3(signature = (*, component_id=None, component_types=None, attribute_id=None, attribute_types=None))]
    fn remove_supplemental_attribute_associations(
        &mut self,
        component_id: Option<i64>,
        component_types: Option<Vec<String>>,
        attribute_id: Option<i64>,
        attribute_types: Option<Vec<String>>,
    ) -> PyResult<usize> {
        let filter = build_supplemental_attribute_filter(
            component_id,
            component_types,
            attribute_id,
            attribute_types,
        );
        self.store_mut()?
            .remove_supplemental_attribute_associations(&filter)
            .map_err(map_err)
    }

    /// Move every attachment from component `old_id` to `new_id`, returning the
    /// rows updated. Raises `DuplicateAssociationError` if `new_id` already
    /// carries one of the attributes being moved.
    fn replace_supplemental_attribute_component_id(
        &mut self,
        old_id: i64,
        new_id: i64,
    ) -> PyResult<usize> {
        self.store_mut()?
            .replace_supplemental_attribute_component_id(old_id, new_id)
            .map_err(map_err)
    }

    /// Number of attachments matching the filter.
    #[pyo3(signature = (*, component_id=None, component_types=None, attribute_id=None, attribute_types=None))]
    fn count_supplemental_attribute_associations(
        &self,
        component_id: Option<i64>,
        component_types: Option<Vec<String>>,
        attribute_id: Option<i64>,
        attribute_types: Option<Vec<String>>,
    ) -> PyResult<i64> {
        let filter = build_supplemental_attribute_filter(
            component_id,
            component_types,
            attribute_id,
            attribute_types,
        );
        self.store()?
            .count_supplemental_attribute_associations(&filter)
            .map_err(map_err)
    }

    /// Number of *distinct* attributes among the attachments matching the
    /// filter.
    #[pyo3(signature = (*, component_id=None, component_types=None, attribute_id=None, attribute_types=None))]
    fn count_supplemental_attributes(
        &self,
        component_id: Option<i64>,
        component_types: Option<Vec<String>>,
        attribute_id: Option<i64>,
        attribute_types: Option<Vec<String>>,
    ) -> PyResult<i64> {
        let filter = build_supplemental_attribute_filter(
            component_id,
            component_types,
            attribute_id,
            attribute_types,
        );
        self.store()?
            .count_supplemental_attributes(&filter)
            .map_err(map_err)
    }

    /// Number of *distinct* components among the attachments matching the
    /// filter.
    #[pyo3(signature = (*, component_id=None, component_types=None, attribute_id=None, attribute_types=None))]
    fn count_components_with_attributes(
        &self,
        component_id: Option<i64>,
        component_types: Option<Vec<String>>,
        attribute_id: Option<i64>,
        attribute_types: Option<Vec<String>>,
    ) -> PyResult<i64> {
        let filter = build_supplemental_attribute_filter(
            component_id,
            component_types,
            attribute_id,
            attribute_types,
        );
        self.store()?
            .count_components_with_attributes(&filter)
            .map_err(map_err)
    }

    /// Attachment counts grouped by attribute type, as `[(type_name, count), …]`.
    fn supplemental_attribute_counts_by_type(&self) -> PyResult<Vec<(String, i64)>> {
        self.store()?
            .supplemental_attribute_counts_by_type()
            .map_err(map_err)
    }

    /// Attachment counts grouped by both type names: one dict per distinct pair
    /// with keys `component_type`, `attribute_type`, `count`.
    fn supplemental_attribute_summary<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        self.store()?
            .supplemental_attribute_summary()
            .map_err(map_err)?
            .iter()
            .map(|r| {
                let d = PyDict::new(py);
                d.set_item("component_type", &r.component_type)?;
                d.set_item("attribute_type", &r.attribute_type)?;
                d.set_item("count", r.count)?;
                Ok(d)
            })
            .collect()
    }

    // ---- Parent/child associations ----------------------------------------
    //
    // Directed edges between components. Same independence from time series as
    // the attachments above.

    /// Record a parent/child edge, returning the catalog id it was filed under.
    /// Raises `DuplicateAssociationError` if that ordered pair is already
    /// related; the reversed pair is a different edge.
    fn add_parent_child_association(
        &mut self,
        association: &PyParentChildAssociation,
    ) -> PyResult<i64> {
        self.store_mut()?
            .add_parent_child_association(association.inner.clone())
            .map_err(map_err)
    }

    /// Record many edges in one all-or-nothing transaction, returning the
    /// catalog id of each in input order. The count is `len()` on the result.
    fn add_parent_child_associations(
        &mut self,
        associations: Vec<PyParentChildAssociation>,
    ) -> PyResult<Vec<i64>> {
        let assocs = associations.into_iter().map(|a| a.inner).collect();
        self.store_mut()?
            .add_parent_child_associations(assocs)
            .map_err(map_err)
    }

    /// Whether any edge matches the filter.
    #[pyo3(signature = (*, parent_id=None, parent_types=None, child_id=None, child_types=None))]
    fn has_parent_child_association(
        &self,
        parent_id: Option<i64>,
        parent_types: Option<Vec<String>>,
        child_id: Option<i64>,
        child_types: Option<Vec<String>>,
    ) -> PyResult<bool> {
        let filter = build_parent_child_filter(parent_id, parent_types, child_id, child_types);
        self.store()?
            .has_parent_child_association(&filter)
            .map_err(map_err)
    }

    /// Full edge rows matching the filter, in insertion order. Passing no filter
    /// exports the whole table.
    #[pyo3(signature = (*, parent_id=None, parent_types=None, child_id=None, child_types=None))]
    fn list_parent_child_associations(
        &self,
        parent_id: Option<i64>,
        parent_types: Option<Vec<String>>,
        child_id: Option<i64>,
        child_types: Option<Vec<String>>,
    ) -> PyResult<Vec<PyParentChildAssociation>> {
        let filter = build_parent_child_filter(parent_id, parent_types, child_id, child_types);
        Ok(self
            .store()?
            .list_parent_child_associations(&filter)
            .map_err(map_err)?
            .into_iter()
            .map(|inner| PyParentChildAssociation { inner })
            .collect())
    }

    /// Distinct child ids matching the filter, ascending — the children of one
    /// component when `parent_id` is given.
    #[pyo3(signature = (*, parent_id=None, parent_types=None, child_id=None, child_types=None))]
    fn list_children(
        &self,
        parent_id: Option<i64>,
        parent_types: Option<Vec<String>>,
        child_id: Option<i64>,
        child_types: Option<Vec<String>>,
    ) -> PyResult<Vec<i64>> {
        let filter = build_parent_child_filter(parent_id, parent_types, child_id, child_types);
        self.store()?.list_children(&filter).map_err(map_err)
    }

    /// Distinct parent ids matching the filter, ascending — the parents of one
    /// component when `child_id` is given.
    #[pyo3(signature = (*, parent_id=None, parent_types=None, child_id=None, child_types=None))]
    fn list_parents(
        &self,
        parent_id: Option<i64>,
        parent_types: Option<Vec<String>>,
        child_id: Option<i64>,
        child_types: Option<Vec<String>>,
    ) -> PyResult<Vec<i64>> {
        let filter = build_parent_child_filter(parent_id, parent_types, child_id, child_types);
        self.store()?.list_parents(&filter).map_err(map_err)
    }

    /// Remove every edge matching the filter, returning how many were removed.
    /// Matching nothing returns 0 rather than raising.
    #[pyo3(signature = (*, parent_id=None, parent_types=None, child_id=None, child_types=None))]
    fn remove_parent_child_associations(
        &mut self,
        parent_id: Option<i64>,
        parent_types: Option<Vec<String>>,
        child_id: Option<i64>,
        child_types: Option<Vec<String>>,
    ) -> PyResult<usize> {
        let filter = build_parent_child_filter(parent_id, parent_types, child_id, child_types);
        self.store_mut()?
            .remove_parent_child_associations(&filter)
            .map_err(map_err)
    }

    /// Rewrite component `old_id` to `new_id` on both ends of every edge,
    /// returning the rows updated. Raises `DuplicateAssociationError` if the
    /// rewrite would duplicate an edge `new_id` already has.
    fn replace_parent_child_component_id(&mut self, old_id: i64, new_id: i64) -> PyResult<usize> {
        self.store_mut()?
            .replace_parent_child_component_id(old_id, new_id)
            .map_err(map_err)
    }

    /// Number of edges matching the filter.
    #[pyo3(signature = (*, parent_id=None, parent_types=None, child_id=None, child_types=None))]
    fn count_parent_child_associations(
        &self,
        parent_id: Option<i64>,
        parent_types: Option<Vec<String>>,
        child_id: Option<i64>,
        child_types: Option<Vec<String>>,
    ) -> PyResult<i64> {
        let filter = build_parent_child_filter(parent_id, parent_types, child_id, child_types);
        self.store()?
            .count_parent_child_associations(&filter)
            .map_err(map_err)
    }

    // ---- OpenAPI-row association serde -------------------------------------
    //
    // Direct JSON serde of the two association catalogs, in the wire spelling
    // SiennaSchemas defines. The Rust core (`infrastore_core::openapi`) owns
    // the mapping between catalog rows and schema rows; these four methods are
    // a thin wrapper over it.

    /// Export `time_series_associations` matching the filter (the same filter
    /// keywords as `list_metadata`) as a sorted OpenAPI-row JSON array.
    /// Each row's `uri` and `data_hash` are the hex-encoded content hash the
    /// store already has for that row — never a caller-supplied locator.
    /// With no filter this exports the whole catalog.
    #[pyo3(signature = (
        *, owner_id=None, owner_category=None, owner_type=None, time_series_type=None,
        name=None, name_glob=None, component_field=None, zoneless=None, resolution=None,
        interval=None, features=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn export_time_series_associations_openapi(
        &self,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
        owner_type: Option<String>,
        time_series_type: Option<Bound<'_, PyAny>>,
        name: Option<String>,
        name_glob: Option<String>,
        component_field: Option<String>,
        zoneless: Option<bool>,
        resolution: Option<Bound<'_, PyAny>>,
        interval: Option<Bound<'_, PyAny>>,
        features: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<String> {
        let filter = build_list_filter(
            owner_id,
            owner_category,
            owner_type,
            time_series_type.as_ref(),
            name,
            name_glob,
            component_field,
            zoneless,
            resolution,
            interval,
            features,
        )?;
        self.store()?
            .export_time_series_associations_openapi(&filter)
            .map_err(map_err)
    }

    /// Bulk-ingest a JSON array of time-series association OpenAPI rows in one
    /// all-or-nothing transaction, returning the number inserted. This is the
    /// import half of the round trip whose export is
    /// `export_time_series_associations_openapi()`.
    ///
    /// Rows only: the document carries locators, never values, so every row
    /// must name an array this store already holds, and each row keeps the
    /// `association_id` it carries — an import that assigned fresh ids would
    /// leave every reference the document records pointing at the wrong
    /// series. A row whose array is absent, or a `NonSequentialTimeSeries` row
    /// (whose timestamp vector is not on the wire), raises
    /// `InvalidParameterError`.
    fn import_time_series_associations_openapi(&mut self, json: &str) -> PyResult<usize> {
        self.store_mut()?
            .import_time_series_associations_openapi(json)
            .map_err(map_err)
    }

    /// Export the whole `supplemental_attribute_associations` table as an
    /// OpenAPI-row JSON array, sorted by `(component_id, attribute_id)`.
    fn export_supplemental_attribute_associations_openapi(&self) -> PyResult<String> {
        self.store()?
            .export_supplemental_attribute_associations_openapi()
            .map_err(map_err)
    }

    /// Bulk-ingest a JSON array of supplemental-attribute association OpenAPI
    /// rows in one all-or-nothing transaction, returning the number inserted.
    /// This is the import half of the round trip whose export is
    /// `export_supplemental_attribute_associations_openapi()`.
    fn import_supplemental_attribute_associations_openapi(
        &mut self,
        json: &str,
    ) -> PyResult<usize> {
        self.store_mut()?
            .import_supplemental_attribute_associations_openapi(json)
            .map_err(map_err)
    }
}

// ---- period helpers -------------------------------------------------------

/// Accept a period as either a `datetime.timedelta` (fixed span) or an ISO-8601
/// duration `str` (e.g. "PT1H", "P1M", "P1Y"); the latter is required for
/// calendar (irregular) periods.
fn pyany_to_period(v: &Bound<'_, PyAny>) -> PyResult<core_lib::Period> {
    if let Ok(s) = v.extract::<String>() {
        // A malformed value stays inside the library's exception hierarchy;
        // only a wholly wrong argument type raises TypeError below.
        core_lib::Period::from_iso8601(&s)
            .map_err(|e| InvalidParameterError::new_err(e.to_string()))
    } else if let Ok(d) = v.extract::<chrono::Duration>() {
        Ok(core_lib::Period::Fixed(d))
    } else {
        Err(pyo3::exceptions::PyTypeError::new_err(
            "period must be a datetime.timedelta or an ISO-8601 duration string",
        ))
    }
}

// ---- requested-type helpers -----------------------------------------------

/// Accept a requested time series type as a `TimeSeriesType`.
///
/// `TimeSeriesType.Deterministic` also matches a stored
/// `DeterministicSingleTimeSeries` — the transform is an implementation detail
/// of how a forecast is stored, and it reads back as a `Deterministic` either
/// way. `TimeSeriesType.DeterministicSingleTimeSeries` narrows to the
/// transformed form, which is how a caller inspects what it has.
///
/// `param` names the argument in error messages.
///
/// The enum member and its name are both accepted. The name is what the
/// docstrings and the type stub have always shown
/// (`time_series_type="NonSequentialTimeSeries"`), and what a value read back
/// out of a metadata dict already is, so refusing it made the documented call
/// fail and forced a round trip through the enum for no gain.
fn pyany_to_requested_type(
    v: &Bound<'_, PyAny>,
    param: &str,
) -> PyResult<core_lib::TimeSeriesType> {
    if let Ok(t) = v.extract::<PyTimeSeriesType>() {
        return Ok(t.into());
    }
    if let Ok(name) = v.extract::<String>() {
        return core_lib::TimeSeriesType::parse(&name).ok_or_else(|| {
            InvalidParameterError::new_err(format!(
                "{param} '{name}' is not a time series type; expected one of {}",
                TIME_SERIES_TYPE_NAMES.join(", ")
            ))
        });
    }
    Err(pyo3::exceptions::PyTypeError::new_err(format!(
        "{param} must be a TimeSeriesType or one of its names ({})",
        TIME_SERIES_TYPE_NAMES.join(", ")
    )))
}

/// Every `TimeSeriesType` spelling, for the messages above. Held here rather
/// than derived, so a new variant shows up as a failing test rather than a
/// silently short list.
const TIME_SERIES_TYPE_NAMES: [&str; 6] = [
    "SingleTimeSeries",
    "NonSequentialTimeSeries",
    "Deterministic",
    "DeterministicSingleTimeSeries",
    "Probabilistic",
    "Scenarios",
];

/// [`pyany_to_requested_type`] over an optional argument, for the filter kwargs
/// that default to "any type".
fn pyany_to_requested_type_opt(
    v: Option<&Bound<'_, PyAny>>,
    param: &str,
) -> PyResult<Option<core_lib::TimeSeriesType>> {
    match v {
        Some(v) => Ok(Some(pyany_to_requested_type(v, param)?)),
        None => Ok(None),
    }
}

/// Decode a 64-character lowercase-or-uppercase hex string into a 32-byte hash.
/// Every key `add_time_series_bulk` reads out of one item dict.
const BULK_ITEM_KEYS: [&str; 13] = [
    "owner_id",
    "owner_type",
    "owner_category",
    "time_series",
    "features",
    "units",
    "element_type",
    "application_data",
    "quantity_kind",
    "unit_system",
    "time_reference",
    "component_field",
    "id",
];

/// Refuse an item carrying a key the bulk add does not read.
///
/// The single-series path gets this from PyO3, which rejects an unexpected
/// keyword argument. The bulk path takes a dict and reads the keys it knows, so
/// a misspelled one -- `unit_sytem`, `owner_typ` -- was simply not applied, and
/// the series landed missing whatever that key carried, with no error and no
/// way to notice short of reading every row back.
fn reject_unknown_item_keys(item: &Bound<'_, PyDict>, index: usize) -> PyResult<()> {
    for key in item.keys() {
        let name: String = match key.extract() {
            Ok(n) => n,
            Err(_) => {
                return Err(InvalidParameterError::new_err(format!(
                    "bulk add item {index} has a non-string key {key}"
                )));
            }
        };
        if !BULK_ITEM_KEYS.contains(&name.as_str()) {
            return Err(InvalidParameterError::new_err(format!(
                "bulk add item {index} has unknown key '{name}'; expected one of {}",
                BULK_ITEM_KEYS.join(", ")
            )));
        }
    }
    Ok(())
}

fn hash_from_hex(s: &str) -> PyResult<[u8; 32]> {
    // Over bytes, not `&str` slices: the length guard counts bytes, so a
    // 64-*byte* string of multi-byte characters passed it and then sliced
    // through a character boundary. That panics, and PyO3 surfaces a panic as
    // `PanicException`, which inherits from `BaseException` and so escapes both
    // `except Exception` and this module's own exception hierarchy — an
    // uncatchable error from an ordinary bad argument.
    let bytes = s.as_bytes();
    if bytes.len() != 64 || !s.is_ascii() {
        return Err(InvalidParameterError::new_err(
            "data_hash must be a 64-character hex string",
        ));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = std::str::from_utf8(&bytes[i * 2..i * 2 + 2])
            .map_err(|_| InvalidParameterError::new_err("data_hash is not valid hex"))?;
        *byte = u8::from_str_radix(pair, 16)
            .map_err(|_| InvalidParameterError::new_err("data_hash is not valid hex"))?;
    }
    Ok(out)
}

/// Build a [`core_lib::ListFilter`] from the optional filter kwargs shared by the
/// listing/removal methods. `resolution` and `interval` accept a `timedelta` or
/// an ISO-8601 duration string (see [`pyany_to_period`]); `time_series_type`
/// accepts a `TimeSeriesType` or the family string (see
/// [`pyany_to_requested_type`]).
#[allow(clippy::too_many_arguments)]
fn build_list_filter(
    owner_id: Option<i64>,
    owner_category: Option<PyOwnerCategory>,
    owner_type: Option<String>,
    time_series_type: Option<&Bound<'_, PyAny>>,
    name: Option<String>,
    name_glob: Option<String>,
    component_field: Option<String>,
    zoneless: Option<bool>,
    resolution: Option<Bound<'_, PyAny>>,
    interval: Option<Bound<'_, PyAny>>,
    features: Option<&Bound<'_, PyDict>>,
) -> PyResult<core_lib::ListFilter> {
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
        filter = filter.time_series_type(pyany_to_requested_type(t, "time_series_type")?);
    }
    if let Some(n) = name {
        filter = filter.name(n);
    }
    if let Some(g) = name_glob {
        filter = filter.name_glob(g);
    }
    if let Some(f) = component_field {
        filter = filter.component_field(f);
    }
    if let Some(z) = zoneless {
        filter = filter.zoneless(z);
    }
    if let Some(r) = resolution {
        filter = filter.resolution(pyany_to_period(&r)?);
    }
    if let Some(i) = interval {
        filter = filter.interval(pyany_to_period(&i)?);
    }
    if let Some(f) = features {
        filter = filter.features(features_from_dict(Some(f))?);
    }
    Ok(filter)
}

/// Build a core [`SupplementalAttributeFilter`](core_lib::SupplementalAttributeFilter)
/// from the keyword-only filter arguments every supplemental-attribute method
/// takes. An omitted argument leaves the field unconstrained; an empty type list
/// is an empty allow-list and matches nothing (the core's rule, preserved here).
fn build_supplemental_attribute_filter(
    component_id: Option<i64>,
    component_types: Option<Vec<String>>,
    attribute_id: Option<i64>,
    attribute_types: Option<Vec<String>>,
) -> core_lib::SupplementalAttributeFilter {
    let mut filter = core_lib::SupplementalAttributeFilter::new();
    if let Some(id) = component_id {
        filter = filter.component_id(id);
    }
    if let Some(t) = component_types {
        filter = filter.component_types(t);
    }
    if let Some(id) = attribute_id {
        filter = filter.attribute_id(id);
    }
    if let Some(t) = attribute_types {
        filter = filter.attribute_types(t);
    }
    filter
}

/// Build a core [`ParentChildFilter`](core_lib::ParentChildFilter) from the
/// keyword-only filter arguments every parent/child method takes. Same
/// omitted-vs-empty rules as [`build_supplemental_attribute_filter`].
fn build_parent_child_filter(
    parent_id: Option<i64>,
    parent_types: Option<Vec<String>>,
    child_id: Option<i64>,
    child_types: Option<Vec<String>>,
) -> core_lib::ParentChildFilter {
    let mut filter = core_lib::ParentChildFilter::new();
    if let Some(id) = parent_id {
        filter = filter.parent_id(id);
    }
    if let Some(t) = parent_types {
        filter = filter.parent_types(t);
    }
    if let Some(id) = child_id {
        filter = filter.child_id(id);
    }
    if let Some(t) = child_types {
        filter = filter.child_types(t);
    }
    filter
}

/// Build the full metadata dict for one association row (shared by
/// `list_metadata` and `get_metadata_by_id`).
/// Python hands ids over as plain integers; the core addresses series with the
/// newtype that keeps them apart from every other id stream.
fn to_ids(ids: &[i64]) -> Vec<core_lib::TimeSeriesId> {
    ids.iter().copied().map(core_lib::TimeSeriesId).collect()
}

/// Read the optional `(owner_id, owner_category)` guard off an id-addressed
/// call. Both or neither: an owner is the pair, since a component and a
/// supplemental attribute can share an id, so half of one would be a guard that
/// silently checks less than the caller asked for.
fn owner_guard(
    owner_id: Option<i64>,
    owner_category: Option<PyOwnerCategory>,
    method: &str,
) -> PyResult<Option<(i64, core_lib::OwnerCategory)>> {
    match (owner_id, owner_category) {
        (Some(id), Some(cat)) => Ok(Some((id, cat.into()))),
        (None, None) => Ok(None),
        _ => Err(InvalidParameterError::new_err(format!(
            "{method} requires both owner_id and owner_category, or neither"
        ))),
    }
}

fn metadata_to_dict<'py>(
    py: Python<'py>,
    m: &core_lib::TimeSeriesMetadata,
) -> PyResult<Bound<'py, PyDict>> {
    let iso = |p: Option<core_lib::Period>| p.map(|x| x.to_iso8601());
    let d = PyDict::new(py);
    d.set_item("owner_id", m.owner_id)?;
    d.set_item("owner_type", &m.owner_type)?;
    d.set_item("owner_category", m.owner_category.as_str())?;
    d.set_item("time_series_type", m.time_series_type.as_str())?;
    d.set_item("name", &m.name)?;
    d.set_item("data_hash", core_lib::hash_hex(&m.data_hash))?;
    // Always set on a row read out of the catalog; `None` only for metadata a
    // caller built itself.
    d.set_item("id", m.id.map(|id| id.get()))?;
    d.set_item(
        "initial_timestamp",
        m.initial_timestamp
            .map(|t| render_catalog_timestamp(t, m.time_reference.as_ref())),
    )?;
    d.set_item("length", m.length)?;
    d.set_item("resolution", iso(m.resolution))?;
    d.set_item("horizon", iso(m.horizon))?;
    d.set_item("interval", iso(m.interval))?;
    d.set_item("count", m.count)?;
    d.set_item("percentiles", m.percentiles.clone())?;
    d.set_item("element_type", m.element_type.to_string())?;
    d.set_item("element_shape", m.element_shape.clone())?;
    d.set_item(
        "timestamps",
        m.timestamps.as_ref().map(|ts| {
            ts.iter()
                .map(|t| render_catalog_timestamp(*t, m.time_reference.as_ref()))
                .collect::<Vec<_>>()
        }),
    )?;
    d.set_item("features", features_to_dict(py, &m.features)?)?;
    d.set_item("units", m.units.clone())?;
    d.set_item("quantity_kind", m.quantity_kind.clone())?;
    d.set_item("unit_system", m.unit_system.map(|u| u.as_str()))?;
    d.set_item(
        "time_reference",
        m.time_reference
            .as_ref()
            .map(core_lib::TimeReference::as_storage_string),
    )?;
    d.set_item("component_field", m.component_field.clone())?;
    d.set_item("application_data", m.application_data.clone())?;
    Ok(d)
}

/// Render a catalog timestamp as a string for the metadata dict.
///
/// The instant, spelled honestly rather than converted. Everything that names an
/// instant — including an unset reference — stays RFC 3339 UTC; a caller wanting
/// it rendered at the row's own offset or zone has `time_reference` beside it
/// and a datetime library to do it with, which is more than this function has
/// (rendering a named zone needs a tz database, and the row's own spelling is
/// what says whether that is even the right question).
///
/// A **zoneless** row is the exception, and the reason this is not just
/// `to_rfc3339`: its timestamps are wall clocks, so a trailing `Z` would assert
/// an instant the row explicitly does not name.
fn render_catalog_timestamp(
    t: DateTime<Utc>,
    reference: Option<&core_lib::TimeReference>,
) -> String {
    match reference {
        Some(core_lib::TimeReference::Zoneless) => {
            t.naive_utc().format("%Y-%m-%dT%H:%M:%S%.f").to_string()
        }
        _ => t.to_rfc3339(),
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
/// `"debug"`, `"infrastore_core=debug"`, or `"warn,infrastore_core=trace"`.
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

// ---- Element-type codec ---------------------------------------------------

/// Decode a stored array into its per-timestep logical values.
///
/// `data` is the array as stored (row-major, first dims the leading axes),
/// `element_type` its canonical string, and `leading_dims` how many leading axes
/// precede the per-step element shape: 1 for a static series, 2 for a
/// `Deterministic`, 3 for a `Probabilistic` or `Scenarios`.
///
/// Returns one entry per timestep, in row-major order over the leading axes:
///
/// - `linear_function` -> `{"proportional": float, "constant": float}`
/// - `quadratic_function` -> `{"quadratic": float, "proportional": float, "constant": float}`
/// - `piecewise_linear` -> `list[{"x": float, "y": float}]`
/// - `piecewise_step` -> `{"x": list[float], "y": list[float]}`
/// - `tuple(N,dtype)` -> `list[float]` of length `N`
///
/// Returns `None` for a scalar element type and for any array whose physical
/// dtype is not `float64`: there the stored elements already are the values, so
/// the numpy array itself is the answer.
#[pyfunction]
#[pyo3(signature = (data, element_type, leading_dims=1))]
fn decode_element_values<'py>(
    py: Python<'py>,
    data: &Bound<'py, PyAny>,
    element_type: &str,
    leading_dims: usize,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let array = typed_array_from_numpy(data)?;
    let element_type = parse_element_type(element_type)?;
    let decoded = core_lib::decode(&array, element_type, leading_dims).map_err(map_err)?;
    Ok(match decoded {
        core_lib::DecodedValues::Raw => None,
        other => Some(decoded_to_py(py, &other)?),
    })
}

fn decoded_to_py<'py>(
    py: Python<'py>,
    values: &core_lib::DecodedValues,
) -> PyResult<Bound<'py, PyAny>> {
    use core_lib::DecodedValues;
    let out = PyList::empty(py);
    match values {
        // `Raw` never reaches here: the caller returns `None` for it.
        DecodedValues::Raw => {}
        DecodedValues::Tuple(rows) => {
            for row in rows {
                out.append(row.clone())?;
            }
        }
        DecodedValues::LinearFunction(rows) => {
            for f in rows {
                let d = PyDict::new(py);
                d.set_item("proportional", f.proportional)?;
                d.set_item("constant", f.constant)?;
                out.append(d)?;
            }
        }
        DecodedValues::QuadraticFunction(rows) => {
            for f in rows {
                let d = PyDict::new(py);
                d.set_item("quadratic", f.quadratic)?;
                d.set_item("proportional", f.proportional)?;
                d.set_item("constant", f.constant)?;
                out.append(d)?;
            }
        }
        DecodedValues::PiecewiseLinear(rows) => {
            for points in rows {
                let step = PyList::empty(py);
                for p in points {
                    let d = PyDict::new(py);
                    d.set_item("x", p.x)?;
                    d.set_item("y", p.y)?;
                    step.append(d)?;
                }
                out.append(step)?;
            }
        }
        DecodedValues::PiecewiseStep(rows) => {
            for s in rows {
                let d = PyDict::new(py);
                d.set_item("x", s.x.clone())?;
                d.set_item("y", s.y.clone())?;
                out.append(d)?;
            }
        }
    }
    Ok(out.into_any())
}

/// Encode per-timestep logical values into the flat array the store holds.
///
/// The inverse of `decode_element_values`, and the write-side half a caller
/// needed to build by hand: `values` is a list in the shape that function
/// returns, `element_type` names the layout to pack them into, and
/// `leading_dims` is the shape of the axes that precede the per-step element
/// shape — `(len,)` for a static series, `(H, count)` for a `Deterministic`,
/// `(P, H, count)` for a `Probabilistic` or `Scenarios`. It defaults to
/// `(len(values),)`, which is the static case.
///
/// Returns a `float64` numpy array of shape `(*leading_dims, width)`, ready to
/// pass to `add_time_series` alongside the same `element_type`.
///
/// The ragged kinds are padded to the widest entry across the whole input, so
/// the same curve encodes differently in a differently-shaped series — that is
/// the storage layout, not a property of the value.
#[pyfunction]
#[pyo3(signature = (values, element_type, leading_dims=None))]
fn encode_element_values<'py>(
    py: Python<'py>,
    values: &Bound<'py, PyAny>,
    element_type: &str,
    leading_dims: Option<Vec<usize>>,
) -> PyResult<Bound<'py, PyAny>> {
    let element_type = parse_element_type(element_type)?;
    let decoded = py_to_decoded(values, element_type)?;
    let dims = leading_dims.unwrap_or_else(|| vec![decoded.len()]);
    // `encode_as` is the core's declared-type encoder: it takes an empty tuple
    // series' arity from `element_type` (the generic encoder cannot infer one
    // from no rows, and a zero-length series is storable), and it checks the
    // packing against the declaration — `tuple(3,f64)` given two-value rows packs
    // to width 2 — which is what makes the returned array actually ready for
    // `add_time_series`, as the docstring promises.
    let array = core_lib::encode_as(&decoded, &dims, element_type).map_err(map_err)?;
    numpy_from_typed(py, &array)
}

/// Read a Python payload back into the core's `DecodedValues`, keyed on the
/// element type it is being encoded as. The shapes are the ones
/// `decode_element_values` produces, so a round trip through Python needs no
/// reshaping in between.
fn py_to_decoded(
    values: &Bound<'_, PyAny>,
    element_type: core_lib::ElementType,
) -> PyResult<core_lib::DecodedValues> {
    use core_lib::{DecodedValues, ElementType, LinearFunction, QuadraticFunction};

    let rows: Vec<Bound<'_, PyAny>> = values.try_iter()?.collect::<PyResult<_>>()?;
    let field = |row: &Bound<'_, PyAny>, name: &str| -> PyResult<f64> {
        row.get_item(name)?.extract::<f64>()
    };
    Ok(match element_type {
        ElementType::Scalar(_) => {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "a scalar element_type has no values to encode: pass the numpy array \
                 to add_time_series directly",
            ));
        }
        ElementType::Tuple { .. } => DecodedValues::Tuple(
            rows.iter()
                .map(|r| r.extract::<Vec<f64>>())
                .collect::<PyResult<_>>()?,
        ),
        ElementType::LinearFunction => DecodedValues::LinearFunction(
            rows.iter()
                .map(|r| {
                    Ok(LinearFunction {
                        proportional: field(r, "proportional")?,
                        constant: field(r, "constant")?,
                    })
                })
                .collect::<PyResult<_>>()?,
        ),
        ElementType::QuadraticFunction => DecodedValues::QuadraticFunction(
            rows.iter()
                .map(|r| {
                    Ok(QuadraticFunction {
                        quadratic: field(r, "quadratic")?,
                        proportional: field(r, "proportional")?,
                        constant: field(r, "constant")?,
                    })
                })
                .collect::<PyResult<_>>()?,
        ),
        ElementType::PiecewiseLinear => DecodedValues::PiecewiseLinear(
            rows.iter()
                .map(|step| {
                    step.try_iter()?
                        .map(|p| {
                            let p = p?;
                            Ok(core_lib::XyPoint {
                                x: field(&p, "x")?,
                                y: field(&p, "y")?,
                            })
                        })
                        .collect::<PyResult<Vec<_>>>()
                })
                .collect::<PyResult<_>>()?,
        ),
        ElementType::PiecewiseStep => DecodedValues::PiecewiseStep(
            rows.iter()
                .map(|r| {
                    Ok(core_lib::StepFunction {
                        x: r.get_item("x")?.extract::<Vec<f64>>()?,
                        y: r.get_item("y")?.extract::<Vec<f64>>()?,
                    })
                })
                .collect::<PyResult<_>>()?,
        ),
    })
}

// ---- Module init ----------------------------------------------------------

#[pymodule]
fn infrastore(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Auto-initialize from RUST_LOG if set. try_init() is a no-op when a
    // subscriber is already registered, so this is safe to call unconditionally.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    m.add_class::<PyStore>()?;
    m.add_class::<PyTransaction>()?;
    m.add_class::<PySingleTimeSeries>()?;
    m.add_class::<PyNonSequentialTimeSeries>()?;
    m.add_class::<PyDeterministic>()?;
    m.add_class::<PyProbabilistic>()?;
    m.add_class::<PyScenarios>()?;
    m.add_class::<PyTimeSeriesType>()?;
    m.add_class::<PyOwnerCategory>()?;
    m.add_class::<PySupplementalAttributeAssociation>()?;
    m.add_class::<PyParentChildAssociation>()?;
    m.add_class::<PyStaticReader>()?;
    m.add_class::<PyForecastReader>()?;

    m.add("TimeSeriesError", py.get_type::<TimeSeriesError>())?;
    m.add("NotFoundError", py.get_type::<NotFoundError>())?;
    m.add("OwnerMismatchError", py.get_type::<OwnerMismatchError>())?;
    m.add(
        "DuplicateTimeSeriesError",
        py.get_type::<DuplicateTimeSeriesError>(),
    )?;
    m.add(
        "DuplicateAssociationError",
        py.get_type::<DuplicateAssociationError>(),
    )?;
    m.add(
        "DuplicateAssociationIdError",
        py.get_type::<DuplicateAssociationIdError>(),
    )?;
    m.add(
        "InvalidParameterError",
        py.get_type::<InvalidParameterError>(),
    )?;
    m.add("IntegrityError", py.get_type::<IntegrityError>())?;
    m.add("ReadOnlyStoreError", py.get_type::<ReadOnlyStoreError>())?;
    m.add("IoError", py.get_type::<IoError>())?;
    m.add("ConnectionError", py.get_type::<ConnectionError>())?;
    m.add(
        "IncompatibleFormatError",
        py.get_type::<IncompatibleFormatError>(),
    )?;
    m.add(
        "IncompatibleForecastError",
        py.get_type::<IncompatibleForecastError>(),
    )?;
    m.add("StorageError", py.get_type::<StorageError>())?;
    m.add("StoreExistsError", py.get_type::<StoreExistsError>())?;
    m.add(
        "MismatchedArtifactError",
        py.get_type::<MismatchedArtifactError>(),
    )?;
    m.add(
        "CatalogMigrationRequiredError",
        py.get_type::<CatalogMigrationRequiredError>(),
    )?;
    m.add("CatalogTooNewError", py.get_type::<CatalogTooNewError>())?;

    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_function(wrap_pyfunction!(init_tracing, m)?)?;
    m.add_function(wrap_pyfunction!(decode_element_values, m)?)?;
    m.add_function(wrap_pyfunction!(encode_element_values, m)?)?;
    Ok(())
}
