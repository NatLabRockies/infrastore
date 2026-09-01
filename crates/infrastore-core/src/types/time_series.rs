use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::array::TypedArray;
use super::element_type::ElementType;
use super::metadata::UnitSystem;
use super::period::Period;
use super::time_reference::TimeReference;
use crate::codec::{self, DecodedValues};

/// Discriminator for the time series types this store models.
///
/// Static series carry runtime variants in [`TimeSeriesData`]. Forecast types
/// use the forecast-specific store API.
///
/// # Encodings
///
/// Two, deliberately: [`Self::as_str`] is the *display and serde* form (JSON,
/// proto, CLI, binding names), and [`Self::code`] is the *storage* form written
/// to the SQLite catalog and passed across the C ABI.
///
/// The codes are part of the on-disk contract — changing one requires a
/// [`crate::DATA_FORMAT_VERSION`] bump. In particular `Deterministic` and
/// `DeterministicSingleTimeSeries` **must stay adjacent**: a request for
/// `Deterministic` matches both, and [`Self::code_span`] turns that into a
/// single index range scan rather than a two-value `IN`. The adjacency is
/// asserted by `deterministic_codes_are_adjacent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimeSeriesType {
    SingleTimeSeries,
    NonSequentialTimeSeries,
    // Keep these two adjacent — see the type docs.
    Deterministic,
    DeterministicSingleTimeSeries,
    Probabilistic,
    Scenarios,
    /// A sparse step function: breakpoints plus one value each, holding the
    /// last value forward. **Appended, not inserted** — the codes are an
    /// on-disk contract, and the `Deterministic`/`DeterministicSingleTimeSeries`
    /// adjacency that [`Self::code_span`] relies on must not be disturbed. See
    /// [`PersistentTimeSeries`].
    PersistentTimeSeries,
}

impl TimeSeriesType {
    pub fn as_str(&self) -> &'static str {
        match self {
            TimeSeriesType::SingleTimeSeries => "SingleTimeSeries",
            TimeSeriesType::NonSequentialTimeSeries => "NonSequentialTimeSeries",
            TimeSeriesType::Deterministic => "Deterministic",
            TimeSeriesType::DeterministicSingleTimeSeries => "DeterministicSingleTimeSeries",
            TimeSeriesType::Probabilistic => "Probabilistic",
            TimeSeriesType::Scenarios => "Scenarios",
            TimeSeriesType::PersistentTimeSeries => "PersistentTimeSeries",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "SingleTimeSeries" => TimeSeriesType::SingleTimeSeries,
            "NonSequentialTimeSeries" => TimeSeriesType::NonSequentialTimeSeries,
            "Deterministic" => TimeSeriesType::Deterministic,
            "DeterministicSingleTimeSeries" => TimeSeriesType::DeterministicSingleTimeSeries,
            "Probabilistic" => TimeSeriesType::Probabilistic,
            "Scenarios" => TimeSeriesType::Scenarios,
            "PersistentTimeSeries" => TimeSeriesType::PersistentTimeSeries,
            _ => return None,
        })
    }

    /// The storage code written to the SQLite catalog and passed across the C
    /// ABI. Part of the on-disk contract — see the type docs.
    pub fn code(self) -> i64 {
        match self {
            TimeSeriesType::SingleTimeSeries => 0,
            TimeSeriesType::NonSequentialTimeSeries => 1,
            TimeSeriesType::Deterministic => 2,
            TimeSeriesType::DeterministicSingleTimeSeries => 3,
            TimeSeriesType::Probabilistic => 4,
            TimeSeriesType::Scenarios => 5,
            TimeSeriesType::PersistentTimeSeries => 6,
        }
    }

    /// Inverse of [`Self::code`]. `None` for an unknown code, which in the
    /// catalog means a store written by an incompatible version.
    pub fn from_code(code: i64) -> Option<Self> {
        Some(match code {
            0 => TimeSeriesType::SingleTimeSeries,
            1 => TimeSeriesType::NonSequentialTimeSeries,
            2 => TimeSeriesType::Deterministic,
            3 => TimeSeriesType::DeterministicSingleTimeSeries,
            4 => TimeSeriesType::Probabilistic,
            5 => TimeSeriesType::Scenarios,
            6 => TimeSeriesType::PersistentTimeSeries,
            _ => return None,
        })
    }

    /// How many leading array dims come *before* the per-step element shape.
    ///
    /// Static series are `[length, *E]`; a `Deterministic` stacks windows as
    /// `[H, count, *E]`; `Probabilistic` and `Scenarios` add a percentile /
    /// scenario axis in front, giving `[P, H, count, *E]`. Anything that has to
    /// find the per-step element dims in a raw shape — element-type validation,
    /// the codecs — asks here rather than re-deriving the layout.
    pub fn leading_dims(self) -> usize {
        match self {
            TimeSeriesType::SingleTimeSeries
            | TimeSeriesType::NonSequentialTimeSeries
            | TimeSeriesType::PersistentTimeSeries => 1,
            TimeSeriesType::Deterministic | TimeSeriesType::DeterministicSingleTimeSeries => 2,
            TimeSeriesType::Probabilistic | TimeSeriesType::Scenarios => 3,
        }
    }

    /// The inclusive `(low, high)` code range a *request* for `self` matches.
    ///
    /// Every type spans only itself, with one deliberate exception: requesting
    /// `Deterministic` also matches a stored `DeterministicSingleTimeSeries`. A
    /// DST is a synthetic view over a `SingleTimeSeries` produced by
    /// [`crate::Store::transform_single_time_series`], it reads back as a
    /// `Deterministic`, and callers should not have to know which of the two a
    /// store happens to hold. This mirrors InfrastructureSystems.jl, where a
    /// `Deterministic` request lowers to both concrete types.
    ///
    /// Requesting `DeterministicSingleTimeSeries` narrows to DST alone, which
    /// is how a caller inspecting the catalog asks "which of these are
    /// synthetic?".
    ///
    /// Because the two codes are adjacent this is a contiguous range, so the
    /// SQL predicate is `BETWEEN` rather than `IN` — one index seek instead of
    /// two. [`Self::accepts`] is the same rule in memory.
    pub fn code_span(self) -> (i64, i64) {
        match self {
            TimeSeriesType::Deterministic => (
                TimeSeriesType::Deterministic.code(),
                TimeSeriesType::DeterministicSingleTimeSeries.code(),
            ),
            other => (other.code(), other.code()),
        }
    }

    /// Does a stored series of type `stored` satisfy a *request* for `self`?
    ///
    /// Derived from [`Self::code_span`] so the in-memory rule and the SQL
    /// predicate cannot drift apart.
    pub fn accepts(self, stored: TimeSeriesType) -> bool {
        let (lo, hi) = self.code_span();
        (lo..=hi).contains(&stored.code())
    }

    /// Is this a forecast (windowed) type rather than a static series?
    pub fn is_forecast(self) -> bool {
        match self {
            TimeSeriesType::SingleTimeSeries
            | TimeSeriesType::NonSequentialTimeSeries
            | TimeSeriesType::PersistentTimeSeries => false,
            TimeSeriesType::Deterministic
            | TimeSeriesType::DeterministicSingleTimeSeries
            | TimeSeriesType::Probabilistic
            | TimeSeriesType::Scenarios => true,
        }
    }

    /// The storage codes of the static types, for a summary query that wants
    /// "all static rows".
    ///
    /// A *list*, not a range. The static types were codes 0-1 and the forecast
    /// types 2-5, two contiguous blocks a `BETWEEN` could select — until
    /// `PersistentTimeSeries` was appended as 6 rather than inserted, because
    /// the codes are an on-disk contract and renumbering is not available. The
    /// static group is therefore non-contiguous and its consumers render
    /// `WHERE time_series_type IN (…)`. `idx_ts_type` serves that as happily as
    /// it served the range.
    ///
    /// `code_groups_partition_cleanly` asserts that this and
    /// [`Self::forecast_codes`] are disjoint and together cover every variant,
    /// which is the property the old contiguity assertion was really standing
    /// in for.
    pub fn static_codes() -> &'static [i64] {
        // Written out rather than derived from `is_forecast()` at call time:
        // these are on-disk codes, so seeing the literals here is the point.
        &[0, 1, 6]
    }

    /// The storage codes of the forecast types. See [`Self::static_codes`].
    pub fn forecast_codes() -> &'static [i64] {
        &[2, 3, 4, 5]
    }
}

impl FromStr for TimeSeriesType {
    type Err = ();
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s).ok_or(())
    }
}

/// A time series array at regular intervals.
///
/// `data` is a [`TypedArray`]: its first dimension is time (`length`) and any
/// trailing dimensions are the per-step element shape (e.g. the 3 coefficients
/// of a quadratic cost curve). The element dtype is part of the array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SingleTimeSeries {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Period,
    pub length: usize,
    pub data: TypedArray,
    pub name: String,
    /// What the stored elements mean and how one timestep is laid out.
    ///
    /// Always concrete: a constructor resolves it to `Scalar(data.dtype)`, which
    /// is what an ordinary numeric series is, and `with_element_type` replaces
    /// it. There is deliberately no "undeclared" spelling — it would be a second
    /// way to say `Scalar(dtype)`, and a series written that way would not
    /// compare equal to the same series read back.
    ///
    /// Assigning a new `data` array without updating this is a mismatch the
    /// store rejects on write; build the series again instead.
    pub element_type: ElementType,
    /// User-declared units label for the values (e.g. `"MW"`), or `None`.
    ///
    /// Set by whoever creates the series and returned unchanged on read. The
    /// store never interprets or validates it, and it is not part of a series'
    /// identity: it cannot be filtered on, and two series differing only in
    /// their label are a duplicate.
    pub units: Option<String>,
    /// What kind of physical quantity the values measure (e.g. `"ActivePower"`),
    /// or `None`. Free-form; the recommended vocabulary is a QUDT `QuantityKind`
    /// local name. See [`crate::TimeSeriesMetadata::quantity_kind`].
    pub quantity_kind: Option<String>,
    /// Which basis the values are expressed in, or `None` for unspecified.
    /// See [`UnitSystem`].
    pub unit_system: Option<UnitSystem>,
    /// How this series' timestamps were spelled, or `None` for unspecified.
    /// See [`TimeReference`] and [`crate::TimeSeriesMetadata::time_reference`].
    pub time_reference: Option<TimeReference>,
    /// The field on the owning component whose value varies over time here
    /// (e.g. `"max_active_power"`), or `None`.
    /// See [`crate::TimeSeriesMetadata::component_field`].
    pub component_field: Option<String>,
    /// Opaque, package-owned payload (typically JSON) stored verbatim for an
    /// application to reconstruct its domain objects; the store never interprets
    /// it. End users are not expected to set it.
    pub application_data: Option<String>,
}

impl SingleTimeSeries {
    pub fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: impl Into<Period>,
        data: TypedArray,
        name: impl Into<String>,
    ) -> Self {
        let length = data.length();
        let element_type = ElementType::Scalar(data.dtype);
        Self {
            initial_timestamp,
            resolution: resolution.into(),
            length,
            data,
            name: name.into(),
            element_type,
            units: None,
            quantity_kind: None,
            unit_system: None,
            time_reference: None,
            component_field: None,
            application_data: None,
        }
    }
}

impl SingleTimeSeries {
    /// Construct from per-timestep logical values, encoding them into the flat
    /// array the store holds and declaring the element type they imply.
    ///
    /// The pairing is the point. An `element_type` and the array it describes
    /// are two independent things a caller can get out of step;
    /// [`Store::add`](crate::Store::add) rejects the mismatch, but only after
    /// the fact. Deriving both from one set of values means there is no
    /// mismatch to reject.
    ///
    /// Returns `Err(String)` if the values cannot be encoded: a
    /// [`DecodedValues::Raw`], which carries no values of its own (build the
    /// [`TypedArray`] and call [`Self::new`] instead), tuple rows of differing
    /// arity, or a step function whose `x` and `y` lengths disagree.
    ///
    /// A tuple series with *no* rows is the one storable series these
    /// constructors cannot name, because a tuple's arity lives in its rows.
    /// Encode that one with [`encode_as`](crate::encode_as), which takes the
    /// arity from a declared element type, and pair the two through
    /// [`Self::new`] and [`Self::with_element_type`].
    ///
    /// One entry per timestep, so `length` is `values.len()`.
    ///
    /// ```
    /// # use infrastore_core::{DecodedValues, XyPoint, SingleTimeSeries, Period};
    /// # use chrono::{TimeZone, Utc, Duration};
    /// let curves = DecodedValues::PiecewiseLinear(vec![
    ///     vec![XyPoint { x: 0.0, y: 1.0 }, XyPoint { x: 1.0, y: 3.0 }],
    ///     vec![XyPoint { x: 0.0, y: 2.0 }],
    /// ]);
    /// let series = SingleTimeSeries::from_values(
    ///     Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
    ///     Period::Fixed(Duration::hours(1)),
    ///     &curves,
    ///     "variable_cost",
    /// )?;
    /// assert_eq!(series.element_type.to_string(), "piecewise_linear");
    /// assert_eq!(series.length, 2);
    /// # Ok::<(), String>(())
    /// ```
    pub fn from_values(
        initial_timestamp: DateTime<Utc>,
        resolution: impl Into<Period>,
        values: &DecodedValues,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        let (data, element_type) = encode_with_type(values, &[values.len()])?;
        Ok(Self::new(initial_timestamp, resolution, data, name).with_element_type(element_type))
    }

    /// Declare the logical element type of the array. Validated on commit
    /// against the array's dtype and per-step shape.
    pub fn with_element_type(mut self, element_type: ElementType) -> Self {
        self.element_type = element_type;
        self
    }

    /// Set the user-declared units label.
    pub fn with_units(mut self, units: impl Into<String>) -> Self {
        self.units = Some(units.into());
        self
    }

    /// Set the quantity kind the values measure (e.g. `"ActivePower"`).
    pub fn with_quantity_kind(mut self, quantity_kind: impl Into<String>) -> Self {
        self.quantity_kind = Some(quantity_kind.into());
        self
    }

    /// Declare which unit basis the values are expressed in.
    pub fn with_unit_system(mut self, unit_system: UnitSystem) -> Self {
        self.unit_system = Some(unit_system);
        self
    }

    /// Declare how this series' timestamps were spelled. Validated on commit
    /// (a zone name's *shape* only — see [`TimeReference::validate`]).
    pub fn with_time_reference(mut self, time_reference: TimeReference) -> Self {
        self.time_reference = Some(time_reference);
        self
    }

    /// Name the component field these values vary over time (e.g.
    /// `"max_active_power"`).
    pub fn with_component_field(mut self, component_field: impl Into<String>) -> Self {
        self.component_field = Some(component_field.into());
        self
    }

    /// Set the opaque application payload carried through to the metadata row.
    pub fn with_application_data(mut self, application_data: impl Into<String>) -> Self {
        self.application_data = Some(application_data.into());
        self
    }
}

/// A time series array at explicit, irregular timestamps.
///
/// Timestamps must be strictly increasing and the timestamp count must equal
/// the first dimension of `data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NonSequentialTimeSeries {
    pub timestamps: Vec<DateTime<Utc>>,
    pub length: usize,
    pub data: TypedArray,
    pub name: String,
    /// What the stored elements mean and how one timestep is laid out.
    ///
    /// Always concrete: a constructor resolves it to `Scalar(data.dtype)`, which
    /// is what an ordinary numeric series is, and `with_element_type` replaces
    /// it. There is deliberately no "undeclared" spelling — it would be a second
    /// way to say `Scalar(dtype)`, and a series written that way would not
    /// compare equal to the same series read back.
    ///
    /// Assigning a new `data` array without updating this is a mismatch the
    /// store rejects on write; build the series again instead.
    pub element_type: ElementType,
    /// User-declared units label for the values (e.g. `"MW"`), or `None`.
    ///
    /// Set by whoever creates the series and returned unchanged on read. The
    /// store never interprets or validates it, and it is not part of a series'
    /// identity: it cannot be filtered on, and two series differing only in
    /// their label are a duplicate.
    pub units: Option<String>,
    /// What kind of physical quantity the values measure (e.g. `"ActivePower"`),
    /// or `None`. Free-form; the recommended vocabulary is a QUDT `QuantityKind`
    /// local name. See [`crate::TimeSeriesMetadata::quantity_kind`].
    pub quantity_kind: Option<String>,
    /// Which basis the values are expressed in, or `None` for unspecified.
    /// See [`UnitSystem`].
    pub unit_system: Option<UnitSystem>,
    /// How this series' timestamps were spelled, or `None` for unspecified.
    /// See [`TimeReference`] and [`crate::TimeSeriesMetadata::time_reference`].
    pub time_reference: Option<TimeReference>,
    /// The field on the owning component whose value varies over time here
    /// (e.g. `"max_active_power"`), or `None`.
    /// See [`crate::TimeSeriesMetadata::component_field`].
    pub component_field: Option<String>,
    /// Opaque, package-owned payload (typically JSON) stored verbatim for an
    /// application to reconstruct its domain objects; the store never interprets
    /// it. End users are not expected to set it.
    pub application_data: Option<String>,
}

impl NonSequentialTimeSeries {
    pub fn new(
        timestamps: Vec<DateTime<Utc>>,
        data: TypedArray,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        let length = data.length();
        if timestamps.len() != length {
            return Err(format!(
                "timestamp count {} does not match data length {length}",
                timestamps.len()
            ));
        }
        if timestamps.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("timestamps must be strictly increasing".to_string());
        }
        let element_type = ElementType::Scalar(data.dtype);
        Ok(Self {
            timestamps,
            length,
            data,
            name: name.into(),
            element_type,
            units: None,
            quantity_kind: None,
            unit_system: None,
            time_reference: None,
            component_field: None,
            application_data: None,
        })
    }
}

impl NonSequentialTimeSeries {
    /// Construct from per-timestep logical values, encoding them into the flat
    /// array the store holds and declaring the element type they imply.
    ///
    /// The pairing is the point. An `element_type` and the array it describes
    /// are two independent things a caller can get out of step;
    /// [`Store::add`](crate::Store::add) rejects the mismatch, but only after
    /// the fact. Deriving both from one set of values means there is no
    /// mismatch to reject.
    ///
    /// Returns `Err(String)` if the values cannot be encoded: a
    /// [`DecodedValues::Raw`], which carries no values of its own (build the
    /// [`TypedArray`] and call [`Self::new`] instead), tuple rows of differing
    /// arity, or a step function whose `x` and `y` lengths disagree.
    ///
    /// A tuple series with *no* rows is the one storable series these
    /// constructors cannot name, because a tuple's arity lives in its rows.
    /// Encode that one with [`encode_as`](crate::encode_as), which takes the
    /// arity from a declared element type, and pair the two through
    /// [`Self::new`] and [`Self::with_element_type`].
    /// It also returns `Err` when the timestamp count does not match the number
    /// of timesteps, or the timestamps are not strictly increasing — the same
    /// checks [`Self::new`] makes.
    pub fn from_values(
        timestamps: Vec<DateTime<Utc>>,
        values: &DecodedValues,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        let (data, element_type) = encode_with_type(values, &[values.len()])?;
        Ok(Self::new(timestamps, data, name)?.with_element_type(element_type))
    }

    /// Declare the logical element type of the array. Validated on commit
    /// against the array's dtype and per-step shape.
    pub fn with_element_type(mut self, element_type: ElementType) -> Self {
        self.element_type = element_type;
        self
    }

    /// Set the user-declared units label.
    pub fn with_units(mut self, units: impl Into<String>) -> Self {
        self.units = Some(units.into());
        self
    }

    /// Set the quantity kind the values measure (e.g. `"ActivePower"`).
    pub fn with_quantity_kind(mut self, quantity_kind: impl Into<String>) -> Self {
        self.quantity_kind = Some(quantity_kind.into());
        self
    }

    /// Declare which unit basis the values are expressed in.
    pub fn with_unit_system(mut self, unit_system: UnitSystem) -> Self {
        self.unit_system = Some(unit_system);
        self
    }

    /// Declare how this series' timestamps were spelled. Validated on commit
    /// (a zone name's *shape* only — see [`TimeReference::validate`]).
    pub fn with_time_reference(mut self, time_reference: TimeReference) -> Self {
        self.time_reference = Some(time_reference);
        self
    }

    /// Name the component field these values vary over time (e.g.
    /// `"max_active_power"`).
    pub fn with_component_field(mut self, component_field: impl Into<String>) -> Self {
        self.component_field = Some(component_field.into());
        self
    }

    /// Set the opaque application payload carried through to the metadata row.
    pub fn with_application_data(mut self, application_data: impl Into<String>) -> Self {
        self.application_data = Some(application_data.into());
        self
    }
}

/// A sparse step function: breakpoints plus one value each, holding the last
/// value forward.
///
/// Structurally identical to [`NonSequentialTimeSeries`] — a strictly
/// increasing `Vec<DateTime<Utc>>` plus a [`TypedArray`] of the same length —
/// and stored identically (the two pool into the same `nsts_…` dataset when
/// they share a breakpoint vector, dtype, element shape, and length). They are
/// separate *types* because they answer the same question differently:
///
/// |                                   | `NonSequentialTimeSeries` | `PersistentTimeSeries` |
/// |-----------------------------------|---------------------------|------------------------|
/// | value **at** a stored instant     | that instant's value      | that instant's value   |
/// | value **between** stored instants | a hard error              | the previous value     |
/// | value **after** the last instant  | a hard error              | the last value         |
/// | value **before** the first instant| a hard error              | a hard error           |
///
/// Put formally, the values define a **right-continuous step function**,
/// constant on `[b_k, b_{k+1})`, extending to `+∞` past the last breakpoint,
/// and **undefined before the first**. That last clause is deliberate and is
/// reported as an error rather than clamped: a value before the first
/// breakpoint was never declared, and inventing one would be a guess. Look one
/// up with [`Self::index_in_force_at`].
///
/// The motivating data is a monthly fuel or gas price curve: a dozen
/// breakpoints spanning a year, read at simulation timestamps that almost never
/// coincide with one. Reading that as a `NonSequentialTimeSeries` would error
/// at nearly every step, which is exactly the guarantee that type is *for* —
/// an irregular timeline has no value between its timestamps — so making it
/// conditional was not an option.
///
/// Policy about how a step function collapses for a downstream solver (whether
/// to expand it to a full series, whether to evaluate it once at a midpoint)
/// belongs to the application and travels in
/// [`Self::application_data`](Self#structfield.application_data). The store
/// records breakpoints and values, and nothing else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistentTimeSeries {
    /// The breakpoints, strictly increasing. Each one is the instant from which
    /// the value beside it is in force.
    pub timestamps: Vec<DateTime<Utc>>,
    pub length: usize,
    pub data: TypedArray,
    pub name: String,
    /// What the stored elements mean and how one timestep is laid out.
    ///
    /// Always concrete: a constructor resolves it to `Scalar(data.dtype)`, which
    /// is what an ordinary numeric series is, and `with_element_type` replaces
    /// it. There is deliberately no "undeclared" spelling — it would be a second
    /// way to say `Scalar(dtype)`, and a series written that way would not
    /// compare equal to the same series read back.
    ///
    /// Assigning a new `data` array without updating this is a mismatch the
    /// store rejects on write; build the series again instead.
    pub element_type: ElementType,
    /// User-declared units label for the values (e.g. `"USD/MMBtu"`), or `None`.
    ///
    /// Set by whoever creates the series and returned unchanged on read. The
    /// store never interprets or validates it, and it is not part of a series'
    /// identity: it cannot be filtered on, and two series differing only in
    /// their label are a duplicate.
    pub units: Option<String>,
    /// What kind of physical quantity the values measure (e.g. `"ActivePower"`),
    /// or `None`. Free-form; the recommended vocabulary is a QUDT `QuantityKind`
    /// local name. See [`crate::TimeSeriesMetadata::quantity_kind`].
    pub quantity_kind: Option<String>,
    /// Which basis the values are expressed in, or `None` for unspecified.
    /// See [`UnitSystem`].
    pub unit_system: Option<UnitSystem>,
    /// How this series' breakpoints were spelled, or `None` for unspecified.
    /// See [`TimeReference`] and [`crate::TimeSeriesMetadata::time_reference`].
    pub time_reference: Option<TimeReference>,
    /// The field on the owning component whose value varies over time here
    /// (e.g. `"fuel_cost"`), or `None`.
    /// See [`crate::TimeSeriesMetadata::component_field`].
    pub component_field: Option<String>,
    /// Opaque, package-owned payload (typically JSON) stored verbatim for an
    /// application to reconstruct its domain objects; the store never interprets
    /// it. This is where a consumer's own expansion policy lives — see the type
    /// docs.
    pub application_data: Option<String>,
}

impl PersistentTimeSeries {
    pub fn new(
        timestamps: Vec<DateTime<Utc>>,
        data: TypedArray,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        let length = data.length();
        if timestamps.len() != length {
            return Err(format!(
                "timestamp count {} does not match data length {length}",
                timestamps.len()
            ));
        }
        if timestamps.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("timestamps must be strictly increasing".to_string());
        }
        let element_type = ElementType::Scalar(data.dtype);
        Ok(Self {
            timestamps,
            length,
            data,
            name: name.into(),
            element_type,
            units: None,
            quantity_kind: None,
            unit_system: None,
            time_reference: None,
            component_field: None,
            application_data: None,
        })
    }

    /// The index into [`Self::timestamps`] and [`Self::data`] of the breakpoint
    /// **in force at** `at` — the greatest breakpoint `<= at`.
    ///
    /// `Err` if `at` is strictly before the first breakpoint, where the step
    /// function is undefined, or if the series is empty. This is the single
    /// source of truth for the lookup: nothing else should re-derive it.
    pub fn index_in_force_at(&self, at: DateTime<Utc>) -> Result<usize, String> {
        crate::timestamps::index_in_force_at(&self.timestamps, at).ok_or_else(|| {
            match self.timestamps.first() {
                Some(first) => format!(
                    "PersistentTimeSeries '{}' has no value at {at}: it is before the \
                     first breakpoint {first}, where a step function is undefined",
                    self.name
                ),
                None => format!(
                    "PersistentTimeSeries '{}' has no breakpoints, so it has no value at {at}",
                    self.name
                ),
            }
        })
    }

    /// Evaluate the step function at each instant in `at`, in the order given.
    ///
    /// Returns an array shaped `[at.len(), *E]`, where `*E` is this series'
    /// per-step element shape, with the dtype unchanged — so a caller decodes
    /// the result exactly as it decodes [`Self::data`], including for a ragged
    /// composite [`ElementType`] whose rows are copied whole, padding included.
    ///
    /// This is [`Self::index_in_force_at`] applied `at.len()` times and
    /// gathered; it makes no policy choice. Deciding *which* instants to ask
    /// for, or how to collapse the answer for a downstream solver, belongs to
    /// the caller — see the type docs on where policy lives.
    ///
    /// It is a **gather, not a slice**: `at` may be unsorted and may repeat, and
    /// each instant resolves independently, so the caller's order is the output
    /// order. An empty `at` yields an empty array of the right element shape
    /// rather than an error, matching what a zero-width range read selects.
    ///
    /// `Err` if *any* instant precedes the first breakpoint, where the step
    /// function is undefined. Every index is resolved before a single byte is
    /// copied, so such a call produces no partial output. `Err` too — rather
    /// than a panic — if `timestamps` and `data` have been driven out of step
    /// through the public fields since construction.
    pub fn project_onto(&self, at: &[DateTime<Utc>]) -> Result<TypedArray, String> {
        // Resolve first, gather second: a bad instant has to fail the whole
        // call, and a half-built array handed back with an error would invite
        // exactly the partial read this type refuses elsewhere.
        let rows = at
            .iter()
            .map(|&t| self.index_in_force_at(t))
            .collect::<Result<Vec<_>, _>>()?;

        let element_shape = self.data.element_shape();
        // The empty product is 1, which is what a scalar series wants: one
        // value per breakpoint.
        let row_bytes = element_shape
            .iter()
            .try_fold(self.data.dtype.size(), |acc, &d| acc.checked_mul(d))
            .ok_or_else(|| {
                format!(
                    "PersistentTimeSeries '{}': one row of element shape {element_shape:?} \
                     needs more bytes than usize can hold",
                    self.name
                )
            })?;
        let total = rows.len().checked_mul(row_bytes).ok_or_else(|| {
            format!(
                "PersistentTimeSeries '{}': projecting onto {} instants needs more bytes \
                 than usize can hold",
                self.name,
                rows.len()
            )
        })?;

        let mut bytes = Vec::with_capacity(total);
        for row in rows {
            // Checked rather than indexed. `timestamps` and `data` are public,
            // so a caller can push a breakpoint without extending the array and
            // leave `row` pointing past the end. `new` rules that out and the
            // store re-checks it on the write path, but this method is fallible
            // and reachable on a value that has been through neither since it
            // was last touched -- so it reports the mismatch rather than
            // aborting the process out of an API that promised a `Result`.
            let slice = row
                .checked_mul(row_bytes)
                .and_then(|start| Some(start..start.checked_add(row_bytes)?))
                .and_then(|range| self.data.bytes.get(range))
                .ok_or_else(|| {
                    format!(
                        "PersistentTimeSeries '{}': breakpoint {row} names no row in an array of                          {} bytes at {row_bytes} bytes per row; `timestamps` and `data` have been                          driven out of step since construction",
                        self.name,
                        self.data.bytes.len(),
                    )
                })?;
            bytes.extend_from_slice(slice);
        }

        let mut shape = Vec::with_capacity(1 + element_shape.len());
        shape.push(at.len());
        shape.extend_from_slice(element_shape);
        TypedArray::new(self.data.dtype, shape, bytes)
    }
}

impl PersistentTimeSeries {
    /// Declare the logical element type of the array. Validated on commit
    /// against the array's dtype and per-step shape.
    pub fn with_element_type(mut self, element_type: ElementType) -> Self {
        self.element_type = element_type;
        self
    }

    /// Set the user-declared units label.
    pub fn with_units(mut self, units: impl Into<String>) -> Self {
        self.units = Some(units.into());
        self
    }

    /// Set the quantity kind the values measure (e.g. `"ActivePower"`).
    pub fn with_quantity_kind(mut self, quantity_kind: impl Into<String>) -> Self {
        self.quantity_kind = Some(quantity_kind.into());
        self
    }

    /// Declare which unit basis the values are expressed in.
    pub fn with_unit_system(mut self, unit_system: UnitSystem) -> Self {
        self.unit_system = Some(unit_system);
        self
    }

    /// Declare how this series' breakpoints were spelled. Validated on commit
    /// (a zone name's *shape* only — see [`TimeReference::validate`]).
    pub fn with_time_reference(mut self, time_reference: TimeReference) -> Self {
        self.time_reference = Some(time_reference);
        self
    }

    /// Name the component field these values vary over time (e.g.
    /// `"fuel_cost"`).
    pub fn with_component_field(mut self, component_field: impl Into<String>) -> Self {
        self.component_field = Some(component_field.into());
        self
    }

    /// Set the opaque application payload carried through to the metadata row.
    pub fn with_application_data(mut self, application_data: impl Into<String>) -> Self {
        self.application_data = Some(application_data.into());
        self
    }
}

/// A deterministic forecast: one complete horizon array per count window.
///
/// `data` has shape `[H, count, *E]` in row-major order, where
/// `H = horizon / resolution` and `*E` is the per-step element shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Deterministic {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Period,
    pub horizon: Period,
    pub interval: Period,
    pub count: usize,
    /// Shape `[H, count, *E]`.
    pub data: TypedArray,
    pub name: String,
    /// What the stored elements mean and how one timestep is laid out.
    ///
    /// Always concrete: a constructor resolves it to `Scalar(data.dtype)`, which
    /// is what an ordinary numeric series is, and `with_element_type` replaces
    /// it. There is deliberately no "undeclared" spelling — it would be a second
    /// way to say `Scalar(dtype)`, and a series written that way would not
    /// compare equal to the same series read back.
    ///
    /// Assigning a new `data` array without updating this is a mismatch the
    /// store rejects on write; build the series again instead.
    pub element_type: ElementType,
    /// User-declared units label for the values (e.g. `"MW"`), or `None`.
    ///
    /// Set by whoever creates the series and returned unchanged on read. The
    /// store never interprets or validates it, and it is not part of a series'
    /// identity: it cannot be filtered on, and two series differing only in
    /// their label are a duplicate.
    pub units: Option<String>,
    /// What kind of physical quantity the values measure (e.g. `"ActivePower"`),
    /// or `None`. Free-form; the recommended vocabulary is a QUDT `QuantityKind`
    /// local name. See [`crate::TimeSeriesMetadata::quantity_kind`].
    pub quantity_kind: Option<String>,
    /// Which basis the values are expressed in, or `None` for unspecified.
    /// See [`UnitSystem`].
    pub unit_system: Option<UnitSystem>,
    /// How this series' timestamps were spelled, or `None` for unspecified.
    /// See [`TimeReference`] and [`crate::TimeSeriesMetadata::time_reference`].
    pub time_reference: Option<TimeReference>,
    /// The field on the owning component whose value varies over time here
    /// (e.g. `"max_active_power"`), or `None`.
    /// See [`crate::TimeSeriesMetadata::component_field`].
    pub component_field: Option<String>,
    /// Opaque, package-owned payload (typically JSON) stored verbatim for an
    /// application to reconstruct its domain objects; the store never interprets
    /// it. End users are not expected to set it.
    pub application_data: Option<String>,
}

impl Deterministic {
    /// Construct, validating that `data.shape` matches the canonical layout.
    ///
    /// Returns `Err(String)` (mapped to `IntegrityError` by the store) if any
    /// dimension is inconsistent. Shape must be `[H, count, *E]` where
    /// `H = horizon / resolution` and `*E` is any trailing element dims.
    pub fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: impl Into<Period>,
        horizon: impl Into<Period>,
        interval: impl Into<Period>,
        count: usize,
        data: TypedArray,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        let element_type = ElementType::Scalar(data.dtype);
        let out = Self {
            initial_timestamp,
            resolution: resolution.into(),
            horizon: horizon.into(),
            interval: interval.into(),
            count,
            data,
            name: name.into(),
            element_type,
            units: None,
            quantity_kind: None,
            unit_system: None,
            time_reference: None,
            component_field: None,
            application_data: None,
        };
        out.validate()?;
        Ok(out)
    }

    /// Re-check the invariants [`Self::new`] establishes, against the values the
    /// struct currently holds.
    ///
    /// Every field is `pub` and the type derives `Deserialize`, so a struct
    /// literal, a field assignment, or `serde_json::from_str` all produce a
    /// `Deterministic` that never met a constructor. The store calls this on the
    /// write path for exactly that reason: the constructor is not a boundary
    /// anything can rely on, and a forecast whose periods or shape disagree is
    /// writable but unreadable.
    pub fn validate(&self) -> Result<(), String> {
        validate_forecast_periods(self.resolution, self.horizon, self.interval, self.count)?;
        let h = compute_h(self.horizon, self.resolution)?;
        // Derive element dims from trailing shape after [H, count].
        if self.data.shape.len() < 2 {
            return Err(format!(
                "Deterministic: shape {:?} must have at least 2 dims [H, count]",
                self.data.shape
            ));
        }
        let elem_dims = &self.data.shape[2..];
        let expected_shape: Vec<usize> = std::iter::once(h)
            .chain(std::iter::once(self.count))
            .chain(elem_dims.iter().copied())
            .collect();
        if self.data.shape != expected_shape {
            return Err(format!(
                "Deterministic: expected shape {expected_shape:?}, got {:?}",
                self.data.shape
            ));
        }
        Ok(())
    }
}

/// A probabilistic forecast: per-percentile, per-window horizon arrays.
///
/// `data` has shape `[num_percentiles, H, count, *E]` in row-major order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Probabilistic {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Period,
    pub horizon: Period,
    pub interval: Period,
    pub count: usize,
    pub percentiles: Vec<f64>,
    /// Shape `[num_percentiles, H, count, *E]`.
    pub data: TypedArray,
    pub name: String,
    /// What the stored elements mean and how one timestep is laid out.
    ///
    /// Always concrete: a constructor resolves it to `Scalar(data.dtype)`, which
    /// is what an ordinary numeric series is, and `with_element_type` replaces
    /// it. There is deliberately no "undeclared" spelling — it would be a second
    /// way to say `Scalar(dtype)`, and a series written that way would not
    /// compare equal to the same series read back.
    ///
    /// Assigning a new `data` array without updating this is a mismatch the
    /// store rejects on write; build the series again instead.
    pub element_type: ElementType,
    /// User-declared units label for the values (e.g. `"MW"`), or `None`.
    ///
    /// Set by whoever creates the series and returned unchanged on read. The
    /// store never interprets or validates it, and it is not part of a series'
    /// identity: it cannot be filtered on, and two series differing only in
    /// their label are a duplicate.
    pub units: Option<String>,
    /// What kind of physical quantity the values measure (e.g. `"ActivePower"`),
    /// or `None`. Free-form; the recommended vocabulary is a QUDT `QuantityKind`
    /// local name. See [`crate::TimeSeriesMetadata::quantity_kind`].
    pub quantity_kind: Option<String>,
    /// Which basis the values are expressed in, or `None` for unspecified.
    /// See [`UnitSystem`].
    pub unit_system: Option<UnitSystem>,
    /// How this series' timestamps were spelled, or `None` for unspecified.
    /// See [`TimeReference`] and [`crate::TimeSeriesMetadata::time_reference`].
    pub time_reference: Option<TimeReference>,
    /// The field on the owning component whose value varies over time here
    /// (e.g. `"max_active_power"`), or `None`.
    /// See [`crate::TimeSeriesMetadata::component_field`].
    pub component_field: Option<String>,
    /// Opaque, package-owned payload (typically JSON) stored verbatim for an
    /// application to reconstruct its domain objects; the store never interprets
    /// it. End users are not expected to set it.
    pub application_data: Option<String>,
}

impl Deterministic {
    /// Construct from per-window logical values, encoding them into the flat
    /// array the store holds and declaring the element type they imply.
    ///
    /// The pairing is the point. An `element_type` and the array it describes
    /// are two independent things a caller can get out of step;
    /// [`Store::add`](crate::Store::add) rejects the mismatch, but only after
    /// the fact. Deriving both from one set of values means there is no
    /// mismatch to reject.
    ///
    /// Returns `Err(String)` if the values cannot be encoded: a
    /// [`DecodedValues::Raw`], which carries no values of its own (build the
    /// [`TypedArray`] and call [`Self::new`] instead), tuple rows of differing
    /// arity, or a step function whose `x` and `y` lengths disagree.
    ///
    /// A tuple series with *no* rows is the one storable series these
    /// constructors cannot name, because a tuple's arity lives in its rows.
    /// Encode that one with [`encode_as`](crate::encode_as), which takes the
    /// arity from a declared element type, and pair the two through
    /// [`Self::new`] and [`Self::with_element_type`].
    ///
    /// One entry per timestep in row-major order over the leading axes, so
    /// entry `i * count + j` is window `j`'s step `i`, and there must be
    /// exactly `H * count` of them.
    pub fn from_values(
        initial_timestamp: DateTime<Utc>,
        resolution: impl Into<Period>,
        horizon: impl Into<Period>,
        interval: impl Into<Period>,
        count: usize,
        values: &DecodedValues,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        let resolution = resolution.into();
        let horizon = horizon.into();
        let h = compute_h(horizon, resolution)?;
        let (data, element_type) = encode_with_type(values, &[h, count])?;
        Ok(Self::new(
            initial_timestamp,
            resolution,
            horizon,
            interval,
            count,
            data,
            name,
        )?
        .with_element_type(element_type))
    }

    /// Declare the logical element type of the array. Validated on commit
    /// against the array's dtype and per-step shape.
    pub fn with_element_type(mut self, element_type: ElementType) -> Self {
        self.element_type = element_type;
        self
    }

    /// Set the user-declared units label.
    pub fn with_units(mut self, units: impl Into<String>) -> Self {
        self.units = Some(units.into());
        self
    }

    /// Set the quantity kind the values measure (e.g. `"ActivePower"`).
    pub fn with_quantity_kind(mut self, quantity_kind: impl Into<String>) -> Self {
        self.quantity_kind = Some(quantity_kind.into());
        self
    }

    /// Declare which unit basis the values are expressed in.
    pub fn with_unit_system(mut self, unit_system: UnitSystem) -> Self {
        self.unit_system = Some(unit_system);
        self
    }

    /// Declare how this series' timestamps were spelled. Validated on commit
    /// (a zone name's *shape* only — see [`TimeReference::validate`]).
    pub fn with_time_reference(mut self, time_reference: TimeReference) -> Self {
        self.time_reference = Some(time_reference);
        self
    }

    /// Name the component field these values vary over time (e.g.
    /// `"max_active_power"`).
    pub fn with_component_field(mut self, component_field: impl Into<String>) -> Self {
        self.component_field = Some(component_field.into());
        self
    }

    /// Set the opaque application payload carried through to the metadata row.
    pub fn with_application_data(mut self, application_data: impl Into<String>) -> Self {
        self.application_data = Some(application_data.into());
        self
    }
}

impl Probabilistic {
    /// Construct, validating shape, percentile ordering, and positive durations.
    ///
    /// Returns `Err(String)` if any constraint is violated. Shape must be
    /// `[num_percentiles, H, count, *E]` where `H = horizon / resolution`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: impl Into<Period>,
        horizon: impl Into<Period>,
        interval: impl Into<Period>,
        count: usize,
        percentiles: Vec<f64>,
        data: TypedArray,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        let element_type = ElementType::Scalar(data.dtype);
        let out = Self {
            initial_timestamp,
            resolution: resolution.into(),
            horizon: horizon.into(),
            interval: interval.into(),
            count,
            percentiles,
            data,
            name: name.into(),
            element_type,
            units: None,
            quantity_kind: None,
            unit_system: None,
            time_reference: None,
            component_field: None,
            application_data: None,
        };
        out.validate()?;
        Ok(out)
    }

    /// Re-check the invariants [`Self::new`] establishes. See
    /// [`Deterministic::validate`] for why the store calls this on write.
    pub fn validate(&self) -> Result<(), String> {
        validate_forecast_periods(self.resolution, self.horizon, self.interval, self.count)?;
        if self.percentiles.is_empty() {
            return Err("Probabilistic: percentiles must be non-empty".to_string());
        }
        if self.percentiles.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("Probabilistic: percentiles must be strictly increasing".to_string());
        }
        let h = compute_h(self.horizon, self.resolution)?;
        let p = self.percentiles.len();
        let count = self.count;
        if self.data.shape.len() < 3 {
            return Err(format!(
                "Probabilistic: shape {:?} must have at least 3 dims [P, H, count]",
                self.data.shape
            ));
        }
        let elem_dims = &self.data.shape[3..];
        let expected_shape: Vec<usize> = std::iter::once(p)
            .chain(std::iter::once(h))
            .chain(std::iter::once(count))
            .chain(elem_dims.iter().copied())
            .collect();
        if self.data.shape != expected_shape {
            return Err(format!(
                "Probabilistic: expected shape {expected_shape:?} \
                 (percentiles={p}, H={h}, count={count}), got {:?}",
                self.data.shape
            ));
        }
        Ok(())
    }
}

/// A scenarios forecast: per-scenario, per-window horizon arrays.
///
/// `data` has shape `[scenario_count, H, count, *E]` in row-major order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scenarios {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Period,
    pub horizon: Period,
    pub interval: Period,
    pub count: usize,
    pub scenario_count: usize,
    /// Shape `[scenario_count, H, count, *E]`.
    pub data: TypedArray,
    pub name: String,
    /// What the stored elements mean and how one timestep is laid out.
    ///
    /// Always concrete: a constructor resolves it to `Scalar(data.dtype)`, which
    /// is what an ordinary numeric series is, and `with_element_type` replaces
    /// it. There is deliberately no "undeclared" spelling — it would be a second
    /// way to say `Scalar(dtype)`, and a series written that way would not
    /// compare equal to the same series read back.
    ///
    /// Assigning a new `data` array without updating this is a mismatch the
    /// store rejects on write; build the series again instead.
    pub element_type: ElementType,
    /// User-declared units label for the values (e.g. `"MW"`), or `None`.
    ///
    /// Set by whoever creates the series and returned unchanged on read. The
    /// store never interprets or validates it, and it is not part of a series'
    /// identity: it cannot be filtered on, and two series differing only in
    /// their label are a duplicate.
    pub units: Option<String>,
    /// What kind of physical quantity the values measure (e.g. `"ActivePower"`),
    /// or `None`. Free-form; the recommended vocabulary is a QUDT `QuantityKind`
    /// local name. See [`crate::TimeSeriesMetadata::quantity_kind`].
    pub quantity_kind: Option<String>,
    /// Which basis the values are expressed in, or `None` for unspecified.
    /// See [`UnitSystem`].
    pub unit_system: Option<UnitSystem>,
    /// How this series' timestamps were spelled, or `None` for unspecified.
    /// See [`TimeReference`] and [`crate::TimeSeriesMetadata::time_reference`].
    pub time_reference: Option<TimeReference>,
    /// The field on the owning component whose value varies over time here
    /// (e.g. `"max_active_power"`), or `None`.
    /// See [`crate::TimeSeriesMetadata::component_field`].
    pub component_field: Option<String>,
    /// Opaque, package-owned payload (typically JSON) stored verbatim for an
    /// application to reconstruct its domain objects; the store never interprets
    /// it. End users are not expected to set it.
    pub application_data: Option<String>,
}

impl Probabilistic {
    /// Construct from per-window logical values, encoding them into the flat
    /// array the store holds and declaring the element type they imply.
    ///
    /// The pairing is the point. An `element_type` and the array it describes
    /// are two independent things a caller can get out of step;
    /// [`Store::add`](crate::Store::add) rejects the mismatch, but only after
    /// the fact. Deriving both from one set of values means there is no
    /// mismatch to reject.
    ///
    /// Returns `Err(String)` if the values cannot be encoded: a
    /// [`DecodedValues::Raw`], which carries no values of its own (build the
    /// [`TypedArray`] and call [`Self::new`] instead), tuple rows of differing
    /// arity, or a step function whose `x` and `y` lengths disagree.
    ///
    /// A tuple series with *no* rows is the one storable series these
    /// constructors cannot name, because a tuple's arity lives in its rows.
    /// Encode that one with [`encode_as`](crate::encode_as), which takes the
    /// arity from a declared element type, and pair the two through
    /// [`Self::new`] and [`Self::with_element_type`].
    ///
    /// One entry per timestep in row-major order over `[num_percentiles, H,
    /// count]`, so there must be exactly `percentiles.len() * H * count` of
    /// them.
    #[allow(clippy::too_many_arguments)]
    pub fn from_values(
        initial_timestamp: DateTime<Utc>,
        resolution: impl Into<Period>,
        horizon: impl Into<Period>,
        interval: impl Into<Period>,
        count: usize,
        percentiles: Vec<f64>,
        values: &DecodedValues,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        let resolution = resolution.into();
        let horizon = horizon.into();
        let h = compute_h(horizon, resolution)?;
        let (data, element_type) = encode_with_type(values, &[percentiles.len(), h, count])?;
        Ok(Self::new(
            initial_timestamp,
            resolution,
            horizon,
            interval,
            count,
            percentiles,
            data,
            name,
        )?
        .with_element_type(element_type))
    }

    /// Declare the logical element type of the array. Validated on commit
    /// against the array's dtype and per-step shape.
    pub fn with_element_type(mut self, element_type: ElementType) -> Self {
        self.element_type = element_type;
        self
    }

    /// Set the user-declared units label.
    pub fn with_units(mut self, units: impl Into<String>) -> Self {
        self.units = Some(units.into());
        self
    }

    /// Set the quantity kind the values measure (e.g. `"ActivePower"`).
    pub fn with_quantity_kind(mut self, quantity_kind: impl Into<String>) -> Self {
        self.quantity_kind = Some(quantity_kind.into());
        self
    }

    /// Declare which unit basis the values are expressed in.
    pub fn with_unit_system(mut self, unit_system: UnitSystem) -> Self {
        self.unit_system = Some(unit_system);
        self
    }

    /// Declare how this series' timestamps were spelled. Validated on commit
    /// (a zone name's *shape* only — see [`TimeReference::validate`]).
    pub fn with_time_reference(mut self, time_reference: TimeReference) -> Self {
        self.time_reference = Some(time_reference);
        self
    }

    /// Name the component field these values vary over time (e.g.
    /// `"max_active_power"`).
    pub fn with_component_field(mut self, component_field: impl Into<String>) -> Self {
        self.component_field = Some(component_field.into());
        self
    }

    /// Set the opaque application payload carried through to the metadata row.
    pub fn with_application_data(mut self, application_data: impl Into<String>) -> Self {
        self.application_data = Some(application_data.into());
        self
    }
}

impl Scenarios {
    /// Construct, validating shape against the canonical layout.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: impl Into<Period>,
        horizon: impl Into<Period>,
        interval: impl Into<Period>,
        count: usize,
        scenario_count: usize,
        data: TypedArray,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        let element_type = ElementType::Scalar(data.dtype);
        let out = Self {
            initial_timestamp,
            resolution: resolution.into(),
            horizon: horizon.into(),
            interval: interval.into(),
            count,
            scenario_count,
            data,
            name: name.into(),
            element_type,
            units: None,
            quantity_kind: None,
            unit_system: None,
            time_reference: None,
            component_field: None,
            application_data: None,
        };
        out.validate()?;
        Ok(out)
    }

    /// Re-check the invariants [`Self::new`] establishes. See
    /// [`Deterministic::validate`] for why the store calls this on write.
    pub fn validate(&self) -> Result<(), String> {
        validate_forecast_periods(self.resolution, self.horizon, self.interval, self.count)?;
        let h = compute_h(self.horizon, self.resolution)?;
        let (scenario_count, count) = (self.scenario_count, self.count);
        let elem_dims: Vec<usize> = if self.data.shape.len() > 3 {
            self.data.shape[3..].to_vec()
        } else {
            vec![]
        };
        let expected_shape: Vec<usize> = std::iter::once(scenario_count)
            .chain(std::iter::once(h))
            .chain(std::iter::once(count))
            .chain(elem_dims)
            .collect();
        if self.data.shape != expected_shape {
            return Err(format!(
                "Scenarios: expected shape {expected_shape:?} \
                 (scenario_count={scenario_count}, H={h}, count={count}), got {:?}",
                self.data.shape
            ));
        }
        Ok(())
    }
}

/// Compute H = horizon / resolution, requiring an exact integer division > 0.
///
/// Because [`Period::divide_into`] requires both periods to be the same kind,
/// this also enforces that a forecast's horizon and resolution are both fixed
/// or both calendar (so `H` is a constant integer).
pub(crate) fn compute_h(horizon: Period, resolution: Period) -> Result<usize, String> {
    resolution.divide_into(&horizon).map_err(|e| e.to_string())
}

/// Encode `values` over `leading_dims` and name the element type they produce.
///
/// The two halves that have to agree, derived from one input. Every
/// `from_values` constructor goes through here, which is what makes the
/// agreement structural rather than something the caller maintains.
fn encode_with_type(
    values: &DecodedValues,
    leading_dims: &[usize],
) -> Result<(TypedArray, ElementType), String> {
    // Encode first: it is the call that rejects `Raw`, and its message names the
    // remedy. `element_type_of` only returns `None` for the same case, so the
    // fallback below is unreachable in practice and exists to avoid a panic if
    // that ever stops being true.
    let data = codec::encode(values, leading_dims).map_err(|e| e.to_string())?;
    let element_type = codec::element_type_of(values)
        .ok_or_else(|| "these values have no element type of their own".to_string())?;
    Ok((data, element_type))
}

/// Validate a forecast's periods: resolution and horizon must be strictly
/// positive; interval must be strictly positive unless the forecast has a
/// single window (`count == 1`), where a zero interval is meaningful — there
/// is no second window to step to.
fn validate_forecast_periods(
    resolution: Period,
    horizon: Period,
    interval: Period,
    count: usize,
) -> Result<(), String> {
    let check = |p: Period, name: &str| {
        if !p.is_positive() {
            Err(format!("{name} must be strictly positive"))
        } else {
            Ok(())
        }
    };
    check(resolution, "resolution")?;
    check(horizon, "horizon")?;
    // `count <= 1`, not `count == 1`: the interval is the step *between* windows,
    // so a forecast with one window has none to take and a forecast with none at
    // all has none either. Restricting the allowance to exactly one window made
    // a legitimate query on a zero-interval single-window forecast fail —
    // `resolve_windows` returns an empty selection for a zero-width `time_range`,
    // and rebuilding that as `count = 0` tripped this check, which
    // `Store::get_time_series` reports as `IntegrityError`. A caller asking a
    // well-formed question about an intact store was told the store was corrupt,
    // and only for the zero-interval encoding: the same query against a
    // positive-interval forecast returned an empty result.
    if !(interval.is_positive() || count <= 1 && interval.is_zero()) {
        return Err(
            "interval must be strictly positive (zero is allowed only for a forecast with at \
             most one window)"
                .to_string(),
        );
    }
    Ok(())
}

impl Scenarios {
    /// Construct from per-window logical values, encoding them into the flat
    /// array the store holds and declaring the element type they imply.
    ///
    /// The pairing is the point. An `element_type` and the array it describes
    /// are two independent things a caller can get out of step;
    /// [`Store::add`](crate::Store::add) rejects the mismatch, but only after
    /// the fact. Deriving both from one set of values means there is no
    /// mismatch to reject.
    ///
    /// Returns `Err(String)` if the values cannot be encoded: a
    /// [`DecodedValues::Raw`], which carries no values of its own (build the
    /// [`TypedArray`] and call [`Self::new`] instead), tuple rows of differing
    /// arity, or a step function whose `x` and `y` lengths disagree.
    ///
    /// A tuple series with *no* rows is the one storable series these
    /// constructors cannot name, because a tuple's arity lives in its rows.
    /// Encode that one with [`encode_as`](crate::encode_as), which takes the
    /// arity from a declared element type, and pair the two through
    /// [`Self::new`] and [`Self::with_element_type`].
    ///
    /// One entry per timestep in row-major order over `[scenario_count, H,
    /// count]`, so there must be exactly `scenario_count * H * count` of them.
    #[allow(clippy::too_many_arguments)]
    pub fn from_values(
        initial_timestamp: DateTime<Utc>,
        resolution: impl Into<Period>,
        horizon: impl Into<Period>,
        interval: impl Into<Period>,
        count: usize,
        scenario_count: usize,
        values: &DecodedValues,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        let resolution = resolution.into();
        let horizon = horizon.into();
        let h = compute_h(horizon, resolution)?;
        let (data, element_type) = encode_with_type(values, &[scenario_count, h, count])?;
        Ok(Self::new(
            initial_timestamp,
            resolution,
            horizon,
            interval,
            count,
            scenario_count,
            data,
            name,
        )?
        .with_element_type(element_type))
    }

    /// Declare the logical element type of the array. Validated on commit
    /// against the array's dtype and per-step shape.
    pub fn with_element_type(mut self, element_type: ElementType) -> Self {
        self.element_type = element_type;
        self
    }

    /// Set the user-declared units label.
    pub fn with_units(mut self, units: impl Into<String>) -> Self {
        self.units = Some(units.into());
        self
    }

    /// Set the quantity kind the values measure (e.g. `"ActivePower"`).
    pub fn with_quantity_kind(mut self, quantity_kind: impl Into<String>) -> Self {
        self.quantity_kind = Some(quantity_kind.into());
        self
    }

    /// Declare which unit basis the values are expressed in.
    pub fn with_unit_system(mut self, unit_system: UnitSystem) -> Self {
        self.unit_system = Some(unit_system);
        self
    }

    /// Declare how this series' timestamps were spelled. Validated on commit
    /// (a zone name's *shape* only — see [`TimeReference::validate`]).
    pub fn with_time_reference(mut self, time_reference: TimeReference) -> Self {
        self.time_reference = Some(time_reference);
        self
    }

    /// Name the component field these values vary over time (e.g.
    /// `"max_active_power"`).
    pub fn with_component_field(mut self, component_field: impl Into<String>) -> Self {
        self.component_field = Some(component_field.into());
        self
    }

    /// Set the opaque application payload carried through to the metadata row.
    pub fn with_application_data(mut self, application_data: impl Into<String>) -> Self {
        self.application_data = Some(application_data.into());
        self
    }
}

/// The descriptive attributes a series carries alongside its array: everything
/// that describes the values without addressing them.
///
/// None of these are part of a series' identity — they are absent from
/// [`crate::KeyIdentity`] and from both content hashes — so the read path
/// reconstructs a series from its array and then fills these in from the
/// catalog row via [`TimeSeriesData::set_descriptors`].
///
/// This is a struct rather than a positional argument list because four of the
/// seven fields are `Option<String>`: as bare parameters, `units`,
/// `quantity_kind`, `component_field`, and `application_data` would be silently
/// interchangeable at every call site.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Descriptors {
    pub element_type: ElementType,
    pub units: Option<String>,
    pub quantity_kind: Option<String>,
    pub unit_system: Option<UnitSystem>,
    pub time_reference: Option<TimeReference>,
    pub component_field: Option<String>,
    pub application_data: Option<String>,
}

impl Descriptors {
    /// The descriptors of a series that declares nothing but its element type.
    pub fn new(element_type: ElementType) -> Self {
        Self {
            element_type,
            units: None,
            quantity_kind: None,
            unit_system: None,
            time_reference: None,
            component_field: None,
            application_data: None,
        }
    }
}

/// Runtime variant container for all supported time-series types.
///
/// `DeterministicSingleTimeSeries` is synthesized into `Deterministic` on
/// read; there is no separate `DeterministicSingleTimeSeries` variant here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TimeSeriesData {
    SingleTimeSeries(SingleTimeSeries),
    NonSequentialTimeSeries(NonSequentialTimeSeries),
    Deterministic(Deterministic),
    Probabilistic(Probabilistic),
    Scenarios(Scenarios),
    PersistentTimeSeries(PersistentTimeSeries),
}

impl TimeSeriesData {
    pub fn time_series_type(&self) -> TimeSeriesType {
        match self {
            TimeSeriesData::SingleTimeSeries(_) => TimeSeriesType::SingleTimeSeries,
            TimeSeriesData::NonSequentialTimeSeries(_) => TimeSeriesType::NonSequentialTimeSeries,
            TimeSeriesData::Deterministic(_) => TimeSeriesType::Deterministic,
            TimeSeriesData::Probabilistic(_) => TimeSeriesType::Probabilistic,
            TimeSeriesData::Scenarios(_) => TimeSeriesType::Scenarios,
            TimeSeriesData::PersistentTimeSeries(_) => TimeSeriesType::PersistentTimeSeries,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => &s.name,
            TimeSeriesData::NonSequentialTimeSeries(s) => &s.name,
            TimeSeriesData::Deterministic(d) => &d.name,
            TimeSeriesData::Probabilistic(p) => &p.name,
            TimeSeriesData::Scenarios(s) => &s.name,
            TimeSeriesData::PersistentTimeSeries(p) => &p.name,
        }
    }

    /// The stored array of the wrapped series.
    fn array(&self) -> &TypedArray {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => &s.data,
            TimeSeriesData::NonSequentialTimeSeries(s) => &s.data,
            TimeSeriesData::Deterministic(d) => &d.data,
            TimeSeriesData::Probabilistic(p) => &p.data,
            TimeSeriesData::Scenarios(s) => &s.data,
            TimeSeriesData::PersistentTimeSeries(p) => &p.data,
        }
    }

    /// Decode the wrapped array into the per-timestep values its element type
    /// describes — the read-side counterpart of the `from_values` constructors,
    /// and the reason a caller never has to know the row layouts.
    ///
    /// Entries are in row-major order over the leading axes, so for a
    /// `Deterministic` entry `i * count + j` is window `j`'s step `i`.
    ///
    /// [`DecodedValues::Raw`] for every scalar element type and for any array
    /// whose physical dtype is not `f64`: there the stored elements already are
    /// the values, and the array itself is the answer.
    ///
    /// ```
    /// # use infrastore_core::{DecodedValues, TimeSeriesData, XyPoint, SingleTimeSeries, Period};
    /// # use chrono::{TimeZone, Utc, Duration};
    /// # let curves = DecodedValues::PiecewiseLinear(vec![
    /// #     vec![XyPoint { x: 0.0, y: 1.0 }, XyPoint { x: 1.0, y: 3.0 }],
    /// #     vec![XyPoint { x: 0.0, y: 2.0 }],
    /// # ]);
    /// let series = SingleTimeSeries::from_values(
    ///     Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
    ///     Period::Fixed(Duration::hours(1)),
    ///     &curves,
    ///     "variable_cost",
    /// )?;
    /// let data = TimeSeriesData::SingleTimeSeries(series);
    /// assert_eq!(data.decoded_values().unwrap(), curves);
    /// # Ok::<(), String>(())
    /// ```
    pub fn decoded_values(&self) -> crate::Result<DecodedValues> {
        codec::decode(
            self.array(),
            self.element_type(),
            self.time_series_type().leading_dims(),
        )
    }

    /// The element type of the wrapped series — always concrete, defaulting to
    /// plain scalars of the array's own dtype.
    pub fn element_type(&self) -> ElementType {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.element_type,
            TimeSeriesData::NonSequentialTimeSeries(s) => s.element_type,
            TimeSeriesData::Deterministic(d) => d.element_type,
            TimeSeriesData::Probabilistic(p) => p.element_type,
            TimeSeriesData::Scenarios(s) => s.element_type,
            TimeSeriesData::PersistentTimeSeries(p) => p.element_type,
        }
    }

    /// The user-declared units label, or `None`.
    pub fn units(&self) -> Option<&str> {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.units.as_deref(),
            TimeSeriesData::NonSequentialTimeSeries(s) => s.units.as_deref(),
            TimeSeriesData::Deterministic(d) => d.units.as_deref(),
            TimeSeriesData::Probabilistic(p) => p.units.as_deref(),
            TimeSeriesData::Scenarios(s) => s.units.as_deref(),
            TimeSeriesData::PersistentTimeSeries(p) => p.units.as_deref(),
        }
    }

    /// The quantity kind the values measure, or `None`.
    pub fn quantity_kind(&self) -> Option<&str> {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.quantity_kind.as_deref(),
            TimeSeriesData::NonSequentialTimeSeries(s) => s.quantity_kind.as_deref(),
            TimeSeriesData::Deterministic(d) => d.quantity_kind.as_deref(),
            TimeSeriesData::Probabilistic(p) => p.quantity_kind.as_deref(),
            TimeSeriesData::Scenarios(s) => s.quantity_kind.as_deref(),
            TimeSeriesData::PersistentTimeSeries(p) => p.quantity_kind.as_deref(),
        }
    }

    /// The declared unit basis, or `None` if unspecified.
    pub fn unit_system(&self) -> Option<UnitSystem> {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.unit_system,
            TimeSeriesData::NonSequentialTimeSeries(s) => s.unit_system,
            TimeSeriesData::Deterministic(d) => d.unit_system,
            TimeSeriesData::Probabilistic(p) => p.unit_system,
            TimeSeriesData::Scenarios(s) => s.unit_system,
            TimeSeriesData::PersistentTimeSeries(p) => p.unit_system,
        }
    }

    /// How the timestamps were spelled, or `None` if unspecified.
    pub fn time_reference(&self) -> Option<&TimeReference> {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.time_reference.as_ref(),
            TimeSeriesData::NonSequentialTimeSeries(s) => s.time_reference.as_ref(),
            TimeSeriesData::Deterministic(d) => d.time_reference.as_ref(),
            TimeSeriesData::Probabilistic(p) => p.time_reference.as_ref(),
            TimeSeriesData::Scenarios(s) => s.time_reference.as_ref(),
            TimeSeriesData::PersistentTimeSeries(p) => p.time_reference.as_ref(),
        }
    }

    /// The component field these values vary over time, or `None`.
    pub fn component_field(&self) -> Option<&str> {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.component_field.as_deref(),
            TimeSeriesData::NonSequentialTimeSeries(s) => s.component_field.as_deref(),
            TimeSeriesData::Deterministic(d) => d.component_field.as_deref(),
            TimeSeriesData::Probabilistic(p) => p.component_field.as_deref(),
            TimeSeriesData::Scenarios(s) => s.component_field.as_deref(),
            TimeSeriesData::PersistentTimeSeries(p) => p.component_field.as_deref(),
        }
    }

    /// The opaque application payload, or `None`.
    pub fn application_data(&self) -> Option<&str> {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.application_data.as_deref(),
            TimeSeriesData::NonSequentialTimeSeries(s) => s.application_data.as_deref(),
            TimeSeriesData::Deterministic(d) => d.application_data.as_deref(),
            TimeSeriesData::Probabilistic(p) => p.application_data.as_deref(),
            TimeSeriesData::Scenarios(s) => s.application_data.as_deref(),
            TimeSeriesData::PersistentTimeSeries(p) => p.application_data.as_deref(),
        }
    }

    /// Declare the logical element type of the wrapped series.
    pub fn with_element_type(mut self, element_type: ElementType) -> Self {
        self.set_element_type(element_type);
        self
    }

    /// Set the user-declared units label on the wrapped series.
    pub fn with_units(mut self, units: impl Into<String>) -> Self {
        self.set_units(Some(units.into()));
        self
    }

    /// Set the quantity kind on the wrapped series.
    pub fn with_quantity_kind(mut self, quantity_kind: impl Into<String>) -> Self {
        self.set_quantity_kind(Some(quantity_kind.into()));
        self
    }

    /// Declare the unit basis on the wrapped series.
    pub fn with_unit_system(mut self, unit_system: UnitSystem) -> Self {
        self.set_unit_system(Some(unit_system));
        self
    }

    /// Declare how the wrapped series' timestamps were spelled.
    pub fn with_time_reference(mut self, time_reference: TimeReference) -> Self {
        self.set_time_reference(Some(time_reference));
        self
    }

    /// Name the component field on the wrapped series.
    pub fn with_component_field(mut self, component_field: impl Into<String>) -> Self {
        self.set_component_field(Some(component_field.into()));
        self
    }

    /// Set the opaque application payload on the wrapped series.
    pub fn with_application_data(mut self, application_data: impl Into<String>) -> Self {
        self.set_application_data(Some(application_data.into()));
        self
    }

    /// Set the element type in place.
    pub fn set_element_type(&mut self, element_type: ElementType) {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.element_type = element_type,
            TimeSeriesData::NonSequentialTimeSeries(s) => s.element_type = element_type,
            TimeSeriesData::Deterministic(d) => d.element_type = element_type,
            TimeSeriesData::Probabilistic(p) => p.element_type = element_type,
            TimeSeriesData::Scenarios(s) => s.element_type = element_type,
            TimeSeriesData::PersistentTimeSeries(p) => p.element_type = element_type,
        }
    }

    /// Set the units label in place.
    pub fn set_units(&mut self, units: Option<String>) {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.units = units,
            TimeSeriesData::NonSequentialTimeSeries(s) => s.units = units,
            TimeSeriesData::Deterministic(d) => d.units = units,
            TimeSeriesData::Probabilistic(p) => p.units = units,
            TimeSeriesData::Scenarios(s) => s.units = units,
            TimeSeriesData::PersistentTimeSeries(p) => p.units = units,
        }
    }

    /// Set the quantity kind in place.
    pub fn set_quantity_kind(&mut self, quantity_kind: Option<String>) {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.quantity_kind = quantity_kind,
            TimeSeriesData::NonSequentialTimeSeries(s) => s.quantity_kind = quantity_kind,
            TimeSeriesData::Deterministic(d) => d.quantity_kind = quantity_kind,
            TimeSeriesData::Probabilistic(p) => p.quantity_kind = quantity_kind,
            TimeSeriesData::Scenarios(s) => s.quantity_kind = quantity_kind,
            TimeSeriesData::PersistentTimeSeries(p) => p.quantity_kind = quantity_kind,
        }
    }

    /// Set the unit basis in place.
    pub fn set_unit_system(&mut self, unit_system: Option<UnitSystem>) {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.unit_system = unit_system,
            TimeSeriesData::NonSequentialTimeSeries(s) => s.unit_system = unit_system,
            TimeSeriesData::Deterministic(d) => d.unit_system = unit_system,
            TimeSeriesData::Probabilistic(p) => p.unit_system = unit_system,
            TimeSeriesData::Scenarios(s) => s.unit_system = unit_system,
            TimeSeriesData::PersistentTimeSeries(p) => p.unit_system = unit_system,
        }
    }

    /// Set the timestamp spelling in place.
    pub fn set_time_reference(&mut self, time_reference: Option<TimeReference>) {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.time_reference = time_reference,
            TimeSeriesData::NonSequentialTimeSeries(s) => s.time_reference = time_reference,
            TimeSeriesData::Deterministic(d) => d.time_reference = time_reference,
            TimeSeriesData::Probabilistic(p) => p.time_reference = time_reference,
            TimeSeriesData::Scenarios(s) => s.time_reference = time_reference,
            TimeSeriesData::PersistentTimeSeries(p) => p.time_reference = time_reference,
        }
    }

    /// Set the component field in place.
    pub fn set_component_field(&mut self, component_field: Option<String>) {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.component_field = component_field,
            TimeSeriesData::NonSequentialTimeSeries(s) => s.component_field = component_field,
            TimeSeriesData::Deterministic(d) => d.component_field = component_field,
            TimeSeriesData::Probabilistic(p) => p.component_field = component_field,
            TimeSeriesData::Scenarios(s) => s.component_field = component_field,
            TimeSeriesData::PersistentTimeSeries(p) => p.component_field = component_field,
        }
    }

    /// Set the application payload in place.
    pub fn set_application_data(&mut self, application_data: Option<String>) {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => s.application_data = application_data,
            TimeSeriesData::NonSequentialTimeSeries(s) => s.application_data = application_data,
            TimeSeriesData::Deterministic(d) => d.application_data = application_data,
            TimeSeriesData::Probabilistic(p) => p.application_data = application_data,
            TimeSeriesData::Scenarios(s) => s.application_data = application_data,
            TimeSeriesData::PersistentTimeSeries(p) => p.application_data = application_data,
        }
    }

    /// Set the descriptive attributes in place. Used on the read path to fill
    /// a reconstructed series in from its catalog row.
    pub fn set_descriptors(&mut self, descriptors: Descriptors) {
        let Descriptors {
            element_type,
            units,
            quantity_kind,
            unit_system,
            time_reference,
            component_field,
            application_data,
        } = descriptors;
        self.set_element_type(element_type);
        self.set_units(units);
        self.set_quantity_kind(quantity_kind);
        self.set_unit_system(unit_system);
        self.set_time_reference(time_reference);
        self.set_component_field(component_field);
        self.set_application_data(application_data);
    }

    pub fn as_single(&self) -> Option<&SingleTimeSeries> {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_non_sequential(&self) -> Option<&NonSequentialTimeSeries> {
        match self {
            TimeSeriesData::NonSequentialTimeSeries(s) => Some(s),
            _ => None,
        }
    }

    /// Access the inner [`Deterministic`] forecast, if present.
    ///
    /// Also returns `Some` for a `DeterministicSingleTimeSeries` read, since
    /// that is synthesized into `Deterministic` by the store.
    pub fn as_deterministic(&self) -> Option<&Deterministic> {
        match self {
            TimeSeriesData::Deterministic(d) => Some(d),
            _ => None,
        }
    }

    /// Access the inner [`Probabilistic`] forecast, if present.
    pub fn as_probabilistic(&self) -> Option<&Probabilistic> {
        match self {
            TimeSeriesData::Probabilistic(p) => Some(p),
            _ => None,
        }
    }

    /// Access the inner [`Scenarios`] forecast, if present.
    pub fn as_scenarios(&self) -> Option<&Scenarios> {
        match self {
            TimeSeriesData::Scenarios(s) => Some(s),
            _ => None,
        }
    }

    /// Access the inner [`PersistentTimeSeries`], if present.
    pub fn as_persistent(&self) -> Option<&PersistentTimeSeries> {
        match self {
            TimeSeriesData::PersistentTimeSeries(p) => Some(p),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::array::Dtype;
    use chrono::{Duration, TimeZone};

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
    }

    fn arr(shape: Vec<usize>) -> TypedArray {
        let n: usize = shape.iter().product();
        let values: Vec<f64> = (0..n).map(|i| i as f64).collect();
        TypedArray::from_f64(shape, &values)
    }

    // ---- TimeSeriesType round trip ----------------------------------------

    #[test]
    fn time_series_type_str_round_trip_is_exhaustive() {
        for t in ALL_TYPES {
            assert_eq!(TimeSeriesType::parse(t.as_str()), Some(t));
            assert_eq!(t.as_str().parse::<TimeSeriesType>(), Ok(t));
        }
        assert_eq!(TimeSeriesType::parse("NotAType"), None);
        // Case sensitivity is part of the contract: the catalog stores exactly
        // `as_str()`, so a lower-cased spelling must not silently match.
        assert_eq!(TimeSeriesType::parse("singletimeseries"), None);
        assert!("".parse::<TimeSeriesType>().is_err());
    }

    #[test]
    fn deterministic_request_accepts_both_concrete_storage_forms() {
        // The whole point of the rule: a caller asking for `Deterministic` gets
        // a transformed DST without naming it.
        let det = TimeSeriesType::Deterministic;
        assert!(det.accepts(TimeSeriesType::Deterministic));
        assert!(det.accepts(TimeSeriesType::DeterministicSingleTimeSeries));
        assert!(!det.accepts(TimeSeriesType::Probabilistic));
        assert!(!det.accepts(TimeSeriesType::SingleTimeSeries));
    }

    #[test]
    fn dst_request_narrows_to_dst_alone() {
        // The inspection direction still discriminates.
        let dst = TimeSeriesType::DeterministicSingleTimeSeries;
        assert!(dst.accepts(TimeSeriesType::DeterministicSingleTimeSeries));
        assert!(!dst.accepts(TimeSeriesType::Deterministic));
    }

    #[test]
    fn every_other_type_accepts_only_itself() {
        for t in [
            TimeSeriesType::SingleTimeSeries,
            TimeSeriesType::NonSequentialTimeSeries,
            TimeSeriesType::Probabilistic,
            TimeSeriesType::Scenarios,
        ] {
            for stored in [
                TimeSeriesType::SingleTimeSeries,
                TimeSeriesType::NonSequentialTimeSeries,
                TimeSeriesType::Deterministic,
                TimeSeriesType::DeterministicSingleTimeSeries,
                TimeSeriesType::Probabilistic,
                TimeSeriesType::Scenarios,
            ] {
                assert_eq!(t.accepts(stored), t == stored, "{t:?} vs {stored:?}");
            }
        }
    }

    const ALL_TYPES: [TimeSeriesType; 7] = [
        TimeSeriesType::SingleTimeSeries,
        TimeSeriesType::NonSequentialTimeSeries,
        TimeSeriesType::Deterministic,
        TimeSeriesType::DeterministicSingleTimeSeries,
        TimeSeriesType::Probabilistic,
        TimeSeriesType::Scenarios,
        TimeSeriesType::PersistentTimeSeries,
    ];

    #[test]
    fn storage_codes_round_trip_and_are_unique() {
        // The codes are an on-disk contract: a silent renumbering would
        // misread every existing catalog row.
        let mut seen = Vec::new();
        for t in ALL_TYPES {
            assert_eq!(TimeSeriesType::from_code(t.code()), Some(t), "{t:?}");
            assert!(!seen.contains(&t.code()), "duplicate code for {t:?}");
            seen.push(t.code());
        }
        assert_eq!(TimeSeriesType::from_code(7), None);
        assert_eq!(TimeSeriesType::from_code(-1), None);
    }

    #[test]
    fn deterministic_codes_are_adjacent() {
        // Load-bearing: `code_span` relies on it to emit a contiguous BETWEEN
        // instead of a two-value IN. Reordering the enum breaks this loudly
        // rather than silently degrading the query plan.
        assert_eq!(
            TimeSeriesType::DeterministicSingleTimeSeries.code(),
            TimeSeriesType::Deterministic.code() + 1
        );
    }

    #[test]
    fn code_span_widens_only_deterministic() {
        let (lo, hi) = TimeSeriesType::Deterministic.code_span();
        assert_eq!(
            (lo, hi),
            (
                TimeSeriesType::Deterministic.code(),
                TimeSeriesType::DeterministicSingleTimeSeries.code()
            )
        );
        for t in ALL_TYPES {
            if t == TimeSeriesType::Deterministic {
                continue;
            }
            assert_eq!(t.code_span(), (t.code(), t.code()), "{t:?} must not widen");
        }
    }

    #[test]
    fn code_groups_partition_cleanly() {
        // The summary queries select "all static" / "all forecast" with one
        // `IN` list each, which is correct exactly while the two lists are
        // disjoint and together cover every type. They used to be contiguous
        // ranges too; appending `PersistentTimeSeries` as code 6 ended that,
        // and the partition is the property that actually mattered.
        let statics = TimeSeriesType::static_codes();
        let forecasts = TimeSeriesType::forecast_codes();
        for t in ALL_TYPES {
            let c = t.code();
            let in_static = statics.contains(&c);
            let in_forecast = forecasts.contains(&c);
            assert!(
                in_static ^ in_forecast,
                "{t:?} must be in exactly one group"
            );
            assert_eq!(in_forecast, t.is_forecast(), "{t:?}");
        }
        // ...and neither list names a code no type claims.
        let known: Vec<i64> = ALL_TYPES.iter().map(|t| t.code()).collect();
        for c in statics.iter().chain(forecasts) {
            assert!(known.contains(c), "code {c} belongs to no TimeSeriesType");
        }
        assert_eq!(statics.len() + forecasts.len(), ALL_TYPES.len());
    }

    #[test]
    fn the_persistent_type_is_static_and_appended() {
        let p = TimeSeriesType::PersistentTimeSeries;
        // Appended, not inserted: the codes are an on-disk contract, and
        // `code_span`'s Deterministic/DST adjacency depends on it.
        assert_eq!(p.code(), 6);
        assert_eq!(
            TimeSeriesType::Deterministic.code() + 1,
            TimeSeriesType::DeterministicSingleTimeSeries.code()
        );
        assert!(!p.is_forecast());
        assert_eq!(p.leading_dims(), 1);
        assert_eq!(p.code_span(), (6, 6));
        assert!(p.accepts(p));
        assert!(!p.accepts(TimeSeriesType::NonSequentialTimeSeries));
        assert!(!TimeSeriesType::NonSequentialTimeSeries.accepts(p));
    }

    /// The public fields make an inconsistent value constructible, and
    /// `project_onto` is fallible, so it has to *say so* rather than abort.
    ///
    /// `new` checks that the breakpoints and the array agree, and the store
    /// re-checks on write -- but nothing stops a caller pushing a breakpoint
    /// onto a value in hand and projecting it before it goes anywhere near
    /// either. The extra breakpoint is in force at the instant asked for, so
    /// the gather reaches for a row the array does not have.
    #[test]
    fn projecting_a_series_whose_fields_disagree_errors_rather_than_panics() {
        let mut p = PersistentTimeSeries::new(
            vec![t0(), t0() + Duration::days(31)],
            TypedArray::from_f64(vec![2], &[3.5, 4.25]),
            "gas_price",
        )
        .unwrap();

        // A third breakpoint with no third value behind it.
        p.timestamps.push(t0() + Duration::days(60));

        // Everything before the new breakpoint still resolves normally.
        assert!(p.project_onto(&[t0() + Duration::days(40)]).is_ok());

        let err = p
            .project_onto(&[t0() + Duration::days(70)])
            .expect_err("a breakpoint with no row behind it must not gather");
        assert!(
            err.contains("out of step"),
            "the message must name the cause, not just fail: {err}"
        );
    }

    #[test]
    fn code_span_agrees_with_accepts_over_every_pair() {
        // `accepts` is derived from `code_span`, so this pins the derivation
        // against the behavior the bindings document.
        for t in ALL_TYPES {
            for stored in ALL_TYPES {
                let (lo, hi) = t.code_span();
                let in_span = (lo..=hi).contains(&stored.code());
                assert_eq!(in_span, t.accepts(stored), "{t:?} vs {stored:?}");
            }
        }
        assert!(
            TimeSeriesType::Deterministic.accepts(TimeSeriesType::DeterministicSingleTimeSeries)
        );
        assert!(
            !TimeSeriesType::DeterministicSingleTimeSeries.accepts(TimeSeriesType::Deterministic)
        );
    }

    // ---- SingleTimeSeries -------------------------------------------------

    #[test]
    fn single_time_series_length_comes_from_the_leading_dim() {
        let s = SingleTimeSeries::new(t0(), Duration::hours(1), arr(vec![4, 3]), "load");
        assert_eq!(s.length, 4);
        assert_eq!(s.data.element_shape(), &[3]);
        assert_eq!(s.resolution, Period::Fixed(Duration::hours(1)));

        // A rank-0 array holds exactly one element (the empty shape's product
        // is 1) but has no leading dim, so `length()` reports 0.
        let scalar = TypedArray::from_f64(vec![], &[1.0]);
        assert_eq!(scalar.num_elements(), 1);
        assert_eq!(scalar.bytes.len(), Dtype::F64.size());
        let s = SingleTimeSeries::new(t0(), Duration::hours(1), scalar, "rank0");
        assert_eq!(s.length, 0);
    }

    // ---- NonSequentialTimeSeries -----------------------------------------

    #[test]
    fn non_sequential_single_point_is_accepted() {
        // A one-timestamp series has no adjacent pair, so the strictly-
        // increasing check trivially holds.
        let s = NonSequentialTimeSeries::new(vec![t0()], arr(vec![1]), "one").unwrap();
        assert_eq!(s.length, 1);
        assert_eq!(s.timestamps, vec![t0()]);
    }

    #[test]
    fn non_sequential_empty_is_accepted() {
        // PIN: zero timestamps + a zero-length array is currently accepted.
        let empty = TypedArray::from_f64(vec![0], &[]);
        let s = NonSequentialTimeSeries::new(vec![], empty, "none").unwrap();
        assert_eq!(s.length, 0);
    }

    #[test]
    fn non_sequential_rejects_count_mismatch_and_non_increasing() {
        let err = NonSequentialTimeSeries::new(vec![t0()], arr(vec![3]), "x").unwrap_err();
        assert!(err.contains("does not match data length"), "{err}");

        // Equal adjacent timestamps are rejected (strictly increasing).
        let err = NonSequentialTimeSeries::new(vec![t0(), t0()], arr(vec![2]), "x").unwrap_err();
        assert!(err.contains("strictly increasing"), "{err}");

        // Decreasing is rejected.
        let err =
            NonSequentialTimeSeries::new(vec![t0() + Duration::hours(1), t0()], arr(vec![2]), "x")
                .unwrap_err();
        assert!(err.contains("strictly increasing"), "{err}");
    }

    // ---- compute_h / validate_positive_periods ----------------------------

    #[test]
    fn compute_h_requires_exact_positive_division() {
        let h = Period::Fixed(Duration::hours(6));
        let r = Period::Fixed(Duration::hours(2));
        assert_eq!(compute_h(h, r).unwrap(), 3);

        // Non-divisible: 5h horizon over a 2h resolution.
        let err = compute_h(Period::Fixed(Duration::hours(5)), r).unwrap_err();
        assert!(err.contains("not a positive integer multiple"), "{err}");

        // Horizon shorter than resolution divides to 0, which is rejected.
        let err = compute_h(Period::Fixed(Duration::hours(1)), r).unwrap_err();
        assert!(err.contains("not a positive integer multiple"), "{err}");

        // Mixing kinds is rejected rather than coerced.
        let err = compute_h(Period::Months(3), r).unwrap_err();
        assert!(err.contains("different kinds"), "{err}");

        // Calendar months divide exactly.
        assert_eq!(compute_h(Period::Months(12), Period::Months(3)).unwrap(), 4);
        let err = compute_h(Period::Months(5), Period::Months(2)).unwrap_err();
        assert!(err.contains("not a positive integer multiple"), "{err}");
    }

    #[test]
    fn validate_forecast_periods_rejects_zero_and_negative() {
        let ok = Period::Fixed(Duration::hours(1));
        let zero = Period::Fixed(Duration::zero());
        let neg = Period::Fixed(Duration::hours(-1));

        assert!(validate_forecast_periods(ok, ok, ok, 4).is_ok());
        for (r, h, which) in [
            (zero, ok, "resolution"),
            (neg, ok, "resolution"),
            (ok, zero, "horizon"),
            (ok, neg, "horizon"),
        ] {
            let err = validate_forecast_periods(r, h, ok, 4).unwrap_err();
            assert_eq!(err, format!("{which} must be strictly positive"));
        }
        for bad_interval in [zero, neg] {
            let err = validate_forecast_periods(ok, ok, bad_interval, 4).unwrap_err();
            assert!(err.contains("interval must be strictly positive"), "{err}");
        }
        // A forecast with at most one window may carry a zero interval (there
        // is no second window to step to) — but never a negative one. Zero
        // windows is the empty selection a zero-width `time_range` produces.
        assert!(validate_forecast_periods(ok, ok, zero, 1).is_ok());
        assert!(validate_forecast_periods(ok, ok, zero, 0).is_ok());
        assert!(validate_forecast_periods(ok, ok, neg, 1).is_err());
        assert!(validate_forecast_periods(ok, ok, neg, 0).is_err());
        // Calendar months follow the same rule.
        assert!(
            validate_forecast_periods(Period::Months(0), Period::Months(1), Period::Months(1), 4)
                .is_err()
        );
        assert!(
            validate_forecast_periods(Period::Months(-1), Period::Months(1), Period::Months(1), 4)
                .is_err()
        );
    }

    // ---- Deterministic::new ----------------------------------------------

    #[test]
    fn deterministic_accepts_the_canonical_shape() {
        // H = 3 (6h / 2h), count = 4, element shape [2].
        let d = Deterministic::new(
            t0(),
            Duration::hours(2),
            Duration::hours(6),
            Duration::hours(6),
            4,
            arr(vec![3, 4, 2]),
            "f",
        )
        .unwrap();
        assert_eq!(d.count, 4);
        assert_eq!(d.data.shape, vec![3, 4, 2]);
    }

    #[test]
    fn deterministic_rejects_fewer_than_two_dims() {
        for shape in [vec![], vec![6]] {
            let err = Deterministic::new(
                t0(),
                Duration::hours(1),
                Duration::hours(2),
                Duration::hours(1),
                3,
                arr(shape.clone()),
                "f",
            )
            .unwrap_err();
            assert!(
                err.contains("must have at least 2 dims"),
                "shape {shape:?}: {err}"
            );
        }
    }

    #[test]
    fn deterministic_rejects_shape_mismatch() {
        // H = 2, count = 3 expected -> [2, 3]. Wrong H:
        let err = Deterministic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            3,
            arr(vec![5, 3]),
            "f",
        )
        .unwrap_err();
        assert!(err.contains("expected shape [2, 3]"), "{err}");

        // Wrong count:
        let err = Deterministic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            3,
            arr(vec![2, 7]),
            "f",
        )
        .unwrap_err();
        assert!(err.contains("expected shape [2, 3]"), "{err}");
    }

    #[test]
    fn deterministic_rejects_non_divisible_horizon() {
        let err = Deterministic::new(
            t0(),
            Duration::hours(2),
            Duration::hours(5),
            Duration::hours(2),
            3,
            arr(vec![2, 3]),
            "f",
        )
        .unwrap_err();
        assert!(err.contains("not a positive integer multiple"), "{err}");
    }

    #[test]
    fn deterministic_rejects_non_positive_periods() {
        let err = Deterministic::new(
            t0(),
            Duration::zero(),
            Duration::hours(2),
            Duration::hours(1),
            3,
            arr(vec![2, 3]),
            "f",
        )
        .unwrap_err();
        assert_eq!(err, "resolution must be strictly positive");

        let err = Deterministic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(-1),
            3,
            arr(vec![2, 3]),
            "f",
        )
        .unwrap_err();
        assert!(err.contains("interval must be strictly positive"), "{err}");
    }

    #[test]
    fn deterministic_accepts_zero_interval_for_a_single_window() {
        // count == 1: there is no second window to step to, so a zero interval
        // is the natural encoding (no interval-equals-horizon sentinel needed).
        let d = Deterministic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::zero(),
            1,
            arr(vec![2, 1]),
            "f",
        )
        .unwrap();
        assert!(d.interval.is_zero());

        // count > 1 still requires a positive interval.
        let err = Deterministic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::zero(),
            3,
            arr(vec![2, 3]),
            "f",
        )
        .unwrap_err();
        assert!(err.contains("interval must be strictly positive"), "{err}");
    }

    // ---- Probabilistic::new ----------------------------------------------

    #[test]
    fn probabilistic_accepts_the_canonical_shape() {
        // P = 2, H = 2, count = 3 -> [2, 2, 3].
        let p = Probabilistic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            3,
            vec![0.1, 0.9],
            arr(vec![2, 2, 3]),
            "f",
        )
        .unwrap();
        assert_eq!(p.percentiles, vec![0.1, 0.9]);
    }

    #[test]
    fn probabilistic_rejects_empty_percentiles() {
        let err = Probabilistic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            3,
            vec![],
            arr(vec![0, 2, 3]),
            "f",
        )
        .unwrap_err();
        assert!(err.contains("percentiles must be non-empty"), "{err}");
    }

    #[test]
    fn probabilistic_rejects_non_increasing_percentiles() {
        for pcts in [
            vec![0.9, 0.1],      // decreasing
            vec![0.5, 0.5],      // equal
            vec![0.1, 0.5, 0.4], // dip at the tail
        ] {
            let err = Probabilistic::new(
                t0(),
                Duration::hours(1),
                Duration::hours(2),
                Duration::hours(1),
                3,
                pcts.clone(),
                arr(vec![pcts.len(), 2, 3]),
                "f",
            )
            .unwrap_err();
            assert!(
                err.contains("strictly increasing"),
                "{pcts:?} should be rejected: {err}"
            );
        }
    }

    #[test]
    fn probabilistic_rejects_percentile_length_mismatch() {
        // Two percentiles declared, three planes of data.
        let err = Probabilistic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            3,
            vec![0.1, 0.9],
            arr(vec![3, 2, 3]),
            "f",
        )
        .unwrap_err();
        assert!(err.contains("expected shape [2, 2, 3]"), "{err}");
    }

    #[test]
    fn probabilistic_rejects_fewer_than_three_dims() {
        let err = Probabilistic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            3,
            vec![0.5],
            arr(vec![2, 3]),
            "f",
        )
        .unwrap_err();
        assert!(err.contains("must have at least 3 dims"), "{err}");
    }

    // ---- Scenarios::new --------------------------------------------------

    #[test]
    fn scenarios_accepts_the_canonical_shape() {
        // S = 4, H = 2, count = 3 -> [4, 2, 3].
        let s = Scenarios::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            3,
            4,
            arr(vec![4, 2, 3]),
            "f",
        )
        .unwrap();
        assert_eq!(s.scenario_count, 4);
    }

    #[test]
    fn scenarios_rejects_scenario_count_mismatch() {
        let err = Scenarios::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            3,
            4,
            arr(vec![2, 2, 3]),
            "f",
        )
        .unwrap_err();
        assert!(err.contains("expected shape [4, 2, 3]"), "{err}");
        assert!(err.contains("scenario_count=4"), "{err}");
    }

    #[test]
    fn scenarios_rejects_too_few_dims() {
        // A rank-2 array can never match [S, H, count]; the elem-dims branch
        // treats `shape.len() <= 3` as "no element dims" and the comparison
        // fails on rank.
        let err = Scenarios::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            3,
            1,
            arr(vec![2, 3]),
            "f",
        )
        .unwrap_err();
        assert!(err.contains("expected shape [1, 2, 3]"), "{err}");
    }

    // ---- TimeSeriesData accessors ----------------------------------------

    #[test]
    fn time_series_data_accessors_are_variant_exact() {
        let single = TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
            t0(),
            Duration::hours(1),
            arr(vec![2]),
            "s",
        ));
        let det = TimeSeriesData::Deterministic(
            Deterministic::new(
                t0(),
                Duration::hours(1),
                Duration::hours(2),
                Duration::hours(1),
                3,
                arr(vec![2, 3]),
                "d",
            )
            .unwrap(),
        );

        assert_eq!(single.time_series_type(), TimeSeriesType::SingleTimeSeries);
        assert_eq!(single.name(), "s");
        assert!(single.as_single().is_some());
        assert!(single.as_deterministic().is_none());
        assert!(single.as_non_sequential().is_none());
        assert!(single.as_probabilistic().is_none());
        assert!(single.as_scenarios().is_none());

        assert_eq!(det.time_series_type(), TimeSeriesType::Deterministic);
        assert_eq!(det.name(), "d");
        assert!(det.as_deterministic().is_some());
        assert!(det.as_single().is_none());
    }

    // ---- from_values / decoded_values -------------------------------------

    fn curves() -> DecodedValues {
        DecodedValues::PiecewiseLinear(vec![
            vec![
                crate::codec::XyPoint { x: 0.0, y: 1.0 },
                crate::codec::XyPoint { x: 1.0, y: 3.0 },
            ],
            vec![crate::codec::XyPoint { x: 0.0, y: 2.0 }],
            vec![],
            vec![crate::codec::XyPoint { x: 2.0, y: 9.5 }],
        ])
    }

    fn hour() -> Period {
        Period::Fixed(Duration::hours(1))
    }

    /// The invariant the constructors exist for: whatever leading dims a type
    /// stacks in front of the element shape, the values come back out of
    /// `decoded_values` exactly as they went in, with an element type nobody
    /// had to declare.
    #[test]
    fn from_values_round_trips_through_decoded_values_for_every_type() {
        let ts = [
            TimeSeriesData::SingleTimeSeries(
                SingleTimeSeries::from_values(t0(), hour(), &curves(), "s").unwrap(),
            ),
            TimeSeriesData::NonSequentialTimeSeries(
                NonSequentialTimeSeries::from_values(
                    (0..4).map(|i| t0() + Duration::hours(i * 3)).collect(),
                    &curves(),
                    "n",
                )
                .unwrap(),
            ),
            // [H = 2, count = 2]
            TimeSeriesData::Deterministic(
                Deterministic::from_values(
                    t0(),
                    hour(),
                    Period::Fixed(Duration::hours(2)),
                    hour(),
                    2,
                    &curves(),
                    "d",
                )
                .unwrap(),
            ),
            // [P = 2, H = 2, count = 1]
            TimeSeriesData::Probabilistic(
                Probabilistic::from_values(
                    t0(),
                    hour(),
                    Period::Fixed(Duration::hours(2)),
                    hour(),
                    1,
                    vec![0.1, 0.9],
                    &curves(),
                    "p",
                )
                .unwrap(),
            ),
            // [S = 2, H = 2, count = 1]
            TimeSeriesData::Scenarios(
                Scenarios::from_values(
                    t0(),
                    hour(),
                    Period::Fixed(Duration::hours(2)),
                    hour(),
                    1,
                    2,
                    &curves(),
                    "sc",
                )
                .unwrap(),
            ),
        ];
        for data in ts {
            let what = data.time_series_type().as_str();
            assert_eq!(
                data.element_type(),
                ElementType::PiecewiseLinear,
                "{what} did not derive its element type"
            );
            assert_eq!(
                data.decoded_values().unwrap(),
                curves(),
                "{what} did not round-trip"
            );
        }
    }

    #[test]
    fn from_values_rejects_values_that_do_not_fill_the_leading_dims() {
        // 4 curves cannot fill [H = 2, count = 3].
        let err = Deterministic::from_values(
            t0(),
            hour(),
            Period::Fixed(Duration::hours(2)),
            hour(),
            3,
            &curves(),
            "d",
        )
        .unwrap_err();
        assert!(err.contains("4 decoded timesteps"), "{err}");

        // The timestamp count is still checked against the values.
        let err = NonSequentialTimeSeries::from_values(vec![t0()], &curves(), "n").unwrap_err();
        assert!(err.contains("does not match data length"), "{err}");
    }

    #[test]
    fn from_values_refuses_raw_and_names_the_alternative() {
        let err =
            SingleTimeSeries::from_values(t0(), hour(), &DecodedValues::Raw, "s").unwrap_err();
        assert!(err.contains("carries no values"), "{err}");
        assert!(err.contains("TypedArray"), "{err}");
    }

    /// A scalar series has no logical structure to decode, so the array itself
    /// stays the answer — the case a caller must not mistake for "no values".
    #[test]
    fn decoded_values_is_raw_for_a_plain_numeric_series() {
        let single = SingleTimeSeries::new(t0(), hour(), arr(vec![4]), "s");
        assert_eq!(single.element_type, ElementType::Scalar(Dtype::F64));
        let data = TimeSeriesData::SingleTimeSeries(single);
        assert_eq!(data.decoded_values().unwrap(), DecodedValues::Raw);
    }
}
