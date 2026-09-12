//! PyO3 bindings for `infrastore`.
//!
//! Exposed module name: `infrastore`. Top-level surface:
//!
//! ```python
//! from infrastore import (
//!     Store, SingleTimeSeries, NonSequentialTimeSeries, PersistentTimeSeries,
//!     TimeSeriesType, OwnerCategory,
//!     SupplementalAttributeAssociation, ParentChildAssociation,
//!     TimeSeriesError, NotFoundError, OwnerMismatchError, DuplicateTimeSeriesError,
//!     InvalidParameterError,
//!     IntegrityError, ReadOnlyStoreError, IoError, ConnectionError,
//!     IncompatibleFormatError, StorageError,
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
    PyAny, PyBool, PyBytes, PyDateTime, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple, PyTzInfo,
};

// ---- Exceptions -----------------------------------------------------------

/// Each exception named once: defined here and registered on the module by the
/// generated `add_exceptions`. `TimeSeriesError` is the base of the rest.
macro_rules! exceptions {
    ($($name:ident),* $(,)?) => {
        create_exception!(infrastore, TimeSeriesError, PyException);
        $(create_exception!(infrastore, $name, TimeSeriesError);)*

        fn add_exceptions(m: &Bound<'_, PyModule>) -> PyResult<()> {
            let py = m.py();
            m.add("TimeSeriesError", py.get_type::<TimeSeriesError>())?;
            $(m.add(stringify!($name), py.get_type::<$name>())?;)*
            Ok(())
        }
    };
}

exceptions!(
    NotFoundError,
    OwnerMismatchError,
    DuplicateTimeSeriesError,
    DuplicateAssociationError,
    DuplicateAssociationIdError,
    InvalidParameterError,
    IntegrityError,
    ReadOnlyStoreError,
    IoError,
    ConnectionError,
    IncompatibleFormatError,
    StorageError,
    StoreExistsError,
    MismatchedArtifactError,
    CatalogMigrationRequiredError,
    CatalogTooNewError,
);

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
            // A wall clock: the stored instant's UTC fields, unlabeled.
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

/// The descriptors every one of the six series types carries, as a
/// `#[pymethods]` block of their own.
///
/// These getters read the same fields off `self.inner` whatever the type is, so
/// they were six identical copies before this. They live in a second
/// `#[pymethods]` impl (PyO3's `multiple-pymethods` feature) beside each type's
/// hand-written one rather than wrapping it, so the hand-written block stays a
/// plain impl that `rustfmt` formats — a macro invocation's body is out of its
/// reach.
///
/// `$leading_axes` is how many axes precede the element in the stored array: one
/// timestep axis for a static series, `[H, count]` for a `Deterministic`, and a
/// percentile or scenario axis in front of those for the other two forecasts.
macro_rules! series_pymethods {
    ($ty:ident, $leading_axes:literal) => {
        #[pymethods]
        impl $ty {
            /// Decode this series' array into the per-timestep values its element type
            /// describes — the read-side counterpart of `from_values`, and the reason a
            /// caller never has to know the stored row layouts.
            ///
            /// Same shapes as `decode_element_values`, which this is: the element type
            /// and the number of leading axes both come from the series, so there is
            /// nothing left to pass and nothing to get wrong.
            ///
            /// `None` for a scalar element type and for any array whose physical dtype
            /// is not `float64`: there the stored elements already are the values, and
            /// `.data` is the answer.
            fn decoded_values<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
                decoded_or_none(py, &self.inner.data, self.inner.element_type, $leading_axes)
            }

            /// Value equality: all fields including the data array (bitwise).
            fn __eq__(&self, other: &Self) -> bool {
                self.inner == other.inner
            }

            /// The values, as a numpy array.
            #[getter]
            fn data<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
                numpy_from_typed(py, &self.inner.data)
            }

            /// The series' name. Fixed once written — there is no rename.
            #[getter]
            fn name(&self) -> String {
                self.inner.name.clone()
            }

            /// Opaque, package-owned payload stored verbatim, or `None`.
            #[getter]
            fn application_data(&self) -> Option<String> {
                self.inner.application_data.clone()
            }

            /// What the stored elements mean, in the store's own vocabulary.
            /// Always concrete: a plain numeric series reports the array's dtype
            /// spelling.
            #[getter]
            fn element_type(&self) -> String {
                self.inner.element_type.to_string()
            }

            /// User-declared units label for the values (e.g. `"MW"`), or `None`.
            #[getter]
            fn units(&self) -> Option<String> {
                self.inner.units.clone()
            }

            /// What kind of physical quantity the values measure, or `None`.
            #[getter]
            fn quantity_kind(&self) -> Option<String> {
                self.inner.quantity_kind.clone()
            }

            /// `"natural_units"`, `"component_base"`, or `None` for unspecified.
            #[getter]
            fn unit_system(&self) -> Option<&'static str> {
                self.inner.unit_system.map(|u| u.as_str())
            }

            /// The owning component's field these values vary, or `None`.
            #[getter]
            fn component_field(&self) -> Option<String> {
                self.inner.component_field.clone()
            }

            /// How this series' timestamps were spelled: `"utc"`, `"zoneless"`,
            /// a fixed offset (`"-07:00"`), an IANA zone name, or `None` for
            /// unspecified.
            #[getter]
            fn time_reference(&self) -> Option<String> {
                self.inner
                    .time_reference
                    .as_ref()
                    .map(core_lib::TimeReference::as_storage_string)
            }
        }
    };
}

/// The window-timeline getters the three dense forecast types share.
macro_rules! forecast_pymethods {
    ($ty:ident) => {
        #[pymethods]
        impl $ty {
            /// The first window's timestamp, spelled the way it was written.
            #[getter]
            fn initial_timestamp<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
                spell_instant(
                    py,
                    self.inner.initial_timestamp,
                    self.inner.time_reference.as_ref(),
                )
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

            /// Number of forecast windows (`count`).
            fn __len__(&self) -> usize {
                self.inner.count
            }
        }
    };
}

/// A Python-visible enum mirroring a core one variant for variant, with the
/// conversions both ways.
macro_rules! mirror_enum {
    ($py_name:literal, $py:ident, $core:ident { $($v:ident),* $(,)? }) => {
        #[pyclass(eq, eq_int, name = $py_name, module = "infrastore", from_py_object)]
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub enum $py {
            $($v),*
        }

        impl From<$py> for core_lib::$core {
            fn from(v: $py) -> Self {
                match v {
                    $($py::$v => Self::$v),*
                }
            }
        }

        impl From<core_lib::$core> for $py {
            fn from(v: core_lib::$core) -> Self {
                match v {
                    $(core_lib::$core::$v => Self::$v),*
                }
            }
        }
    };
}

mirror_enum!(
    "TimeSeriesType",
    PyTimeSeriesType,
    TimeSeriesType {
        SingleTimeSeries,
        NonSequentialTimeSeries,
        PersistentTimeSeries,
        Deterministic,
        DeterministicSingleTimeSeries,
        Probabilistic,
        Scenarios,
    }
);

mirror_enum!(
    "OwnerCategory",
    PyOwnerCategory,
    OwnerCategory {
        Component,
        SupplementalAttribute,
    }
);

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
/// little-endian — the core's documented buffer encoding, whatever the HDF5
/// file holds — so decoding them under the native order would read them
/// backwards on a big-endian host.
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

// ---- Arrow export ---------------------------------------------------------

/// Import `pyarrow`, or explain which extra provides it.
///
/// Deliberately not a runtime dependency of the wheel: pyarrow is several times
/// the size of everything else installed, and the binding's own currency is
/// numpy arrays. Only `to_arrow` needs it, so only `to_arrow` asks for it.
fn pyarrow(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
    py.import("pyarrow").map_err(|_| {
        pyo3::exceptions::PyImportError::new_err(
            "to_arrow() requires pyarrow, which infrastore does not install by default. \
             Install it with `pip install 'infrastore[arrow]'` (or `pip install pyarrow`).",
        )
    })
}

/// The pyarrow timestamp type that spells `reference`.
///
/// Arrow's `timestamp(unit, tz)` is the same shape as the store's own model — an
/// instant plus the spelling it was written in — so the mapping is total and
/// lossless. Millisecond unit throughout, which is the precision every instant
/// the store records is held to, so nothing is widened or truncated:
///
/// | reference | Arrow type |
/// | --- | --- |
/// | `None`, `utc` | `timestamp[ms, tz=UTC]` |
/// | `zoneless` | `timestamp[ms]` (no zone) |
/// | `-07:00` | `timestamp[ms, tz=-07:00]` |
/// | `America/Denver` | `timestamp[ms, tz=America/Denver]` |
///
/// A zone this interpreter's tz database does not know warns and falls back to
/// UTC, matching [`spell_instant`]: the instants are intact either way, and
/// failing a read over a label nobody can resolve would be worse.
fn arrow_timestamp_type<'py>(
    pa: &Bound<'py, PyModule>,
    reference: Option<&core_lib::TimeReference>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = pa.py();
    let zone: Option<String> = match reference {
        None | Some(core_lib::TimeReference::Utc) => Some("UTC".to_string()),
        Some(core_lib::TimeReference::Zoneless) => None,
        Some(r @ core_lib::TimeReference::FixedOffset(_)) => Some(r.as_storage_string()),
        Some(core_lib::TimeReference::Zone(name)) => {
            // pyarrow builds the type from any string, so probe the zone the way
            // the datetime path does rather than trusting it to complain later.
            if zone_info(py, name).is_err() {
                warn_unknown_zone(py, name)?;
                Some("UTC".to_string())
            } else {
                Some(name.clone())
            }
        }
    };
    match zone {
        Some(tz) => pa.call_method1("timestamp", ("ms", tz)),
        None => pa.call_method1("timestamp", ("ms",)),
    }
}

/// `instants` as a pyarrow `timestamp[ms, …]` array.
///
/// Built from the raw milliseconds through numpy rather than from a list of
/// `datetime` objects: an 8760-row year should not allocate 8760 Python objects
/// on its way out, and `pa.array` reads a `datetime64[ms]` buffer as the UTC
/// instants they are before labelling them with the zone.
fn arrow_timestamp_array<'py>(
    pa: &Bound<'py, PyModule>,
    instants: &[DateTime<Utc>],
    reference: Option<&core_lib::TimeReference>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = pa.py();
    let mut raw = Vec::with_capacity(instants.len() * 8);
    for t in instants {
        raw.extend_from_slice(&t.timestamp_millis().to_le_bytes());
    }
    let np = py.import("numpy")?;
    let millis = np.call_method1("frombuffer", (PyBytes::new(py, &raw), "<i8"))?;
    let as_dt64 = millis.call_method1("astype", ("datetime64[ms]",))?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("type", arrow_timestamp_type(pa, reference)?)?;
    pa.call_method("array", (as_dt64,), Some(&kwargs))
}

/// A numpy array shaped `(entries, *element_shape)` as one pyarrow column.
///
/// A scalar element gives a primitive array. A multidimensional one gives nested
/// `fixed_size_list`s — one level per element dimension, innermost first, which
/// is the order the flat C-ordered buffer is already in.
fn arrow_column<'py>(
    pa: &Bound<'py, PyModule>,
    values: &Bound<'py, PyAny>,
    element_shape: &[usize],
) -> PyResult<Bound<'py, PyAny>> {
    // The stored bytes are little-endian; pyarrow wants the platform's order. A
    // no-op everywhere infrastore is built today.
    let native = values.call_method1(
        "astype",
        (values
            .getattr("dtype")?
            .call_method1("newbyteorder", ("=",))?,),
    )?;
    let flat = native.call_method1("reshape", ((-1i64,),))?;
    let mut column = pa.call_method1("array", (flat,))?;
    for dim in element_shape.iter().rev() {
        column = pa
            .getattr("FixedSizeListArray")?
            .call_method1("from_arrays", (column, *dim))?;
    }
    Ok(column)
}

/// `data` as one pyarrow column, one entry per timestep.
fn arrow_value_array<'py>(
    pa: &Bound<'py, PyModule>,
    data: &core_lib::TypedArray,
) -> PyResult<Bound<'py, PyAny>> {
    let values = numpy_from_typed(pa.py(), data)?;
    arrow_column(pa, &values, data.element_shape())
}

/// A two-column `pyarrow.Table` — `timestamp` and `value` — carrying the
/// series' descriptive attributes as schema metadata.
///
/// The value column is named `value` rather than after the series so that tables
/// from different components concatenate without renaming; the series' own name
/// rides in the metadata along with everything else that describes the values
/// but does not address them. Schema metadata is the right home for those: it
/// survives a Parquet round trip, so the table is not lossy against the object
/// it came from.
fn arrow_table<'py>(
    py: Python<'py>,
    instants: &[DateTime<Utc>],
    reference: Option<&core_lib::TimeReference>,
    data: &core_lib::TypedArray,
    metadata: BTreeMap<String, String>,
) -> PyResult<Bound<'py, PyAny>> {
    let pa = pyarrow(py)?;
    let column = arrow_value_array(&pa, data)?;
    arrow_table_from_column(&pa, instants, reference, column, metadata)
}

/// [`arrow_table`] over a value column that is already built — the forecast
/// path, where one window is a slice of the stored array rather than the whole
/// of it.
fn arrow_table_from_column<'py>(
    pa: &Bound<'py, PyModule>,
    instants: &[DateTime<Utc>],
    reference: Option<&core_lib::TimeReference>,
    column: Bound<'py, PyAny>,
    metadata: BTreeMap<String, String>,
) -> PyResult<Bound<'py, PyAny>> {
    let columns = PyDict::new(pa.py());
    columns.set_item("timestamp", arrow_timestamp_array(pa, instants, reference)?)?;
    columns.set_item("value", column)?;
    let kwargs = PyDict::new(pa.py());
    kwargs.set_item("metadata", metadata)?;
    pa.call_method("table", (columns,), Some(&kwargs))
}

/// A list of integers as a JSON array: `[]`, `[3]`, `[2,3]`.
fn json_int_list(values: &[usize]) -> String {
    format!("{values:?}").replace(' ', "")
}

/// What the `time_reference` metadata key holds for a series that records no
/// spelling.
///
/// Deliberately **not** a `TimeReference` variant and not something
/// `TimeReference::parse` accepts: unspecified is `None`, not a fourth kind of
/// reference, and teaching the core's parser this literal would also make
/// `time_reference="unspecified"` a thing a constructor accepted. It is a
/// metadata encoding, and `from_arrow` decodes it back to `None`.
///
/// The CLI's Parquet export writes the same literal
/// (`infrastore_parquet::schema::UNSPECIFIED_REFERENCE`); the pytest
/// `test_the_unspecified_literal_is_the_one_the_cli_writes` pins the two
/// together, since the two producers must agree on it exactly.
const UNSPECIFIED_REFERENCE: &str = "unspecified";

/// The descriptive attributes a `to_arrow` table carries as schema metadata.
///
/// A macro for the same reason as `apply_descriptors!`: the three static types
/// name these fields identically but share no trait. Absent values are left out
/// rather than written as an empty string, so `b"units" in table.schema.metadata`
/// answers "was a label declared?".
macro_rules! arrow_metadata {
    ($inner:expr, $type_name:literal) => {{
        let mut meta: BTreeMap<String, String> = BTreeMap::new();
        meta.insert("time_series_type".to_string(), $type_name.to_string());
        meta.insert("name".to_string(), $inner.name.clone());
        meta.insert("element_type".to_string(), $inner.element_type.to_string());
        // A JSON list, `[]` for a scalar element, and written even when empty
        // unlike the descriptors below: an absent descriptor means "not
        // declared", where an empty shape is a fact about the data. It is
        // recoverable from the value column's own nested type, and is here so a
        // reader that only opens the footer does not have to walk it -- and
        // because the CLI's Parquet export writes it, and the two producers
        // write one schema.
        meta.insert(
            "element_shape".to_string(),
            json_int_list($inner.data.element_shape()),
        );
        if let Some(v) = &$inner.units {
            meta.insert("units".to_string(), v.clone());
        }
        if let Some(v) = &$inner.quantity_kind {
            meta.insert("quantity_kind".to_string(), v.clone());
        }
        if let Some(v) = $inner.unit_system {
            meta.insert("unit_system".to_string(), v.as_str().to_string());
        }
        if let Some(v) = &$inner.component_field {
            meta.insert("component_field".to_string(), v.clone());
        }
        if let Some(v) = &$inner.application_data {
            meta.insert("application_data".to_string(), v.clone());
        }
        // Written for every series, unlike the descriptors above. An absent
        // descriptor means "not declared"; an absent reference would mean "read
        // it off the timestamp column's zone", and an unspecified reference
        // writes a UTC-zoned column -- so it would come back as `utc`, a claim
        // the series never made. `UNSPECIFIED_REFERENCE` says so instead.
        meta.insert(
            "time_reference".to_string(),
            $inner.time_reference.as_ref().map_or_else(
                || UNSPECIFIED_REFERENCE.to_string(),
                core_lib::TimeReference::as_storage_string,
            ),
        );
        meta
    }};
}

// ---- Arrow import ---------------------------------------------------------
//
// `from_arrow` is the inverse of `to_arrow`, and reads a *foreign* table too --
// anything with a `timestamp` and a `value` column, whether or not it carries
// the schema metadata `to_arrow` writes. The rules it applies when the metadata
// is silent are the same ones the CLI's Parquet import applies, and they are
// documented in one place (the CLI reference's "Parquet import") so the two
// implementations cannot drift apart quietly.

/// The pieces of an Arrow table this binding reads back.
struct ArrowParts<'py> {
    /// Instants in unix milliseconds.
    millis: Vec<i64>,
    /// The timestamp column's own zone, if it declares one.
    zone: Option<String>,
    /// The values as a numpy array shaped `(rows, *element_shape)`.
    values: Bound<'py, PyAny>,
    /// The schema metadata, decoded from its bytes keys and values.
    metadata: BTreeMap<String, String>,
}

/// Pull a `pyarrow.Table` apart into the pieces a constructor needs.
fn arrow_parts<'py>(py: Python<'py>, table: &Bound<'py, PyAny>) -> PyResult<ArrowParts<'py>> {
    let pa = pyarrow(py)?;
    let metadata = arrow_metadata_map(table)?;
    let (millis, zone) = arrow_instants(&pa, table)?;
    let values = arrow_values(&pa, table, millis.len())?;
    Ok(ArrowParts {
        millis,
        zone,
        values,
        metadata,
    })
}

/// `table.schema.metadata` as text. Absent (the schema carries none) is an empty
/// map, not an error: a foreign table is expected to have none.
fn arrow_metadata_map(table: &Bound<'_, PyAny>) -> PyResult<BTreeMap<String, String>> {
    let raw = table.getattr("schema")?.getattr("metadata")?;
    let mut out = BTreeMap::new();
    if raw.is_none() {
        return Ok(out);
    }
    let dict: Bound<'_, PyDict> = raw.extract().map_err(|_| {
        InvalidParameterError::new_err("table.schema.metadata is not a mapping".to_string())
    })?;
    for (key, value) in dict.iter() {
        // Arrow stores both halves as bytes. A key or value that is not UTF-8
        // was not written by this project, so it is skipped rather than
        // erroring: a foreign table may carry metadata of its own.
        let (Ok(key), Ok(value)) = (bytes_to_string(&key), bytes_to_string(&value)) else {
            continue;
        };
        out.insert(key, value);
    }
    Ok(out)
}

fn bytes_to_string(value: &Bound<'_, PyAny>) -> Result<String, ()> {
    if let Ok(text) = value.extract::<String>() {
        return Ok(text);
    }
    let raw: Vec<u8> = value.extract::<Vec<u8>>().map_err(|_| ())?;
    String::from_utf8(raw).map_err(|_| ())
}

fn table_column<'py>(table: &Bound<'py, PyAny>, name: &str) -> PyResult<Bound<'py, PyAny>> {
    let names: Vec<String> = table.getattr("column_names")?.extract()?;
    if !names.iter().any(|n| n == name) {
        return Err(InvalidParameterError::new_err(format!(
            "the table has no `{name}` column (it has {names:?})"
        )));
    }
    table.call_method1("column", (name,))
}

/// The timestamp column as unix milliseconds, plus the zone it declares.
///
/// Seconds and milliseconds cross as they are. Microseconds and nanoseconds are
/// accepted **only when every value is a whole millisecond** — the rule the
/// store's write path enforces on every instant it records, so a finer timestamp
/// is refused rather than rounded onto the grid.
fn arrow_instants(
    pa: &Bound<'_, PyModule>,
    table: &Bound<'_, PyAny>,
) -> PyResult<(Vec<i64>, Option<String>)> {
    let column = table_column(table, "timestamp")?;
    let ty = column.getattr("type")?;
    let is_timestamp: bool = pa
        .getattr("types")?
        .call_method1("is_timestamp", (&ty,))?
        .extract()?;
    if !is_timestamp {
        return Err(InvalidParameterError::new_err(format!(
            "the `timestamp` column is {ty}, not a timestamp"
        )));
    }
    if column.getattr("null_count")?.extract::<usize>()? > 0 {
        return Err(InvalidParameterError::new_err(
            "the `timestamp` column has nulls; the store records an instant for every row"
                .to_string(),
        ));
    }
    let unit: String = ty.getattr("unit")?.extract()?;
    let zone: Option<String> = ty.getattr("tz")?.extract()?;

    // Cast to int64 first: that is a zero-copy reinterpretation of the same
    // buffer, where casting the timestamp itself to a coarser unit would round.
    let raw = column
        .call_method1("cast", (pa.call_method0("int64")?,))?
        .call_method0("to_numpy")?;
    let raw: Vec<i64> = raw.call_method0("tolist")?.extract()?;

    let per_milli: i64 = match unit.as_str() {
        "s" => -1_000, // negative marks a multiply rather than a divide
        "ms" => 1,
        "us" => 1_000,
        "ns" => 1_000_000,
        other => {
            return Err(InvalidParameterError::new_err(format!(
                "unsupported timestamp unit {other:?}"
            )));
        }
    };
    let millis = raw
        .into_iter()
        .map(|v| {
            if per_milli < 0 {
                v.checked_mul(-per_milli).ok_or_else(|| {
                    InvalidParameterError::new_err(
                        "a timestamp in seconds overflows milliseconds".to_string(),
                    )
                })
            } else if v % per_milli == 0 {
                Ok(v / per_milli)
            } else {
                Err(InvalidParameterError::new_err(format!(
                    "the `timestamp` column is in {unit} and {v} is not a whole millisecond; \
                     the store records millisecond instants and will not round one"
                )))
            }
        })
        .collect::<PyResult<Vec<i64>>>()?;
    Ok((millis, zone))
}

/// The value column as a numpy array shaped `(rows, *element_shape)`.
///
/// Nested `FixedSizeList`s are peeled off outermost first, which is the order
/// their sizes make up the element shape. `Struct` and `List` are refused:
/// those are the *decoded* form of a composite element type, which `to_arrow`
/// does not produce and this does not claim to read — and a `List` is ragged,
/// which a per-timestep shape is not.
fn arrow_values<'py>(
    pa: &Bound<'py, PyModule>,
    table: &Bound<'py, PyAny>,
    rows: usize,
) -> PyResult<Bound<'py, PyAny>> {
    let py = pa.py();
    let types = pa.getattr("types")?;
    let column = table_column(table, "value")?;
    // One `Array`, so `flatten` below has a single offset to respect.
    // `combine_chunks` returns an `Array` in some pyarrow versions and a
    // one-chunk `ChunkedArray` in others, and a table with no rows has no chunks
    // at all -- hence all three arms rather than the one the current version
    // happens to need.
    let combined = column.call_method0("combine_chunks")?;
    let mut array = match combined.getattr("num_chunks") {
        Err(_) => combined,
        Ok(count) if count.extract::<usize>()? > 0 => combined.call_method1("chunk", (0,))?,
        Ok(_) => pa.call_method1("array", (PyList::empty(py), column.getattr("type")?))?,
    };

    let mut dims: Vec<usize> = Vec::new();
    loop {
        let ty = array.getattr("type")?;
        if types
            .call_method1("is_fixed_size_list", (&ty,))?
            .extract::<bool>()?
        {
            if array.getattr("null_count")?.extract::<usize>()? > 0 {
                return Err(nulls_refused());
            }
            dims.push(ty.getattr("list_size")?.extract()?);
            array = array.call_method0("flatten")?;
            continue;
        }
        for (predicate, what) in [
            ("is_struct", "a struct"),
            ("is_list", "a variable-length list"),
            ("is_large_list", "a variable-length list"),
        ] {
            if types.call_method1(predicate, (&ty,))?.extract::<bool>()? {
                return Err(InvalidParameterError::new_err(format!(
                    "the `value` column is {what}; this reads the packed form composite \
                     element types are stored in, not a decoded one"
                )));
            }
        }
        break;
    }
    if array.getattr("null_count")?.extract::<usize>()? > 0 {
        return Err(nulls_refused());
    }

    let kwargs = PyDict::new(py);
    kwargs.set_item("zero_copy_only", false)?;
    let flat = array.call_method("to_numpy", (), Some(&kwargs))?;
    let mut shape = vec![rows];
    shape.extend(dims);
    flat.call_method1("reshape", (shape,))
}

fn nulls_refused() -> PyErr {
    InvalidParameterError::new_err(
        "the `value` column has nulls; the store holds no nulls, and NaN is a value rather \
         than an absence, so this is refused rather than coerced"
            .to_string(),
    )
}

/// Resolve the descriptors a `from_arrow` ends up with.
///
/// Every keyword **overrides** the metadata, with one exception: `element_type`
/// is an **assertion**. It is how `tuple(3,f64)` gets named for a table whose
/// bytes cannot say whether a `fixed_size_list<double>[3]` is a tuple or a dense
/// row, and a value that contradicts the table's own `element_type` is an error
/// rather than a silent replacement — the rule this project applies to every
/// assertion.
///
/// `time_reference` falls back to the timestamp column's Arrow zone, and a
/// column with no zone reads as `zoneless`: a naive timestamp is a wall clock.
/// This is why the metadata spells the reference out at all — Arrow cannot
/// distinguish `zoneless` from *unspecified*.
fn arrow_descriptor_args(parts: &ArrowParts<'_>, args: DescriptorArgs) -> PyResult<DescriptorArgs> {
    let DescriptorArgs {
        application_data,
        element_type,
        units,
        quantity_kind,
        unit_system,
        component_field,
        time_reference,
    } = args;
    let meta = &parts.metadata;
    let element_type = match (meta.get("element_type"), element_type) {
        (Some(from_table), Some(asserted)) if from_table != &asserted => {
            return Err(InvalidParameterError::new_err(format!(
                "the table declares element_type {from_table:?}, but {asserted:?} was asserted"
            )));
        }
        (Some(from_table), _) => Some(from_table.clone()),
        (None, asserted) => asserted,
    };
    let zone_reference = match parts.zone.as_deref() {
        None => "zoneless".to_string(),
        Some(z) if z.eq_ignore_ascii_case("UTC") => "utc".to_string(),
        Some(z) => z.to_string(),
    };
    // The keyword wins, then the metadata -- `unspecified` included, which
    // resolves to *no* reference rather than leaving a gap for the zone to fill.
    // Only a table with no `time_reference` key at all (a foreign one) falls
    // through to the column's own zone.
    let time_reference = match (time_reference, meta.get("time_reference")) {
        (Some(declared), _) => Some(declared),
        (None, Some(from_table)) if from_table == UNSPECIFIED_REFERENCE => None,
        (None, Some(from_table)) => Some(from_table.clone()),
        (None, None) => Some(zone_reference),
    };
    Ok(DescriptorArgs {
        application_data: application_data.or_else(|| meta.get("application_data").cloned()),
        element_type,
        units: units.or_else(|| meta.get("units").cloned()),
        quantity_kind: quantity_kind.or_else(|| meta.get("quantity_kind").cloned()),
        unit_system: unit_system.or_else(|| meta.get("unit_system").cloned()),
        component_field: component_field.or_else(|| meta.get("component_field").cloned()),
        time_reference,
    })
}

/// The series name: the keyword, then the table's, then a refusal.
///
/// A name is part of a series' identity, so there is nothing sensible to default
/// it to.
fn arrow_name(parts: &ArrowParts<'_>, name: Option<String>) -> PyResult<String> {
    name.or_else(|| parts.metadata.get("name").cloned())
        .ok_or_else(|| {
            InvalidParameterError::new_err(
                "the table records no series name and none was given; a name is part of a \
                 series' identity"
                    .to_string(),
            )
        })
}

fn arrow_instant_vec(millis: &[i64]) -> PyResult<Vec<DateTime<Utc>>> {
    millis
        .iter()
        .map(|ms| {
            Utc.timestamp_millis_opt(*ms).single().ok_or_else(|| {
                InvalidParameterError::new_err(format!("timestamp {ms} ms is not representable"))
            })
        })
        .collect()
}

// ---- Descriptive attributes -----------------------------------------------

/// The descriptive keyword arguments every time-series constructor accepts.
///
/// These describe the values without addressing them: none is part of a series'
/// identity, so two series differing only in these are a duplicate. They belong
/// to the value object rather than to `Store.add_time_series`, which is what
/// makes a read-then-re-add lossless — a write path that took them as its own
/// arguments could only default them to `None`, and then either clobbered
/// whatever the object already carried or, guarding against that, quietly kept a
/// stale label the caller thought they had replaced.
struct DescriptorArgs {
    application_data: Option<String>,
    element_type: Option<String>,
    units: Option<String>,
    quantity_kind: Option<String>,
    unit_system: Option<String>,
    component_field: Option<String>,
    time_reference: Option<String>,
}

impl DescriptorArgs {
    /// Resolve the raw keyword strings into core descriptors.
    ///
    /// `element_type` falls back to what the core constructor derived from the
    /// array's own dtype, and `time_reference` to the spelling `inferred` from
    /// the timestamps the caller handed in. Omitting `time_reference` therefore
    /// means *infer*, not *unspecified*; a series that records no spelling comes
    /// from a store that never had one, not from this constructor.
    fn resolve(
        self,
        py: Python<'_>,
        element_type: core_lib::ElementType,
        inferred: Option<core_lib::TimeReference>,
    ) -> PyResult<core_lib::Descriptors> {
        Ok(core_lib::Descriptors {
            element_type: match self.element_type {
                Some(declared) => parse_element_type(&declared)?,
                None => element_type,
            },
            units: self.units,
            quantity_kind: self.quantity_kind,
            unit_system: parse_unit_system(self.unit_system.as_deref())?,
            time_reference: match self.time_reference {
                Some(spelling) => Some(parse_time_reference(py, &spelling)?),
                None => inferred,
            },
            component_field: self.component_field,
            application_data: self.application_data,
        })
    }
}

/// Write resolved descriptors onto a concrete core time-series value.
///
/// A macro rather than a function because the six types name these fields
/// identically but share no trait, and `TimeSeriesData::set_descriptors` is out
/// of reach here — a constructor holds the concrete type, not the enum.
macro_rules! apply_descriptors {
    ($inner:expr, $descriptors:expr) => {{
        let core_lib::Descriptors {
            element_type,
            units,
            quantity_kind,
            unit_system,
            time_reference,
            component_field,
            application_data,
        } = $descriptors;
        $inner.element_type = element_type;
        $inner.units = units;
        $inner.quantity_kind = quantity_kind;
        $inner.unit_system = unit_system;
        $inner.time_reference = time_reference;
        $inner.component_field = component_field;
        $inner.application_data = application_data;
    }};
}

// ---- Deterministic --------------------------------------------------------

#[pyclass(name = "Deterministic", module = "infrastore", from_py_object)]
#[derive(Clone)]
pub struct PyDeterministic {
    inner: core_lib::Deterministic,
}

series_pymethods!(PyDeterministic, 2);
forecast_pymethods!(PyDeterministic);

#[pymethods]
impl PyDeterministic {
    /// Build a `Deterministic` forecast. `data` is a numpy array of shape
    /// `[H, count, *E]`. `name` is required.
    ///
    /// The keyword-only arguments are the descriptive attributes documented on
    /// `SingleTimeSeries`; they travel with the forecast into the store and
    /// come back on a read.
    #[new]
    #[pyo3(signature = (
            initial_timestamp, resolution, horizon, interval, count, data, name, *,
            application_data=None, element_type=None, units=None, quantity_kind=None,
            unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        initial_timestamp: PyInstant,
        resolution: Bound<'_, PyAny>,
        horizon: Bound<'_, PyAny>,
        interval: Bound<'_, PyAny>,
        count: usize,
        data: &Bound<'_, PyAny>,
        name: String,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        let resolution = pyany_to_period(&resolution)?;
        let horizon = pyany_to_period(&horizon)?;
        let interval = pyany_to_period(&interval)?;
        let typed = typed_array_from_numpy(data)?;
        let mut inner = core_lib::Deterministic::new(
            initial_timestamp.instant,
            resolution,
            horizon,
            interval,
            count,
            typed,
            name,
        )
        .map_err(InvalidParameterError::new_err)?;
        // The spelling is inferred from the timestamp the caller handed us
        // rather than asked for separately: the intent is in the object, and it
        // is erased the moment the instant reaches the core. `time_reference=`
        // is the override for a caller who means a different one.
        let descriptors = DescriptorArgs {
            application_data,
            element_type,
            units,
            quantity_kind,
            unit_system,
            component_field,
            time_reference,
        }
        .resolve(py, inner.element_type, Some(initial_timestamp.reference))?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    /// Build from per-timestep logical values, encoding them into the array the
    /// store holds and declaring the element type they imply. See
    /// `SingleTimeSeries.from_values` for the value shapes and the rules.
    ///
    /// `values` is one entry per timestep in row-major order over the leading
    /// axes, so entry `i * count + j` is window `j`'s step `i`. Those axes are
    /// `[H, count]`, with `H` derived from `horizon`/`resolution` — the
    /// arithmetic `encode_element_values` otherwise leaves to the caller.
    #[classmethod]
    #[pyo3(signature = (
            initial_timestamp, resolution, horizon, interval, count, values, name, *,
            application_data=None, element_type=None, units=None, quantity_kind=None,
            unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn from_values(
        _cls: &Bound<'_, pyo3::types::PyType>,
        py: Python<'_>,
        initial_timestamp: PyInstant,
        resolution: Bound<'_, PyAny>,
        horizon: Bound<'_, PyAny>,
        interval: Bound<'_, PyAny>,
        count: usize,
        values: &Bound<'_, PyAny>,
        name: String,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        let resolution = pyany_to_period(&resolution)?;
        let horizon = pyany_to_period(&horizon)?;
        let interval = pyany_to_period(&interval)?;
        let decoded = from_values_payload(values, element_type.as_deref())?;
        let mut inner = core_lib::Deterministic::from_values(
            initial_timestamp.instant,
            resolution,
            horizon,
            interval,
            count,
            &decoded,
            name,
        )
        .map_err(InvalidParameterError::new_err)?;
        let descriptors = DescriptorArgs {
            application_data,
            element_type,
            units,
            quantity_kind,
            unit_system,
            component_field,
            time_reference,
        }
        .resolve(py, inner.element_type, Some(initial_timestamp.reference))?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    /// The forecast as `{issue_time: pyarrow.Table}`, one entry per window.
    ///
    /// Requires pyarrow, which is not installed with infrastore — use
    /// `pip install 'infrastore[arrow]'`.
    ///
    /// The key is the window's issue time — `initial_timestamp + k · interval`,
    /// spelled the way the series was written. Each value is a two-column
    /// `timestamp`/`value` table over that window's horizon, shaped exactly like
    /// a `SingleTimeSeries.to_arrow()`: `horizon / resolution` rows stepping by
    /// `resolution` from the issue time.
    ///
    /// **The dict is in window order**, which Python's insertion-ordered `dict`
    /// makes an ordering you can rely on: `next(iter(windows))` is the earliest
    /// issue time and iteration is chronological. It is not a sorted *container*
    /// — there is no O(log n) range lookup — so `bisect` over `list(windows)` is
    /// the way to select a span of issue times.
    ///
    /// ```python
    /// windows = forecast.to_arrow_windows()
    /// windows[datetime(2024, 1, 2, tzinfo=timezone.utc)]   # that day's forecast
    /// for issue_time, table in windows.items(): ...        # chronological
    /// ```
    ///
    /// Note that the two grids differ and both are needed to place a value:
    /// windows step by `interval`, the rows inside one step by `resolution`.
    /// They coincide only for a forecast whose windows abut, which is not the
    /// common case — a day-ahead forecast reissued hourly overlaps 23 of every
    /// 24 rows, so the tables deliberately repeat those values rather than
    /// pretending one timeline covers them.
    ///
    /// Each table carries the forecast's descriptive attributes as schema
    /// metadata, plus its own `issue_time`, so a window written to Parquet on
    /// its own still knows which one it is.
    ///
    /// This materializes every window. The stored array is `[H, count, *E]` —
    /// window index innermost — so it is transposed once here; for a
    /// per-timestamp sweep the cheap path is `Store.build_forecast_reader`,
    /// which reads on the axis the data is already laid out along.
    fn to_arrow_windows<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let pa = pyarrow(py)?;
        let inner = &self.inner;
        let reference = inner.time_reference.as_ref();
        // `[H, count, *E]` -> `[count, H, *E]`, once and contiguous, so each
        // window below is a view rather than its own gather.
        let np = py.import("numpy")?;
        let stored = numpy_from_typed(py, &inner.data)?;
        let by_window = np.call_method1(
            "ascontiguousarray",
            (stored.call_method1("swapaxes", (0, 1))?,),
        )?;
        // The element shape is what follows `[H, count]`; `TypedArray`'s own
        // `element_shape` drops one axis, which is the static layout's rule.
        let element_shape: Vec<usize> = inner.data.shape.get(2..).unwrap_or(&[]).to_vec();

        let windows = PyDict::new(py);
        for k in 0..inner.count {
            let stamps = inner.window_timestamps(k).map_err(map_err)?;
            let start = inner.window_start(k).map_err(map_err)?;
            let mut metadata = arrow_metadata!(inner, "Deterministic");
            // The macro takes `TypedArray::element_shape`, which strips one
            // axis -- right for a static series, one axis short for a forecast,
            // whose stored shape is `[H, count, *E]`. A window's element shape
            // is what follows both.
            metadata.insert("element_shape".to_string(), json_int_list(&element_shape));
            metadata.insert("resolution".to_string(), inner.resolution.to_iso8601());
            metadata.insert("horizon".to_string(), inner.horizon.to_iso8601());
            metadata.insert("interval".to_string(), inner.interval.to_iso8601());
            metadata.insert("count".to_string(), inner.count.to_string());
            metadata.insert(
                "issue_time".to_string(),
                render_catalog_timestamp(start, reference),
            );
            let column = arrow_column(&pa, &by_window.get_item(k)?, &element_shape)?;
            let table = arrow_table_from_column(&pa, &stamps, reference, column, metadata)?;
            windows.set_item(spell_instant(py, start, reference)?, table)?;
        }
        Ok(windows)
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

series_pymethods!(PyProbabilistic, 3);
forecast_pymethods!(PyProbabilistic);

#[pymethods]
impl PyProbabilistic {
    /// Build a `Probabilistic` forecast. `data` is a numpy array of shape
    /// `[num_percentiles, H, count, *E]`. `name` is required.
    ///
    /// The keyword-only arguments are the descriptive attributes documented on
    /// `SingleTimeSeries`; they travel with the forecast into the store and
    /// come back on a read.
    #[new]
    #[pyo3(signature = (
            initial_timestamp, resolution, horizon, interval, count, percentiles, data, name, *,
            application_data=None, element_type=None, units=None, quantity_kind=None,
            unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        initial_timestamp: PyInstant,
        resolution: Bound<'_, PyAny>,
        horizon: Bound<'_, PyAny>,
        interval: Bound<'_, PyAny>,
        count: usize,
        percentiles: Vec<f64>,
        data: &Bound<'_, PyAny>,
        name: String,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        let resolution = pyany_to_period(&resolution)?;
        let horizon = pyany_to_period(&horizon)?;
        let interval = pyany_to_period(&interval)?;
        let typed = typed_array_from_numpy(data)?;
        let mut inner = core_lib::Probabilistic::new(
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
        // The spelling is inferred from the timestamp the caller handed us
        // rather than asked for separately: the intent is in the object, and it
        // is erased the moment the instant reaches the core. `time_reference=`
        // is the override for a caller who means a different one.
        let descriptors = DescriptorArgs {
            application_data,
            element_type,
            units,
            quantity_kind,
            unit_system,
            component_field,
            time_reference,
        }
        .resolve(py, inner.element_type, Some(initial_timestamp.reference))?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    /// Build from per-timestep logical values, encoding them into the array the
    /// store holds and declaring the element type they imply. See
    /// `SingleTimeSeries.from_values` for the value shapes and the rules.
    ///
    /// `values` is one entry per timestep in row-major order over the leading
    /// axes, so entry `(p * H + i) * count + j` is percentile `p`'s window `j`,
    /// step `i`. Those axes are `[len(percentiles), H, count]`, with `H` derived
    /// from `horizon`/`resolution` — the arithmetic `encode_element_values`
    /// otherwise leaves to the caller.
    #[classmethod]
    #[pyo3(signature = (
            initial_timestamp, resolution, horizon, interval, count, percentiles, values, name, *,
            application_data=None, element_type=None, units=None, quantity_kind=None,
            unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn from_values(
        _cls: &Bound<'_, pyo3::types::PyType>,
        py: Python<'_>,
        initial_timestamp: PyInstant,
        resolution: Bound<'_, PyAny>,
        horizon: Bound<'_, PyAny>,
        interval: Bound<'_, PyAny>,
        count: usize,
        percentiles: Vec<f64>,
        values: &Bound<'_, PyAny>,
        name: String,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        let resolution = pyany_to_period(&resolution)?;
        let horizon = pyany_to_period(&horizon)?;
        let interval = pyany_to_period(&interval)?;
        let decoded = from_values_payload(values, element_type.as_deref())?;
        let mut inner = core_lib::Probabilistic::from_values(
            initial_timestamp.instant,
            resolution,
            horizon,
            interval,
            count,
            percentiles,
            &decoded,
            name,
        )
        .map_err(InvalidParameterError::new_err)?;
        let descriptors = DescriptorArgs {
            application_data,
            element_type,
            units,
            quantity_kind,
            unit_system,
            component_field,
            time_reference,
        }
        .resolve(py, inner.element_type, Some(initial_timestamp.reference))?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    #[getter]
    fn percentiles(&self) -> Vec<f64> {
        self.inner.percentiles.clone()
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

series_pymethods!(PyScenarios, 3);
forecast_pymethods!(PyScenarios);

#[pymethods]
impl PyScenarios {
    /// Build a `Scenarios` forecast. `data` is a numpy array of shape
    /// `[scenario_count, H, count, *E]`; `scenario_count` is taken from the
    /// leading axis. `name` is required.
    ///
    /// The keyword-only arguments are the descriptive attributes documented on
    /// `SingleTimeSeries`; they travel with the forecast into the store and
    /// come back on a read.
    #[new]
    #[pyo3(signature = (
            initial_timestamp, resolution, horizon, interval, count, data, name, *,
            application_data=None, element_type=None, units=None, quantity_kind=None,
            unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        initial_timestamp: PyInstant,
        resolution: Bound<'_, PyAny>,
        horizon: Bound<'_, PyAny>,
        interval: Bound<'_, PyAny>,
        count: usize,
        data: &Bound<'_, PyAny>,
        name: String,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        let resolution = pyany_to_period(&resolution)?;
        let horizon = pyany_to_period(&horizon)?;
        let interval = pyany_to_period(&interval)?;
        let typed = typed_array_from_numpy(data)?;
        let scenario_count = *typed.shape.first().ok_or_else(|| {
            InvalidParameterError::new_err("Scenarios: data must have at least one axis")
        })?;
        let mut inner = core_lib::Scenarios::new(
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
        // The spelling is inferred from the timestamp the caller handed us
        // rather than asked for separately: the intent is in the object, and it
        // is erased the moment the instant reaches the core. `time_reference=`
        // is the override for a caller who means a different one.
        let descriptors = DescriptorArgs {
            application_data,
            element_type,
            units,
            quantity_kind,
            unit_system,
            component_field,
            time_reference,
        }
        .resolve(py, inner.element_type, Some(initial_timestamp.reference))?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    /// Build from per-timestep logical values, encoding them into the array the
    /// store holds and declaring the element type they imply. See
    /// `SingleTimeSeries.from_values` for the value shapes and the rules.
    ///
    /// `values` is one entry per timestep in row-major order over the leading
    /// axes, so entry `(s * H + i) * count + j` is scenario `s`'s window `j`,
    /// step `i`. Those axes are `[scenario_count, H, count]`, with `H` derived
    /// from `horizon`/`resolution` — the arithmetic `encode_element_values`
    /// otherwise leaves to the caller.
    ///
    /// `scenario_count` is explicit here, where the constructor reads it off the
    /// array's first axis: there is no array yet to read it from.
    #[classmethod]
    #[pyo3(signature = (
            initial_timestamp, resolution, horizon, interval, count, scenario_count, values, name, *,
            application_data=None, element_type=None, units=None, quantity_kind=None,
            unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn from_values(
        _cls: &Bound<'_, pyo3::types::PyType>,
        py: Python<'_>,
        initial_timestamp: PyInstant,
        resolution: Bound<'_, PyAny>,
        horizon: Bound<'_, PyAny>,
        interval: Bound<'_, PyAny>,
        count: usize,
        scenario_count: usize,
        values: &Bound<'_, PyAny>,
        name: String,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        let resolution = pyany_to_period(&resolution)?;
        let horizon = pyany_to_period(&horizon)?;
        let interval = pyany_to_period(&interval)?;
        let decoded = from_values_payload(values, element_type.as_deref())?;
        let mut inner = core_lib::Scenarios::from_values(
            initial_timestamp.instant,
            resolution,
            horizon,
            interval,
            count,
            scenario_count,
            &decoded,
            name,
        )
        .map_err(InvalidParameterError::new_err)?;
        let descriptors = DescriptorArgs {
            application_data,
            element_type,
            units,
            quantity_kind,
            unit_system,
            component_field,
            time_reference,
        }
        .resolve(py, inner.element_type, Some(initial_timestamp.reference))?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    #[getter]
    fn scenario_count(&self) -> usize {
        self.inner.scenario_count
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

series_pymethods!(PySingleTimeSeries, 1);

#[pymethods]
impl PySingleTimeSeries {
    /// `name` is required.
    ///
    /// The keyword-only arguments are the series' descriptive attributes. They
    /// describe the values without addressing them, so none is part of a
    /// series' identity: two series differing only in these are a duplicate,
    /// and none can be filtered on except `component_field`. Each is stored on
    /// the association and handed back on a read.
    ///
    /// `units` labels the values (`"MW"`). `quantity_kind` names what kind of
    /// physical quantity they measure (`"ActivePower"`) — free-form, with QUDT
    /// `QuantityKind` local names the recommended vocabulary; it separates
    /// active from reactive power, which dimensional analysis cannot.
    /// `unit_system` is `"natural_units"` or `"component_base"`; omitting it
    /// leaves the basis unspecified, which is not the same as declaring natural
    /// units. `component_field` names the field on the owning component whose
    /// value these values are the time-varying form of
    /// (`"max_active_power"`) — free-form and never interpreted by the store.
    /// `application_data` is an opaque, package-owned payload (typically JSON)
    /// stored verbatim; end users are not expected to set it. `element_type`
    /// declares what the array's elements mean in the store's own vocabulary
    /// (`"tuple(3,f64)"`, `"piecewise_linear"`, …); omit it for plain numbers,
    /// where it defaults to the array's own dtype spelling.
    ///
    /// `time_reference` overrides the timestamp spelling, which is otherwise
    /// inferred from `initial_timestamp` (naive is zoneless; a `ZoneInfo` with
    /// a `key` names its zone). It takes `"utc"`, `"zoneless"`, a fixed offset
    /// (`"-07:00"`), or an IANA zone name.
    #[new]
    #[pyo3(signature = (
            initial_timestamp, resolution, data, name, *, application_data=None,
            element_type=None, units=None, quantity_kind=None, unit_system=None,
            component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        initial_timestamp: PyInstant,
        resolution: Bound<'_, PyAny>,
        data: &Bound<'_, PyAny>,
        name: String,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        let resolution = pyany_to_period(&resolution)?;
        let typed = typed_array_from_numpy(data)?;
        let mut inner =
            core_lib::SingleTimeSeries::new(initial_timestamp.instant, resolution, typed, name);
        // See the forecast constructors: the spelling rides in on the timestamp.
        let descriptors = DescriptorArgs {
            application_data,
            element_type,
            units,
            quantity_kind,
            unit_system,
            component_field,
            time_reference,
        }
        .resolve(py, inner.element_type, Some(initial_timestamp.reference))?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    /// Build from the timeline you actually hold, inferring `resolution` and
    /// **proving** the instants lie on it.
    ///
    /// The constructor takes `initial_timestamp` + `resolution` and the store
    /// cannot check the claim — the vector it describes is never supplied. This
    /// takes the vector: it either fits a period exactly, or raises
    /// `InvalidParameterError` naming the entry that broke the pattern and
    /// pointing at `NonSequentialTimeSeries`.
    ///
    /// **This is how a local-clock timeline reaches the store.** The store has
    /// no time-zone database and never runs local → instant; you materialize the
    /// grid with `zoneinfo` — where the policy for a nonexistent or ambiguous
    /// wall clock belongs — and hand over the instants. An hourly local grid in
    /// a DST zone *is* a uniform instant grid, so it compacts here; a daily or
    /// monthly one is not, and is refused so you store it explicitly.
    ///
    /// The timestamp spelling is inferred from the vector exactly as the
    /// constructor infers it from `initial_timestamp`, and the vector must agree
    /// on one spelling.
    ///
    /// Step on **UTC**, then convert back: adding a `timedelta` to an aware
    /// `datetime` is wall-clock arithmetic, so stepping in local time skips the
    /// repeated hour at a fall-back transition and leaves a two-hour gap in the
    /// instants — which this refuses, correctly, as not a grid.
    ///
    /// ```python
    /// denver = ZoneInfo("America/Denver")
    /// start = datetime(2024, 11, 3, tzinfo=denver).astimezone(timezone.utc)
    /// hours = [(start + timedelta(hours=k)).astimezone(denver) for k in range(6)]
    /// # 00:00 MDT, 01:00 MDT, 01:00 MST, 02:00 MST, ... -- the repeated hour is
    /// # two distinct instants an hour apart, which is why this is a grid.
    /// SingleTimeSeries.from_timestamps(hours, values, "load")   # -> resolution "PT1H"
    ///
    /// days = [datetime(2024, 11, d, tzinfo=denver) for d in range(1, 6)]
    /// SingleTimeSeries.from_timestamps(days, values, "peak")    # InvalidParameterError
    /// ```
    #[classmethod]
    #[pyo3(signature = (
            timestamps, data, name, *, application_data=None, element_type=None, units=None,
            quantity_kind=None, unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn from_timestamps(
        _cls: &Bound<'_, pyo3::types::PyType>,
        py: Python<'_>,
        timestamps: Vec<PyInstant>,
        data: &Bound<'_, PyAny>,
        name: String,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        // One vector, one spelling -- the same rule the irregular constructors
        // apply, and for the same reason: a series records one reference.
        let inferred = vector_reference(&timestamps)?;
        let instants: Vec<DateTime<Utc>> = timestamps.iter().map(|t| t.instant).collect();
        let typed = typed_array_from_numpy(data)?;
        let mut inner = core_lib::SingleTimeSeries::from_timestamps(&instants, typed, name)
            .map_err(InvalidParameterError::new_err)?;
        let descriptors = DescriptorArgs {
            application_data,
            element_type,
            units,
            quantity_kind,
            unit_system,
            component_field,
            time_reference,
        }
        .resolve(py, inner.element_type, inferred)?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    /// Build from per-timestep logical values, encoding them into the array the
    /// store holds and declaring the element type they imply.
    ///
    /// The pairing is the point. An `element_type` and the array it describes
    /// are two independent things a caller can get out of step; the store
    /// rejects the mismatch, but only after the fact. Deriving both from one
    /// set of values means there is none to reject — and for a forecast it also
    /// derives the leading dimensions, which `encode_element_values` otherwise
    /// asks the caller to compute.
    ///
    /// `values` is one entry per timestep, in the shapes `decoded_values`
    /// returns, and the entry's own shape is what names the element type:
    ///
    /// ```python
    /// SingleTimeSeries.from_values(
    ///     start, timedelta(hours=1),
    ///     [[{"x": 0.0, "y": 1.0}, {"x": 1.0, "y": 3.0}], [{"x": 0.0, "y": 2.0}]],
    ///     "variable_cost",
    /// )                                            # -> element_type "piecewise_linear"
    /// ```
    ///
    /// | `values` entry                                | element type          |
    /// | --------------------------------------------- | --------------------- |
    /// | `{"proportional": _, "constant": _}`          | `linear_function`     |
    /// | `{"quadratic": _, "proportional": _, ...}`    | `quadratic_function`  |
    /// | `list[{"x": _, "y": _}]`                      | `piecewise_linear`    |
    /// | `{"x": list, "y": list}`                      | `piecewise_step`      |
    /// | `list[float]` of length N                     | `tuple(N,f64)`        |
    ///
    /// A series of plain numbers has no encoding to do: pass the numpy array to
    /// the constructor as `data=`.
    ///
    /// `element_type=` is accepted as an assertion, not an override — it raises
    /// `InvalidParameterError` if it disagrees with the values. Where the values
    /// name nothing it is the only thing to go on: an empty `values`, or rows
    /// that are all empty and read equally as a curve with no points or a tuple
    /// with no fields. The remaining keyword arguments are the descriptive
    /// attributes documented on the constructor.
    ///
    /// Raises `InvalidParameterError` if the values cannot be encoded: tuple
    /// rows of differing arity, a step function whose `x` and `y` lengths
    /// disagree, or (for a forecast) a count that does not fill the windows.
    #[classmethod]
    #[pyo3(signature = (
            initial_timestamp, resolution, values, name, *,
            application_data=None, element_type=None, units=None, quantity_kind=None,
            unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn from_values(
        _cls: &Bound<'_, pyo3::types::PyType>,
        py: Python<'_>,
        initial_timestamp: PyInstant,
        resolution: Bound<'_, PyAny>,
        values: &Bound<'_, PyAny>,
        name: String,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        let resolution = pyany_to_period(&resolution)?;
        let decoded = from_values_payload(values, element_type.as_deref())?;
        let mut inner = core_lib::SingleTimeSeries::from_values(
            initial_timestamp.instant,
            resolution,
            &decoded,
            name,
        )
        .map_err(InvalidParameterError::new_err)?;
        let descriptors = DescriptorArgs {
            application_data,
            element_type,
            units,
            quantity_kind,
            unit_system,
            component_field,
            time_reference,
        }
        .resolve(py, inner.element_type, Some(initial_timestamp.reference))?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
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

    #[getter]
    fn length(&self) -> usize {
        self.inner.length
    }

    #[getter]
    fn resolution(&self) -> String {
        self.inner.resolution.to_iso8601()
    }

    /// The whole grid materialized, `initial_timestamp` first, spelled the way
    /// it was written — the regular counterpart of the explicit vector
    /// `NonSequentialTimeSeries` and `PersistentTimeSeries` carry.
    ///
    /// This is the only correct way to rebuild the timeline. A `P1M` resolution
    /// steps on the calendar, so multiplying a fixed span by the index gets a
    /// monthly series wrong.
    #[getter]
    fn timestamps<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyAny>>> {
        let grid: Vec<DateTime<Utc>> = self.inner.timestamps().collect();
        spell_instants(py, &grid, self.inner.time_reference.as_ref())
    }

    /// Build a `SingleTimeSeries` from a `pyarrow.Table` — the inverse of
    /// `to_arrow()`, and a reader of foreign tables too.
    ///
    /// Requires pyarrow, which is not installed with infrastore — use
    /// `pip install 'infrastore[arrow]'`.
    ///
    /// The table needs a `timestamp` column and a `value` column. Everything
    /// else is read from `table.schema.metadata` when it is there, and inferred
    /// when it is not:
    ///
    /// | Missing | Read as |
    /// | --- | --- |
    /// | `resolution` | Inferred from the timestamps, which must walk a grid. |
    /// | `element_type` | The leaf Arrow type. A `fixed_size_list<T>[N]` becomes dtype `T` with element shape `[N]` — *dense*, not `tuple(N,T)`, because the bytes cannot say and dense assumes less. |
    /// | `time_reference` | The timestamp column's zone; a column with no zone reads as `zoneless`, since a naive timestamp is a wall clock. |
    /// | `name` | Nothing — a name is part of a series' identity, so pass `name=`. |
    ///
    /// Every keyword overrides the metadata, except `element_type`, which is an
    /// **assertion**: it states the reading the bytes cannot, and a value that
    /// contradicts the table's own is an error rather than a silent
    /// replacement.
    ///
    /// Refused rather than coerced: nulls in either column; a microsecond or
    /// nanosecond timestamp that is not a whole millisecond (the store's own
    /// precision, and rounding one would move it); rows that leave a declared
    /// grid; and `struct`/`list` value columns, which are the decoded form
    /// `to_arrow()` does not produce.
    ///
    /// ```python
    /// series = SingleTimeSeries.from_arrow(series.to_arrow())
    /// import pyarrow.parquet as pq
    /// SingleTimeSeries.from_arrow(pq.read_table("load.parquet"))
    /// ```
    #[classmethod]
    #[pyo3(signature = (
            table, *, name=None, resolution=None, application_data=None, element_type=None,
            units=None, quantity_kind=None, unit_system=None, component_field=None,
            time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn from_arrow(
        _cls: &Bound<'_, pyo3::types::PyType>,
        py: Python<'_>,
        table: &Bound<'_, PyAny>,
        name: Option<String>,
        resolution: Option<Bound<'_, PyAny>>,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        let parts = arrow_parts(py, table)?;
        let name = arrow_name(&parts, name)?;
        let instants = arrow_instant_vec(&parts.millis)?;
        let typed = typed_array_from_numpy(&parts.values)?;
        // The keyword, then the table's, then whatever the timestamps imply.
        let resolution = match resolution {
            Some(r) => Some(pyany_to_period(&r)?),
            None => parts
                .metadata
                .get("resolution")
                .map(|iso| {
                    core_lib::Period::from_iso8601(iso)
                        .map_err(|e| InvalidParameterError::new_err(e.to_string()))
                })
                .transpose()?,
        };
        let mut inner = match resolution {
            None => core_lib::SingleTimeSeries::from_timestamps(&instants, typed, name)
                .map_err(InvalidParameterError::new_err)?,
            Some(resolution) => {
                let Some(&first) = instants.first() else {
                    return Err(InvalidParameterError::new_err(
                        "a SingleTimeSeries is anchored at its first timestamp, and this table \
                             has no rows; give the anchor another way, or read it as a \
                             NonSequentialTimeSeries"
                            .to_string(),
                    ));
                };
                let series = core_lib::SingleTimeSeries::new(first, resolution, typed, name);
                // Checked against the grid the resolution *generates*, not
                // against successive differences: `Period::Months` clamps to
                // month end, so those are not the same test.
                let grid: Vec<DateTime<Utc>> = series.timestamps().collect();
                if grid != instants {
                    return Err(InvalidParameterError::new_err(format!(
                        "the timestamps do not sit on a {} grid anchored at {first}",
                        resolution.to_iso8601()
                    )));
                }
                series
            }
        };
        let descriptors = arrow_descriptor_args(
            &parts,
            DescriptorArgs {
                application_data,
                element_type,
                units,
                quantity_kind,
                unit_system,
                component_field,
                time_reference,
            },
        )?
        .resolve(py, inner.element_type, None)?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    /// This series as a two-column `pyarrow.Table`: `timestamp` and `value`.
    ///
    /// Requires pyarrow, which is not installed with infrastore — use
    /// `pip install 'infrastore[arrow]'`.
    ///
    /// The timestamp column materializes the grid (calendar-aware for a monthly
    /// resolution) and is typed `timestamp[ms, tz=…]` in the series' own
    /// spelling: UTC, a fixed offset, an IANA zone, or no zone at all for a
    /// zoneless series. The value column is the array — a primitive type for a
    /// scalar series, nested `fixed_size_list` for a multidimensional
    /// per-timestep value. Composite element types stay in their stored
    /// packing; `element_type` in the schema metadata names what they are, and
    /// `decode_element_values` unpacks them.
    ///
    /// The series' descriptive attributes (`name`, `units`, `quantity_kind`,
    /// `unit_system`, `component_field`, `element_type`, `time_reference`,
    /// `resolution`, `application_data`) ride in `table.schema.metadata`, so the
    /// table is not lossy against the object and survives a Parquet round trip.
    ///
    /// ```python
    /// table = series.to_arrow()
    /// table.to_pandas()          # if pandas is installed
    /// polars.from_arrow(table)   # if polars is
    /// ```
    fn to_arrow<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let grid: Vec<DateTime<Utc>> = self.inner.timestamps().collect();
        let mut metadata = arrow_metadata!(self.inner, "SingleTimeSeries");
        metadata.insert("resolution".to_string(), self.inner.resolution.to_iso8601());
        arrow_table(
            py,
            &grid,
            self.inner.time_reference.as_ref(),
            &self.inner.data,
            metadata,
        )
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

series_pymethods!(PyNonSequentialTimeSeries, 1);

#[pymethods]
impl PyNonSequentialTimeSeries {
    /// `name` is required.
    ///
    /// The keyword-only arguments are the descriptive attributes documented on
    /// `SingleTimeSeries`; they travel with the series into the store and come
    /// back on a read.
    #[new]
    #[pyo3(signature = (
            timestamps, data, name, *, application_data=None, element_type=None, units=None,
            quantity_kind=None, unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        timestamps: Vec<PyInstant>,
        data: &Bound<'_, PyAny>,
        name: String,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        let typed = typed_array_from_numpy(data)?;
        // One series records one spelling, so the vector has to agree on one.
        let reference = vector_reference(&timestamps)?;
        let mut inner =
            core_lib::NonSequentialTimeSeries::new(instants_to_utc(&timestamps), typed, name)
                .map_err(InvalidParameterError::new_err)?;
        let descriptors = DescriptorArgs {
            application_data,
            element_type,
            units,
            quantity_kind,
            unit_system,
            component_field,
            time_reference,
        }
        .resolve(py, inner.element_type, reference)?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    /// Build from per-timestamp logical values, encoding them into the array
    /// the store holds and declaring the element type they imply. See
    /// `SingleTimeSeries.from_values` for the value shapes and the rules.
    #[classmethod]
    #[pyo3(signature = (
            timestamps, values, name, *,
            application_data=None, element_type=None, units=None, quantity_kind=None,
            unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn from_values(
        _cls: &Bound<'_, pyo3::types::PyType>,
        py: Python<'_>,
        timestamps: Vec<PyInstant>,
        values: &Bound<'_, PyAny>,
        name: String,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        // One series records one spelling, so the vector has to agree on one.
        let reference = vector_reference(&timestamps)?;
        let decoded = from_values_payload(values, element_type.as_deref())?;
        let mut inner = core_lib::NonSequentialTimeSeries::from_values(
            instants_to_utc(&timestamps),
            &decoded,
            name,
        )
        .map_err(InvalidParameterError::new_err)?;
        let descriptors = DescriptorArgs {
            application_data,
            element_type,
            units,
            quantity_kind,
            unit_system,
            component_field,
            time_reference,
        }
        .resolve(py, inner.element_type, reference)?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
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

    #[getter]
    fn length(&self) -> usize {
        self.inner.length
    }

    /// Build a `NonSequentialTimeSeries` from a `pyarrow.Table` — the inverse of
    /// `to_arrow()`. See `SingleTimeSeries.from_arrow` for the full rules; the
    /// only difference is that the timestamps are taken as they are and need not
    /// walk a grid.
    #[classmethod]
    #[pyo3(signature = (
            table, *, name=None, application_data=None, element_type=None, units=None,
            quantity_kind=None, unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn from_arrow(
        _cls: &Bound<'_, pyo3::types::PyType>,
        py: Python<'_>,
        table: &Bound<'_, PyAny>,
        name: Option<String>,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        let parts = arrow_parts(py, table)?;
        let name = arrow_name(&parts, name)?;
        let instants = arrow_instant_vec(&parts.millis)?;
        let typed = typed_array_from_numpy(&parts.values)?;
        let mut inner = core_lib::NonSequentialTimeSeries::new(instants, typed, name)
            .map_err(InvalidParameterError::new_err)?;
        let descriptors = arrow_descriptor_args(
            &parts,
            DescriptorArgs {
                application_data,
                element_type,
                units,
                quantity_kind,
                unit_system,
                component_field,
                time_reference,
            },
        )?
        .resolve(py, inner.element_type, None)?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    /// This series as a two-column `pyarrow.Table`: `timestamp` and `value`.
    ///
    /// Requires pyarrow, which is not installed with infrastore — use
    /// `pip install 'infrastore[arrow]'`. Identical in shape to
    /// `SingleTimeSeries.to_arrow`, except that the timestamp column is the
    /// stored vector rather than a computed grid, and the metadata carries no
    /// `resolution` because an irregular timeline has no constant step.
    ///
    /// The rows are the timestamps and nothing else: an irregular series has no
    /// value *between* two of them, so nothing is filled in.
    fn to_arrow<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        arrow_table(
            py,
            &self.inner.timestamps,
            self.inner.time_reference.as_ref(),
            &self.inner.data,
            arrow_metadata!(self.inner, "NonSequentialTimeSeries"),
        )
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

// ---- PersistentTimeSeries -------------------------------------------------

/// A sparse step function: breakpoints plus one value each, holding the last
/// value forward.
///
/// Constructed exactly like a `NonSequentialTimeSeries` — a strictly increasing
/// list of timestamps and an array with one value per timestamp — and the
/// timestamp spelling is inferred from the input the same way (naive datetimes
/// are zoneless, a `ZoneInfo` with a `key` names its zone). The difference is
/// what a read *between* the breakpoints means: the value at breakpoint `i`
/// stays in force until breakpoint `i + 1`, and past the last one forever,
/// where a `NonSequentialTimeSeries` has no value there at all. There is no
/// value before the first breakpoint.
#[pyclass(name = "PersistentTimeSeries", module = "infrastore", from_py_object)]
#[derive(Clone)]
pub struct PyPersistentTimeSeries {
    inner: core_lib::PersistentTimeSeries,
}

series_pymethods!(PyPersistentTimeSeries, 1);

#[pymethods]
impl PyPersistentTimeSeries {
    /// `name` is required.
    ///
    /// The keyword-only arguments are the descriptive attributes documented on
    /// `SingleTimeSeries`; they travel with the series into the store and come
    /// back on a read. A step function's scalar-collapse policy belongs in
    /// `application_data` — the store has no column for it.
    #[new]
    #[pyo3(signature = (
            timestamps, data, name, *, application_data=None, element_type=None, units=None,
            quantity_kind=None, unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        timestamps: Vec<PyInstant>,
        data: &Bound<'_, PyAny>,
        name: String,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        let typed = typed_array_from_numpy(data)?;
        // One series records one spelling, so the vector has to agree on one.
        let reference = vector_reference(&timestamps)?;
        let mut inner =
            core_lib::PersistentTimeSeries::new(instants_to_utc(&timestamps), typed, name)
                .map_err(InvalidParameterError::new_err)?;
        let descriptors = DescriptorArgs {
            application_data,
            element_type,
            units,
            quantity_kind,
            unit_system,
            component_field,
            time_reference,
        }
        .resolve(py, inner.element_type, reference)?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    /// Build from per-breakpoint logical values, encoding them into the array
    /// the store holds and declaring the element type they imply. See
    /// `SingleTimeSeries.from_values` for the value shapes and the rules.
    #[classmethod]
    #[pyo3(signature = (
            timestamps, values, name, *,
            application_data=None, element_type=None, units=None, quantity_kind=None,
            unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn from_values(
        _cls: &Bound<'_, pyo3::types::PyType>,
        py: Python<'_>,
        timestamps: Vec<PyInstant>,
        values: &Bound<'_, PyAny>,
        name: String,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        // One series records one spelling, so the vector has to agree on one.
        let reference = vector_reference(&timestamps)?;
        let decoded = from_values_payload(values, element_type.as_deref())?;
        let mut inner = core_lib::PersistentTimeSeries::from_values(
            instants_to_utc(&timestamps),
            &decoded,
            name,
        )
        .map_err(InvalidParameterError::new_err)?;
        let descriptors = DescriptorArgs {
            application_data,
            element_type,
            units,
            quantity_kind,
            unit_system,
            component_field,
            time_reference,
        }
        .resolve(py, inner.element_type, reference)?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    /// The breakpoint vector, spelled the way it was written.
    #[getter]
    fn timestamps<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyAny>>> {
        spell_instants(
            py,
            &self.inner.timestamps,
            self.inner.time_reference.as_ref(),
        )
    }

    #[getter]
    fn length(&self) -> usize {
        self.inner.length
    }

    /// The value in force at `at`.
    ///
    /// A step function is defined at *every* instant from its first breakpoint
    /// onward, so this is the series' value at `at` in the ordinary sense, not an
    /// approximation of one: between breakpoints the previous value is carried
    /// forward, and past the last breakpoint the last value holds indefinitely.
    /// The single error is an `at` strictly *before* the first breakpoint, where
    /// no value was ever declared — `InvalidParameterError`, never a clamp.
    ///
    /// Returns exactly what indexing `data` returns: a numpy scalar of the
    /// series' own dtype for a scalar series, or the per-step subarray for a
    /// series with a shaped element. `at` must be spelled the way the series'
    /// breakpoints are (both aware or both naive).
    fn value_at<'py>(&self, py: Python<'py>, at: PyInstant) -> PyResult<Bound<'py, PyAny>> {
        check_point_spelling(&at, self.inner.time_reference.as_ref(), "this series")?;
        let row = self
            .inner
            .row_at(at.instant)
            .map_err(InvalidParameterError::new_err)?;
        let array = numpy_from_typed(py, &row)?;
        if row.shape.is_empty() {
            // A scalar step is a 0-d array; `arr[()]` is numpy's own spelling
            // for the scalar inside one, and keeps the dtype that `.item()`
            // would discard.
            array.get_item(PyTuple::empty(py))
        } else {
            Ok(array)
        }
    }

    /// The 0-based index into `timestamps` and `data` of the breakpoint
    /// governing `at` — the greatest breakpoint `<= at`.
    ///
    /// `value_at` is the usual way to ask; this is for a caller that wants the
    /// row itself (to look up a parallel array, say). Errors like `value_at`.
    fn index_at(&self, at: PyInstant) -> PyResult<usize> {
        check_point_spelling(&at, self.inner.time_reference.as_ref(), "this series")?;
        self.inner
            .index_at(at.instant)
            .map_err(InvalidParameterError::new_err)
    }

    /// The breakpoint governing `at` — the instant from which the value at `at`
    /// has been in force, spelled the way the series' breakpoints are.
    ///
    /// Equal to `at` exactly when `at` is itself a breakpoint. Errors like
    /// `value_at`.
    fn breakpoint_at<'py>(&self, py: Python<'py>, at: PyInstant) -> PyResult<Bound<'py, PyAny>> {
        let index = self.index_at(at)?;
        spell_instant(
            py,
            self.inner.timestamps[index],
            self.inner.time_reference.as_ref(),
        )
    }

    /// Build a `PersistentTimeSeries` from a `pyarrow.Table` — the inverse of
    /// `to_arrow()`. See `SingleTimeSeries.from_arrow` for the full rules.
    ///
    /// The rows are **breakpoints**, not instants: a step function is stored
    /// sparsely and the table is that sparse form. This class has to be named,
    /// because a `PersistentTimeSeries` table is shaped exactly like a
    /// `NonSequentialTimeSeries` one — the two differ only in what the values
    /// mean between the rows.
    #[classmethod]
    #[pyo3(signature = (
            table, *, name=None, application_data=None, element_type=None, units=None,
            quantity_kind=None, unit_system=None, component_field=None, time_reference=None
        ))]
    #[allow(clippy::too_many_arguments)]
    fn from_arrow(
        _cls: &Bound<'_, pyo3::types::PyType>,
        py: Python<'_>,
        table: &Bound<'_, PyAny>,
        name: Option<String>,
        application_data: Option<String>,
        element_type: Option<String>,
        units: Option<String>,
        quantity_kind: Option<String>,
        unit_system: Option<String>,
        component_field: Option<String>,
        time_reference: Option<String>,
    ) -> PyResult<Self> {
        let parts = arrow_parts(py, table)?;
        let name = arrow_name(&parts, name)?;
        let instants = arrow_instant_vec(&parts.millis)?;
        let typed = typed_array_from_numpy(&parts.values)?;
        let mut inner = core_lib::PersistentTimeSeries::new(instants, typed, name)
            .map_err(InvalidParameterError::new_err)?;
        let descriptors = arrow_descriptor_args(
            &parts,
            DescriptorArgs {
                application_data,
                element_type,
                units,
                quantity_kind,
                unit_system,
                component_field,
                time_reference,
            },
        )?
        .resolve(py, inner.element_type, None)?;
        apply_descriptors!(inner, descriptors);
        Ok(Self { inner })
    }

    /// This series as a two-column `pyarrow.Table`: `timestamp` and `value`.
    ///
    /// Requires pyarrow, which is not installed with infrastore — use
    /// `pip install 'infrastore[arrow]'`.
    ///
    /// **One row per breakpoint, not per instant.** A step function is stored
    /// sparsely and the table is that sparse form: the value at a row stays in
    /// force until the next row, and past the last one forever. Resampling it
    /// onto a dense grid is the caller's to do, and needs a grid the series
    /// itself does not carry — there is no value before the first breakpoint, so
    /// a grid starting earlier has no answer to give.
    fn to_arrow<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        arrow_table(
            py,
            &self.inner.timestamps,
            self.inner.time_reference.as_ref(),
            &self.inner.data,
            arrow_metadata!(self.inner, "PersistentTimeSeries"),
        )
    }

    /// Number of breakpoints (`length`).
    fn __len__(&self) -> usize {
        self.inner.length
    }

    fn __repr__(&self) -> String {
        format!(
            "PersistentTimeSeries(name={:?}, length={}, shape={:?}, time_reference={})",
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
/// `NonSequentialTimeSeries`, `PersistentTimeSeries`, `Deterministic`,
/// `Probabilistic`, or `Scenarios`).
fn extract_time_series_data(time_series: &Bound<'_, PyAny>) -> PyResult<core_lib::TimeSeriesData> {
    if let Ok(single) = time_series.extract::<PySingleTimeSeries>() {
        Ok(core_lib::TimeSeriesData::SingleTimeSeries(single.inner))
    } else if let Ok(ns) = time_series.extract::<PyNonSequentialTimeSeries>() {
        Ok(core_lib::TimeSeriesData::NonSequentialTimeSeries(ns.inner))
    } else if let Ok(p) = time_series.extract::<PyPersistentTimeSeries>() {
        Ok(core_lib::TimeSeriesData::PersistentTimeSeries(p.inner))
    } else if let Ok(det) = time_series.extract::<PyDeterministic>() {
        Ok(core_lib::TimeSeriesData::Deterministic(det.inner))
    } else if let Ok(prob) = time_series.extract::<PyProbabilistic>() {
        Ok(core_lib::TimeSeriesData::Probabilistic(prob.inner))
    } else if let Ok(scen) = time_series.extract::<PyScenarios>() {
        Ok(core_lib::TimeSeriesData::Scenarios(scen.inner))
    } else {
        Err(InvalidParameterError::new_err(
            "time_series must be SingleTimeSeries, NonSequentialTimeSeries, \
                 PersistentTimeSeries, Deterministic, Probabilistic, or Scenarios",
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
        core_lib::TimeSeriesData::PersistentTimeSeries(s) => {
            Ok(Py::new(py, PyPersistentTimeSeries { inner: s })?.into_any())
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
    /// `resolution` is `None` for a `NonSequentialTimeSeries` or
    /// `PersistentTimeSeries` reader: an
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
        let arr = core_lib::TypedArray::new(group.dtype(), shape, group.values().to_vec())
            .map_err(InvalidParameterError::new_err)?;
        numpy_from_typed(py, &arr)
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
        let arr = core_lib::TypedArray::new(
            slot.dtype(),
            slot.window_shape().to_vec(),
            slot.window().to_vec(),
        )
        .map_err(InvalidParameterError::new_err)?;
        numpy_from_typed(py, &arr)
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

    /// The text [`PyStore::show`] prints.
    ///
    /// Every number here comes from a catalog aggregate query -- no array is
    /// touched -- so the cost does not grow with how much data the store holds.
    fn summary_text(&self) -> PyResult<String> {
        let store = self.store()?;
        let by_type = store.counts_by_type().map_err(map_err)?;
        let detailed = store.time_series_counts_detailed().map_err(map_err)?;
        let arrays = store.num_distinct_arrays().map_err(map_err)?;
        let attachments = store
            .count_supplemental_attribute_associations(&Default::default())
            .map_err(map_err)?;
        let edges = store
            .count_parent_child_associations(&Default::default())
            .map_err(map_err)?;

        let mut out = format!(
            "Store: {} ({})\n",
            self.descr,
            if self.read_only {
                "read-only"
            } else {
                "read-write"
            }
        );

        let total: i64 = by_type.iter().map(|(_, n)| n).sum();
        if total == 0 {
            out.push_str("Time series: none\n");
        } else {
            out.push_str(&format!(
                "Time series: {} association{} over {} distinct array{}\n",
                total,
                plural(total),
                arrays,
                plural(arrays),
            ));
            // Static types first, then forecasts -- the grouping the docs use.
            // `counts_by_type` orders by the numeric type code instead, which
            // puts `PersistentTimeSeries` after the forecasts because it was
            // appended to a list that is an on-disk contract.
            let mut rows = by_type;
            rows.sort_by_key(|(t, _)| type_display_rank(*t));
            let name_width = rows
                .iter()
                .map(|(t, _)| t.as_str().len())
                .max()
                .unwrap_or(0);
            let count_width = rows
                .iter()
                .map(|(_, n)| n.to_string().len())
                .max()
                .unwrap_or(0);
            for (t, n) in rows {
                out.push_str(&format!(
                    "  {:<name_width$}  {:>count_width$}\n",
                    t.as_str(),
                    n,
                ));
            }
        }

        out.push_str(&format!(
            "Owners with time series: {} component{}, {} supplemental attribute{}\n",
            detailed.components_with_time_series,
            plural(detailed.components_with_time_series),
            detailed.supplemental_attributes_with_time_series,
            plural(detailed.supplemental_attributes_with_time_series),
        ));
        out.push_str(&format!(
            "Supplemental attribute attachments: {attachments}\n"
        ));
        out.push_str(&format!("Parent/child edges: {edges}"));
        Ok(out)
    }
}

/// `"s"` unless `n` is 1, for the counted nouns in a `show()` summary.
fn plural(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Sort key putting the three static types before the four forecast ones in a
/// `show()` listing, each group in the order the documentation introduces them.
fn type_display_rank(t: core_lib::TimeSeriesType) -> u8 {
    use core_lib::TimeSeriesType::*;
    match t {
        SingleTimeSeries => 0,
        NonSequentialTimeSeries => 1,
        PersistentTimeSeries => 2,
        Deterministic => 3,
        DeterministicSingleTimeSeries => 4,
        Probabilistic => 5,
        Scenarios => 6,
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
                core_lib::Store::create_replacing(path, compression, catalog)
            }
            (false, _) => core_lib::Store::create_with_catalog(
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
        let store = core_lib::Store::open_copy(&src, &dest, catalog).map_err(map_err)?;
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
            core_lib::Store::open_with_catalog(&path, read_only, catalog).map_err(map_err)?;
        Ok(Self {
            inner: Some(store),
            read_only,
            descr,
        })
    }

    /// Open the array half of an artifact whose catalog is **absent**, minting
    /// an empty one, and return a writable store holding every array and no
    /// rows.
    ///
    /// The way in to a store shipped as arrays plus an OpenAPI document — a
    /// `system.json` beside a `time_series.h5`, with no `.sqlite` carried along.
    /// Replay the document's rows with
    /// `import_time_series_associations_openapi()` and
    /// `import_supplemental_attribute_associations_openapi()` and the artifact
    /// is whole again, association ids included.
    ///
    /// `Store.open()` cannot do this: the array file carries a generation stamp
    /// and a catalog created on the spot does not, so it reports a mismatched
    /// artifact — the right answer everywhere except here. The catalog minted
    /// here inherits the array file's own stamp, so every later `open()`
    /// behaves normally.
    ///
    /// Raises `StoreExistsError` when `<path>.sqlite` is already there: minting
    /// over a real catalog would discard its rows, and a store that has one
    /// wants `Store.open()`.
    #[classmethod]
    #[pyo3(signature = (path, *, catalog="attached"))]
    fn open_without_catalog(
        _cls: &Bound<'_, pyo3::types::PyType>,
        path: PathBuf,
        catalog: &str,
    ) -> PyResult<Self> {
        let catalog = parse_catalog(Some(catalog), false)?;
        let descr = path.display().to_string();
        let store = core_lib::Store::open_without_catalog(&path, catalog).map_err(map_err)?;
        Ok(Self {
            inner: Some(store),
            read_only: false,
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

    /// The byte budget an open transaction's buffered adds are held to, and
    /// through it **how wide a dataset a run of single adds can write**.
    ///
    /// Inside a transaction a packed add joins a pending block per shape group
    /// rather than filling a growth-pool slot, and each block becomes one
    /// dataset at the commit. This is the ceiling on what those blocks hold
    /// across every group: cross it and the widest is written out early, which
    /// costs an extra dataset and nothing else. Raising it gives a loop of
    /// `add_time_series` the dataset `add_time_series_bulk` of the same series
    /// would write, and the memory that buys is the memory the bulk call's
    /// caller was holding anyway:
    ///
    /// ```python
    /// store.write_buffer_bytes = 1 << 30      # 1 GiB
    /// with store.transaction():
    ///     for s in series:
    ///         store.add_time_series(owner_id=..., time_series=s, ...)
    /// ```
    ///
    /// The figure belongs to this `Store` object, not to the artifact: nothing
    /// is persisted, and a store reopened elsewhere is back to the 128 MiB
    /// default. A per-group block still stops at the width one chunk row holds,
    /// which no budget raises. Setting it below what an open transaction has
    /// already buffered writes those blocks out immediately. Zero raises
    /// `InvalidParameterError`. An in-memory store records it without acting on
    /// it, having no datasets to size.
    #[getter]
    fn write_buffer_bytes(&self) -> PyResult<u64> {
        Ok(self.store()?.write_buffer_bytes() as u64)
    }

    #[setter]
    fn set_write_buffer_bytes(&mut self, bytes: u64) -> PyResult<()> {
        let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
        self.store_mut()?
            .set_write_buffer_bytes(bytes)
            .map_err(map_err)
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

    /// Print a summary of what this store holds: the time-series associations
    /// broken down by type, how many distinct arrays back them, how many owners
    /// have one, and the size of the two association catalogs.
    ///
    /// `file` is any writable object, defaulting to `sys.stdout` -- the same
    /// argument `print` takes, and passed straight to it.
    ///
    /// Every number is one catalog aggregate query, so this stays cheap on a
    /// large store; nothing reads an array.
    ///
    /// ```python
    /// store.show()
    /// # Store: system.h5 (read-write)
    /// # Time series: 128 associations over 128 distinct arrays
    /// #   SingleTimeSeries      100
    /// #   PersistentTimeSeries    8
    /// #   Deterministic          20
    /// # Owners with time series: 108 components, 0 supplemental attributes
    /// # Supplemental attribute attachments: 12
    /// # Parent/child edges: 5
    /// ```
    #[pyo3(signature = (*, file=None))]
    fn show(&self, py: Python<'_>, file: Option<Bound<'_, PyAny>>) -> PyResult<()> {
        let text = self.summary_text()?;
        let kwargs = PyDict::new(py);
        if let Some(f) = file {
            kwargs.set_item("file", f)?;
        }
        py.import("builtins")?
            .call_method("print", (text,), Some(&kwargs))?;
        Ok(())
    }

    /// Add a time series. The association `name` comes from the time series
    /// object (`time_series.name`).
    ///
    /// `features` is a `dict[str, int|float|bool|str]`. A feature name that
    /// shadows a time-series or key field (`name`, `resolution`, `owner_id`,
    /// …) is rejected with `InvalidParameterError`.
    ///
    /// `features` is the only thing this call adds to the series. Everything
    /// that *describes* the values — `units`, `quantity_kind`, `unit_system`,
    /// `component_field`, `application_data`, `element_type`,
    /// `time_reference` — is set on the time-series object itself, because that
    /// is what a read hands back: a series read from one store can be added to
    /// another unchanged, and no descriptor can be lost between the two calls.
    #[pyo3(signature = (owner_id, owner_type, owner_category, time_series, *, features=None))]
    fn add_time_series(
        &mut self,
        owner_id: i64,
        owner_type: &str,
        owner_category: PyOwnerCategory,
        time_series: &Bound<'_, PyAny>,
        features: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<i64> {
        let features = features_from_dict(features)?;
        let data = extract_time_series_data(time_series)?;
        let request = core_lib::AddRequest::new(owner_id, owner_type, owner_category.into(), data)
            .with_features(features);
        let added = self.store_mut()?.add(request).map_err(map_err)?;
        Ok(added.get())
    }

    /// Add many time series in one call, committing the metadata catalog once
    /// for the whole batch. This is much faster than calling
    /// `add_time_series` in a loop **outside a transaction**, where each call
    /// pays its own SQLite transaction and its own HDF5 flush. Inside one it is
    /// not faster: the per-call savepoints release into the enclosing
    /// transaction rather than committing, and the adds buffer into the same
    /// blocks this call writes. Reach for this when the whole batch is already
    /// in hand; see `begin_transaction` for the run-of-single-adds spelling and
    /// the one thing it costs (a pending buffer that spills at 128 MiB, where
    /// this call has no ceiling of its own).
    ///
    /// `items` is a list of dicts whose keys mirror `add_time_series`'s
    /// parameters: `owner_id`, `owner_type`, `owner_category`, `time_series`,
    /// and optionally `features`. Any other key raises, as the misspelled
    /// keyword it almost always is: `add_time_series` rejects one for free, and
    /// reading only the keys it knows made the bulk path silently drop
    /// `feautres` and every other typo, along with whatever it was carrying.
    /// The descriptive attributes ride on each item's `time_series` object, as
    /// they do on the single-series path.
    ///
    /// All-or-nothing: if any item fails, the entire batch is rolled back.
    /// Returns the catalog `id` of each new row, in input order.
    ///
    /// No item may name its own `id`: the catalog assigns, and the write reports
    /// what it chose. "Never reissued" is a guarantee of `AUTOINCREMENT`, and a
    /// caller free to name an id could re-file a retired one. The one writer
    /// that files rows under supplied ids is
    /// `import_time_series_associations_openapi`, which replays a document that
    /// recorded them.
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
            let data = extract_time_series_data(&time_series)?;
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
        let resolution = resolution.as_ref().map(pyany_to_period).transpose()?;
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

    /// Return True if the store holds no persistent content of any kind — no
    /// time series, no associations in any catalog, and no store attributes.
    ///
    /// Answered by short-circuited existence probes, one per catalog table, so
    /// the cost does not grow with the store. Prefer it over a conjunction over
    /// the ``count_*`` methods: that runs a full aggregation, and it silently
    /// stops being correct when the catalog gains a table.
    fn is_empty(&self) -> PyResult<bool> {
        self.store()?.is_empty().map_err(map_err)
    }

    #[pyo3(signature = (time_series_type=None))]
    fn get_resolutions(&self, time_series_type: Option<Bound<'_, PyAny>>) -> PyResult<Vec<String>> {
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
        let resolution = resolution.as_ref().map(pyany_to_period).transpose()?;
        let interval = interval.as_ref().map(pyany_to_period).transpose()?;
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
    /// timestamp vector, which is also what pools their arrays on disk.
    ///
    /// `time_series_type="PersistentTimeSeries"` also takes no resolution, and
    /// is the one case whose columns need **not** share a timeline: a step
    /// function has a value at every instant from its first breakpoint on, so
    /// each column carries its values forward on breakpoints of its own.
    /// `timestamps()`
    /// is then the sorted union of every column's breakpoints, and reading
    /// before some column's first breakpoint raises `InvalidParameterError`
    /// naming that column.
    ///
    /// Drive any of them with `static_read`.
    ///
    /// **`window_start` lifts the shared-grid requirement.** Pass one and the
    /// reader sweeps the span you name rather than the grid the series happen to
    /// share: each column then reads at an offset of its own, so
    /// `SingleTimeSeries` that begin at different instants, or run for different
    /// lengths, read together as long as they all cover the window.
    /// `window_length` gives it an extent in timesteps; without one the reader
    /// runs as far from the anchor as *every* matched series reaches.
    ///
    /// It is distinct from the `initial_timestamp` / `length` *filter*
    /// arguments, which select only the series already on a given grid. The
    /// window says "sweep this span across whatever matched"; the filter says
    /// "match only the series on this grid".
    ///
    /// The window is checked, not clamped, in the three ways that would
    /// otherwise return plausible wrong numbers:
    ///
    /// * a matched series that does not cover it raises `InvalidParameterError`
    ///   naming that series, rather than being dropped from the columns;
    /// * the anchor must fall at or after each series' start and on one of its
    ///   own step boundaries — a timestamp part-way through a step is an error,
    ///   not a floor;
    /// * a monthly resolution is refused where re-anchoring would move the
    ///   dates, by the same rule that governs a sliced read.
    ///
    /// `window_start` must be spelled the way the series are (aware for a zoned
    /// series, naive for a zoneless one), and belongs to `SingleTimeSeries`
    /// alone: the two irregular types carry their timeline rather than deriving
    /// it, so there is nothing to re-anchor.
    ///
    /// ```python
    /// # a year of load and a shorter series, swept over the span they share
    /// reader = store.build_static_reader(
    ///     "PT1H",
    ///     window_start=datetime(2024, 1, 1, 7, tzinfo=timezone.utc),
    /// )
    /// reader.grid()["length"]
    /// ```
    #[pyo3(signature = (resolution=None, *, window_start=None, window_length=None, time_series_type=None, owner_id=None, owner_category=None, owner_type=None, name=None, name_glob=None, component_field=None, zoneless=None, initial_timestamp=None, length=None, features=None, features_exact=false))]
    #[allow(clippy::too_many_arguments)]
    fn build_static_reader(
        &self,
        resolution: Option<Bound<'_, PyAny>>,
        window_start: Option<PyInstant>,
        window_length: Option<usize>,
        time_series_type: Option<&Bound<'_, PyAny>>,
        owner_id: Option<i64>,
        owner_category: Option<PyOwnerCategory>,
        owner_type: Option<String>,
        name: Option<String>,
        name_glob: Option<String>,
        component_field: Option<String>,
        zoneless: Option<bool>,
        initial_timestamp: Option<PyInstant>,
        length: Option<usize>,
        features: Option<&Bound<'_, PyDict>>,
        features_exact: bool,
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
            initial_timestamp,
            length,
            None,
            features,
            features_exact,
        )?;
        let window = core_lib::ReadWindow {
            start: window_start.as_ref().map(|s| s.instant),
            zoneless: window_start.as_ref().is_some_and(|s| s.is_zoneless()),
            len: window_length,
            count: None,
        };
        let reader = self
            .store()?
            .build_static_reader_over(filter, window)
            .map_err(map_err)?;
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
    #[pyo3(signature = (time_series_type, resolution, *, owner_id=None, owner_category=None, owner_type=None, name=None, name_glob=None, component_field=None, zoneless=None, initial_timestamp=None, length=None, features=None, features_exact=false))]
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
        initial_timestamp: Option<PyInstant>,
        length: Option<usize>,
        features: Option<&Bound<'_, PyDict>>,
        features_exact: bool,
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
            initial_timestamp,
            length,
            None,
            features,
            features_exact,
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
    /// One slice is refused by both forms. A series whose resolution is a
    /// calendar period (`P1M`, `P1Y`) is stored as an anchor plus a count, and
    /// month-end arithmetic clamps: a monthly grid from Jan-31 is Jan-31,
    /// Feb-29, Mar-31, but re-anchored at its own Feb-29 it would read Feb-29,
    /// Mar-29, Apr-29. A slice that would have to describe itself that way
    /// raises `InvalidParameterError` rather than returning the stored values
    /// under dates the store does not hold. Read the series whole and slice
    /// `timestamps` yourself, or store the instants with
    /// `NonSequentialTimeSeries`.
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
        let resolution = resolution.as_ref().map(pyany_to_period).transpose()?;
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
        let resolution = resolution.as_ref().map(pyany_to_period).transpose()?;
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

    /// Write only the **array half** to `path`, leaving no catalog beside it.
    ///
    /// The mirror of `persist_catalog()`, which writes only the other half, and
    /// the write-side counterpart of `Store.open_without_catalog()`: together
    /// they are how a consumer ships an artifact as arrays plus a document of
    /// its own, carrying the catalog's rows in that document rather than in a
    /// `.sqlite` nobody reads. Which arrays land follows the backend, exactly as
    /// `persist_to()` does: an in-memory store is materialized, so only the
    /// arrays the catalog still references are written, while an on-disk store's
    /// file is copied whole — dead slots included, since HDF5 does not reclaim
    /// that space in place. Call `compact()` first when the bundle's size
    /// matters.
    ///
    /// Atomic, unlike `persist_to()` — one file, so one rename. The file still
    /// carries a fresh generation stamp, which `open_without_catalog()` copies
    /// onto the catalog it mints, so the rebuilt pair agrees.
    ///
    /// Raises `StoreExistsError` when a `<path>.sqlite` is already beside the
    /// destination: it is paired with the file this would replace, so
    /// publishing new arrays under it would leave its rows dangling.
    fn persist_arrays_to(&mut self, path: PathBuf) -> PyResult<()> {
        self.store_mut()?.persist_arrays_to(&path).map_err(map_err)
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

    /// Rewrite component `old_id` to `new_id` on both ends of every edge,
    /// returning the rows updated. Raises `DuplicateAssociationError` if the
    /// rewrite would duplicate an edge `new_id` already has.
    fn replace_parent_child_component_id(&mut self, old_id: i64, new_id: i64) -> PyResult<usize> {
        self.store_mut()?
            .replace_parent_child_component_id(old_id, new_id)
            .map_err(map_err)
    }

    // ---- Store attributes --------------------------------------------------
    //
    // Key/value provenance about the artifact as a whole. Every name carries the
    // `store_` prefix because a bare "attribute" already means a supplemental
    // attribute here, and a bare "metadata" already means a time-series row.

    /// Record ``key`` -> ``value`` as provenance about the whole artifact — who
    /// built it, from what source system, under which of your own schema
    /// versions. The store never interprets a value, in the same spirit as a
    /// series' ``application_data``; a caller wanting structure stores JSON.
    ///
    /// Setting a key that is already there replaces its value: an artifact
    /// records one creator, not a history of them.
    ///
    /// Raises `InvalidParameterError` for an empty key or one beginning with the
    /// reserved ``infrastore.`` prefix, and `ReadOnlyStoreError` on a read-only
    /// store.
    fn set_store_attribute(&mut self, key: &str, value: &str) -> PyResult<()> {
        self.store_mut()?
            .set_store_attribute(key, value)
            .map_err(map_err)
    }

    /// The value recorded for ``key``, or ``None`` if the artifact carries no
    /// such key.
    ///
    /// ``None`` rather than an exception because a consumer asking whether a key
    /// is there is asking a question. ``None`` and ``""`` are different answers:
    /// a key may legitimately hold the empty string.
    fn get_store_attribute(&self, key: &str) -> PyResult<Option<String>> {
        self.store()?.get_store_attribute(key).map_err(map_err)
    }

    /// Every store attribute as a ``dict``, sorted by key. Empty for a store
    /// carrying none.
    fn list_store_attributes(&self) -> PyResult<BTreeMap<String, String>> {
        self.store()?.list_store_attributes().map_err(map_err)
    }

    /// Remove ``key``, returning whether it was there. Removing an absent key is
    /// ``False``, not an error.
    ///
    /// A key in the reserved ``infrastore.`` namespace is refused here as well
    /// as on write, so the reservation cannot be worked around by deleting one.
    fn remove_store_attribute(&mut self, key: &str) -> PyResult<bool> {
        self.store_mut()?
            .remove_store_attribute(key)
            .map_err(map_err)
    }

    // ---- OpenAPI-row association serde -------------------------------------
    //
    // Direct JSON serde of the two association catalogs, in the wire spelling
    // SiennaSchemas defines. The Rust core (`infrastore_core::openapi`) owns
    // the mapping between catalog rows and schema rows; these four methods are
    // a thin wrapper over it.

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

/// Store methods taking one of the three filters' keyword arguments, each in a
/// `#[pymethods]` block of its own (PyO3's `multiple-pymethods`), so a filter's
/// arguments are spelled once here rather than once per method.
///
/// `list` is the time-series `ListFilter`; `supplemental_attribute` and
/// `parent_child` are the association catalogs'. The body receives the store —
/// `store()` for `&self`, `store_mut()` for `&mut self` — and the built filter.
/// The reader builders keep their own signatures: their arguments differ.
macro_rules! filter_pymethods {
    (@list $recv:tt $accessor:ident; $(#[doc = $doc:literal])* fn $name:ident($($py:ident)?) -> $ret:ty {
        |$store:ident, $filter:ident| $body:expr
    }) => {
        filter_pymethods!(@emit $recv $accessor; $(#[doc = $doc])* fn $name($($py)?) -> $ret {
            |$store, $filter| $body
        } signature = (
            *, owner_id=None, owner_category=None, owner_type=None,
                    time_series_type=None, name=None, name_glob=None, component_field=None,
                    zoneless=None, resolution=None, interval=None, initial_timestamp=None,
                    length=None, features=None, features_exact=false
        ) params = (
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
            initial_timestamp: Option<PyInstant>,
            length: Option<usize>,
            features: Option<&Bound<'_, PyDict>>,
            features_exact: bool,
        ) build = build_list_filter(
            owner_id,
            owner_category,
            owner_type,
            time_series_type.as_ref(),
            name,
            name_glob,
            component_field,
            zoneless,
            resolution,
            initial_timestamp,
            length,
            interval,
            features,
            features_exact,
        )?);
    };
    (@supplemental_attribute $recv:tt $accessor:ident; $(#[doc = $doc:literal])* fn $name:ident($($py:ident)?) -> $ret:ty {
        |$store:ident, $filter:ident| $body:expr
    }) => {
        filter_pymethods!(@emit $recv $accessor; $(#[doc = $doc])* fn $name($($py)?) -> $ret {
            |$store, $filter| $body
        } signature = (
            *, component_id=None, component_types=None, attribute_id=None, attribute_types=None
        ) params = (
            component_id: Option<i64>,
            component_types: Option<Vec<String>>,
            attribute_id: Option<i64>,
            attribute_types: Option<Vec<String>>,
        ) build = build_supplemental_attribute_filter(
            component_id,
            component_types,
            attribute_id,
            attribute_types,
        ));
    };
    (@parent_child $recv:tt $accessor:ident; $(#[doc = $doc:literal])* fn $name:ident($($py:ident)?) -> $ret:ty {
        |$store:ident, $filter:ident| $body:expr
    }) => {
        filter_pymethods!(@emit $recv $accessor; $(#[doc = $doc])* fn $name($($py)?) -> $ret {
            |$store, $filter| $body
        } signature = (
            *, parent_id=None, parent_types=None, child_id=None, child_types=None
        ) params = (
            parent_id: Option<i64>,
            parent_types: Option<Vec<String>>,
            child_id: Option<i64>,
            child_types: Option<Vec<String>>,
        ) build = build_parent_child_filter(parent_id, parent_types, child_id, child_types));
    };
    (@emit [$($m:tt)?] $accessor:ident; $(#[doc = $doc:literal])* fn $name:ident($($py:ident)?) -> $ret:ty {
        |$store:ident, $filter:ident| $body:expr
    } signature = ($($sig:tt)*) params = ($($param:ident: $pty:ty),* $(,)?) build = $build:expr) => {
        #[pymethods]
        impl PyStore {
            $(#[doc = $doc])*
            #[pyo3(signature = ($($sig)*))]
            // `'py` is unused when the invocation takes no `py`.
            #[allow(clippy::too_many_arguments, clippy::extra_unused_lifetimes)]
            fn $name<'py>(
                &$($m)? self,
                $($py: Python<'py>,)?
                $($param: $pty),*
            ) -> PyResult<$ret> {
                let $filter = $build;
                let $store = self.$accessor()?;
                $body
            }
        }
    };
    ($kind:ident, $(#[doc = $doc:literal])* fn $name:ident(&self $(, $py:ident)?) -> $ret:ty {
        |$store:ident, $filter:ident| $body:expr
    }) => {
        filter_pymethods!(@$kind [] store; $(#[doc = $doc])* fn $name($($py)?) -> $ret {
            |$store, $filter| $body
        });
    };
    ($kind:ident, $(#[doc = $doc:literal])* fn $name:ident(&mut self $(, $py:ident)?) -> $ret:ty {
        |$store:ident, $filter:ident| $body:expr
    }) => {
        filter_pymethods!(@$kind [mut] store_mut; $(#[doc = $doc])* fn $name($($py)?) -> $ret {
            |$store, $filter| $body
        });
    };
}

filter_pymethods! {
    list,
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
    fn list_metadata(&self, py) -> Vec<Bound<'py, PyDict>> {
        |store, filter| {
            store
                .list_metadata(filter)
                .map_err(map_err)?
                .iter()
                .map(|m| metadata_to_dict(py, m))
                .collect()
        }
    }
}

filter_pymethods! {
    list,
    /// Return True if at least one time series matches the filters — e.g.
    /// "does this owner have any time series (of type T)?" — without listing
    /// them. Accepts the same keyword-only filters as `list_metadata`, and
    /// answers from index probes that hydrate no rows — a `features` filter
    /// included — so it is safe to call in hot loops.
    fn has_any_time_series(&self) -> bool {
        |store, filter| store.has_any_time_series(filter).map_err(map_err)
    }
}

filter_pymethods! {
    list,
    /// Distinct series names matching the filter, sorted.
    fn list_names(&self) -> Vec<String> {
        |store, filter| store.list_names(filter).map_err(map_err)
    }
}

filter_pymethods! {
    list,
    /// Distinct owner types matching the filter, sorted.
    fn list_owner_types(&self) -> Vec<String> {
        |store, filter| store.list_owner_types(filter).map_err(map_err)
    }
}

filter_pymethods! {
    list,
    /// Remove every series matching the filter in one all-or-nothing
    /// transaction. Returns the number of associations removed.
    fn remove_by_filter(&mut self) -> usize {
        |store, filter| store.remove_by_filter(filter).map_err(map_err)
    }
}

filter_pymethods! {
    supplemental_attribute,
    /// Whether any attachment matches the filter.
    fn has_supplemental_attribute_association(&self) -> bool {
        |store, filter| store.has_supplemental_attribute_association(&filter).map_err(map_err)
    }
}

filter_pymethods! {
    supplemental_attribute,
    /// Full attachment rows matching the filter, in insertion order. Passing no
    /// filter exports the whole table.
    fn list_supplemental_attribute_associations(&self) -> Vec<PySupplementalAttributeAssociation> {
        |store, filter| {
            Ok(store
                .list_supplemental_attribute_associations(&filter)
                .map_err(map_err)?
                .into_iter()
                .map(|inner| PySupplementalAttributeAssociation { inner })
                .collect())
        }
    }
}

filter_pymethods! {
    supplemental_attribute,
    /// Distinct attribute ids matching the filter, ascending — the attributes
    /// attached to one component when `component_id` is given.
    fn list_supplemental_attribute_ids(&self) -> Vec<i64> {
        |store, filter| store.list_supplemental_attribute_ids(&filter).map_err(map_err)
    }
}

filter_pymethods! {
    supplemental_attribute,
    /// Distinct component ids matching the filter, ascending — the components
    /// carrying one attribute when `attribute_id` is given.
    fn list_components_with_attributes(&self) -> Vec<i64> {
        |store, filter| store.list_components_with_attributes(&filter).map_err(map_err)
    }
}

filter_pymethods! {
    supplemental_attribute,
    /// Remove every attachment matching the filter, returning how many were
    /// removed. Matching nothing returns 0 rather than raising: only the caller
    /// knows whether a hit was expected.
    fn remove_supplemental_attribute_associations(&mut self) -> usize {
        |store, filter| {
            store
                .remove_supplemental_attribute_associations(&filter)
                .map_err(map_err)
        }
    }
}

filter_pymethods! {
    supplemental_attribute,
    /// Number of attachments matching the filter.
    fn count_supplemental_attribute_associations(&self) -> i64 {
        |store, filter| {
            store
                .count_supplemental_attribute_associations(&filter)
                .map_err(map_err)
        }
    }
}

filter_pymethods! {
    supplemental_attribute,
    /// Number of *distinct* attributes among the attachments matching the
    /// filter.
    fn count_supplemental_attributes(&self) -> i64 {
        |store, filter| store.count_supplemental_attributes(&filter).map_err(map_err)
    }
}

filter_pymethods! {
    supplemental_attribute,
    /// Number of *distinct* components among the attachments matching the
    /// filter.
    fn count_components_with_attributes(&self) -> i64 {
        |store, filter| store.count_components_with_attributes(&filter).map_err(map_err)
    }
}

filter_pymethods! {
    parent_child,
    /// Whether any edge matches the filter.
    fn has_parent_child_association(&self) -> bool {
        |store, filter| store.has_parent_child_association(&filter).map_err(map_err)
    }
}

filter_pymethods! {
    parent_child,
    /// Full edge rows matching the filter, in insertion order. Passing no filter
    /// exports the whole table.
    fn list_parent_child_associations(&self) -> Vec<PyParentChildAssociation> {
        |store, filter| {
            Ok(store
                .list_parent_child_associations(&filter)
                .map_err(map_err)?
                .into_iter()
                .map(|inner| PyParentChildAssociation { inner })
                .collect())
        }
    }
}

filter_pymethods! {
    parent_child,
    /// Distinct child ids matching the filter, ascending — the children of one
    /// component when `parent_id` is given.
    fn list_children(&self) -> Vec<i64> {
        |store, filter| store.list_children(&filter).map_err(map_err)
    }
}

filter_pymethods! {
    parent_child,
    /// Distinct parent ids matching the filter, ascending — the parents of one
    /// component when `child_id` is given.
    fn list_parents(&self) -> Vec<i64> {
        |store, filter| store.list_parents(&filter).map_err(map_err)
    }
}

filter_pymethods! {
    parent_child,
    /// Remove every edge matching the filter, returning how many were removed.
    /// Matching nothing returns 0 rather than raising.
    fn remove_parent_child_associations(&mut self) -> usize {
        |store, filter| store.remove_parent_child_associations(&filter).map_err(map_err)
    }
}

filter_pymethods! {
    parent_child,
    /// Number of edges matching the filter.
    fn count_parent_child_associations(&self) -> i64 {
        |store, filter| store.count_parent_child_associations(&filter).map_err(map_err)
    }
}

filter_pymethods! {
    list,
    /// Export `time_series_associations` matching the filter (the same filter
    /// keywords as `list_metadata`) as a sorted OpenAPI-row JSON array.
    /// Each row's `uri` and `data_hash` are the hex-encoded content hash the
    /// store already has for that row — never a caller-supplied locator.
    /// With no filter this exports the whole catalog, minus `PersistentTimeSeries`
    /// rows: the type is an infrastore-local extension the wire contract has no
    /// schema for, so it is omitted, and a filter naming it raises
    /// `InvalidParameterError`.
    fn export_time_series_associations_openapi(&self) -> String {
        |store, filter| {
            store
                .export_time_series_associations_openapi(&filter)
                .map_err(map_err)
        }
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
const TIME_SERIES_TYPE_NAMES: [&str; 7] = [
    "SingleTimeSeries",
    "NonSequentialTimeSeries",
    "PersistentTimeSeries",
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
    v.map(|v| pyany_to_requested_type(v, param)).transpose()
}

/// Decode a 64-character lowercase-or-uppercase hex string into a 32-byte hash.
/// Every key `add_time_series_bulk` reads out of one item dict.
const BULK_ITEM_KEYS: [&str; 5] = [
    "owner_id",
    "owner_type",
    "owner_category",
    "time_series",
    "features",
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

/// [`core_lib::hash_from_hex`] with this layer's error.
///
/// The shared decoder compares over bytes rather than `&str` slices, which is
/// what keeps a 64-*byte* string of multi-byte characters from slicing through
/// a character boundary. That panicked, and PyO3 surfaces a panic as
/// `PanicException` — inheriting from `BaseException`, so it escapes both
/// `except Exception` and this module's own exception hierarchy, an uncatchable
/// error from an ordinary bad argument.
fn hash_from_hex(s: &str) -> PyResult<[u8; 32]> {
    core_lib::hash_from_hex(s).ok_or_else(|| {
        InvalidParameterError::new_err("data_hash must be a 64-character hex string")
    })
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
    initial_timestamp: Option<PyInstant>,
    length: Option<usize>,
    interval: Option<Bound<'_, PyAny>>,
    features: Option<&Bound<'_, PyDict>>,
    features_exact: bool,
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
    if let Some(t) = initial_timestamp {
        // Matched on the instant the row stores. No spelling check: a filter
        // selects rather than reads, so a bound the rows cannot answer is an
        // empty result, not an error -- pair it with `zoneless=` to pick the
        // coherence group.
        filter = filter.initial_timestamp(t.instant);
    }
    if let Some(n) = length {
        filter = filter.length(n);
    }
    if let Some(i) = interval {
        filter = filter.interval(pyany_to_period(&i)?);
    }
    if features_exact {
        // The row's whole feature set, by content hash -- so `features=None`
        // here selects the rows that carry no features at all.
        filter = filter.exact_features(features_from_dict(features)?);
    } else if let Some(f) = features {
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
    decoded_or_none(py, &array, element_type, leading_dims)
}

/// Decode `array` for Python, mapping the core's `Raw` — "the elements already
/// are the values" — onto `None`.
///
/// Shared by `decode_element_values` and by every series' `decoded_values`, so
/// the two can never come to disagree about what a scalar series decodes to.
fn decoded_or_none<'py>(
    py: Python<'py>,
    array: &core_lib::TypedArray,
    element_type: core_lib::ElementType,
    leading_dims: usize,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let decoded = core_lib::decode(array, element_type, leading_dims).map_err(map_err)?;
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
    let rows: Vec<Bound<'_, PyAny>> = values.try_iter()?.collect::<PyResult<_>>()?;
    let decoded = py_to_decoded(&rows, element_type)?;
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

/// What a scan of a `from_values` payload's rows settles about its element type.
enum Inferred {
    /// A row discriminated, so this is what the values are.
    Known(core_lib::ElementType),
    /// There are rows, but every one of them is empty, which reads equally as a
    /// `piecewise_linear` curve with no points or a tuple with no fields. Only a
    /// declaration can break the tie.
    Ambiguous,
    /// There are no rows at all, so the values imply nothing.
    Empty,
}

/// The element type a `from_values` payload implies.
///
/// Rust and Julia never need this: a `DecodedValues` and a
/// `Vector{PiecewiseLinear}` each carry their variant in the type system. A
/// Python list of dicts carries nothing, so the shape of a row is the only tag
/// there is. The five shapes are disjoint, which is what makes reading one a
/// decision rather than a guess:
///
/// - `{"quadratic", "proportional", "constant"}` -> `quadratic_function`
/// - `{"proportional", "constant"}`              -> `linear_function`
/// - `{"x": [...], "y": [...]}`                  -> `piecewise_step`
/// - `[{"x": _, "y": _}, ...]`                   -> `piecewise_linear`
/// - `[float, ...]`                              -> `tuple(N,f64)`
///
/// The arity of a tuple is deliberately *not* settled here — it is read off the
/// decoded rows by [`core_lib::element_type_of`], the same call the core's own
/// `from_values` uses, so the two can never disagree about it.
///
/// A `piecewise_linear` row is an empty list when a timestep's curve has no
/// points, and so is a zero-arity tuple row, so the scan walks past empty rows
/// looking for one that discriminates. A payload of nothing but empty rows is
/// genuinely ambiguous, and says so rather than failing here: such a series is
/// storable, and a declared `element_type=` settles which one it is. Deciding
/// that is [`from_values_payload`]'s job, because only it knows the declaration.
fn infer_element_type(rows: &[Bound<'_, PyAny>]) -> PyResult<Inferred> {
    use core_lib::{Dtype, ElementType};

    // `get_item` on a mapping without the key raises `KeyError`, and on a
    // sequence raises `TypeError` for a string index -- either way, "no".
    let has = |row: &Bound<'_, PyAny>, key: &str| row.get_item(key).is_ok();

    let mut saw_empty_row = false;
    for (index, row) in rows.iter().enumerate() {
        if has(row, "quadratic") {
            return Ok(Inferred::Known(ElementType::QuadraticFunction));
        }
        if has(row, "proportional") {
            return Ok(Inferred::Known(ElementType::LinearFunction));
        }
        if has(row, "x") && has(row, "y") {
            return Ok(Inferred::Known(ElementType::PiecewiseStep));
        }
        let Ok(mut points) = row.try_iter() else {
            return Err(InvalidParameterError::new_err(format!(
                "from_values takes per-timestep composite values (a cost curve, a \
                 linear function, a tuple), and row {index} is {}. A series of \
                 plain numbers is `data=` on the constructor, which needs no \
                 encoding.",
                row.get_type().name()?
            )));
        };
        match points.next() {
            // An empty row cannot discriminate `piecewise_linear` from a
            // zero-arity tuple; a later row may.
            None => saw_empty_row = true,
            Some(point) => {
                let point = point?;
                return Ok(Inferred::Known(if has(&point, "x") && has(&point, "y") {
                    ElementType::PiecewiseLinear
                } else {
                    // Arity comes from the decoded rows, not from this one.
                    ElementType::Tuple {
                        arity: 0,
                        dtype: Dtype::F64,
                    }
                }));
            }
        }
    }
    Ok(if saw_empty_row {
        Inferred::Ambiguous
    } else {
        Inferred::Empty
    })
}

/// Read a `from_values` payload into the core's `DecodedValues`, cross-checking
/// a declared `element_type` against what the values actually are.
///
/// The declaration is an assertion, never an override: the encoded array and
/// the element type recorded beside it both come from the values, which is the
/// whole reason these constructors exist. A declaration that disagrees is a
/// mistake worth naming rather than a preference to honor.
fn from_values_payload(
    values: &Bound<'_, PyAny>,
    declared: Option<&str>,
) -> PyResult<core_lib::DecodedValues> {
    let declared = declared.map(parse_element_type).transpose()?;
    // One pass over `values`. Both halves below need the rows, and a generator
    // is spent by whoever iterates it first -- iterating twice would drop the
    // rows inference consumed, silently, since the series takes its length from
    // what survives.
    let rows: Vec<Bound<'_, PyAny>> = values.try_iter()?.collect::<PyResult<_>>()?;
    if let Some(scalar @ core_lib::ElementType::Scalar(_)) = declared {
        return Err(InvalidParameterError::new_err(format!(
            "from_values encodes composite per-timestep values (a cost curve, a \
             linear function, a tuple), and element_type \"{scalar}\" is a scalar. \
             A series of plain numbers is `data=` on the constructor, which needs \
             no encoding."
        )));
    }
    let read_as = match (declared, infer_element_type(&rows)?) {
        // Non-empty values always win: they are the thing being encoded.
        (_, Inferred::Known(inferred)) => inferred,
        // Rows too empty to speak for themselves, settled by the declaration:
        // an all-empty `piecewise_linear` series is storable, so this is the one
        // place a declaration is load-bearing rather than a cross-check. The rows
        // are still sequences, though, so a declaration whose rows are mappings
        // disagrees with them as surely as a populated row would.
        (
            Some(
                declared @ (core_lib::ElementType::PiecewiseLinear
                | core_lib::ElementType::Tuple { .. }),
            ),
            Inferred::Ambiguous,
        ) => declared,
        (Some(declared), Inferred::Ambiguous) => {
            return Err(InvalidParameterError::new_err(format!(
                "element_type \"{declared}\" disagrees with the values, whose rows are \
                 all empty sequences -- which read as `piecewise_linear` curves with \
                 no points, not as \"{declared}\"."
            )));
        }
        (None, Inferred::Ambiguous) => {
            return Err(InvalidParameterError::new_err(
                "cannot tell what these values are: every row is empty, which reads \
                 equally as a `piecewise_linear` curve with no points or a tuple with \
                 no fields. Declare `element_type=` to say which.",
            ));
        }
        // Nothing to read at all, so the declaration is all there is to go on.
        (Some(core_lib::ElementType::Tuple { arity, dtype }), Inferred::Empty) => {
            return Err(InvalidParameterError::new_err(format!(
                "an empty tuple series cannot be built from values, because a \
                 tuple's arity lives in its rows and there are none. Encode it \
                 with encode_element_values([], \"tuple({arity},{})\") and \
                 pass the array to the constructor with the same element_type=.",
                dtype.as_str()
            )));
        }
        (Some(declared), Inferred::Empty) => declared,
        (None, Inferred::Empty) => {
            return Err(InvalidParameterError::new_err(
                "cannot infer an element_type from an empty `values`: declare \
                 element_type= to say what the series holds.",
            ));
        }
    };
    let decoded = py_to_decoded(&rows, read_as)?;
    if let Some(declared) = declared {
        let implied = core_lib::element_type_of(&decoded).unwrap_or(read_as);
        if declared != implied {
            return Err(InvalidParameterError::new_err(format!(
                "element_type \"{declared}\" disagrees with the values, which are \
                 \"{implied}\". Drop the declaration -- from_values derives it."
            )));
        }
    }
    Ok(decoded)
}

/// Read a Python payload back into the core's `DecodedValues`, keyed on the
/// element type it is being encoded as. The shapes are the ones
/// `decode_element_values` produces, so a round trip through Python needs no
/// reshaping in between.
fn py_to_decoded(
    rows: &[Bound<'_, PyAny>],
    element_type: core_lib::ElementType,
) -> PyResult<core_lib::DecodedValues> {
    use core_lib::{DecodedValues, ElementType, LinearFunction, QuadraticFunction};

    let field = |row: &Bound<'_, PyAny>, name: &str| -> PyResult<f64> {
        row.get_item(name)?.extract::<f64>()
    };
    Ok(match element_type {
        ElementType::Scalar(_) => {
            return Err(InvalidParameterError::new_err(
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
fn infrastore(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Auto-initialize from RUST_LOG if set. try_init() is a no-op when a
    // subscriber is already registered, so this is safe to call unconditionally.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    m.add_class::<PyStore>()?;
    m.add_class::<PyTransaction>()?;
    m.add_class::<PySingleTimeSeries>()?;
    m.add_class::<PyNonSequentialTimeSeries>()?;
    m.add_class::<PyPersistentTimeSeries>()?;
    m.add_class::<PyDeterministic>()?;
    m.add_class::<PyProbabilistic>()?;
    m.add_class::<PyScenarios>()?;
    m.add_class::<PyTimeSeriesType>()?;
    m.add_class::<PyOwnerCategory>()?;
    m.add_class::<PySupplementalAttributeAssociation>()?;
    m.add_class::<PyParentChildAssociation>()?;
    m.add_class::<PyStaticReader>()?;
    m.add_class::<PyForecastReader>()?;

    add_exceptions(m)?;

    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_function(wrap_pyfunction!(init_tracing, m)?)?;
    m.add_function(wrap_pyfunction!(decode_element_values, m)?)?;
    m.add_function(wrap_pyfunction!(encode_element_values, m)?)?;
    Ok(())
}
