use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::array::{Element, TypedArray};
use super::element_type::ElementType;
use super::metadata::UnitSystem;
use super::period::Period;
use super::time_reference::TimeReference;
use crate::codec::{self, DecodedValues};
use crate::reader::timestamp_on_grid;

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
    /// A *list*, not a range. The static types are codes 0-1 and 6, around the
    /// forecast types' 2-5: `PersistentTimeSeries` is appended rather than
    /// inserted, because the codes are an on-disk contract and renumbering is
    /// not available. The static group is therefore non-contiguous and its
    /// consumers render `WHERE time_series_type IN (…)`, which `idx_ts_type`
    /// serves as well as it would a range.
    ///
    /// `code_groups_partition_cleanly` asserts that this and
    /// [`Self::forecast_codes`] are disjoint and together cover every variant.
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

/// The seven descriptor builders every series struct shares. `$timeline` names
/// what the series' instants are called in the docs; `$field` is an example
/// component field.
macro_rules! descriptor_builders {
    ($timeline:literal, $field:literal) => {
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

        #[doc = concat!("Declare how this series' ", $timeline, " were spelled. Validated on commit")]
        /// (a zone name's *shape* only — see [`TimeReference::validate`]).
        pub fn with_time_reference(mut self, time_reference: TimeReference) -> Self {
            self.time_reference = Some(time_reference);
            self
        }

        /// Name the component field these values vary over time
        #[doc = concat!("(e.g. `\"", $field, "\"`).")]
        pub fn with_component_field(mut self, component_field: impl Into<String>) -> Self {
            self.component_field = Some(component_field.into());
            self
        }

        /// Set the opaque application payload carried through to the metadata row.
        pub fn with_application_data(mut self, application_data: impl Into<String>) -> Self {
            self.application_data = Some(application_data.into());
            self
        }
    };
}

/// One `match` over every [`TimeSeriesData`] variant, binding the inner series
/// to `$s` and evaluating `$body` in each arm.
macro_rules! each_variant {
    ($data:expr, $s:ident => $body:expr) => {
        match $data {
            TimeSeriesData::SingleTimeSeries($s) => $body,
            TimeSeriesData::NonSequentialTimeSeries($s) => $body,
            TimeSeriesData::Deterministic($s) => $body,
            TimeSeriesData::Probabilistic($s) => $body,
            TimeSeriesData::Scenarios($s) => $body,
            TimeSeriesData::PersistentTimeSeries($s) => $body,
        }
    };
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

    /// Build from the timeline a caller actually holds, inferring the
    /// resolution and **proving** the instants lie on it.
    ///
    /// [`Self::new`] takes `initial_timestamp` + `resolution` and has no way to
    /// check the claim — the vector it describes is never supplied, so a caller
    /// whose values sit on a drifting timeline gets a grid that silently
    /// disagrees with their data. This constructor closes that gap by taking the
    /// vector: it either fits a [`Period`] exactly, or it is refused with the
    /// index that broke the pattern and a pointer at
    /// [`NonSequentialTimeSeries`].
    ///
    /// **This is how a local-clock timeline reaches the store.** The store has
    /// no time-zone database and never runs local → instant; the caller
    /// materializes their local grid in their own date library — where the
    /// policy for a nonexistent or ambiguous wall clock belongs — and hands over
    /// the instants. An hourly local grid in a DST zone *is* a uniform instant
    /// grid, so it compacts here; a daily or monthly local grid is not, so it is
    /// refused and stored explicitly instead. Either way the store records the
    /// timeline the caller has rather than one a resolution implies.
    ///
    /// Timestamps must be strictly increasing and match the array's first axis.
    /// See [`Period::infer`] for which period wins when more than one fits.
    ///
    /// ```
    /// # use infrastore_core::{SingleTimeSeries, TypedArray, Period};
    /// # use chrono::{TimeZone, Utc};
    /// let month_ends = [
    ///     Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap(),
    ///     Utc.with_ymd_and_hms(2024, 2, 29, 0, 0, 0).unwrap(),
    ///     Utc.with_ymd_and_hms(2024, 3, 31, 0, 0, 0).unwrap(),
    /// ];
    /// let series = SingleTimeSeries::from_timestamps(
    ///     &month_ends,
    ///     TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]),
    ///     "monthly",
    /// )?;
    /// assert_eq!(series.resolution, Period::Months(1));
    /// assert_eq!(series.initial_timestamp, month_ends[0]);
    /// # Ok::<(), String>(())
    /// ```
    pub fn from_timestamps(
        timestamps: &[DateTime<Utc>],
        data: TypedArray,
        name: impl Into<String>,
    ) -> std::result::Result<Self, String> {
        let length = data.length();
        if timestamps.len() != length {
            return Err(format!(
                "SingleTimeSeries::from_timestamps: {} timestamps for {length} value(s); \
                 the vector must have one entry per time step",
                timestamps.len()
            ));
        }
        let resolution = Period::infer(timestamps)?;
        Ok(Self::new(timestamps[0], resolution, data, name))
    }

    descriptor_builders!("timestamps", "max_active_power");

    /// The timestamp at 0-based `index` — `initial_timestamp + index ·
    /// resolution`, calendar-aware for a [`Period::Months`] grid. Errors if
    /// `index >= length` or the date arithmetic overflows.
    ///
    /// The instant is UTC, like every instant the core holds; how it was
    /// *spelled* is `time_reference`, which this does not apply.
    pub fn timestamp_at(&self, index: usize) -> crate::Result<DateTime<Utc>> {
        timestamp_on_grid(
            self.initial_timestamp,
            self.resolution,
            self.length,
            index,
            "grid",
        )
    }

    /// Materialize the whole grid, `[0, length)` in order — the regular
    /// counterpart of [`NonSequentialTimeSeries::timestamps`], which is a
    /// stored vector rather than a computed one.
    ///
    /// This is the only correct way to reconstruct the timeline: a
    /// [`Period::Months`] resolution steps on the calendar, so a caller
    /// multiplying a fixed span by the index gets a month grid wrong.
    ///
    /// ```
    /// # use infrastore_core::{SingleTimeSeries, TypedArray, Period};
    /// # use chrono::{TimeZone, Utc};
    /// let series = SingleTimeSeries::new(
    ///     Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap(),
    ///     Period::Months(1),
    ///     TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]),
    ///     "monthly",
    /// );
    /// let grid: Vec<_> = series.timestamps().collect();
    /// assert_eq!(grid[1], Utc.with_ymd_and_hms(2024, 2, 29, 0, 0, 0).unwrap());
    /// ```
    pub fn timestamps(&self) -> impl Iterator<Item = DateTime<Utc>> + '_ {
        (0..self.length).map(move |k| {
            self.timestamp_at(k)
                .expect("timestamp on the series grid is representable")
        })
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

    descriptor_builders!("timestamps", "max_active_power");
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
/// breakpoint was never declared, and inventing one would be a guess. Read a
/// value with [`Self::value_at`] (or [`Self::row_at`] for a non-scalar step);
/// [`Self::index_at`] and [`Self::breakpoint_at`] locate the row it came from.
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
    /// Construct from per-breakpoint logical values, encoding them into the
    /// flat array the store holds and declaring the element type they imply.
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
    /// It also returns `Err` when the breakpoint count does not match the
    /// number of values, or the breakpoints are not strictly increasing — the
    /// same checks [`Self::new`] makes.
    ///
    /// One entry per breakpoint, so the step function holds entry `i` from
    /// `timestamps[i]` until the next breakpoint.
    pub fn from_values(
        timestamps: Vec<DateTime<Utc>>,
        values: &DecodedValues,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        let (data, element_type) = encode_with_type(values, &[values.len()])?;
        Ok(Self::new(timestamps, data, name)?.with_element_type(element_type))
    }

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
    /// governing `at` — the greatest breakpoint `<= at`, whose value is carried
    /// forward to `at`.
    ///
    /// `Err` if `at` is strictly before the first breakpoint, where the step
    /// function is undefined, or if the series is empty. This is the single
    /// source of truth for the lookup: [`Self::value_at`], [`Self::row_at`] and
    /// [`Self::breakpoint_at`] all go through it, and nothing else should
    /// re-derive it.
    ///
    /// Note the asymmetry with [`Self::value_at`]: a step function has a genuine
    /// value *at* `at`, but the row it comes from generally sits earlier, which
    /// is why only this one is spelled as a lookup.
    pub fn index_at(&self, at: DateTime<Utc>) -> Result<usize, String> {
        crate::timestamps::index_at(&self.timestamps, at).ok_or_else(|| {
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

    /// The breakpoint governing `at` — the greatest one `<= at`, i.e. the
    /// instant from which the value at `at` has been in force.
    ///
    /// Equal to `at` itself exactly when `at` is a stored breakpoint. Errors
    /// under the same conditions as [`Self::index_at`].
    pub fn breakpoint_at(&self, at: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
        Ok(self.timestamps[self.index_at(at)?])
    }

    /// The value in force at `at`, for a series of scalars.
    ///
    /// The step function is total on `[first breakpoint, +∞)`, so this is the
    /// series' value at `at` in the ordinary sense — not an approximation of one:
    /// between breakpoints the previous value is carried forward, and past the
    /// last breakpoint the last value holds indefinitely. Only an `at` strictly
    /// before the first breakpoint is an error, because no value was ever
    /// declared there.
    ///
    /// `T` must match the array's dtype. A series whose per-step element is not a
    /// scalar (a non-empty [`TypedArray::element_shape`], e.g. a piecewise curve
    /// or a vector per step) is an error here — use [`Self::row_at`], which
    /// returns the whole per-step slice for any shape.
    pub fn value_at<T: Element>(&self, at: DateTime<Utc>) -> Result<T, String> {
        let element_shape = self.data.element_shape();
        if !element_shape.is_empty() {
            return Err(format!(
                "PersistentTimeSeries '{}' holds {element_shape:?} per step, not a scalar; \
                 use row_at to read the whole step",
                self.name
            ));
        }
        self.data.element_at::<T>(self.index_at(at)?)
    }

    /// The whole per-step slice in force at `at`, as a [`TypedArray`] of shape
    /// [`TypedArray::element_shape`] — `[]` for a scalar series.
    ///
    /// The shape-generic form of [`Self::value_at`], with the same semantics and
    /// the same single error case (an `at` before the first breakpoint).
    pub fn row_at(&self, at: DateTime<Utc>) -> Result<TypedArray, String> {
        self.data.step(self.index_at(at)?)
    }
}

impl PersistentTimeSeries {
    descriptor_builders!("breakpoints", "fuel_cost");
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

    /// Number of steps in one window — the first axis of `data`, which
    /// [`Self::validate`] holds equal to `horizon / resolution`.
    pub fn horizon_count(&self) -> usize {
        self.data.shape.first().copied().unwrap_or(0)
    }

    /// The issue time of window `index`: `initial_timestamp + index ·
    /// interval`, calendar-aware for a [`Period::Months`] interval. Errors if
    /// `index >= count` or the arithmetic overflows.
    pub fn window_start(&self, index: usize) -> crate::Result<DateTime<Utc>> {
        timestamp_on_grid(
            self.initial_timestamp,
            self.interval,
            self.count,
            index,
            "forecast window",
        )
    }

    /// Every timestamp inside window `index` — [`Self::horizon_count`] of them,
    /// stepping by `resolution` from the window's issue time.
    ///
    /// The two grids are distinct and both are needed to place a value: windows
    /// step by `interval`, and the steps inside one step by `resolution`. They
    /// are equal only for a forecast whose windows abut without overlapping,
    /// which is not the common case — a day-ahead forecast reissued hourly
    /// overlaps 23 of every 24 steps.
    ///
    /// ```
    /// # use infrastore_core::{Deterministic, TypedArray, Period};
    /// # use chrono::{TimeZone, Utc, Duration};
    /// let forecast = Deterministic::new(
    ///     Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
    ///     Period::Fixed(Duration::hours(1)),   // resolution
    ///     Period::Fixed(Duration::hours(2)),   // horizon: H = 2
    ///     Period::Fixed(Duration::hours(1)),   // interval
    ///     3,
    ///     TypedArray::from_f64(vec![2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
    ///     "day_ahead",
    /// )?;
    /// assert_eq!(
    ///     forecast.window_timestamps(1)?,
    ///     vec![
    ///         Utc.with_ymd_and_hms(2024, 1, 1, 1, 0, 0).unwrap(),
    ///         Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap(),
    ///     ],
    /// );
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn window_timestamps(&self, index: usize) -> crate::Result<Vec<DateTime<Utc>>> {
        let start = self.window_start(index)?;
        let steps = self.horizon_count();
        (0..steps)
            .map(|h| timestamp_on_grid(start, self.resolution, steps, h, "forecast horizon"))
            .collect()
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

    descriptor_builders!("timestamps", "max_active_power");
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

    descriptor_builders!("timestamps", "max_active_power");
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
    // all has none either. The zero-window case is live: `resolve_windows`
    // returns an empty selection for a zero-width `time_range`, and the read
    // path rebuilds that as `count = 0` and reports a failure here as
    // `IntegrityError`. Limiting the allowance to exactly one window would tell
    // a caller asking a well-formed question about an intact store that the
    // store is corrupt, and only for the zero-interval encoding: the same query
    // against a positive-interval forecast returns an empty result.
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

    descriptor_builders!("timestamps", "max_active_power");
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
        each_variant!(self, s => &s.name)
    }

    /// The stored array of the wrapped series, whatever its type.
    pub fn array(&self) -> &TypedArray {
        each_variant!(self, s => &s.data)
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
        each_variant!(self, s => s.element_type)
    }

    /// The user-declared units label, or `None`.
    pub fn units(&self) -> Option<&str> {
        each_variant!(self, s => s.units.as_deref())
    }

    /// The quantity kind the values measure, or `None`.
    pub fn quantity_kind(&self) -> Option<&str> {
        each_variant!(self, s => s.quantity_kind.as_deref())
    }

    /// The declared unit basis, or `None` if unspecified.
    pub fn unit_system(&self) -> Option<UnitSystem> {
        each_variant!(self, s => s.unit_system)
    }

    /// How the timestamps were spelled, or `None` if unspecified.
    pub fn time_reference(&self) -> Option<&TimeReference> {
        each_variant!(self, s => s.time_reference.as_ref())
    }

    /// The component field these values vary over time, or `None`.
    pub fn component_field(&self) -> Option<&str> {
        each_variant!(self, s => s.component_field.as_deref())
    }

    /// The opaque application payload, or `None`.
    pub fn application_data(&self) -> Option<&str> {
        each_variant!(self, s => s.application_data.as_deref())
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
        each_variant!(self, s => s.element_type = element_type)
    }

    /// Set the units label in place.
    pub fn set_units(&mut self, units: Option<String>) {
        each_variant!(self, s => s.units = units)
    }

    /// Set the quantity kind in place.
    pub fn set_quantity_kind(&mut self, quantity_kind: Option<String>) {
        each_variant!(self, s => s.quantity_kind = quantity_kind)
    }

    /// Set the unit basis in place.
    pub fn set_unit_system(&mut self, unit_system: Option<UnitSystem>) {
        each_variant!(self, s => s.unit_system = unit_system)
    }

    /// Set the timestamp spelling in place.
    pub fn set_time_reference(&mut self, time_reference: Option<TimeReference>) {
        each_variant!(self, s => s.time_reference = time_reference)
    }

    /// Set the component field in place.
    pub fn set_component_field(&mut self, component_field: Option<String>) {
        each_variant!(self, s => s.component_field = component_field)
    }

    /// Set the application payload in place.
    pub fn set_application_data(&mut self, application_data: Option<String>) {
        each_variant!(self, s => s.application_data = application_data)
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

    // ---- Materialized timelines -------------------------------------------

    fn hourly(length: usize) -> SingleTimeSeries {
        SingleTimeSeries::new(
            t0(),
            Period::Fixed(Duration::hours(1)),
            arr(vec![length]),
            "s",
        )
    }

    #[test]
    fn single_time_series_timestamps_walk_the_fixed_grid() {
        let series = hourly(3);
        assert_eq!(
            series.timestamps().collect::<Vec<_>>(),
            vec![t0(), t0() + Duration::hours(1), t0() + Duration::hours(2)]
        );
    }

    #[test]
    fn single_time_series_timestamps_step_a_month_grid_on_the_calendar() {
        // The reason this lives in the core rather than in each binding: a
        // month is not a span, so no multiplication reproduces this.
        let jan31 = Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap();
        let series = SingleTimeSeries::new(jan31, Period::Months(1), arr(vec![4]), "monthly");
        assert_eq!(
            series.timestamps().collect::<Vec<_>>(),
            vec![
                jan31,
                Utc.with_ymd_and_hms(2024, 2, 29, 0, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2024, 3, 31, 0, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2024, 4, 30, 0, 0, 0).unwrap(),
            ]
        );
    }

    #[test]
    fn single_time_series_timestamps_agree_with_timestamp_at() {
        let series = hourly(5);
        for (k, t) in series.timestamps().enumerate() {
            assert_eq!(series.timestamp_at(k).unwrap(), t);
        }
    }

    #[test]
    fn single_time_series_timestamp_at_rejects_an_index_past_the_grid() {
        let series = hourly(3);
        assert!(series.timestamp_at(2).is_ok());
        let err = series.timestamp_at(3).unwrap_err().to_string();
        assert!(err.contains("past the grid extent"), "{err}");
    }

    #[test]
    fn single_time_series_with_no_steps_has_no_timestamps() {
        assert_eq!(hourly(0).timestamps().count(), 0);
        assert!(hourly(0).timestamp_at(0).is_err());
    }

    fn forecast(horizon_hours: i64, interval_hours: i64, count: usize) -> Deterministic {
        let h = horizon_hours as usize;
        Deterministic::new(
            t0(),
            Period::Fixed(Duration::hours(1)),
            Period::Fixed(Duration::hours(horizon_hours)),
            Period::Fixed(Duration::hours(interval_hours)),
            count,
            arr(vec![h, count]),
            "f",
        )
        .unwrap()
    }

    #[test]
    fn deterministic_horizon_count_is_the_first_axis() {
        let f = forecast(3, 1, 4);
        assert_eq!(f.horizon_count(), 3);
        assert_eq!(f.horizon_count(), f.data.shape[0]);
        assert_eq!(
            f.horizon_count(),
            compute_h(f.horizon, f.resolution).unwrap()
        );
    }

    #[test]
    fn deterministic_window_starts_step_by_interval() {
        let f = forecast(2, 3, 3);
        let starts: Vec<_> = (0..f.count).map(|k| f.window_start(k).unwrap()).collect();
        assert_eq!(
            starts,
            vec![t0(), t0() + Duration::hours(3), t0() + Duration::hours(6)]
        );
    }

    #[test]
    fn deterministic_window_timestamps_step_by_resolution() {
        // The two grids are independent: windows every 3h, rows every 1h.
        let f = forecast(2, 3, 3);
        assert_eq!(
            f.window_timestamps(1).unwrap(),
            vec![t0() + Duration::hours(3), t0() + Duration::hours(4)]
        );
    }

    #[test]
    fn deterministic_windows_overlap_when_reissued_faster_than_the_horizon() {
        // The property the Arrow binding leans on: neighbouring windows share
        // instants, so they cannot be flattened onto one timeline.
        let f = forecast(3, 1, 4);
        let first = f.window_timestamps(0).unwrap();
        let second = f.window_timestamps(1).unwrap();
        assert_eq!(first[1..], second[..second.len() - 1]);
    }

    #[test]
    fn deterministic_window_accessors_reject_an_index_past_the_count() {
        let f = forecast(2, 1, 3);
        assert!(f.window_start(2).is_ok());
        let err = f.window_start(3).unwrap_err().to_string();
        assert!(err.contains("past the forecast window extent"), "{err}");
        assert!(f.window_timestamps(3).is_err());
    }

    #[test]
    fn deterministic_window_starts_step_a_month_interval_on_the_calendar() {
        let jan31 = Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap();
        let f = Deterministic::new(
            jan31,
            Period::Months(1),
            Period::Months(2),
            Period::Months(1),
            3,
            arr(vec![2, 3]),
            "monthly",
        )
        .unwrap();
        assert_eq!(
            f.window_start(1).unwrap(),
            Utc.with_ymd_and_hms(2024, 2, 29, 0, 0, 0).unwrap()
        );
        assert_eq!(
            f.window_timestamps(1).unwrap(),
            vec![
                Utc.with_ymd_and_hms(2024, 2, 29, 0, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2024, 3, 29, 0, 0, 0).unwrap(),
            ]
        );
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
        // disjoint and together cover every type. With `PersistentTimeSeries`
        // appended as code 6 the static group is not a contiguous range, so the
        // partition is the property to pin.
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
            TimeSeriesData::PersistentTimeSeries(
                PersistentTimeSeries::from_values(
                    (0..4).map(|i| t0() + Duration::hours(i * 3)).collect(),
                    &curves(),
                    "pt",
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
