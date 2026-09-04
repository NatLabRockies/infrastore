//! High-level `Store` composing the storage backend and metadata store.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::error::{Result, TimeSeriesError};
use crate::hash::array_hash;
use crate::metadata::{
    AssociationIdentity, MetadataFilter, MetadataStore, ParentChildAssociation, ParentChildFilter,
    SeriesFamily, SharedSetCache, SupplementalAttributeAssociation, SupplementalAttributeFilter,
    SupplementalAttributeSummaryRow, TypeMatch, references_to_in_tx, timestamp_references_in_tx,
};
use crate::reader::{ForecastReader, StaticReader};
use crate::storage::{
    ArrayLayout, ArrayLocation, CompactionReport, Compression, Hdf5Backend, IntegrityReport,
    MemoryBackend, PackGroup, StorageBackend,
};
use crate::types::array::{Dtype, TypedArray};
use crate::types::element_type::ElementType;
use crate::types::id::TimeSeriesId;
use crate::types::key::KeyIdentity;
use crate::types::metadata::{Features, OwnerCategory, TimeSeriesMetadata, validate_features};
use crate::types::period::Period;
use crate::types::time_reference::{TimeRange, TimeReference};
use crate::types::time_series::{
    Descriptors, Deterministic, NonSequentialTimeSeries, Probabilistic, Scenarios,
    SingleTimeSeries, TimeSeriesData, TimeSeriesType, compute_h,
};

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub owner_id: Option<i64>,
    pub owner_category: Option<OwnerCategory>,
    pub owner_type: Option<String>,
    /// Type filter, interpreted by [`TimeSeriesType::accepts`]: `Deterministic`
    /// spans both of its concrete storage forms (so a transformed
    /// `DeterministicSingleTimeSeries` is included), every other type matches
    /// only itself. Rows keep their stored type in
    /// [`TimeSeriesMetadata::time_series_type`], so a caller that cares which
    /// form it got can still tell.
    pub time_series_type: Option<TimeSeriesType>,
    pub name: Option<String>,
    /// SQLite `GLOB` pattern on the name (case-sensitive; `*` and `?`
    /// wildcards). Applied in addition to `name` when both are set.
    pub name_glob: Option<String>,
    /// Exact, case-sensitive match on [`TimeSeriesMetadata::component_field`] —
    /// "every series that varies this field", across owners or scoped to one.
    ///
    /// It is a descriptor, not part of a series' identity, so this narrows a
    /// listing but never addresses a single row on its own: one component may
    /// carry several series for one field, distinguished by name or features.
    /// A row that declares no `component_field` matches no value at all, so
    /// this cannot be used to find the rows that left it unset.
    pub component_field: Option<String>,
    /// Coherence predicate on the timestamp spelling: `Some(true)` keeps only
    /// the [`TimeReference::Zoneless`] series, `Some(false)` keeps everything
    /// that accepts a zoned query bound — the three zoned spellings *and* the
    /// rows that left the reference unset.
    ///
    /// This is the constructive half of the mixed-selection rules. A selection
    /// that spans both groups has no single valid time bound and no single
    /// spelling for a shared timestamp axis, so [`Store::bulk_read`] and
    /// [`Store::build_static_reader`] reject it; this is how a caller builds a
    /// coherent one instead. It is deliberately a binary predicate rather than
    /// an exact match on a reference: the unset rows are part of the zoned
    /// group, and an exact match cannot express that (see
    /// [`Self::component_field`] for the same trap).
    pub zoneless: Option<bool>,
    pub resolution: Option<Period>,
    pub interval: Option<Period>,
    pub features: Option<Features>,
    /// Whether `features` names the series' *whole* feature set rather than a
    /// subset it must contain.
    ///
    /// A listing matches features as a subset — the useful default for "every
    /// series tagged `scenario=high`". An existence check posed against a
    /// complete identity wants the other rule, or a series carrying an extra
    /// feature would answer yes to a question about a series that does not
    /// exist. Exact matching is a hash equality on an indexed column, so it is
    /// the cheaper of the two.
    pub features_exact: bool,
}

impl ListFilter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn owner_id(mut self, id: i64) -> Self {
        self.owner_id = Some(id);
        self
    }
    pub fn owner_category(mut self, c: OwnerCategory) -> Self {
        self.owner_category = Some(c);
        self
    }
    pub fn owner_type(mut self, t: impl Into<String>) -> Self {
        self.owner_type = Some(t.into());
        self
    }
    pub fn time_series_type(mut self, t: TimeSeriesType) -> Self {
        self.time_series_type = Some(t);
        self
    }
    pub fn name(mut self, n: impl Into<String>) -> Self {
        self.name = Some(n.into());
        self
    }
    pub fn name_glob(mut self, pattern: impl Into<String>) -> Self {
        self.name_glob = Some(pattern.into());
        self
    }
    pub fn component_field(mut self, field: impl Into<String>) -> Self {
        self.component_field = Some(field.into());
        self
    }
    /// Keep only the zoneless series (`true`) or only those that accept a zoned
    /// bound (`false`). See [`Self::zoneless`].
    pub fn zoneless(mut self, zoneless: bool) -> Self {
        self.zoneless = Some(zoneless);
        self
    }
    pub fn resolution(mut self, r: impl Into<Period>) -> Self {
        self.resolution = Some(r.into());
        self
    }
    pub fn interval(mut self, i: impl Into<Period>) -> Self {
        self.interval = Some(i.into());
        self
    }
    /// Match `features` as the series' *whole* feature set rather than a
    /// subset it must contain. See [`Self::features_exact`] on the struct for
    /// when each rule is the right one.
    pub fn exact_features(mut self, f: Features) -> Self {
        self.features = Some(f);
        self.features_exact = true;
        self
    }
    pub fn features(mut self, f: Features) -> Self {
        self.features = Some(f);
        self
    }
}

impl From<ListFilter> for MetadataFilter {
    fn from(value: ListFilter) -> Self {
        MetadataFilter {
            owner_id: value.owner_id,
            owner_category: value.owner_category,
            owner_type: value.owner_type,
            time_series_type: value.time_series_type.map(TypeMatch::Requested),
            name: value.name,
            name_glob: value.name_glob,
            component_field: value.component_field,
            zoneless: value.zoneless,
            resolution: value.resolution,
            interval: value.interval,
            features_hash: if value.features_exact {
                Some(crate::hash::features_hash(
                    value.features.as_ref().unwrap_or(&Features::new()),
                ))
            } else {
                None
            },
            features: if value.features_exact {
                None
            } else {
                value.features
            },
            // `ListFilter` has no id predicate by design; by-id reads are point
            // lookups with their own entry points.
            ids: None,
        }
    }
}

/// The slice of one association a read asks for, in the terms its caller thinks
/// in: where to start, and how much to take from there.
/// [`Store::read_by_id`] resolves it against the row's own grid.
///
/// `len` counts timesteps and belongs to the static types; `count` counts
/// windows and belongs to the forecasts. Neither means anything for the other
/// family, so supplying the wrong one is an error rather than an argument the
/// store quietly drops. Every field unset reads the whole series.
///
/// A window is *checked* where a [`TimeRange`] is clamped, and that is the
/// reason it exists. A range says "whatever lies between these bounds", so a
/// bound past the end of the series is a smaller answer; a window says "these
/// exact steps", so it is a mistake. A caller that asked for 24 steps and
/// silently got 3 has a bug the store can see and the caller cannot — which is
/// why every binding that reads by name has grown its own copy of this
/// arithmetic, off the row it had to fetch first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadWindow {
    /// First timestamp to read; the series' own start when unset. For a
    /// forecast this is a window boundary, `initial_timestamp + k·interval`.
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    /// Whether `start` was written without a zone, exactly as on
    /// [`TimeRange`]: the bound has to be spelled the way the series is.
    pub zoneless: bool,
    /// Timesteps to read from `start` (static types); to the end when unset.
    pub len: Option<usize>,
    /// Forecast windows to read from `start`; to the end when unset.
    pub count: Option<usize>,
}

impl ReadWindow {
    /// The whole series — what a read with no slicing asks for.
    pub fn full() -> Self {
        Self::default()
    }

    /// A zoned start, the native Rust spelling. Chain [`Self::with_len`] or
    /// [`Self::with_count`].
    pub fn from(start: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            start: Some(start),
            ..Self::default()
        }
    }

    /// A start written as a wall clock, for a [`TimeReference::Zoneless`] series.
    pub fn from_zoneless(start: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            start: Some(start),
            zoneless: true,
            ..Self::default()
        }
    }

    /// Take `len` timesteps (a static series).
    pub fn with_len(mut self, len: usize) -> Self {
        self.len = Some(len);
        self
    }

    /// Take `count` windows (a forecast).
    pub fn with_count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    /// Whether this asks for the whole series, in which case the read skips the
    /// grid arithmetic entirely and hands the array back whole.
    fn is_full(&self) -> bool {
        self.start.is_none() && self.len.is_none() && self.count.is_none()
    }

    /// The [`TimeRange`] this window names on `meta`'s grid, or `None` for a
    /// whole-series read. Errors — rather than clamping — when the start is off
    /// the grid or the requested extent runs past the end.
    fn resolve(&self, meta: &TimeSeriesMetadata) -> Result<Option<TimeRange>> {
        if self.is_full() {
            return Ok(None);
        }
        let range = match meta.time_series_type {
            TimeSeriesType::SingleTimeSeries => self.resolve_single(meta)?,
            TimeSeriesType::NonSequentialTimeSeries => self.resolve_non_sequential(meta)?,
            TimeSeriesType::Deterministic
            | TimeSeriesType::DeterministicSingleTimeSeries
            | TimeSeriesType::Probabilistic
            | TimeSeriesType::Scenarios => self.resolve_forecast(meta)?,
        };
        Ok(Some(range))
    }

    /// Reject the extent argument belonging to the other family, naming the one
    /// that applies. A silently ignored argument is a wrong answer the caller
    /// cannot see.
    fn reject_count(&self, label: &str) -> Result<()> {
        match self.count {
            None => Ok(()),
            Some(_) => Err(TimeSeriesError::InvalidParameter(format!(
                "count selects forecast windows and does not apply to {label}; use len"
            ))),
        }
    }

    fn reject_len(&self, label: &str) -> Result<()> {
        match self.len {
            None => Ok(()),
            Some(_) => Err(TimeSeriesError::InvalidParameter(format!(
                "len selects timesteps and does not apply to {label}, whose windows are \
                 selected by count"
            ))),
        }
    }

    /// The extent to take from `start_idx`, defaulting to the rest of `total`.
    /// Zero is refused: an empty read is a caller error, not a smaller answer.
    fn extent(
        requested: Option<usize>,
        start_idx: usize,
        total: usize,
        unit: &str,
    ) -> Result<usize> {
        // Every caller resolves `start_idx` against `total` before asking, so
        // the remainder cannot underflow -- and comparing against it, rather
        // than adding `n` to `start_idx`, keeps the check itself from
        // overflowing on an extent a caller is free to make arbitrarily large.
        let remaining = total - start_idx;
        let n = requested.unwrap_or(remaining);
        if n == 0 {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "requested 0 {unit}; a read selects at least one"
            )));
        }
        if n > remaining {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "requested {n} {unit} from index {start_idx}, past the end of the {total} \
                 stored"
            )));
        }
        Ok(n)
    }

    /// The resolved bounds, spelled the way the read has to be for `meta` to
    /// answer it.
    ///
    /// A caller who named a start is held to their own spelling — that bound is
    /// theirs, and a mismatch with the series is the category error
    /// [`TimeRange::check_against`] refuses. A caller who named none did not
    /// choose a spelling: the bound is the series' own first timestamp, so it is
    /// spelled the way the series is. Otherwise `read_by_id(id,
    /// ReadWindow::full().with_len(2))` — a request with no timestamp anywhere in
    /// it — would fail against a zoneless series for a bound the caller never
    /// wrote.
    fn range(
        &self,
        meta: &TimeSeriesMetadata,
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
    ) -> TimeRange {
        let zoneless = match self.start {
            Some(_) => self.zoneless,
            None => !TimeReference::accepts_zoned_bound(meta.time_reference.as_ref()),
        };
        TimeRange::spelled(start, end, zoneless)
    }

    fn resolve_single(&self, meta: &TimeSeriesMetadata) -> Result<TimeRange> {
        self.reject_count("a SingleTimeSeries")?;
        let initial = required_initial(meta, "SingleTimeSeries")?;
        let resolution = required_resolution(meta, "SingleTimeSeries")?;
        let length = required_length(meta, "SingleTimeSeries")?;
        let start = self.start.unwrap_or(initial);
        // `steps_between` is the strict counterpart of the `floor_steps` a raw
        // range would use: a start between two steps is off the grid, not the
        // step below it.
        let start_idx = resolution.steps_between(initial, start)?;
        if start_idx >= length {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "start_time is past the last timestep (resolves to index {start_idx}, but \
                 only {length} step(s) are stored)"
            )));
        }
        let n = Self::extent(self.len, start_idx, length, "timesteps")?;
        let end = resolution.add_to(start, n as i64).ok_or_else(|| {
            TimeSeriesError::IntegrityError("window end timestamp overflow".into())
        })?;
        Ok(self.range(meta, start, end))
    }

    fn resolve_non_sequential(&self, meta: &TimeSeriesMetadata) -> Result<TimeRange> {
        self.reject_count("a NonSequentialTimeSeries")?;
        let timestamps = meta.timestamps.as_ref().ok_or_else(|| {
            TimeSeriesError::IntegrityError("NonSequentialTimeSeries missing timestamps".into())
        })?;
        let total = timestamps.len();
        if total == 0 {
            return Err(TimeSeriesError::InvalidParameter(
                "cannot select a window of an empty NonSequentialTimeSeries".into(),
            ));
        }
        let start = self.start.unwrap_or(timestamps[0]);
        // An irregular series has no grid to round onto, so the start has to be
        // one of its own timestamps. Answering with the next one along would be
        // a different series than the caller named.
        let start_idx = timestamps.partition_point(|t| *t < start);
        if start_idx >= total || timestamps[start_idx] != start {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "start_time {start} is not one of this NonSequentialTimeSeries' timestamps"
            )));
        }
        let n = Self::extent(self.len, start_idx, total, "timestamps")?;
        // The bound is exclusive, so it is the timestamp after the last one
        // selected — or just past the final timestamp when the window runs to
        // the end. One millisecond is the catalog's own timestamp resolution.
        let end = if start_idx + n < total {
            timestamps[start_idx + n]
        } else {
            timestamps[total - 1]
                .checked_add_signed(chrono::Duration::milliseconds(1))
                .ok_or_else(|| {
                    TimeSeriesError::IntegrityError("window end timestamp overflow".into())
                })?
        };
        Ok(self.range(meta, start, end))
    }

    fn resolve_forecast(&self, meta: &TimeSeriesMetadata) -> Result<TimeRange> {
        let label = format!("a {:?}", meta.time_series_type);
        self.reject_len(&label)?;
        let initial = required_initial(meta, &label)?;
        let interval = required_interval(meta, &label)?;
        let stored = required_count(meta, &label)?;
        let start = self.start.unwrap_or(initial);
        // A single-window forecast carries a zero interval — there is no second
        // window to step to — so its only valid start is `initial`.
        let start_idx = if interval.is_zero() {
            if start != initial {
                return Err(TimeSeriesError::InvalidParameter(
                    "forecast start_time must align to a window boundary \
                     (initial_timestamp + k·interval)"
                        .into(),
                ));
            }
            0
        } else {
            interval.steps_between(initial, start)?
        };
        if start_idx >= stored {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "start_time is past the last window (resolves to window index {start_idx}, \
                 but only {stored} window(s) are stored)"
            )));
        }
        let n = Self::extent(self.count, start_idx, stored, "windows")?;
        // With a zero interval the arithmetic below would put `end` on `start`,
        // selecting nothing; the one window starts at `start`, so any bound past
        // it selects exactly that window.
        let end = if interval.is_zero() {
            start
                .checked_add_signed(chrono::Duration::milliseconds(1))
                .ok_or_else(|| {
                    TimeSeriesError::IntegrityError("window end timestamp overflow".into())
                })?
        } else {
            interval.add_to(start, n as i64).ok_or_else(|| {
                TimeSeriesError::IntegrityError("window end timestamp overflow".into())
            })?
        };
        Ok(self.range(meta, start, end))
    }
}

/// Single item in a bulk add.
///
/// A request names no catalog id. Every add — this one, the wide positional
/// forms, and the association catalogs' — lets the catalog assign, and the id
/// it chose comes back as a [`TimeSeriesId`]. The one place a caller supplies
/// ids is [`Store::import_association_rows`], where the document being replayed
/// already recorded them and the references have to survive; see
/// [`TimeSeriesMetadata::id`].
#[derive(Debug, Clone)]
pub struct AddRequest {
    pub owner_id: i64,
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub data: TimeSeriesData,
    pub features: Features,
}

impl AddRequest {
    /// Start a request with empty features. Chain [`Self::with_features`] to
    /// set them.
    ///
    /// A series' descriptive attributes — `element_type`, `units`, `quantity_kind`,
    /// `unit_system`, `component_field`, and `application_data` —
    /// live on the [`TimeSeriesData`] itself, not here: they describe the data,
    /// so they travel with it and come back on a read. Set them with the
    /// `with_element_type` / `with_units` / `with_application_data` builders on the concrete
    /// series type before wrapping it in a request.
    pub fn new(
        owner_id: i64,
        owner_type: impl Into<String>,
        owner_category: OwnerCategory,
        data: TimeSeriesData,
    ) -> Self {
        Self {
            owner_id,
            owner_type: owner_type.into(),
            owner_category,
            data,
            features: Features::new(),
        }
    }

    /// Set the feature set.
    pub fn with_features(mut self, features: Features) -> Self {
        self.features = features;
        self
    }
}

#[derive(Debug, Default, Clone)]
pub struct TimeSeriesCounts {
    pub components_with_time_series: i64,
    pub static_time_series: i64,
    pub forecasts: i64,
}

/// Owner- and array-oriented counts (distinct owners per category, distinct
/// stored arrays per kind). Unlike [`TimeSeriesCounts`] the time-series counts
/// here are de-duplicated by array content, and owners are split by category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeSeriesCountsDetailed {
    pub components_with_time_series: i64,
    pub supplemental_attributes_with_time_series: i64,
    pub static_time_series_count: i64,
    pub forecast_count: i64,
}

/// One resolution's shared static grid, as reported by
/// [`Store::check_static_consistency`]: every `SingleTimeSeries` at
/// `resolution` shares this `(initial_timestamp, length)`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StaticConsistency {
    pub resolution: Period,
    pub initial_timestamp: chrono::DateTime<chrono::Utc>,
    pub length: usize,
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ForecastParameters {
    pub horizon: Option<Period>,
    pub interval: Option<Period>,
    pub count: Option<usize>,
    pub resolution: Option<Period>,
    pub initial_timestamp: Option<chrono::DateTime<chrono::Utc>>,
}

/// How a caller wants [`Store::transform_single_time_series`] to run: the rules
/// it applies on top of the eligibility checks everyone gets, plus whether to
/// commit.
///
/// The two rules encode a *client's* contract rather than a storage invariant:
/// the store itself is happy to hold forecasts on more than one grid, and both
/// single-window interval encodings are legal. InfrastructureSystems.jl is
/// stricter on both counts, so it opts in. `Default` is the permissive,
/// committing behavior.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TransformPolicy {
    /// Run every check and report what *would* happen, writing nothing. The
    /// returned [`TransformOutcome::transformed`] is the count a committing run
    /// would produce. This is how a caller answers "would this transform
    /// succeed?" without a trial-and-rollback.
    pub dry_run: bool,
    /// Store a single-window request (an interval equal to a horizon that spans
    /// the whole series) as the zero interval rather than verbatim. The
    /// interval is part of the association identity, so this decides which form
    /// later lookups must use.
    pub normalize_single_window: bool,
    /// Require the derived grid to match any forecast already stored at the
    /// same `(resolution, interval)`, and require every resolution in scope to
    /// derive the same `count` and `initial_timestamp` — i.e. one system, one
    /// forecast grid.
    pub require_uniform_forecast_grid: bool,
}

/// Outcome of [`Store::transform_single_time_series`].
///
/// `interval` is the interval actually stored, which differs from the
/// requested one when a single-window request was normalized (see
/// `interval_normalized`). Bindings that surface a warning for that case read
/// it off this struct rather than re-deriving the condition.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TransformOutcome {
    /// `DeterministicSingleTimeSeries` rows written.
    pub transformed: usize,
    /// `SingleTimeSeries` the filter matched, before idempotent skips. Zero
    /// means there was nothing to transform.
    pub sources: usize,
    /// The stored interval, after single-window normalization.
    pub interval: Period,
    /// True when the requested `interval` equaled the horizon and that horizon
    /// spanned the whole series, so the interval was normalized to zero.
    pub interval_normalized: bool,
    /// The views this call wrote, in the order they were written — the catalog
    /// id each was filed under, so a caller can reference a view it just derived
    /// without listing the store to find it again.
    ///
    /// Empty for a dry run, which writes nothing: [`Self::transformed`] still
    /// reports what *would* have been written, and is the field to read there.
    /// Also empty, with `transformed` zero, when every eligible series already
    /// had its view — the sweep is idempotent, and a series it skipped was not
    /// written by this call.
    pub written: Vec<TimeSeriesId>,
}

/// The window parameters `transform_single_time_series` will write, derived
/// once per resolution from the catalog's distinct static grids.
///
/// This is the entirety of the transform's parameter validation. It is built
/// from one `DISTINCT` query, so its cost scales with the number of distinct
/// resolutions in the store — not the number of series — which is what lets the
/// transform validate a 100,000-series store without listing it.
struct TransformPlan {
    /// What to write for each resolution in scope. Sources are looked up here
    /// instead of recomputing, and under a permissive policy resolutions may
    /// legitimately differ.
    by_resolution: HashMap<Period, GridPlan>,
    /// The first resolution's parameters — the representative used for the
    /// outcome and, under `require_uniform_forecast_grid`, the values every
    /// other resolution was checked against.
    interval: Period,
    count: usize,
    interval_normalized: bool,
    horizon: Period,
    initial_timestamp: chrono::DateTime<chrono::Utc>,
}

/// One resolution's derived window parameters.
#[derive(Debug, Clone, Copy)]
struct GridPlan {
    interval: Period,
    count: usize,
}

impl TransformPlan {
    /// Derive the plan, or `None` when no `SingleTimeSeries` is in scope.
    ///
    /// `horizon` and `interval` are the requested values and are already known
    /// to be regular periods.
    fn derive(
        grids: &[StaticConsistency],
        horizon: Period,
        requested_interval: Period,
        policy: TransformPolicy,
    ) -> Result<Option<Self>> {
        let Some(first) = grids.first() else {
            return Ok(None);
        };

        let mut by_resolution = HashMap::with_capacity(grids.len());
        let mut agreed: Option<(Period, usize)> = None;
        let mut interval_normalized = false;

        for grid in grids {
            // The wording matches what InfrastructureSystems.jl raised before
            // this check moved into the store; `compute_h`'s own message is not
            // appended, as it restates the same two periods.
            let h = compute_h(horizon, grid.resolution).map_err(|_| {
                TimeSeriesError::InvalidParameter(format!(
                    "horizon {} is not evenly divisible by resolution {}",
                    horizon.to_iso8601(),
                    grid.resolution.to_iso8601()
                ))
            })?;
            if h > grid.length {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "horizon {} ({h} steps) exceeds the SingleTimeSeries length ({}) \
                     at resolution {}",
                    horizon.to_iso8601(),
                    grid.length,
                    grid.resolution.to_iso8601()
                )));
            }

            // A horizon that spans the whole series leaves no room for a second
            // window, so a request to step by exactly one horizon means "one
            // window". Both encodings are legal and the caller chose one; the
            // case is reported either way so callers can warn.
            let single_window = grid.length == h && requested_interval == horizon;
            let (interval, normalized) = if single_window {
                let stored = if policy.normalize_single_window {
                    Period::zero()
                } else {
                    requested_interval
                };
                (stored, true)
            } else {
                // Mixed period kinds have no ordering; `divide_into` below
                // rejects them with a message naming the real problem.
                if requested_interval.same_kind(&horizon)
                    && period_ms(requested_interval) > period_ms(horizon)
                {
                    return Err(TimeSeriesError::InvalidParameter(format!(
                        "interval {} is longer than the horizon {}, which would leave \
                         gaps between windows",
                        requested_interval.to_iso8601(),
                        horizon.to_iso8601()
                    )));
                }
                (requested_interval, false)
            };
            interval_normalized |= normalized;

            let count = if interval.is_zero() {
                // A zero interval is the explicit single-window request (the
                // encoding InfrastructureSystems.jl writes for directly-added
                // single-window forecasts): the one window must cover the whole
                // series. Normalization above only produces a zero interval when
                // it already does, so this only rejects an explicit request.
                if h != grid.length {
                    return Err(TimeSeriesError::InvalidParameter(format!(
                        "a zero interval derives a single window covering the whole \
                         series, but horizon {} ({h} steps) does not span the \
                         SingleTimeSeries length ({}) at resolution {}",
                        horizon.to_iso8601(),
                        grid.length,
                        grid.resolution.to_iso8601()
                    )));
                }
                1
            } else {
                let interval_steps = grid.resolution.divide_into(&interval).map_err(|_| {
                    TimeSeriesError::InvalidParameter(format!(
                        "interval {} must be zero or a positive integer multiple of \
                         resolution {}",
                        interval.to_iso8601(),
                        grid.resolution.to_iso8601()
                    ))
                })?;
                (grid.length - h) / interval_steps + 1
            };

            // Under `require_uniform_forecast_grid` one transform produces one
            // forecast grid, so resolutions deriving different window counts or
            // starts are rejected rather than silently written. Without it the
            // store is happy to hold both, and each resolution keeps its own.
            match agreed {
                None => agreed = Some((interval, count)),
                Some((agreed_interval, agreed_count)) if policy.require_uniform_forecast_grid => {
                    if agreed_interval != interval || agreed_count != count {
                        return Err(TimeSeriesError::InvalidParameter(format!(
                            "transform would produce forecasts with different window \
                             parameters per resolution: {} gives count {} at interval {}, \
                             {} gives count {} at interval {}",
                            first.resolution.to_iso8601(),
                            agreed_count,
                            agreed_interval.to_iso8601(),
                            grid.resolution.to_iso8601(),
                            count,
                            interval.to_iso8601(),
                        )));
                    }
                    if grid.initial_timestamp != first.initial_timestamp {
                        return Err(TimeSeriesError::InvalidParameter(format!(
                            "transform is not supported when SingleTimeSeries have \
                             different initial timestamps: {} at resolution {} vs {} at \
                             resolution {}",
                            first.initial_timestamp,
                            first.resolution.to_iso8601(),
                            grid.initial_timestamp,
                            grid.resolution.to_iso8601(),
                        )));
                    }
                }
                Some(_) => {}
            }
            by_resolution.insert(grid.resolution, GridPlan { interval, count });
        }

        let (interval, count) = agreed.expect("grids is non-empty");
        Ok(Some(TransformPlan {
            by_resolution,
            interval,
            count,
            interval_normalized,
            horizon,
            initial_timestamp: first.initial_timestamp,
        }))
    }

    /// The derived parameters for a source's resolution.
    fn for_resolution(&self, resolution: Period) -> Result<GridPlan> {
        self.by_resolution.get(&resolution).copied().ok_or_else(|| {
            // The grid query and the source listing use the same filters, so a
            // miss means the catalog changed underneath the transform.
            TimeSeriesError::IntegrityError(format!(
                "no static grid was derived for resolution {}",
                resolution.to_iso8601()
            ))
        })
    }

    /// Reject a plan that disagrees with a forecast already in the store. A
    /// default (all-`None`) `ForecastParameters` means nothing is stored at this
    /// `(resolution, interval)`, so there is nothing to disagree with.
    fn check_compatible_with(&self, existing: &ForecastParameters) -> Result<()> {
        let Some(existing_count) = existing.count else {
            return Ok(());
        };
        if existing_count != self.count {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "derived forecast count {} does not match the stored forecast count {}",
                self.count, existing_count
            )));
        }
        if let Some(ts) = existing.initial_timestamp
            && ts != self.initial_timestamp
        {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "derived forecast initial_timestamp {} does not match the stored \
                 forecast initial_timestamp {ts}",
                self.initial_timestamp
            )));
        }
        if let Some(h) = existing.horizon
            && h != self.horizon
        {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "derived forecast horizon {} does not match the stored forecast \
                 horizon {}",
                self.horizon.to_iso8601(),
                h.to_iso8601()
            )));
        }
        Ok(())
    }
}

/// Milliseconds in a regular period. `Months` has no fixed millisecond span, so
/// it maps to its month count — callers compare only same-kind periods, having
/// rejected irregular ones up front.
fn period_ms(p: Period) -> i64 {
    match p {
        Period::Fixed(d) => d.num_milliseconds(),
        Period::Months(m) => m as i64,
    }
}

/// Bookkeeping for an open cross-operation transaction (see
/// [`Store::begin_transaction`]).
///
/// The SQLite half rolls back on its own; this tracks the HDF5 half, which has
/// no transaction of its own to enlist. The trick is that the array store is
/// content-addressed, so it can be made **append-only for the transaction's
/// duration**: writes are recorded here and undone on rollback, and frees are
/// deferred here and applied only once the outermost commit succeeds. Together
/// those make both halves of the artifact roll back in step.
#[derive(Debug, Default)]
struct OpenTxn {
    /// Savepoint nesting depth. Only the outermost commit/rollback touches the
    /// backend; inner ones just release or unwind their savepoint.
    depth: usize,
    /// Arrays this transaction physically wrote, in write order. Removed on
    /// rollback — they are unreachable once the catalog rolls back, and leaving
    /// them would orphan bytes no association references.
    staged_hashes: Vec<[u8; 32]>,
    /// Explicit timestamp vectors this transaction physically wrote, in write
    /// order, and unwound on rollback for exactly the reasons above. Tracked
    /// separately from `staged_hashes` because liveness is a different question
    /// for them: an axis is referenced through `timestamps_hash`, not
    /// `data_hash`.
    staged_timestamps: Vec<[u8; 32]>,
    /// The lengths of both staged lists as each nesting level was opened, so a
    /// rollback can tell which writes belong to the level it is unwinding.
    /// Without it an inner rollback unwound the catalog but left its writes in
    /// the file: the outer commit only consults the pending-free sets, so the
    /// bytes stayed with no row referencing them — invisible to
    /// `verify_integrity`, which walks only catalog-referenced objects, and
    /// reclaimable only by `compact`.
    marks: Vec<Mark>,
    /// Arrays that a removal inside this transaction left unreferenced. The free
    /// is deferred to the outermost commit: while the transaction is open the
    /// bytes must survive, because a rollback restores the catalog rows that
    /// point at them.
    pending_free: HashSet<[u8; 32]>,
    /// Timestamp vectors a clear inside this transaction left unreferenced,
    /// deferred on the same terms as `pending_free`.
    pending_free_timestamps: HashSet<[u8; 32]>,
}

/// What one write call physically put into the array file, so it can be undone
/// if the call fails and handed to an enclosing transaction if it succeeds.
///
/// Only what the call *wrote* is recorded. Content addressing means a put of a
/// hash the store already held is a no-op, and unwinding one of those would
/// delete data the call did not create — so both backends report whether a put
/// was a write, and only those land here.
#[derive(Debug, Default)]
struct StagedWrites {
    arrays: Vec<[u8; 32]>,
    timestamps: Vec<[u8; 32]>,
}

/// How much of each staged list belonged to the enclosing nesting level. See
/// [`OpenTxn::marks`].
#[derive(Debug, Clone, Copy, Default)]
struct Mark {
    arrays: usize,
    timestamps: usize,
}

/// Where a store's SQLite catalog lives, independent of where its arrays live.
///
/// The two axes are orthogonal: arrays can sit in an HDF5 file while the
/// catalog sits in RAM. That combination is the point of [`Self::InMemory`] —
/// see its docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CatalogMode {
    /// The catalog *is* the `<store>.sqlite` file: WAL journaling, and every
    /// committed mutation is durable as soon as the OS writes it back. Other
    /// processes holding the same artifact see those commits.
    ///
    /// The default, and what a long-lived on-disk store wants — the CLI mutates
    /// one command per process and relies on each one landing.
    #[default]
    Attached,
    /// The catalog is held in `:memory:`, loaded from `<store>.sqlite` on open
    /// and written back only by [`Store::persist_to`]. Mutations pay no
    /// journaling and no fsync, and **nothing survives a crash**.
    ///
    /// For a consumer that builds a store in a scratch directory beside its own
    /// in-RAM state and only cares about durability at an explicit save: a crash
    /// loses that state regardless, so journaling the scratch catalog buys
    /// nothing. Arrays still stream to the HDF5 file, so this does not require
    /// the data to fit in memory.
    InMemory,
}

/// The catalog placement that reproduces pre-[`CatalogMode`] behavior for a
/// given `in_memory` flag: an in-memory store has always held its catalog in
/// RAM, and an on-disk one has always kept it in `<path>.sqlite`. Keeps the
/// constructors that predate the enum meaning exactly what they used to.
fn default_catalog(in_memory: bool) -> CatalogMode {
    if in_memory {
        CatalogMode::InMemory
    } else {
        CatalogMode::Attached
    }
}

/// Mint a fresh generation stamp for an artifact pair — see
/// [`GENERATION_ATTR`](crate::storage::hdf5::GENERATION_ATTR) and the
/// `catalog_identity` DDL.
///
/// This only has to be unlikely to repeat across saves on one machine, not
/// globally unique, so it is a hash of wall clock, pid, and a per-process
/// counter rather than a real UUID — which keeps `uuid` and `rand` out of the
/// dependency graph (and out of `deny.toml`'s license review). The counter
/// carries the case two saves land inside one clock tick.
fn mint_generation() -> String {
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hasher.finalize()[..16]
        .iter()
        .fold(String::with_capacity(32), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Artifact paths a `Store` in this process currently holds open. See
/// [`TimeSeriesError::StoreInUse`] for why a second handle is refused.
static OPEN_ARTIFACTS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Registration of one artifact path in [`OPEN_ARTIFACTS`], released on drop.
///
/// Taken before anything else an open or create does, so an attempt that fails
/// part-way releases the path with the error, and dropped after the store's
/// backend (field order), so the file is closed before the path is free again.
struct OpenGuard(PathBuf);

impl OpenGuard {
    fn acquire(path: &Path) -> Result<Self> {
        let key = artifact_key(path);
        let mut open = OPEN_ARTIFACTS.lock().unwrap_or_else(|e| e.into_inner());
        if open.contains(&key) {
            return Err(TimeSeriesError::StoreInUse {
                path: path.display().to_string(),
            });
        }
        open.push(key.clone());
        Ok(Self(key))
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        let mut open = OPEN_ARTIFACTS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = open.iter().position(|p| *p == self.0) {
            open.swap_remove(i);
        }
    }
}

/// The key an artifact is registered under: the path canonicalized, so two
/// spellings of one file -- a relative and an absolute form, or a symlink and
/// its target -- collide. A file that does not exist yet (a create registers
/// before writing) is keyed by its directory canonicalized plus the file name,
/// and a path whose directory cannot be resolved either is keyed as written;
/// the open that follows reports why.
fn artifact_key(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.canonicalize(),
        _ => std::env::current_dir(),
    };
    match (dir, path.file_name()) {
        (Ok(dir), Some(name)) => dir.join(name),
        _ => path.to_path_buf(),
    }
}

pub struct Store {
    backend: Box<dyn StorageBackend>,
    metadata: MetadataStore,
    read_only: bool,
    /// Filesystem path for the HDF5 array file (None if `in_memory`).
    file_path: Option<PathBuf>,
    /// Where the catalog lives. Decides whether mutations are durable on commit
    /// or only at [`Self::persist_to`], and which half of `persist_to` runs.
    catalog: CatalogMode,
    /// `Some` while a cross-operation transaction is open.
    txn: Option<OpenTxn>,
    /// Holds `file_path` in [`OPEN_ARTIFACTS`] for this store's lifetime. Last
    /// field on purpose: it drops after `backend` has closed the file.
    _open_guard: Option<OpenGuard>,
}

impl Store {
    /// Create a new store. With `in_memory=true`, no filesystem I/O occurs;
    /// otherwise an HDF5 file is created at `path` and a catalog SQLite
    /// file at `<path>.sqlite` holds metadata.
    ///
    /// Uses the default compression policy ([`Compression::default`]). Use
    /// [`Self::create_with_compression`] to choose a different filter.
    pub fn create(path: Option<&Path>, in_memory: bool) -> Result<Self> {
        Self::create_with_compression(path, in_memory, Compression::default())
    }

    /// Like [`Self::create`], but applies `compression` to HDF5 data
    /// variables. The setting is persisted with the store so later appends
    /// reuse it. It is ignored for `in_memory` stores, which never touch disk.
    pub fn create_with_compression(
        path: Option<&Path>,
        in_memory: bool,
        compression: Compression,
    ) -> Result<Self> {
        Self::create_with_catalog(path, in_memory, compression, default_catalog(in_memory))
    }

    /// Like [`Self::create_with_compression`], but places the catalog
    /// explicitly. See [`CatalogMode`].
    ///
    /// `in_memory=true` admits only [`CatalogMode::InMemory`]: there is no
    /// artifact on disk for a catalog file to sit beside.
    ///
    /// With `in_memory=false` and [`CatalogMode::InMemory`] no `<path>.sqlite`
    /// is created at all — arrays stream to the HDF5 file while the catalog
    /// stays in RAM until [`Self::persist_to`] writes the pair. Such a
    /// half-artifact reopened with [`CatalogMode::Attached`] fails the paired
    /// generation check rather than reading as an empty store: the HDF5 half is
    /// stamped and the freshly created catalog is not. See [`Self::open_with_catalog`].
    ///
    /// # Refuses an existing artifact
    ///
    /// Creating a store where one already lives fails with
    /// [`TimeSeriesError::StoreExists`], checking *both* halves — the HDF5 file
    /// and `<path>.sqlite` — because either one alone is enough to poison the
    /// result. Use [`Self::create_replacing`] to discard the destination on
    /// purpose, or [`Self::open`] to keep it.
    pub fn create_with_catalog(
        path: Option<&Path>,
        in_memory: bool,
        compression: Compression,
        catalog: CatalogMode,
    ) -> Result<Self> {
        compression.validate()?;
        if in_memory {
            if catalog == CatalogMode::Attached {
                return Err(TimeSeriesError::InvalidParameter(
                    "an in-memory store has no file for CatalogMode::Attached to sit beside".into(),
                ));
            }
            return Ok(Self {
                backend: Box::new(MemoryBackend::new()),
                metadata: MetadataStore::open_in_memory()?,
                read_only: false,
                file_path: None,
                catalog,
                txn: None,
                _open_guard: None,
            });
        }
        let file_path = path.ok_or_else(|| {
            TimeSeriesError::InvalidParameter("path is required when in_memory=false".into())
        })?;
        let open_guard = OpenGuard::acquire(file_path)?;
        reject_existing_artifact(file_path)?;
        let metadata = match catalog {
            CatalogMode::Attached => {
                MetadataStore::open_path(&catalog_sqlite_path(file_path), false)?
            }
            CatalogMode::InMemory => MetadataStore::open_in_memory()?,
        };
        let mut backend = Hdf5Backend::create(file_path, compression)?;
        // Stamp the pair at birth, so a later mismatch — one half swapped for a
        // copy from a different save — is detectable even for a store that never
        // goes through `persist_to`.
        let generation = mint_generation();
        backend.set_generation(&generation)?;
        metadata.set_generation(&generation)?;
        Ok(Self {
            backend: Box::new(backend),
            metadata,
            read_only: false,
            file_path: Some(file_path.to_path_buf()),
            catalog,
            txn: None,
            _open_guard: Some(open_guard),
        })
    }

    /// Like [`Self::create_with_catalog`], but discards any artifact already at
    /// `path` instead of refusing it.
    ///
    /// Both halves go — the HDF5 file, `<path>.sqlite`, and the catalog's
    /// `-wal`/`-shm` sidecars. Removing the catalog is the whole point: leaving
    /// it would pair a fresh, empty array file with the old catalog's rows,
    /// which is precisely the state [`Self::create_with_catalog`] refuses to
    /// produce.
    ///
    /// This is destructive and **not** atomic — the old artifact is gone before
    /// the new one exists, so an interrupted call can leave neither. It is for
    /// callers whose explicit intent is to discard the destination; anything
    /// else wants [`Self::create_with_catalog`] (refuses) or [`Self::open`]
    /// (keeps).
    ///
    /// There is no in-memory form: an in-memory store has no artifact to
    /// replace.
    pub fn create_replacing(
        path: &Path,
        compression: Compression,
        catalog: CatalogMode,
    ) -> Result<Self> {
        // Taken before anything is deleted: the files about to go may be the
        // ones another handle in this process is reading and writing. Held
        // across the removals and released just before the create below takes
        // its own.
        let guard = OpenGuard::acquire(path)?;
        let sqlite = catalog_sqlite_path(path);
        // Sidecars before the database they belong to: a `-wal` outliving its
        // database is the one ordering SQLite would try to recover from.
        remove_if_exists(&sqlite_sidecar(&sqlite, "-wal"))?;
        remove_if_exists(&sqlite_sidecar(&sqlite, "-shm"))?;
        remove_if_exists(&sqlite)?;
        remove_if_exists(path)?;
        drop(guard);
        Self::create_with_catalog(Some(path), false, compression, catalog)
    }

    pub fn open(path: &Path, read_only: bool) -> Result<Self> {
        Self::open_with_catalog(path, read_only, CatalogMode::Attached)
    }

    /// Copy the artifact at `src` to `dest` and open the copy read-write.
    ///
    /// Both halves are copied — the HDF5 file and `<src>.sqlite` — so `dest` is
    /// a complete, independent store. `src` is never opened for writing and is
    /// left byte-for-byte alone.
    ///
    /// This is the safe way to load a store someone cares about and then change
    /// it. Opening the original read-write puts every subsequent mutation
    /// directly into their file, and HDF5 has no journal and no repair tool: an
    /// interrupted write there is unrecoverable, whereas working on a copy and
    /// calling [`Self::persist_to`] back over the original leaves it intact
    /// until a single atomic rename replaces it. Both shipped consumers
    /// (infrasys, InfrastructureSystems.jl) already do this by hand; this is the
    /// same thing with the failure modes handled in one place.
    ///
    /// `dest` must not already hold either half of a store, for the same reason
    /// [`Self::create_with_catalog`] refuses one. Nothing is left at `dest` if
    /// this call fails, whether the copy or the open of the copy is what failed
    /// — otherwise the next attempt would refuse a path the caller never
    /// successfully wrote to.
    ///
    /// This copies what is *on disk*. A caller that also holds `src` open in
    /// this process must [`Self::flush`] it first — and on Windows must close it
    /// outright, because HDF5 keeps a byte-range lock on an open file and the
    /// copy fails with `ERROR_LOCK_VIOLATION`. Neither is a real constraint: the
    /// point of this call is that nothing needs to hold `src` open at all.
    ///
    /// Copying a source whose writer crashed reproduces the state that writer
    /// left, which can include catalog rows whose arrays never reached the HDF5
    /// file; [`Self::verify_integrity`] reports those as dangling.
    pub fn open_copy(src: &Path, dest: &Path, catalog: CatalogMode) -> Result<Self> {
        reject_existing_artifact(dest)?;
        let dest_sqlite = catalog_sqlite_path(dest);
        let copied = (|| -> Result<()> {
            std::fs::copy(src, dest)?;
            // A source with no catalog is the half-artifact a
            // `CatalogMode::InMemory` store leaves before its first save. Copying
            // nothing keeps the copy honest about that; opening it then fails the
            // paired-stamp check instead of quietly presenting an empty store.
            let src_sqlite = catalog_sqlite_path(src);
            if src_sqlite.exists() {
                // Deliberately not `fs::copy`. A catalog whose writer crashed
                // still holds committed rows in its `-wal`, and copying the main
                // database alone drops them — silently, because the copy then
                // opens fine and simply lists fewer series. `VACUUM INTO` reads
                // through committed WAL content, the same way `persist_to`'s
                // catalog half does, and writes one self-contained file that
                // needs no sidecar of its own. The source is opened read-only,
                // so this still never writes to it — and it is opened *without*
                // the schema-revision check, because a stale catalog is a
                // reason to take a writable copy, not a reason to refuse one.
                MetadataStore::copy_file_to(&src_sqlite, &dest_sqlite)?;
            }
            Ok(())
        })();
        // The open is inside the cleanup, not after it. A destination left
        // behind is the state `reject_existing_artifact` refuses next time,
        // stranding the caller on a path they never successfully wrote — and
        // that is just as true when the copy succeeded and the *open* failed,
        // which is what a half-artifact source (arrays with no catalog) does
        // every time.
        copied
            .and_then(|()| Self::open_with_catalog(dest, false, catalog))
            .inspect_err(|_| {
                let _ = std::fs::remove_file(dest);
                let _ = std::fs::remove_file(&dest_sqlite);
            })
    }

    /// Like [`Self::open`], but places the catalog explicitly. See
    /// [`CatalogMode`].
    ///
    /// With [`CatalogMode::InMemory`] the `<path>.sqlite` file is read into RAM
    /// and then left alone; mutations never reach it, and only
    /// [`Self::persist_to`] writes them back. Opening this way requires the
    /// catalog file to exist — unlike [`CatalogMode::Attached`], which creates
    /// an empty one when it is missing.
    ///
    /// Caution: with `read_only=false` this does **not** copy the HDF5 half, so
    /// mutations land in `path` itself and an interrupted write damages the
    /// caller's own file — HDF5 offers neither journaling nor a repair tool. A
    /// caller that means to leave the original untouched until an explicit save
    /// wants [`Self::open_copy`].
    ///
    /// # Paired generation stamps
    ///
    /// The two halves must agree: either both carry the same stamp, or neither
    /// carries one (an artifact written before stamping existed). One stamped
    /// and the other not is [`TimeSeriesError::MismatchedArtifact`] as surely as
    /// two different stamps, because every path that writes a stamp writes both
    /// halves together. A lone stamp therefore means one half was replaced,
    /// copied, or created without its partner — including the case that first
    /// motivated the check, a `persist_to` interrupted between its two renames
    /// onto a destination that predates stamping.
    pub fn open_with_catalog(path: &Path, read_only: bool, catalog: CatalogMode) -> Result<Self> {
        // One handle per artifact per process, read-only ones included: a
        // reader's column index is as stale as a writer's -- see `StoreInUse`.
        let open_guard = OpenGuard::acquire(path)?;
        let sqlite_path = catalog_sqlite_path(path);
        // Three steps, in this order, and each one is load-bearing.
        //
        // 1. Open the HDF5 half. This is where `data_format_version` is
        //    evaluated (see [`crate::version`]), and it has to come first: a
        //    store too old to migrate at all should report `IncompatibleFormat`
        //    rather than a raw SQLite error from inside a later query, and a
        //    bad path should not leave a freshly created empty `.sqlite`
        //    behind. An *upgradable* stamp is noted, not yet rewritten.
        // 2. Open the catalog, which is where the migration ladder runs
        //    (`crate::metadata::migrate`).
        // 3. Only if (2) succeeded, and only for a writable open, re-stamp the
        //    HDF5 half to the current version.
        //
        // Catalog-first is the safe order because the two stamps cannot be
        // written atomically together. If step 3 fails, the catalog is at the
        // current revision while the array file still claims the older
        // version; an older build then opens the store and simply never writes
        // a row of a type it does not know, which is harmless. The reverse
        // order would leave a store *claiming* the new format over an
        // un-migrated catalog — the exact failure the ladder exists to
        // eliminate.
        //
        // A read-only store opens both halves read-only: the HDF5 side needs
        // no write permission (works on read-only media, shared HDF5 lock) and
        // its write paths error with `ReadOnlyStore` as a backstop behind the
        // `Store::add_*` / `remove_*` guards. Such an open never re-stamps
        // anything; a stale catalog reports `CatalogMigrationRequired` from
        // step 2, which is the error that tells the caller what to do.
        let mut backend = open_backend(path, read_only)?;

        // Whether these two files are halves of the same save is settled
        // *before* either of them is written to. Opening the catalog runs the
        // ladder, and a rebuild plus a re-stamp is a lot to do to an artifact
        // that is then refused -- it mutates two of a user's files in order to
        // report that they never belonged together. `read_generation_at` reads
        // the stamp off a raw read-only connection, running no DDL.
        //
        // Only when the catalog file is actually there. A *missing* half is not
        // a stamp disagreement, and reporting "catalog: none" would bury the
        // one fact that helps -- which file is not where it should be. That
        // case belongs to the open below, which names it. The post-open check
        // still catches a catalog that opened but carries no stamp.
        if sqlite_path.exists() {
            check_generation_pair(
                backend.generation(),
                MetadataStore::read_generation_at(&sqlite_path)?,
            )?;
        }

        let metadata = match catalog {
            CatalogMode::Attached => MetadataStore::open_path(&sqlite_path, read_only)?,
            CatalogMode::InMemory => MetadataStore::open_path_into_memory(&sqlite_path, read_only)?,
        };
        if backend.pending_format_upgrade() {
            // `CatalogMode::InMemory` migrated a *copy*: the catalog on disk is
            // untouched until `persist_to`, so re-stamping the array file here
            // would break the pair the next open sees. Only an attached
            // catalog has actually been upgraded in place.
            if catalog == CatalogMode::Attached {
                backend.finish_format_upgrade()?;
            }
        }
        // Re-checked against the opened catalog, which is the connection the
        // rest of the session actually uses. The preflight above reads a
        // separate handle, so this closes the gap between the two -- and it is
        // the check that has always been here. A migration cannot change the
        // generation (migrating is not a save), so on any ordinary path the two
        // agree and this is free.
        check_generation_pair(backend.generation(), metadata.generation()?)?;
        Ok(Self {
            backend,
            metadata,
            read_only,
            file_path: Some(path.to_path_buf()),
            catalog,
            txn: None,
            _open_guard: Some(open_guard),
        })
    }

    /// Open the array half of an artifact whose catalog is **absent**, minting
    /// an empty one, and hand back a writable store holding every array and no
    /// rows.
    ///
    /// This is the way in to an artifact shipped as arrays plus a document: a
    /// consumer that already carries the association rows in JSON of its own
    /// (`system.json` beside a `time_series.h5`) has no reason to move the
    /// `.sqlite` around as well. Replay the rows into the returned store —
    /// [`Self::import_time_series_associations_openapi`] and its
    /// supplemental-attribute counterpart — and the artifact is whole again,
    /// ids included.
    ///
    /// Without this there is no such way in. [`Self::open`] refuses the pair
    /// with [`TimeSeriesError::MismatchedArtifact`], because the arrays carry a
    /// generation stamp and a catalog created on the spot does not, and that
    /// refusal is right for every case but this one: a lone stamp normally means
    /// a half-finished save.
    ///
    /// # What it refuses
    ///
    /// A catalog that is already there ([`TimeSeriesError::StoreExists`], naming
    /// it). Minting over one would discard its rows, which is
    /// [`Self::create_replacing`]'s job and nobody else's — and an existing
    /// catalog means [`Self::open`] is the call that was wanted. Delete it first
    /// to rebuild deliberately.
    ///
    /// # The stamp
    ///
    /// The fresh catalog takes the array file's *own* generation rather than a
    /// newly minted one, so the halves are paired and every later [`Self::open`]
    /// behaves normally. An unstamped array file leaves the catalog unstamped
    /// too — the "both unstamped" pairing an artifact predating the stamp has.
    ///
    /// Never read-only: writing the rows back is the entire purpose.
    pub fn open_without_catalog(path: &Path, catalog: CatalogMode) -> Result<Self> {
        // One handle per artifact per process, exactly as `open_with_catalog`:
        // the arrays are opened in place, so a second handle on them would be
        // the same stale-index hazard `StoreInUse` refuses everywhere else.
        let open_guard = OpenGuard::acquire(path)?;
        let sqlite_path = catalog_sqlite_path(path);
        if sqlite_path.exists() {
            return Err(TimeSeriesError::StoreExists {
                path: sqlite_path.display().to_string(),
            });
        }
        let mut backend = open_backend(path, false)?;
        let metadata = match catalog {
            CatalogMode::Attached => MetadataStore::open_path(&sqlite_path, false)?,
            CatalogMode::InMemory => MetadataStore::open_in_memory()?,
        };
        // A catalog born from the current DDL is at the current revision, so
        // there is no ladder to climb — but an array file at an older, still
        // upgradable format stamp is waiting on exactly that, and the catalog
        // beside it now *is* current. Discharge it on the same terms
        // `open_with_catalog` does: only an attached catalog has actually
        // landed on disk.
        if backend.pending_format_upgrade() && catalog == CatalogMode::Attached {
            backend.finish_format_upgrade()?;
        }
        if let Some(generation) = backend.generation() {
            metadata.set_generation(&generation)?;
        }
        Ok(Self {
            backend,
            metadata,
            read_only: false,
            file_path: Some(path.to_path_buf()),
            catalog,
            txn: None,
            _open_guard: Some(open_guard),
        })
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Where this store's catalog lives. See [`CatalogMode`].
    pub fn catalog_mode(&self) -> CatalogMode {
        self.catalog
    }

    /// The `data_format_version` the array file actually carries.
    ///
    /// Not the same thing as [`crate::DATA_FORMAT_VERSION`], which is what this
    /// *build* writes. An upgradable stamp is left in place until the catalog
    /// migration succeeds, and a read-only open never re-stamps at all, so a
    /// store can legitimately be open and readable while its file says
    /// something older. Report this, not the constant.
    ///
    /// `None` for a backend with no version stamp -- an in-memory store.
    pub fn data_format_version(&self) -> Option<String> {
        self.backend.stored_format_version()
    }

    /// The catalog's schema revision, the SQLite half's counterpart to
    /// [`Self::data_format_version`].
    ///
    /// A writable open brings this to
    /// [`CATALOG_SCHEMA_REVISION`](crate::metadata::migrate::CATALOG_SCHEMA_REVISION)
    /// before returning, so on a store opened for writing this always reports
    /// the current revision. See [`crate::metadata::migrate`].
    pub fn catalog_schema_revision(&self) -> Result<i64> {
        self.metadata.schema_revision()
    }

    /// The SQLite savepoint scoping transaction nesting level `depth`.
    fn txn_savepoint(depth: usize) -> String {
        format!("infrastore_txn_{depth}")
    }

    /// True while a cross-operation transaction is open.
    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// Begin a transaction spanning any number of subsequent operations, so that
    /// adds, removals, and transforms either all take effect or none do.
    ///
    /// Every mutating entry point is already atomic on its own; this composes
    /// several of them into one unit. It does *not* replace [`Self::bulk_add`] —
    /// batching is still what buys block-sized HDF5 writes and feature-set
    /// dedup, and a loop of single adds under a transaction gets neither. Open a
    /// transaction when you need several *operations* to be atomic together, and
    /// keep using a bulk add for each one.
    ///
    /// Both halves of the artifact roll back, by different means. SQLite rolls
    /// back its own statements. The HDF5 side, which has no transaction to
    /// enlist, is instead made append-only for the duration: arrays written here
    /// are removed on rollback, and arrays that removals leave unreferenced are
    /// not freed until the outermost commit — so a rollback restores catalog rows
    /// whose data is still there. Removals are therefore fully reversible, which
    /// they are not outside a transaction.
    ///
    /// Reads inside the transaction see its uncommitted writes, because they go
    /// through the same connection. Callers need no staging overlay of their own.
    ///
    /// Calls nest: an inner [`Self::begin_transaction`] opens a nested savepoint,
    /// and only the outermost commit makes anything durable.
    ///
    /// # Cost
    ///
    /// A transaction is also how a caller adding many series amortizes the
    /// per-add HDF5 flush (see `flush_arrays_before_commit`): the flush
    /// happens once for the span instead of once per call. Measured here on
    /// 2000 single adds of a 24-step `f64` `SingleTimeSeries` against an
    /// on-disk store, release build: ~1.07 s one at a time, ~0.12 s inside one
    /// transaction. A bulk add does the same. The cost scales with the bytes a
    /// call actually wrote, so re-adding data the store already holds is
    /// already cheap.
    ///
    /// # Concurrency
    ///
    /// This holds the SQLite write lock until the outermost commit or rollback.
    /// Another writer on the same artifact — a CLI invocation, another process —
    /// will block and then fail once its `busy_timeout` expires. Keep
    /// transactions to the span that actually needs atomicity rather than
    /// wrapping a whole session in one.
    ///
    /// # Errors
    ///
    /// [`TimeSeriesError::ReadOnlyStore`] if the store is read-only.
    pub fn begin_transaction(&mut self) -> Result<()> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let depth = self.txn.as_ref().map_or(0, |t| t.depth);
        self.metadata
            .execute_txn_stmt(&format!("SAVEPOINT {};", Self::txn_savepoint(depth)))?;
        let txn = self.txn.get_or_insert_with(OpenTxn::default);
        txn.marks.push(Mark {
            arrays: txn.staged_hashes.len(),
            timestamps: txn.staged_timestamps.len(),
        });
        txn.depth = depth + 1;
        tracing::debug!(depth = depth + 1, "transaction begun");
        Ok(())
    }

    /// Commit the innermost open transaction. Committing the outermost one makes
    /// the whole span durable and applies the array frees deferred by any
    /// removals it performed.
    ///
    /// # Errors
    ///
    /// [`TimeSeriesError::InvalidParameter`] if no transaction is open.
    pub fn commit_transaction(&mut self) -> Result<()> {
        let depth = self.txn_depth()? - 1;
        if depth == 0 {
            // The outermost release is the durable commit; see
            // `flush_arrays_before_commit`, which deferred to here. Flushed
            // before the pending frees are taken out of the transaction below,
            // so a flush failure leaves the bookkeeping intact for a retry or a
            // rollback rather than dropping candidates that were never freed.
            self.backend.flush()?;
        }
        // Decide what to free *before* releasing, while the transaction's view of
        // the catalog is still the one the commit is about to make permanent.
        let (to_free, axes_to_free) = if depth == 0 {
            (
                self.unreferenced(
                    |t| std::mem::take(&mut t.pending_free).into_iter().collect(),
                    references_to_in_tx,
                )?,
                self.unreferenced(
                    |t| {
                        std::mem::take(&mut t.pending_free_timestamps)
                            .into_iter()
                            .collect()
                    },
                    timestamp_references_in_tx,
                )?,
            )
        } else {
            (Vec::new(), Vec::new())
        };
        self.metadata
            .execute_txn_stmt(&format!("RELEASE {};", Self::txn_savepoint(depth)))?;
        if depth > 0 {
            // The level's writes survive into the enclosing one, so its mark is
            // simply dropped rather than acted on.
            let txn = self.txn.as_mut().expect("checked above");
            txn.marks.pop();
            txn.depth = depth;
            return Ok(());
        }
        self.txn = None;
        for hash in &to_free {
            self.backend.remove_array(hash)?;
        }
        for hash in &axes_to_free {
            self.backend.remove_timestamps(hash)?;
        }
        tracing::debug!(
            freed = to_free.len(),
            axes_freed = axes_to_free.len(),
            "transaction committed"
        );
        Ok(())
    }

    /// Roll back the innermost open transaction, undoing every operation it
    /// covered. Rolling back the outermost one also removes the arrays it wrote
    /// and abandons its deferred frees, leaving both halves of the artifact as
    /// they were when it began.
    ///
    /// # Errors
    ///
    /// [`TimeSeriesError::InvalidParameter`] if no transaction is open.
    pub fn rollback_transaction(&mut self) -> Result<()> {
        let depth = self.txn_depth()? - 1;
        let name = Self::txn_savepoint(depth);
        // ROLLBACK TO rewinds to the savepoint but leaves it on the stack, so it
        // must be released to actually pop this nesting level.
        self.metadata
            .execute_txn_stmt(&format!("ROLLBACK TO {name}; RELEASE {name};"))?;
        if depth > 0 {
            // The catalog has unwound this level, so the arrays and time axes it
            // wrote are as unreachable as an outermost rollback's — free them on
            // the same terms rather than leaving them for the outer commit,
            // which only ever looks at the pending-free sets and would strand
            // them in the file. `unreferenced` rechecks each one, so a hash that
            // predates this level, or that an enclosing level also wrote, is
            // kept.
            let mark = {
                let txn = self.txn.as_mut().expect("checked above");
                txn.depth = depth;
                txn.marks.pop().unwrap_or_default()
            };
            let to_free = self.unreferenced(
                |t| t.staged_hashes.split_off(mark.arrays),
                references_to_in_tx,
            )?;
            let axes_to_free = self.unreferenced(
                |t| t.staged_timestamps.split_off(mark.timestamps),
                timestamp_references_in_tx,
            )?;
            for hash in &to_free {
                self.backend.remove_array(hash)?;
            }
            for hash in &axes_to_free {
                self.backend.remove_timestamps(hash)?;
            }
            tracing::debug!(
                depth,
                removed = to_free.len(),
                axes_removed = axes_to_free.len(),
                "inner transaction rolled back"
            );
            return Ok(());
        }
        // The catalog is back to its pre-transaction state, so anything this
        // transaction wrote is now unreferenced and must go. Recheck rather than
        // trusting the staged lists: an array or an axis can predate the
        // transaction and have been re-referenced by a rolled-back add.
        let to_free = self.unreferenced(
            |t| std::mem::take(&mut t.staged_hashes).into_iter().collect(),
            references_to_in_tx,
        )?;
        let axes_to_free = self.unreferenced(
            |t| {
                std::mem::take(&mut t.staged_timestamps)
                    .into_iter()
                    .collect()
            },
            timestamp_references_in_tx,
        )?;
        // Deferred frees are abandoned: rollback restored the rows pointing at
        // those arrays, so the data must stay.
        self.txn = None;
        for hash in &to_free {
            self.backend.remove_array(hash)?;
        }
        for hash in &axes_to_free {
            self.backend.remove_timestamps(hash)?;
        }
        tracing::debug!(
            removed = to_free.len(),
            axes_removed = axes_to_free.len(),
            "transaction rolled back"
        );
        Ok(())
    }

    /// The current nesting depth, or an error when no transaction is open.
    fn txn_depth(&self) -> Result<usize> {
        self.txn.as_ref().map(|t| t.depth).ok_or_else(|| {
            TimeSeriesError::InvalidParameter("no transaction is open on this store".into())
        })
    }

    /// Take a set of candidate hashes off the open transaction with `take`, and
    /// return those the catalog no longer references.
    ///
    /// `count` is what "references" means for the kind of hash being taken: an
    /// array is referenced through `data_hash`, an explicit time axis through
    /// `timestamps_hash`. Both are counted inside the same savepoint, against
    /// the catalog as this commit or rollback has just left it.
    fn unreferenced(
        &mut self,
        take: impl FnOnce(&mut OpenTxn) -> Vec<[u8; 32]>,
        count: impl Fn(&rusqlite::Connection, &[u8; 32]) -> Result<i64>,
    ) -> Result<Vec<[u8; 32]>> {
        let candidates = take(self.txn.as_mut().expect("caller checked a txn is open"));
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let tx = self.metadata.savepoint()?;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for hash in candidates {
            if seen.insert(hash) && count(&tx, &hash)? == 0 {
                out.push(hash);
            }
        }
        tx.commit()?;
        Ok(out)
    }

    /// Record an array this call physically wrote, so an open transaction can
    /// remove it on rollback. A no-op outside a transaction, where each operation
    /// stages and unwinds its own writes.
    fn note_array_written(&mut self, hash: [u8; 32]) {
        if let Some(txn) = self.txn.as_mut() {
            txn.staged_hashes.push(hash);
        }
    }

    /// Close out a write call: hand its staged writes to an enclosing
    /// transaction if it succeeded, or undo them if it did not.
    ///
    /// This is what makes "all-or-nothing" hold for *every* way a write can
    /// fail, not just the metadata insert. A failing `put_array` part-way
    /// through a batch, a backend error while staging a time axis, and a
    /// `commit` that does not take all leave writes behind otherwise — the
    /// catalog rolls itself back through the savepoint's `Drop`, and the file
    /// has no transaction to enlist, so the unwinding has to be explicit and has
    /// to cover the `?` exits.
    ///
    /// Removal failures during the unwind are swallowed deliberately: the
    /// original error is what the caller needs, and a store that cannot remove
    /// what it just wrote has a bigger problem than an orphaned array, which
    /// `compact` reclaims anyway.
    fn settle<T>(&mut self, staged: StagedWrites, result: Result<T>) -> Result<T> {
        match result {
            Ok(value) => {
                // Outside a transaction these are no-ops: the call has already
                // unwound its own writes on every failing path.
                for hash in staged.arrays {
                    self.note_array_written(hash);
                }
                for hash in staged.timestamps {
                    self.note_timestamps_written(hash);
                }
                Ok(value)
            }
            Err(e) => {
                for hash in &staged.arrays {
                    let _ = self.backend.remove_array(hash);
                }
                for hash in &staged.timestamps {
                    let _ = self.backend.remove_timestamps(hash);
                }
                Err(e)
            }
        }
    }

    /// [`Self::note_array_written`] for an explicit time axis. Same contract:
    /// only a vector this call physically wrote is recorded, so a rollback
    /// removes what it added and leaves what it found.
    fn note_timestamps_written(&mut self, hash: [u8; 32]) {
        if let Some(txn) = self.txn.as_mut() {
            txn.staged_timestamps.push(hash);
        }
    }

    /// Free `hash`, or defer the free to the outermost commit when a transaction
    /// is open — while it is, a rollback can still restore the associations that
    /// reference the array, so its bytes have to survive.
    fn free_or_defer(&mut self, hash: [u8; 32]) -> Result<()> {
        match self.txn.as_mut() {
            Some(txn) => {
                txn.pending_free.insert(hash);
                Ok(())
            }
            None => self.backend.remove_array(&hash),
        }
    }

    /// [`Self::free_or_defer`] for an explicit time axis, deferred on the same
    /// terms: a rollback restores the rows that sat on it, so it has to survive
    /// while the transaction is open.
    fn free_or_defer_timestamps(&mut self, hash: [u8; 32]) -> Result<()> {
        match self.txn.as_mut() {
            Some(txn) => {
                txn.pending_free_timestamps.insert(hash);
                Ok(())
            }
            None => self.backend.remove_timestamps(&hash),
        }
    }

    /// The compression policy applied to newly written arrays. For a store
    /// opened from disk this reflects the policy persisted at creation (restored
    /// from the file); in-memory stores report [`Compression::None`].
    pub fn compression(&self) -> Compression {
        self.backend.compression()
    }

    /// The filesystem path backing this store's HDF5 array file, or `None`
    /// for an in-memory store.
    pub fn file_path(&self) -> Option<&Path> {
        self.file_path.as_deref()
    }

    /// Mirrors the spec's `add_time_series` signature; the public surface is
    /// intentionally wide here. Use [`AddRequest`] + [`Self::add_time_series_bulk`]
    /// for ergonomic call sites.
    ///
    /// **Adding many series one at a time is the slow path.** Each call outside
    /// a transaction flushes the HDF5 file before its catalog row commits, so a
    /// row can never name bytes the file did not receive (see
    /// `flush_arrays_before_commit`), and that flush costs roughly the same
    /// whether it pushes one array or a thousand — it is a walk of libhdf5's
    /// metadata cache, not a write proportional to what changed. Wrap a run of
    /// adds in [`Self::begin_transaction`], or use
    /// [`Self::add_time_series_bulk`], and the flush happens once for the whole
    /// span instead of once per series. Measured on 400 hourly week-long f64
    /// series against an on-disk store, release build: ~2.1 s one at a time
    /// against ~61 ms inside one transaction, with the bulk path unchanged from
    /// before the flush existed.
    pub fn add_time_series(
        &mut self,
        owner_id: i64,
        owner_type: &str,
        owner_category: OwnerCategory,
        data: TimeSeriesData,
        features: Features,
    ) -> Result<TimeSeriesId> {
        self.add_per_column(vec![AddRequest {
            owner_id,
            owner_type: owner_type.to_string(),
            owner_category,
            data,
            features,
        }])
        .map(|mut added| added.remove(0))
    }

    /// Add one time series from an [`AddRequest`]. Equivalent to
    /// [`Self::add_time_series`] — both preserve the series' `element_type`,
    /// `units`, `quantity_kind`, `unit_system`, `component_field`, and
    /// `application_data`, since those travel on the [`TimeSeriesData`] itself.
    /// Routed through the same per-column path, including its per-call flush —
    /// see [`Self::add_time_series`] on batching a run of these.
    pub fn add(&mut self, request: AddRequest) -> Result<TimeSeriesId> {
        self.add_per_column(vec![request])
            .map(|mut added| added.remove(0))
    }

    /// Bulk insert. All-or-nothing: any error rolls back every association and
    /// array put performed in this call.
    ///
    /// This is a managed batch, so it takes the block-write path
    /// ([`Self::bulk_add`] internals): packed series are packed into batch-sized
    /// datasets that fill whole chunks. A one-at-a-time un-managed loop should use
    /// [`Self::add_time_series`], which packs incrementally into shared datasets.
    #[tracing::instrument(skip(self, items), fields(count = items.len()))]
    pub fn add_time_series_bulk(&mut self, items: Vec<AddRequest>) -> Result<Vec<TimeSeriesId>> {
        self.flush_bulk_add(items)
    }

    /// Per-column insert used by single [`Self::add_time_series`] calls: each
    /// packed array is dropped into the first free slot of a shared, default-width
    /// dataset (created on demand, spilling once full). This keeps incremental
    /// un-managed adds space-efficient and still grouped for read-by-timestamp,
    /// at the cost of a per-column read-modify-write under the timestamp-major
    /// chunking. All-or-nothing, like [`Self::add_time_series_bulk`].
    #[tracing::instrument(skip(self, items), fields(count = items.len()))]
    fn add_per_column(&mut self, items: Vec<AddRequest>) -> Result<Vec<TimeSeriesId>> {
        let mut staged = StagedWrites::default();
        let result = self.add_per_column_staged(items, &mut staged);
        self.settle(staged, result)
    }

    /// The body of [`Self::add_per_column`], recording what it physically wrote
    /// into `staged`. Every exit — including the `?` ones — hands `staged` back
    /// to [`Self::settle`], which is what makes the all-or-nothing claim true
    /// for the failures that are not the metadata insert.
    fn add_per_column_staged(
        &mut self,
        items: Vec<AddRequest>,
        staged: &mut StagedWrites,
    ) -> Result<Vec<TimeSeriesId>> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }

        // Derive (and validate) every item's parts before writing anything, so a
        // bad request part-way through the batch cannot leave an array behind.
        let mut parts: Vec<RequestParts> = items
            .iter()
            .map(build_request_parts)
            .collect::<Result<_>>()?;
        resolve_irregular_layouts(&*self.backend, &items, &mut parts);

        let tx = self.metadata.savepoint()?;
        let mut added = Vec::with_capacity(items.len());
        // Feature sets and timestamp vectors are shared, and a batch typically
        // spans only a handful of distinct ones; write each once rather than
        // once per item.
        let mut shared_sets = SharedSetCache::default();

        for (item, part) in items.iter().zip(parts) {
            let RequestParts {
                hash,
                group,
                layout,
                meta,
            } = part;
            let data = request_array(item);

            let already_present = self.backend.contains(&hash)?;
            tracing::debug!(
                owner = item.owner_id,
                bytes = data.bytes.len(),
                packed = layout.is_packed(),
                already_present,
                "backend put_array",
            );
            // The explicit time axis goes in before the row that names it, for
            // the same reason the array does: a committed row must never name
            // something the file does not hold.
            stage_timestamp_vector(
                &mut *self.backend,
                group,
                meta.timestamps.as_deref(),
                &mut shared_sets,
                staged,
            )?;
            self.backend.put_array(&hash, data, group, layout)?;
            if !already_present {
                staged.arrays.push(hash);
            }

            let id = TimeSeriesId(insert_association(&tx, &meta, &mut shared_sets)?);
            added.push(id);
        }

        flush_arrays_before_commit(&mut *self.backend, staged, self.txn.is_some())?;
        tx.commit()?;
        tracing::debug!(count = added.len(), "transaction committed");
        Ok(added)
    }

    /// Begin a buffered bulk add. Requests pushed onto the returned [`BulkAdd`]
    /// are accumulated in memory and written together by [`BulkAdd::commit`],
    /// which packs each shape group into batch-sized datasets so the timestamp-
    /// major chunks are filled whole rather than one slow column at a time.
    /// Dropping the guard without committing discards the buffer (writes nothing).
    pub fn bulk_add(&mut self) -> BulkAdd<'_> {
        BulkAdd {
            store: self,
            items: Vec::new(),
            committed: false,
        }
    }

    /// Flush a buffered bulk add: write every array — packed types as batch-sized
    /// blocks (one or more datasets per shape group, chunks filled whole),
    /// standalone types individually — then insert all associations in one
    /// transaction. All-or-nothing: any metadata error rolls the transaction back
    /// and removes every array staged in this call.
    #[tracing::instrument(skip(self, items), fields(count = items.len()))]
    fn flush_bulk_add(&mut self, items: Vec<AddRequest>) -> Result<Vec<TimeSeriesId>> {
        let mut staged = StagedWrites::default();
        let result = self.flush_bulk_add_staged(items, &mut staged);
        self.settle(staged, result)
    }

    /// The body of [`Self::flush_bulk_add`]. See
    /// [`Self::add_per_column_staged`] for why it is split this way.
    fn flush_bulk_add_staged(
        &mut self,
        items: Vec<AddRequest>,
        staged: &mut StagedWrites,
    ) -> Result<Vec<TimeSeriesId>> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        if items.is_empty() {
            return Ok(Vec::new());
        }

        // Derive parts (validates + hashes) for every item, aligned to `items`.
        let mut parts: Vec<RequestParts> = items
            .iter()
            .map(build_request_parts)
            .collect::<Result<_>>()?;
        resolve_irregular_layouts(&*self.backend, &items, &mut parts);
        // Shared across the whole call: the timestamp vectors are written in the
        // loop below, the feature sets by `insert_association` further down, and
        // both are deduplicated over the same batch.
        let mut shared_sets = SharedSetCache::default();
        for part in &parts {
            stage_timestamp_vector(
                &mut *self.backend,
                part.group,
                part.meta.timestamps.as_deref(),
                &mut shared_sets,
                staged,
            )?;
        }

        // Group packed inputs by their pool — `(dtype, element_shape, length)`
        // plus the time axis — and write each as one or more batch-sized blocks.
        // Standalone inputs (dense forecasts, and irregular series on an
        // unshared axis) keep the per-array path.
        let mut packed_groups: HashMap<PoolKey, Vec<usize>> = HashMap::new();
        for (i, p) in parts.iter().enumerate() {
            let array = request_array(&items[i]);
            if p.layout.is_packed() {
                packed_groups
                    .entry(pool_key(array, p.group))
                    .or_default()
                    .push(i);
            } else {
                let already = self.backend.contains(&p.hash)?;
                self.backend.put_array(&p.hash, array, p.group, p.layout)?;
                if !already {
                    staged.arrays.push(p.hash);
                }
            }
        }
        for (pool, idxs) in &packed_groups {
            let hashes: Vec<[u8; 32]> = idxs.iter().map(|&i| parts[i].hash).collect();
            let arrays: Vec<&TypedArray> = idxs.iter().map(|&i| request_array(&items[i])).collect();
            let written = self.backend.put_packed_block(&hashes, &arrays, pool.3)?;
            for (j, &i) in idxs.iter().enumerate() {
                if written[j] {
                    staged.arrays.push(parts[i].hash);
                }
            }
        }

        // Insert associations in input order; roll the whole batch back on error.
        let tx = self.metadata.savepoint()?;
        let mut ids = Vec::with_capacity(parts.len());
        for p in &parts {
            ids.push(TimeSeriesId(insert_association(
                &tx,
                &p.meta,
                &mut shared_sets,
            )?));
        }
        flush_arrays_before_commit(&mut *self.backend, staged, self.txn.is_some())?;
        tx.commit()?;
        tracing::debug!(count = parts.len(), "bulk-add transaction committed");
        Ok(parts.into_iter().zip(ids).map(|(_, id)| id).collect())
    }

    /// A `DeterministicSingleTimeSeries` is a view over a stored
    /// `SingleTimeSeries` array, so a removal must not leave a DST whose array
    /// no `SingleTimeSeries` association backs any more. Called inside the
    /// removal transaction (after the deletes) with the arrays the removed
    /// `SingleTimeSeries` rows resolved to; errors roll the transaction back.
    /// The check is on the post-removal state, so a batch that removes the DST
    /// together with its backing series passes regardless of order.
    /// Owner-scoped `clear_time_series` is deliberately exempt: it drops every
    /// association of the owner at once.
    ///
    /// The unit is the **forecast family** — `(owner, name, resolution,
    /// features)`, the tuple `transform_single_time_series` files the view under
    /// beside its source — *and* the array both halves reference. A derived view
    /// shares both with its source, and either alone admits a false match:
    /// keying on the hash let two owners' byte-identical `SingleTimeSeries`
    /// stand in for each other, while keying on the family alone pinned a
    /// `SingleTimeSeries` that merely shares the family with a view copied there
    /// over a different array — a row [`Self::copy_time_series`] writes on
    /// purpose. See [`crate::metadata::forecast_family_conflict_on_array`].
    fn check_no_orphaned_dst(
        tx: &rusqlite::Connection,
        removed_sts: impl IntoIterator<Item = crate::metadata::DeletedRow>,
    ) -> Result<()> {
        let mut seen = HashSet::new();
        for row in removed_sts {
            let family = (
                row.owner_id,
                row.owner_category,
                row.name.clone(),
                row.resolution,
                row.features_hash,
                row.data_hash,
            );
            if !seen.insert(family) {
                continue;
            }
            let probe =
                |ts_type| crate::metadata::forecast_family_conflict_on_array(tx, &row, ts_type);
            if probe(TimeSeriesType::DeterministicSingleTimeSeries)?
                && !probe(TimeSeriesType::SingleTimeSeries)?
            {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "cannot remove SingleTimeSeries '{}' (owner {}): it backs a \
                     DeterministicSingleTimeSeries; remove the derived forecast first",
                    row.name, row.owner_id
                )));
            }
        }
        Ok(())
    }

    /// The arrays a removal left unreferenced, decided inside the removal
    /// transaction after *all* the deletes so a hash referenced only by other
    /// rows removed in the same batch is reclaimed too. Deduplicated, so a hash
    /// removed via several rows is checked (and dropped) once.
    fn unreferenced_after_removal(
        tx: &rusqlite::Connection,
        removed_hashes: &[[u8; 32]],
    ) -> Result<Vec<[u8; 32]>> {
        let mut to_drop = Vec::new();
        let mut seen = HashSet::new();
        for h in removed_hashes {
            if seen.insert(*h) && references_to_in_tx(tx, h)? == 0 {
                to_drop.push(*h);
            }
        }
        Ok(to_drop)
    }

    /// Remove every association named by its catalog `id`, in one
    /// all-or-nothing transaction, dropping each underlying array that no
    /// surviving association references (exactly like
    /// [`Self::remove_time_series`]). Returns the number of associations
    /// removed.
    ///
    /// The removal-direction counterpart of [`Self::read_by_ids`]: a consumer
    /// that recorded ids in its own model retires one without reconstructing
    /// the key it was filed under. An id names exactly one row, so this is also
    /// the precise removal — [`Self::remove_time_series`] takes a key, whose
    /// NULL interval matches any interval and can therefore sweep a whole
    /// forecast family.
    ///
    /// [`TimeSeriesError::NotFound`] if any id names no row, rolling the whole
    /// batch back — a stale reference means the caller's model disagrees with
    /// the store, and ids are never reissued, so it cannot come to name a
    /// different series later. Sift the set with [`Self::association_exists`]
    /// first when some references are expected to have gone. A repeated id is
    /// removed (and counted) once.
    #[tracing::instrument(skip(self, ids), fields(count = ids.len()))]
    pub fn remove_by_ids(&mut self, ids: &[TimeSeriesId]) -> Result<usize> {
        self.remove_by_ids_inner(ids, None)
    }

    /// [`Self::remove_by_ids`], but removing only rows that belong to `owner` —
    /// every id is confirmed against the catalog and deleted inside one
    /// transaction, and a single mismatch rolls the whole batch back with
    /// [`TimeSeriesError::OwnerMismatch`].
    ///
    /// For the consumer whose model addresses a series by id but *reasons* about
    /// it as one owner's — "retire this component's series" — where the id alone
    /// is the wrong request. Such a caller cannot assemble this out of the
    /// unguarded parts: an id survives [`Self::replace_owner`], so
    /// a `get_metadata_by_id` that confirms the owner and a `remove_by_ids` that
    /// then deletes are two calls with a window between them, and a reassignment
    /// landing in that window makes the removal retire the *new* owner's series
    /// — precisely what checking the owner was meant to prevent. Here the check
    /// and the delete are the same transaction, so there is no window.
    ///
    /// An owner is `(owner_id, owner_category)`; the category matters as well as
    /// the id, since a component and a supplemental attribute can share one.
    #[tracing::instrument(skip(self, ids), fields(count = ids.len()))]
    pub fn remove_by_ids_for_owner(
        &mut self,
        ids: &[TimeSeriesId],
        owner: (i64, OwnerCategory),
    ) -> Result<usize> {
        self.remove_by_ids_inner(ids, Some(owner))
    }

    /// The shared body of the two id-addressed removals. `expected_owner` is the
    /// guard the [`Self::remove_by_ids_for_owner`] form sets; `None` removes
    /// whatever the ids name.
    fn remove_by_ids_inner(
        &mut self,
        ids: &[TimeSeriesId],
        expected_owner: Option<(i64, OwnerCategory)>,
    ) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        let mut removed_hashes: Vec<[u8; 32]> = Vec::with_capacity(ids.len());
        let mut removed_sts: Vec<crate::metadata::DeletedRow> = Vec::new();
        let mut seen_ids = HashSet::new();
        for &id in ids {
            if !seen_ids.insert(id) {
                continue;
            }
            // Dropping the tx rolls the batch back — an owner mismatch on the
            // last id undoes the deletes the earlier ones already did.
            let Some(row) = MetadataStore::delete_by_id(&tx, id.get(), expected_owner)? else {
                return Err(TimeSeriesError::NotFound);
            };
            removed_hashes.push(row.data_hash);
            if row.time_series_type == TimeSeriesType::SingleTimeSeries {
                removed_sts.push(row);
            }
        }
        // Checked after all deletes, so a batch removing a DST together with
        // its backing series passes regardless of order.
        Self::check_no_orphaned_dst(&tx, removed_sts)?;
        let to_drop = Self::unreferenced_after_removal(&tx, &removed_hashes)?;
        tx.commit()?;
        for h in to_drop {
            self.free_or_defer(h)?;
        }
        Ok(removed_hashes.len())
    }

    /// Remove every time series matching `filter` in one all-or-nothing
    /// transaction, dropping newly unreferenced arrays like
    /// [`Self::remove_by_ids`]. Returns the number of associations removed;
    /// an empty match is `Ok(0)`.
    ///
    /// The one removal that does not take ids, because enumerating them first
    /// is the wrong shape for "remove everything matching": the filter is the
    /// request. It resolves to ids internally and removes those, so the two
    /// removals share one path.
    pub fn remove_by_filter(&mut self, filter: ListFilter) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let ids: Vec<TimeSeriesId> = self
            .list_metadata(filter)?
            .into_iter()
            .filter_map(|m| m.id)
            .collect();
        if ids.is_empty() {
            return Ok(0);
        }
        self.remove_by_ids(&ids)
    }

    /// Remove every time series for the owner `(owner_id, owner_category)`, or
    /// every time series in the store when `owner` is `None`. Returns the count
    /// removed.
    ///
    /// Unlike the targeted removals, this also reclaims the explicit time axes
    /// the clear left unreferenced: it orphans them wholesale, and a cleared
    /// store may never see the compaction that would otherwise sweep them.
    pub fn clear_time_series(&mut self, owner: Option<(i64, OwnerCategory)>) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        let removed = match owner {
            Some((id, category)) => MetadataStore::delete_by_owner(&tx, id, category)?,
            None => MetadataStore::delete_all(&tx)?,
        };
        let count = removed.len();
        let mut to_drop = Vec::new();
        for h in &removed {
            if references_to_in_tx(&tx, h)? == 0 {
                to_drop.push(*h);
            }
        }
        tx.commit()?;
        for h in to_drop {
            self.free_or_defer(h)?;
        }
        // Clearing is the one removal that reclaims time axes eagerly, for the
        // reason the feature sets go in the same breath: it orphans them
        // wholesale, and a cleared store may never see a compaction. Every other
        // removal leaves an unreferenced axis for `compact`, because one series
        // going does not say the cohort is empty.
        for h in self.orphaned_timestamp_vectors()? {
            self.free_or_defer_timestamps(h)?;
        }
        Ok(count)
    }

    /// Reassign every time series owned by `old_owner` to `new_owner`. The
    /// underlying arrays are content-addressed and shared, so only the
    /// association rows change. Returns the number of associations updated.
    pub fn replace_owner(
        &mut self,
        old_owner: i64,
        new_owner: i64,
        owner_category: OwnerCategory,
    ) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        // Moving an owner's rows can land a `Deterministic` in a family that
        // already holds the `DeterministicSingleTimeSeries` view of the same
        // series, or the reverse — a state the rest of the code treats as
        // unreachable. Checked over the whole moved set, because the move is one
        // `UPDATE`.
        if let Some((name, moving, existing)) =
            crate::metadata::forecast_family_conflict_on_owner_move(
                &tx,
                old_owner,
                new_owner,
                owner_category,
            )?
        {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "cannot move owner {old_owner} to {new_owner}: '{name}' would put a {} \
                 and a {} in the same series family; they are mutually exclusive",
                moving.as_str(),
                existing.as_str(),
            )));
        }
        let updated = MetadataStore::replace_owner(&tx, old_owner, new_owner, owner_category)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Copy an existing association onto another owner, optionally renaming it.
    ///
    /// Arrays are content-addressed, so this writes only a new association row
    /// pointing at the same `data_hash`: no array data is duplicated. Every
    /// descriptive column is carried over verbatim — crucially the
    /// `time_series_type`, so a `DeterministicSingleTimeSeries` stays one instead
    /// of being materialized into a dense `Deterministic` (which is what a
    /// read-then-write copy through the bindings would produce).
    ///
    /// The copy keeps the source's `owner_category`; `new_name` defaults to the
    /// source name. Fails with `DuplicateTimeSeries` if the destination already
    /// holds a series with the same identity.
    #[tracing::instrument(skip(self), fields(src_id = src.get()))]
    pub fn copy_time_series(
        &mut self,
        src: TimeSeriesId,
        dst_owner_id: i64,
        dst_owner_type: &str,
        new_name: Option<&str>,
    ) -> Result<TimeSeriesId> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }

        let mut meta = self
            .metadata
            .get_by_id(src.get(), &*self.backend)?
            .ok_or(TimeSeriesError::NotFound)?;
        meta.owner_id = dst_owner_id;
        meta.owner_type = dst_owner_type.to_string();
        if let Some(name) = new_name {
            meta.name = name.to_string();
        }
        // The copy is its own catalog row and gets its own id. `meta` came from
        // a read, so its `id` is the *source's* — carrying it over would make
        // this an explicit-id insert of an id that is by definition already
        // taken, and the primary key would reject it. Every column the copy
        // does keep is descriptive; this one describes the row.
        meta.id = None;

        let dst = KeyIdentity {
            owner_id: meta.owner_id,
            owner_category: meta.owner_category,
            time_series_type: meta.time_series_type,
            name: meta.name.clone(),
            resolution: meta.resolution,
            interval: meta.interval,
            features: meta.features.clone(),
        };
        if self.metadata.exists(&identity_filter(&dst))? {
            return Err(TimeSeriesError::DuplicateTimeSeries);
        }

        let tx = self.metadata.savepoint()?;
        check_forecast_family_free(&tx, &meta, "copy")?;
        // A `DeterministicSingleTimeSeries` copies as itself, source or no
        // source at the destination (a hybrid system copies a subcomponent's
        // view under a prefixed name and never the series behind it). The copy
        // holds the array by hash like any row, so it is never dangling; what
        // it lacks is a source in its own family, which only matters to the
        // removal guard -- and that guard is per family, so the copy neither
        // pins nor is pinned by anyone else's source.
        let id = MetadataStore::insert(&tx, &meta)?;
        tx.commit()?;

        Ok(TimeSeriesId(id))
    }

    /// Insert association rows verbatim, filing each under the `id` it carries.
    ///
    /// The write half of the OpenAPI document round trip (see
    /// [`Self::import_time_series_associations_openapi`], which owns the wire
    /// spelling and calls this). Rows only: every row must name an array the
    /// store already holds, because the document carries locators, never
    /// values. The arrays arrive with the artifact.
    ///
    /// All-or-nothing, and validated before anything is written:
    ///
    /// - Each row's `data_hash` must be present in the backend. A row naming an
    ///   array the store does not hold would be a dangling association — the
    ///   store opens cleanly, lists the series, and reads nothing — which is
    ///   the failure [`TimeSeriesError::StoreExists`] exists to prevent,
    ///   arriving by a different door.
    /// - Each row's declared geometry must be the array's. `[length,
    ///   *element_shape]` is the native shape, so it is checked against the one
    ///   the backend holds: a document naming a real array under a length or
    ///   element shape it was not hashed from would otherwise file a row whose
    ///   metadata and data disagree.
    /// - A `NonSequentialTimeSeries` row must carry its time axis in
    ///   `timestamps`, and that axis must already be in the array file, with as
    ///   many entries as the row declares. The axis cannot be inferred from the
    ///   values: arrays are content-addressed, so two irregular series with
    ///   byte-identical values on *different* axes share one stored array, and
    ///   only `timestamps_hash` tells them apart. The wire form therefore
    ///   locates the axis explicitly (`timestamps_uri`), which
    ///   [`Self::import_time_series_associations_openapi`] resolves before
    ///   calling this.
    /// - A `DeterministicSingleTimeSeries` is a view of a `SingleTimeSeries`,
    ///   so its source must be present — in this batch or already stored. Views
    ///   are therefore written last, after the rows they may depend on.
    ///
    /// Returns the number of rows inserted.
    pub fn import_association_rows(&mut self, rows: Vec<TimeSeriesMetadata>) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        // Resolved once for the batch rather than per row: a cohort of irregular
        // series shares one axis, so this is a handful of hashes however many
        // rows name them.
        let mut stored_axes: Option<HashSet<[u8; 32]>> = None;
        for meta in &rows {
            if meta.time_series_type == TimeSeriesType::NonSequentialTimeSeries {
                let Some(timestamps) = meta.timestamps.as_deref() else {
                    return Err(TimeSeriesError::InvalidParameter(format!(
                        "cannot import NonSequentialTimeSeries '{}' (owner {}): the row names no \
                         time axis, and one cannot be inferred from the values — two irregular \
                         series with identical values on different axes share one \
                         content-addressed array. A document supplies the axis as \
                         `timestamps_uri`",
                        meta.name, meta.owner_id,
                    )));
                };
                if let Some(length) = meta.length
                    && length != timestamps.len()
                {
                    return Err(TimeSeriesError::InvalidParameter(format!(
                        "cannot import NonSequentialTimeSeries '{}' (owner {}): it declares \
                         length {length} but names a time axis of {} timestamps",
                        meta.name,
                        meta.owner_id,
                        timestamps.len(),
                    )));
                }
                let axis = crate::hash::timestamps_hash(timestamps);
                let axes = match &stored_axes {
                    Some(axes) => axes,
                    None => stored_axes.insert(self.stored_time_axes()?),
                };
                if !axes.contains(&axis) {
                    return Err(TimeSeriesError::InvalidParameter(format!(
                        "cannot import NonSequentialTimeSeries '{}' (owner {}): it names time \
                         axis {}, which this store does not hold — the axis arrives with the \
                         artifact, like the arrays",
                        meta.name,
                        meta.owner_id,
                        crate::hash::hash_hex(&axis),
                    )));
                }
            }
            if !self.backend.contains(&meta.data_hash)? {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "cannot import '{}' (owner {}): it names array {}, which this store does \
                     not hold. An import writes rows only — the arrays arrive with the \
                     artifact — so the row would be a dangling reference",
                    meta.name,
                    meta.owner_id,
                    crate::hash::hash_hex(&meta.data_hash),
                )));
            }
            // Holding the array is not the same as the row describing it. The
            // hash proves nothing about *this* row's columns: a document is
            // free to name a real array and declare a length or element shape
            // that is not the one it was hashed from, and the row would then
            // report a geometry the bytes do not have — a static read handing
            // back metadata and data that disagree, a forecast read failing
            // somewhere later with no mention of the import. A declared dtype
            // that lies is already refused on the read path (`check_dtype`);
            // this is the half that was silent.
            if let Some(length) = meta.length {
                let mut declared = Vec::with_capacity(meta.element_shape.len() + 1);
                declared.push(length);
                declared.extend_from_slice(&meta.element_shape);
                let stored = self.backend.array_shape(&meta.data_hash)?;
                if declared != stored {
                    return Err(TimeSeriesError::InvalidParameter(format!(
                        "cannot import '{}' (owner {}): it declares shape {declared:?} for \
                         array {}, which this store holds with shape {stored:?}",
                        meta.name,
                        meta.owner_id,
                        crate::hash::hash_hex(&meta.data_hash),
                    )));
                }
            }
        }

        // The same all-or-none rule as a bulk add: the schema requires
        // `association_id` on every row, so a document missing some is not one
        // this store wrote, and mixing assigned with supplied ids would make the
        // outcome depend on insertion order (which, with views deferred below,
        // is not even the document's own order).
        let explicit = rows.iter().filter(|m| m.id.is_some()).count();
        if explicit != 0 && explicit != rows.len() {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "{explicit} of {} rows carry an association_id and the rest do not; a \
                 document either supplies one for every row or for none",
                rows.len()
            )));
        }

        // Views last: a `DeterministicSingleTimeSeries` is a view of a
        // `SingleTimeSeries`, and `check_forecast_family_free` reads the rows
        // already inserted, so the order within one batch is load-bearing
        // rather than cosmetic.
        let (views, plain): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .partition(|m| m.time_series_type == TimeSeriesType::DeterministicSingleTimeSeries);

        let tx = self.metadata.savepoint()?;
        let ids: Vec<i64> = plain
            .iter()
            .chain(views.iter())
            .filter_map(|m| m.id.map(TimeSeriesId::get))
            .collect();
        MetadataStore::check_explicit_time_series_ids(&tx, &ids)?;
        let mut shared_sets = SharedSetCache::default();
        let mut inserted = 0usize;
        for meta in &plain {
            insert_association(&tx, meta, &mut shared_sets)?;
            inserted += 1;
        }
        for meta in &views {
            // A view without its source is a state `transform_single_time_series`
            // never produces, and one a later remove of the shared array's other
            // holder would leave dangling. The plain rows are already in, so
            // one family probe covers "in this document" and "already stored".
            let has_source = crate::metadata::forecast_family_conflict(
                &tx,
                meta.owner_id,
                meta.owner_category,
                &meta.name,
                meta.resolution,
                &crate::hash::features_hash(&meta.features),
                TimeSeriesType::SingleTimeSeries,
            )?;
            if !has_source {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "cannot import DeterministicSingleTimeSeries '{}' (owner {}): it is a view \
                     of a SingleTimeSeries that is neither in this document nor already stored",
                    meta.name, meta.owner_id,
                )));
            }
            insert_association(&tx, meta, &mut shared_sets)?;
            inserted += 1;
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Reconstruct the series described by `meta`, reading its array (or the
    /// requested `time_range` slice) from the backend.
    fn materialize_time_series(
        &self,
        meta: &TimeSeriesMetadata,
        time_range: Option<TimeRange>,
    ) -> Result<TimeSeriesData> {
        // The descriptors describe the series but live on the catalog row, not
        // in the array bytes. Filling them in here — once, for every variant —
        // is what makes a read round-trip what a write declared.
        let mut data = self.materialize_array(meta, time_range)?;
        data.set_descriptors(descriptors_of(meta));
        Ok(data)
    }

    /// The array-reconstruction half of [`Self::materialize_time_series`]: builds
    /// the variant from the stored bytes and the row's shape/time fields. The
    /// descriptive attributes are left unset for the caller to fill in.
    fn materialize_array(
        &self,
        meta: &TimeSeriesMetadata,
        time_range: Option<TimeRange>,
    ) -> Result<TimeSeriesData> {
        tracing::debug!(ts_type = ?meta.time_series_type, "metadata loaded");
        // Decision 8: the bound has to be spelled the way the series is, and a
        // mismatch is refused rather than coerced. Checked once here, before any
        // arithmetic, so every type and every entry point gets the same rule.
        let time_range = match time_range {
            Some(range) => {
                range.check_against(
                    meta.time_reference.as_ref(),
                    &format!("{:?} on owner {}", meta.name, meta.owner_id),
                )?;
                Some(range.bounds())
            }
            None => None,
        };
        match meta.time_series_type {
            TimeSeriesType::SingleTimeSeries => {
                let initial = meta.initial_timestamp.ok_or_else(|| {
                    TimeSeriesError::IntegrityError(
                        "SingleTimeSeries missing initial_timestamp".into(),
                    )
                })?;
                let resolution = meta.resolution.ok_or_else(|| {
                    TimeSeriesError::IntegrityError("SingleTimeSeries missing resolution".into())
                })?;
                let length = meta.length.ok_or_else(|| {
                    TimeSeriesError::IntegrityError("SingleTimeSeries missing length".into())
                })?;

                let (data, sliced_initial, sliced_length) = match time_range {
                    None => {
                        let data = self
                            .backend
                            .get_array(&meta.data_hash, meta.element_type.physical_dtype())?;
                        (data, initial, length)
                    }
                    Some((start, end)) => {
                        if end < start {
                            return Err(TimeSeriesError::InvalidParameter("end < start".into()));
                        }
                        if !resolution.is_positive() {
                            return Err(TimeSeriesError::InvalidParameter(
                                "resolution must be positive".into(),
                            ));
                        }
                        // `start` is floored and `end` is ceil-ed onto the
                        // resolution grid (calendar-aware for monthly periods); the
                        // bounds need not be grid-aligned.
                        let start_idx = resolution.floor_steps(initial, start).min(length);
                        let end_idx = resolution
                            .ceil_steps(initial, end)
                            .min(length)
                            .max(start_idx);
                        let data = self.backend.get_slice(
                            &meta.data_hash,
                            meta.element_type.physical_dtype(),
                            start_idx..end_idx,
                        )?;
                        let new_initial =
                            resolution
                                .add_to(initial, start_idx as i64)
                                .ok_or_else(|| {
                                    TimeSeriesError::IntegrityError(
                                        "sliced initial overflow".into(),
                                    )
                                })?;
                        (data, new_initial, end_idx - start_idx)
                    }
                };

                Ok(TimeSeriesData::SingleTimeSeries(SingleTimeSeries {
                    initial_timestamp: sliced_initial,
                    resolution,
                    length: sliced_length,
                    data,
                    name: meta.name.clone(),
                    // The descriptors are filled in by
                    // `materialize_time_series`; the element type is set here
                    // too because it has no "unset" spelling, and this is the
                    // value that call resolves to anyway.
                    element_type: meta.element_type,
                    units: None,
                    quantity_kind: None,
                    unit_system: None,
                    time_reference: None,
                    component_field: None,
                    application_data: None,
                }))
            }
            TimeSeriesType::NonSequentialTimeSeries => {
                let timestamps = meta.timestamps.clone().ok_or_else(|| {
                    TimeSeriesError::IntegrityError(
                        "NonSequentialTimeSeries missing timestamps".into(),
                    )
                })?;
                let length = meta.length.ok_or_else(|| {
                    TimeSeriesError::IntegrityError("NonSequentialTimeSeries missing length".into())
                })?;
                if timestamps.len() != length {
                    return Err(TimeSeriesError::IntegrityError(format!(
                        "NonSequentialTimeSeries has {} timestamps but length {length}",
                        timestamps.len()
                    )));
                }

                let (data, timestamps) = match time_range {
                    None => (
                        self.backend
                            .get_array(&meta.data_hash, meta.element_type.physical_dtype())?,
                        timestamps,
                    ),
                    Some((start, end)) => {
                        if end < start {
                            return Err(TimeSeriesError::InvalidParameter("end < start".into()));
                        }
                        let start_idx = timestamps.partition_point(|t| *t < start);
                        let end_idx = timestamps.partition_point(|t| *t < end);
                        let data = self.backend.get_slice(
                            &meta.data_hash,
                            meta.element_type.physical_dtype(),
                            start_idx..end_idx,
                        )?;
                        (data, timestamps[start_idx..end_idx].to_vec())
                    }
                };
                let series = NonSequentialTimeSeries::new(timestamps, data, meta.name.clone())
                    .map_err(TimeSeriesError::IntegrityError)?;
                Ok(TimeSeriesData::NonSequentialTimeSeries(series))
            }
            TimeSeriesType::Deterministic => {
                let arr = self
                    .backend
                    .get_array(&meta.data_hash, meta.element_type.physical_dtype())?;
                let initial = required_initial(meta, "Deterministic")?;
                let resolution = required_resolution(meta, "Deterministic")?;
                let horizon = required_horizon(meta, "Deterministic")?;
                let interval = required_interval(meta, "Deterministic")?;
                let count = required_count(meta, "Deterministic")?;
                let h = compute_h(horizon, resolution).map_err(TimeSeriesError::IntegrityError)?;
                // Validate stored shape: [H, count, *E].
                validate_forecast_shape(&arr, &[h, count], "Deterministic")?;
                let (w0, w1, window_initial) =
                    resolve_windows(initial, resolution, horizon, interval, count, time_range)?;
                let windowed = if w0 == 0 && w1 == count {
                    arr
                } else {
                    slice_count_axis(&arr, 1, w0, w1)
                };
                let det = Deterministic::new(
                    window_initial,
                    resolution,
                    horizon,
                    interval,
                    w1 - w0,
                    windowed,
                    meta.name.clone(),
                )
                .map_err(TimeSeriesError::IntegrityError)?;
                Ok(TimeSeriesData::Deterministic(det))
            }

            TimeSeriesType::Probabilistic => {
                let arr = self
                    .backend
                    .get_array(&meta.data_hash, meta.element_type.physical_dtype())?;
                let initial = required_initial(meta, "Probabilistic")?;
                let resolution = required_resolution(meta, "Probabilistic")?;
                let horizon = required_horizon(meta, "Probabilistic")?;
                let interval = required_interval(meta, "Probabilistic")?;
                let count = required_count(meta, "Probabilistic")?;
                let percentiles = meta.percentiles.clone().ok_or_else(|| {
                    TimeSeriesError::IntegrityError("Probabilistic missing percentiles".into())
                })?;
                let h = compute_h(horizon, resolution).map_err(TimeSeriesError::IntegrityError)?;
                let p = percentiles.len();
                // Validate stored shape: [P, H, count, *E].
                validate_forecast_shape(&arr, &[p, h, count], "Probabilistic")?;
                let (w0, w1, window_initial) =
                    resolve_windows(initial, resolution, horizon, interval, count, time_range)?;
                let windowed = if w0 == 0 && w1 == count {
                    arr
                } else {
                    slice_count_axis(&arr, 2, w0, w1)
                };
                let prob = Probabilistic::new(
                    window_initial,
                    resolution,
                    horizon,
                    interval,
                    w1 - w0,
                    percentiles,
                    windowed,
                    meta.name.clone(),
                )
                .map_err(TimeSeriesError::IntegrityError)?;
                Ok(TimeSeriesData::Probabilistic(prob))
            }

            TimeSeriesType::Scenarios => {
                let arr = self
                    .backend
                    .get_array(&meta.data_hash, meta.element_type.physical_dtype())?;
                let initial = required_initial(meta, "Scenarios")?;
                let resolution = required_resolution(meta, "Scenarios")?;
                let horizon = required_horizon(meta, "Scenarios")?;
                let interval = required_interval(meta, "Scenarios")?;
                let count = required_count(meta, "Scenarios")?;
                let h = compute_h(horizon, resolution).map_err(TimeSeriesError::IntegrityError)?;
                // scenario_count = arr.shape[0]; validate remaining dims.
                if arr.shape.len() < 3 {
                    return Err(TimeSeriesError::IntegrityError(format!(
                        "Scenarios: stored shape {:?} must have at least 3 dims",
                        arr.shape
                    )));
                }
                let scenario_count = arr.shape[0];
                validate_forecast_shape(&arr, &[scenario_count, h, count], "Scenarios")?;
                let (w0, w1, window_initial) =
                    resolve_windows(initial, resolution, horizon, interval, count, time_range)?;
                let windowed = if w0 == 0 && w1 == count {
                    arr
                } else {
                    slice_count_axis(&arr, 2, w0, w1)
                };
                let scen = Scenarios::new(
                    window_initial,
                    resolution,
                    horizon,
                    interval,
                    w1 - w0,
                    scenario_count,
                    windowed,
                    meta.name.clone(),
                )
                .map_err(TimeSeriesError::IntegrityError)?;
                Ok(TimeSeriesData::Scenarios(scen))
            }

            TimeSeriesType::DeterministicSingleTimeSeries => {
                // The stored array is the underlying STS 1-D-like array, shape
                // [total_len, *E]. Synthesize a Deterministic of shape
                // [H, count, *E] by gathering windows.
                let arr = self
                    .backend
                    .get_array(&meta.data_hash, meta.element_type.physical_dtype())?;
                let initial = required_initial(meta, "DeterministicSingleTimeSeries")?;
                let resolution = required_resolution(meta, "DeterministicSingleTimeSeries")?;
                let horizon = required_horizon(meta, "DeterministicSingleTimeSeries")?;
                let interval = required_interval(meta, "DeterministicSingleTimeSeries")?;
                let count = required_count(meta, "DeterministicSingleTimeSeries")?;
                let h = compute_h(horizon, resolution).map_err(TimeSeriesError::IntegrityError)?;
                // A single-window view carries a zero interval; its one window
                // starts at index 0, so the step width is irrelevant.
                let interval_steps = if count == 1 && interval.is_zero() {
                    0
                } else {
                    resolution.divide_into(&interval).map_err(|_| {
                        TimeSeriesError::IntegrityError(format!(
                            "DeterministicSingleTimeSeries: interval ({}) is not an integer \
                             multiple of resolution ({})",
                            interval.to_iso8601(),
                            resolution.to_iso8601()
                        ))
                    })?
                };
                let total_len = arr.length();
                // Validate that all windows fit in the underlying array.
                let required = (count.saturating_sub(1)) * interval_steps + h;
                if required > total_len {
                    return Err(TimeSeriesError::IntegrityError(format!(
                        "DeterministicSingleTimeSeries: (count-1)*interval_steps+H = {required} \
                         exceeds total_len = {total_len}"
                    )));
                }
                // Element bytes per underlying step. An empty `elem_shape` is a
                // scalar per step (its product is 1); a zero-width element
                // dimension makes this zero, and the gather below then copies
                // nothing rather than indexing the empty buffer -- the write
                // boundary refuses such a shape, but a read must not panic on it.
                let elem_shape: Vec<usize> = arr.shape[1..].to_vec();
                let elem_factor: usize = elem_shape.iter().product::<usize>() * arr.dtype.size();

                let (w0, w1, window_initial) =
                    resolve_windows(initial, resolution, horizon, interval, count, time_range)?;
                let selected = w1 - w0;

                // Build output array [H, selected, *E].
                let out_shape: Vec<usize> = std::iter::once(h)
                    .chain(std::iter::once(selected))
                    .chain(elem_shape.iter().copied())
                    .collect();
                let out_nelems: usize = out_shape.iter().product();
                let mut out_bytes = vec![0u8; out_nelems * arr.dtype.size()];

                for j in 0..selected {
                    let k = w0 + j; // source window index
                    for s in 0..h {
                        let src_idx = k * interval_steps + s;
                        let src_off = src_idx * elem_factor;
                        // Row-major offset for [s, j] in [H, selected] with elem_factor.
                        let dst_off = (s * selected + j) * elem_factor;
                        out_bytes[dst_off..dst_off + elem_factor]
                            .copy_from_slice(&arr.bytes[src_off..src_off + elem_factor]);
                    }
                }

                let out_arr = TypedArray::new(arr.dtype, out_shape, out_bytes)
                    .map_err(TimeSeriesError::IntegrityError)?;
                let det = Deterministic::new(
                    window_initial,
                    resolution,
                    horizon,
                    interval,
                    selected,
                    out_arr,
                    meta.name.clone(),
                )
                .map_err(TimeSeriesError::IntegrityError)?;
                Ok(TimeSeriesData::Deterministic(det))
            }
        }
    }

    /// Every matching row *with* its time axis loaded.
    ///
    /// Crate-internal. The public listing is [`Self::list_metadata`], which is
    /// this query without the per-row timestamp read; the only callers that
    /// need the axis are the ones building a `NonSequentialTimeSeries` cohort
    /// (the readers) or serializing a whole store.
    pub(crate) fn list_with_timestamps(
        &self,
        filter: ListFilter,
    ) -> Result<Vec<TimeSeriesMetadata>> {
        self.metadata.list(&filter.into(), &*self.backend)
    }

    /// The stored timestamp vector content-addressed by `hash`, or
    /// [`TimeSeriesError::NotFound`].
    ///
    /// The one place an axis is reachable by its own address rather than through
    /// a row that already names it: an OpenAPI document locates an irregular
    /// series' axis (`timestamps_uri`) instead of carrying it, so the import has
    /// a hash and needs the vector. See [`crate::openapi`].
    pub(crate) fn timestamps_for(
        &self,
        hash: &[u8; 32],
    ) -> Result<Vec<chrono::DateTime<chrono::Utc>>> {
        self.backend.get_timestamps(hash)
    }

    /// Every timestamp vector the array file holds, by content hash — so a
    /// batch can check the axes it names without a read per row.
    pub(crate) fn stored_time_axes(&self) -> Result<HashSet<[u8; 32]>> {
        Ok(self.backend.timestamp_hashes()?.into_iter().collect())
    }

    /// List the catalog row of every association matching `filter`, without its
    /// time axis.
    ///
    /// The listing that answers identity and description questions — which
    /// series exist, what type and grid each is, which array each resolves to
    /// (`data_hash`), and the [`TimeSeriesId`] to address it by. It replaces the
    /// five key-shaped listings this used to carry (`list_keys`,
    /// `list_keys_with_hash`, `list_keys_with_id`, `list_array_groups`,
    /// `get_time_series_keys`): each was this one query projected differently,
    /// and the row already holds everything they projected. Addressing a set of
    /// known ids instead is [`Self::list_metadata_by_ids`].
    ///
    /// The rows carry no time axis: an irregular series' timestamp vector is the
    /// one part of a row that costs a read per row, and a listing almost never
    /// wants it. Read the series itself ([`Self::read_by_id`]) to get it.
    pub fn list_metadata(&self, filter: ListFilter) -> Result<Vec<TimeSeriesMetadata>> {
        self.metadata.list_without_timestamps(&filter.into())
    }

    /// The catalog rows `ids` names, in the order asked for.
    ///
    /// [`Self::list_metadata`] addressed by id instead of by attributes — the
    /// bulk companion to [`Self::get_metadata_by_id`], and what a consumer
    /// hydrating a model full of recorded ids wants: one catalog query for the
    /// whole set rather than one per reference.
    ///
    /// [`TimeSeriesError::NotFound`] if any id names no row. A listing by
    /// attributes returns what matches, but a caller naming ids is asserting
    /// they exist, and a silently short result would let a stale reference pass
    /// as an absent match. Sift the set with [`Self::association_exists`] first
    /// when some are expected to have gone. Repeats are returned once each, in
    /// place.
    ///
    /// Rows carry no time axis, exactly as in [`Self::list_metadata`].
    #[tracing::instrument(skip(self, ids), fields(count = ids.len()))]
    pub fn list_metadata_by_ids(&self, ids: &[TimeSeriesId]) -> Result<Vec<TimeSeriesMetadata>> {
        let raw: Vec<i64> = ids.iter().map(|i| i.get()).collect();
        let found = self.metadata.list_by_ids_without_timestamps(&raw)?;
        let by_id: HashMap<TimeSeriesId, &TimeSeriesMetadata> = found
            .iter()
            .filter_map(|m| m.id.map(|id| (id, m)))
            .collect();
        ids.iter()
            .map(|id| {
                by_id
                    .get(id)
                    .map(|m| (*m).clone())
                    .ok_or(TimeSeriesError::NotFound)
            })
            .collect()
    }

    /// Build a [`StaticReader`] over the static series matching `filter`.
    ///
    /// Every column in a reader must share one timeline, so which type the
    /// filter names decides what has to hold:
    ///
    /// * `SingleTimeSeries` (the default): the filter must pin a resolution —
    ///   one resolution per reader — and every matched series must share one
    ///   grid (`initial_timestamp` + `length`).
    /// * `NonSequentialTimeSeries`: every matched series must lie on one
    ///   *timestamp vector*. Irregular series carry no resolution, so a
    ///   resolution filter is rejected rather than silently matching nothing.
    ///   The cohort is resolved from the catalog's interned vectors, which is
    ///   also what pools those arrays on disk — so a reader over a cohort reads
    ///   each timestamp as one packed hyperslab, exactly as for a regular grid.
    ///
    /// Divergence is an error either way, which is what lets the per-read path
    /// skip presence checks. Drive the reader with [`Self::static_read`]. See
    /// [`crate::reader`].
    pub fn build_static_reader(&self, mut filter: ListFilter) -> Result<StaticReader> {
        let ts_type = filter
            .time_series_type
            .unwrap_or(TimeSeriesType::SingleTimeSeries);
        filter.time_series_type = Some(ts_type);
        match ts_type {
            TimeSeriesType::SingleTimeSeries => {
                if filter.resolution.is_none() {
                    return Err(TimeSeriesError::InvalidParameter(
                        "build_static_reader requires a resolution filter for SingleTimeSeries \
                         (one resolution per reader)"
                            .into(),
                    ));
                }
                let rows = self.list_with_timestamps(filter)?;
                let timeline = crate::reader::regular_timeline(&rows)?;
                crate::reader::build_groups(timeline, rows)
            }
            TimeSeriesType::NonSequentialTimeSeries => {
                if filter.resolution.is_some() {
                    return Err(TimeSeriesError::InvalidParameter(
                        "build_static_reader takes no resolution filter for \
                         NonSequentialTimeSeries: an irregular series has none, so the filter \
                         would match nothing. Its timeline is the timestamp vector its cohort \
                         shares."
                            .into(),
                    ));
                }
                // Rows without their (identical, per-row) timestamp copies, plus
                // the distinct vectors they reference: one cohort is the whole
                // requirement, and the vector itself is then decoded once.
                let (rows, cohorts) = self.metadata.list_timeline_cohorts(&filter.into())?;
                let hash = match cohorts.as_slice() {
                    [hash] => *hash,
                    [] => {
                        return Err(TimeSeriesError::InvalidParameter(
                            "build_static_reader: no NonSequentialTimeSeries match the filter"
                                .into(),
                        ));
                    }
                    many => {
                        return Err(TimeSeriesError::InvalidParameter(format!(
                            "StaticReader requires a uniform timeline; the {} matched \
                             NonSequentialTimeSeries lie on {} different timestamp vectors. \
                             Narrow the filter (by name, owner, or features) to one of them.",
                            rows.len(),
                            many.len()
                        )));
                    }
                };
                let timestamps = self.metadata.timestamps_for_hash(&hash, &*self.backend)?;
                crate::reader::build_groups(crate::reader::Timeline::Irregular { timestamps }, rows)
            }
            other => Err(TimeSeriesError::InvalidParameter(format!(
                "build_static_reader handles the static types (SingleTimeSeries / \
                 NonSequentialTimeSeries); got {}",
                other.as_str()
            ))),
        }
    }

    /// Read the value of every series in `reader` at timestamp `at`, filling the
    /// reader's reusable buffers in place. Afterwards walk
    /// [`StaticReader::groups`] for the columnar bytes. Errors (never clamps) if
    /// `at` is off the reader's grid.
    pub fn static_read(
        &self,
        reader: &mut StaticReader,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        // All-or-nothing: on success every group holds `at`, and on *any*
        // failure the reader is emptied. Both halves matter. `index_at` fails
        // before a group is touched, so without this every group would still
        // hold the previous read -- a full, plausible, wrong answer under an
        // `Err`. A group failing part way through is the other half: the groups
        // already filled hold `at` while the rest hold the previous timestamp,
        // and nothing distinguishes them. See `StaticReader::invalidate`.
        match self.static_read_into(reader, at) {
            Ok(()) => {
                reader.mark_read(at);
                Ok(())
            }
            Err(e) => {
                reader.invalidate();
                Err(e)
            }
        }
    }

    /// The body of [`Self::static_read`], so that every `?` in it lands on one
    /// error path the caller can empty the reader from.
    fn static_read_into(
        &self,
        reader: &mut StaticReader,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let index = reader.index_at(at)?;
        for group in reader.groups_mut() {
            group.fill(|hashes, dtype, out| {
                self.backend.read_index_into(hashes, dtype, index, out)
            })?;
        }
        Ok(())
    }

    /// Build a [`ForecastReader`] over the forecasts matching `filter`.
    ///
    /// The filter must name a forecast type and pin a resolution. A
    /// `Deterministic` reader spans both concrete storage forms — a transformed
    /// `DeterministicSingleTimeSeries` is read into identical `[H, *E]` windows
    /// — per [`TimeSeriesType::accepts`]; `Probabilistic` and `Scenarios` are
    /// exact. All matched forecasts must share one window timeline
    /// (`initial_timestamp` + `interval` + `count`); this is validated and
    /// errors on divergence. Drive with [`Self::forecast_read`]. See
    /// [`crate::reader`].
    pub fn build_forecast_reader(&self, mut filter: ListFilter) -> Result<ForecastReader> {
        let reported = match filter.time_series_type {
            Some(
                t @ (TimeSeriesType::Deterministic
                | TimeSeriesType::DeterministicSingleTimeSeries
                | TimeSeriesType::Probabilistic
                | TimeSeriesType::Scenarios),
            ) => t,
            Some(other) => {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "build_forecast_reader handles forecast types (Deterministic/\
                     DeterministicSingleTimeSeries/Probabilistic/Scenarios); got {}",
                    other.as_str()
                )));
            }
            None => {
                return Err(TimeSeriesError::InvalidParameter(
                    "build_forecast_reader requires a forecast time_series_type in the filter"
                        .into(),
                ));
            }
        };
        if filter.resolution.is_none() {
            return Err(TimeSeriesError::InvalidParameter(
                "build_forecast_reader requires a resolution filter (one resolution per reader)"
                    .into(),
            ));
        }
        // No per-type expansion here: the catalog filter already widens a
        // `Deterministic` request to both storage forms.
        filter.time_series_type = Some(reported);
        let mut items = Vec::new();
        for m in self.list_with_timestamps(filter)? {
            let shape = self.backend.array_shape(&m.data_hash)?;
            items.push((m, shape));
        }
        crate::reader::build_forecast_entries(reported, items)
    }

    /// Read the forecast window at timestamp `at` for every forecast in `reader`,
    /// filling the reader's reusable per-entry buffers in place. Afterwards walk
    /// [`ForecastReader::entries`] for the window bytes. Errors (never clamps) if
    /// `at` is off the reader's window timeline.
    pub fn forecast_read(
        &self,
        reader: &mut ForecastReader,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        // All-or-nothing, exactly as in `static_read`.
        match self.forecast_read_into(reader, at) {
            Ok(()) => {
                reader.mark_read(at);
                Ok(())
            }
            Err(e) => {
                reader.invalidate();
                Err(e)
            }
        }
    }

    /// The body of [`Self::forecast_read`]; see [`Self::static_read_into`].
    fn forecast_read_into(
        &self,
        reader: &mut ForecastReader,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let window = reader.window_index(at)?;
        // One read per *slot*: forecasts that share an array and read plan
        // (e.g. components referencing one shared forecast) collapse to a single
        // slot at build time, so the backend is hit once for all of them. Each
        // slot caches its enclosing chunk block, so stepping the timeline only
        // hits the backend when the window crosses a block boundary.
        let backend = &*self.backend;
        for slot in reader.slots_mut() {
            // A read served from the cached block does no I/O, so it cannot
            // notice that the forecast's array was removed since the block was
            // read; it would hand back a deleted array as `Ok` where the static
            // reader, and this reader on a block boundary, say `NotFound`. An
            // index probe is what the read skipped, so ask that much. Both
            // readers are content-addressed by design: a removed *row* whose
            // array another row still holds keeps reading on every path, cached
            // or not, static or forecast -- a reader does not re-validate its
            // entries against the catalog per timestep.
            if slot.serves_from_cache(window) && !backend.contains(slot.hash())? {
                slot.forget();
                return Err(TimeSeriesError::NotFound);
            }
            slot.read_window(
                window,
                |hash, dtype, count_axis, start, len, out| {
                    backend.read_window_block_into(hash, dtype, count_axis, start, len, out)
                },
                |hash, dtype, start, len, out| {
                    backend.read_range_into(hash, dtype, start, len, out)
                },
            )?;
        }
        Ok(())
    }

    /// Read every series named by its catalog `id`, in the order the ids are
    /// given.
    ///
    /// The read-direction counterpart of the id a write hands back: a consumer
    /// that recorded ids in its own model (a generator's cost function naming
    /// the series that varies it) resolves them here without keeping an
    /// id-to-key map of its own.
    ///
    /// [`TimeSeriesError::NotFound`] if any id names no row — unlike
    /// [`Self::association_exists`], which asks the question, this one is
    /// already committed to reading and a missing id means the caller's
    /// reference is stale. The error does not say *which* id dangled; a caller
    /// that needs to know sifts them with [`Self::association_exists`], which
    /// is the cheaper call for exactly that.
    ///
    /// `window` applies to every id in the set — [`ReadWindow::full()`] reads
    /// each series whole. A window is checked against each row's own grid, so a
    /// set whose series do not all carry the window is an error rather than a
    /// ragged result.
    ///
    /// One catalog query for the whole set.
    #[tracing::instrument(skip(self, ids), fields(count = ids.len()))]
    pub fn read_by_ids(
        &self,
        ids: &[TimeSeriesId],
        window: ReadWindow,
    ) -> Result<Vec<TimeSeriesData>> {
        let metas = self.rows_for_ids(ids)?;
        if window.is_full() {
            return self.bulk_read_metas(&metas);
        }
        // One window for many series, so the selection has to agree on what a
        // bound *means*; a set mixing zoneless and instant-bearing series has no
        // single valid one. That is only so when the window *names* a bound: a
        // `start`-less window (`ReadWindow::full().with_len(n)`) carries no
        // timestamp, and `ReadWindow::range` spells the bound it implies the way
        // each series is, so there is nothing for the two groups to disagree
        // about -- exactly as for an unwindowed read.
        if window.start.is_some() {
            reject_mixed_zoning(&metas, "read_by_ids")?;
        }
        metas
            .iter()
            .map(|m| {
                let range = window.resolve(m)?;
                self.materialize_time_series(m, range)
            })
            .collect()
    }

    /// The catalog rows `ids` names, in the order asked for — repeats included.
    /// One query for the whole set; [`TimeSeriesError::NotFound`] if any id names
    /// no row, because a read is already committed to acting on the reference.
    fn rows_for_ids(&self, ids: &[TimeSeriesId]) -> Result<Vec<TimeSeriesMetadata>> {
        let raw: Vec<i64> = ids.iter().map(|i| i.get()).collect();
        let found = self.metadata.list_by_ids(&raw, &*self.backend)?;
        // The catalog returns each row once, in its own order; the caller asked
        // for a specific order and may have repeated an id.
        let by_id: HashMap<TimeSeriesId, &TimeSeriesMetadata> = found
            .iter()
            .filter_map(|m| m.id.map(|id| (id, m)))
            .collect();
        ids.iter()
            .map(|id| {
                by_id
                    .get(id)
                    .map(|m| (*m).clone())
                    .ok_or(TimeSeriesError::NotFound)
            })
            .collect()
    }

    /// Read the series named by `ids`, each clipped to whatever lies within
    /// `time_range`.
    ///
    /// The *bounds* read, next to [`Self::read_by_ids`]'s *window* read, and the
    /// difference is the whole reason both exist. A window says "these exact
    /// steps" and is checked; a range says "whatever falls between these
    /// instants" and clips to what is there. A caller exporting a month of a
    /// store it did not write knows the bounds it wants and not how many steps
    /// each series has in them — asking that question with a window would be
    /// asking it to fail.
    ///
    /// `start` is inclusive and `end` exclusive, applied to what each type
    /// pairs a value with:
    ///
    /// * a `SingleTimeSeries` value covers the step `[t, t + resolution)`, so a
    ///   `start` inside a step selects that step — the returned
    ///   `initial_timestamp` is floored onto the grid and can precede `start`;
    /// * a `NonSequentialTimeSeries` value is an instant, so only timestamps at
    ///   or after `start` are selected;
    /// * a forecast window is a whole array with nothing partial to return, so
    ///   `start` must *be* a window boundary at or before the last window
    ///   (`initial_timestamp + k·interval`) and is otherwise
    ///   [`TimeSeriesError::InvalidParameter`]; only `end` clips.
    ///
    /// The forecast rule is the one place a range checks rather than clips. A
    /// caller sweeping a mixed set by calendar bounds keeps the forecasts on
    /// their own boundaries, or filters them out with
    /// [`ListFilter::time_series_type`].
    ///
    /// The bound must be spelled the way the series is, and one range over a set
    /// mixing zoneless and instant-bearing series has no single valid spelling,
    /// so that is refused rather than resolved per series.
    pub fn read_by_ids_range(
        &self,
        ids: &[TimeSeriesId],
        time_range: TimeRange,
    ) -> Result<Vec<TimeSeriesData>> {
        let metas = self.rows_for_ids(ids)?;
        reject_mixed_zoning(&metas, "read_by_ids_range")?;
        metas
            .iter()
            .map(|m| self.materialize_time_series(m, Some(time_range)))
            .collect()
    }

    /// Read the series filed under `id`, or the slice of it that `window` names.
    ///
    /// The whole read, for any stored type, in one call: the id is a primary-key
    /// lookup, and the row it returns carries the grid the window resolves
    /// against — the same row the read then materializes from. A caller holding
    /// an id spends nothing to learn a series' `resolution` or `count` before
    /// asking for the second day of it.
    ///
    /// [`ReadWindow::full()`] reads everything, which is
    /// [`Self::read_by_ids`] for a single id. Otherwise the window is resolved
    /// strictly: a start off the series' grid, or an extent running past its
    /// end, is [`TimeSeriesError::InvalidParameter`] rather than the smaller
    /// answer a raw [`TimeRange`] would clamp to. That is the whole reason to
    /// take a window rather than a range — see [`ReadWindow`].
    ///
    /// [`TimeSeriesError::NotFound`] if the id names no row, following
    /// [`Self::read_by_ids`]: a call already committed to reading treats a
    /// stale reference as a failure, where [`Self::association_exists`] treats
    /// it as an answer.
    #[tracing::instrument(skip(self))]
    pub fn read_by_id(&self, id: TimeSeriesId, window: ReadWindow) -> Result<TimeSeriesData> {
        self.read_by_id_inner(id, None, window)
    }

    /// [`Self::read_by_id`], but reading only a row that belongs to `owner`,
    /// and [`TimeSeriesError::OwnerMismatch`] otherwise.
    ///
    /// The read-direction counterpart of [`Self::remove_by_ids_for_owner`], and
    /// for the same caller: one that holds an id but means "this owner's
    /// series", and would otherwise have to confirm the owner in a call of its
    /// own — a second round trip whose answer is about the row as it was, not
    /// the row being read. Here the owner comes off the very row the values are
    /// materialized from, so the two cannot disagree, and the guarded read costs
    /// exactly what the unguarded one does.
    #[tracing::instrument(skip(self))]
    pub fn read_by_id_for_owner(
        &self,
        id: TimeSeriesId,
        owner: (i64, OwnerCategory),
        window: ReadWindow,
    ) -> Result<TimeSeriesData> {
        self.read_by_id_inner(id, Some(owner), window)
    }

    /// The shared body of the two id-addressed reads.
    fn read_by_id_inner(
        &self,
        id: TimeSeriesId,
        expected_owner: Option<(i64, OwnerCategory)>,
        window: ReadWindow,
    ) -> Result<TimeSeriesData> {
        let meta = self
            .metadata
            .get_by_id(id.get(), &*self.backend)?
            .ok_or(TimeSeriesError::NotFound)?;
        if let Some((expected_id, expected_category)) = expected_owner
            && (meta.owner_id != expected_id || meta.owner_category != expected_category)
        {
            return Err(TimeSeriesError::OwnerMismatch {
                id: id.get(),
                expected_id,
                expected_category: expected_category.as_str(),
                actual_id: meta.owner_id,
                actual_category: meta.owner_category.as_str(),
            });
        }
        let time_range = window.resolve(&meta)?;
        self.materialize_time_series(&meta, time_range)
    }

    /// The shared body of [`Self::bulk_read`] and [`Self::read_by_ids`],
    /// working from rows both have already resolved.
    fn bulk_read_metas(&self, metas: &[TimeSeriesMetadata]) -> Result<Vec<TimeSeriesData>> {
        // Batch the packed SingleTimeSeries reads; everything else is standalone
        // and reuses the per-key reconstruction.
        let (single_hashes, single_dtypes): (Vec<[u8; 32]>, Vec<Dtype>) = metas
            .iter()
            .filter(|m| m.time_series_type == TimeSeriesType::SingleTimeSeries)
            .map(|m| (m.data_hash, m.element_type.physical_dtype()))
            .unzip();
        let mut single_arrays = self
            .backend
            .read_arrays(&single_hashes, &single_dtypes)?
            .into_iter();

        let mut out = Vec::with_capacity(metas.len());
        for meta in metas {
            if meta.time_series_type == TimeSeriesType::SingleTimeSeries {
                let data = single_arrays.next().ok_or_else(|| {
                    TimeSeriesError::IntegrityError(
                        "bulk_read: fewer arrays returned than SingleTimeSeries rows".into(),
                    )
                })?;
                let initial = meta.initial_timestamp.ok_or_else(|| {
                    TimeSeriesError::IntegrityError(
                        "SingleTimeSeries missing initial_timestamp".into(),
                    )
                })?;
                let resolution = meta.resolution.ok_or_else(|| {
                    TimeSeriesError::IntegrityError("SingleTimeSeries missing resolution".into())
                })?;
                let length = meta.length.ok_or_else(|| {
                    TimeSeriesError::IntegrityError("SingleTimeSeries missing length".into())
                })?;
                out.push(TimeSeriesData::SingleTimeSeries(SingleTimeSeries {
                    initial_timestamp: initial,
                    resolution,
                    length,
                    data,
                    name: meta.name.clone(),
                    // This fast path bypasses `materialize_time_series`, so the
                    // descriptors come straight off the row it already loaded.
                    element_type: meta.element_type,
                    units: meta.units.clone(),
                    quantity_kind: meta.quantity_kind.clone(),
                    unit_system: meta.unit_system,
                    time_reference: meta.time_reference.clone(),
                    component_field: meta.component_field.clone(),
                    application_data: meta.application_data.clone(),
                }));
            } else {
                // Materialize from the row already in hand rather than calling
                // `get_time_series`, which would look the key up a second time.
                // For a `NonSequentialTimeSeries` that second lookup also
                // re-fetched and re-decoded the row's timestamp vector, so a
                // bulk read of N irregular series did 2N of both.
                out.push(self.materialize_time_series(meta, None)?);
            }
        }
        Ok(out)
    }

    /// The metadata of the association filed under `id`, or `None` if the
    /// catalog holds no such row.
    ///
    /// `None` rather than an error: a consumer validating references it
    /// persisted earlier is asking whether one still resolves, and a stale
    /// reference is an answer.
    pub fn get_metadata_by_id(&self, id: TimeSeriesId) -> Result<Option<TimeSeriesMetadata>> {
        self.metadata.get_by_id(id.get(), &*self.backend)
    }

    /// Whether an association is filed under `id`.
    ///
    /// A primary-key probe — one statement, no row fetched — so a consumer can
    /// check every reference in its model on load rather than discovering a
    /// dangling one mid-run. Use [`Self::get_metadata_by_id`] when the answer
    /// is wanted along with the row.
    pub fn association_exists(&self, id: TimeSeriesId) -> Result<bool> {
        self.metadata.exists_by_id(id.get())
    }

    /// Count `SingleTimeSeries` and `DeterministicSingleTimeSeries` associations
    /// that reference `data_hash`, across all owners, as `(sts, dst)`.
    ///
    /// A `DeterministicSingleTimeSeries` shares the underlying array of the
    /// `SingleTimeSeries` it was derived from, so a binding deciding whether a
    /// `SingleTimeSeries` can be removed without orphaning a DST needs these
    /// counts. Resolved by a single grouped catalog query rather than scanning
    /// every association in the caller.
    pub fn count_array_references(&self, data_hash: &[u8; 32]) -> Result<(usize, usize)> {
        let (sts, dst) = self.metadata.count_array_references(data_hash)?;
        Ok((sts as usize, dst as usize))
    }

    /// Fetch the full stored array for a content hash. The metadata-owning
    /// binding resolves a key to its `data_hash`, then calls this to read the
    /// underlying values.
    ///
    /// How to decode the bytes comes from the catalog, not the array file: the
    /// `element_type` of some association referencing `hash`. `NotFound` if no
    /// association does — an array nothing points at cannot be typed, and so
    /// cannot be read.
    pub fn get_array_by_hash(&self, hash: &[u8; 32]) -> Result<TypedArray> {
        let element_type = self.metadata.element_type_for_hash(hash)?;
        self.backend.get_array(hash, element_type.physical_dtype())
    }

    /// Where a content hash's array physically lives in the backing file.
    ///
    /// Complements [`Self::get_array_by_hash`] for the case where the caller
    /// wants to inspect the bytes with an outside HDF5 tool rather than read
    /// them through this crate. The hash on its own does not locate an array: a
    /// packed array is one column of a shared dataset, and a full packed pool
    /// spills into suffixed datasets, so neither the dataset name nor the column
    /// index is derivable from metadata.
    ///
    /// Errors with [`TimeSeriesError::NotFound`] if no array with that hash is
    /// stored.
    pub fn locate_array(&self, hash: &[u8; 32]) -> Result<ArrayLocation> {
        self.backend.locate(hash)
    }

    /// Derive `DeterministicSingleTimeSeries` forecasts from the stored
    /// `SingleTimeSeries` associations, mirroring InfrastructureSystems.jl's
    /// `transform_single_time_series!`.
    ///
    /// Every `SingleTimeSeries` in the store is re-described as a
    /// `DeterministicSingleTimeSeries` that shares the same underlying array (no
    /// data is copied); the forecast windows are synthesized on read. `horizon`
    /// and `interval` define the windowing and must be positive multiples of
    /// each series' resolution; `count` is derived from each series' length as
    /// `(length - horizon_steps) / interval_steps + 1`.
    ///
    /// All-or-nothing: if any series is too short to fit a single horizon window
    /// or has an incompatible `interval`, nothing is committed.
    ///
    /// Every eligibility rule lives here rather than in the callers. Beyond the
    /// per-series checks, the window parameters are validated once per
    /// *resolution* off the catalog's distinct static grids (see
    /// [`Store::check_static_consistency`]), which is what makes the whole
    /// validation independent of how many series are stored:
    ///
    /// - every `SingleTimeSeries` at a resolution must share one
    ///   `(initial_timestamp, length)` grid;
    /// - a requested `interval` equal to a horizon that spans the whole series
    ///   describes a single window. There are two legal encodings for that and
    ///   the interval is part of the association identity, so the caller picks:
    ///   `policy.normalize_single_window` stores it as the zero interval (what
    ///   InfrastructureSystems.jl looks up by), while `false` stores the
    ///   requested interval verbatim. Either way the case is reported via
    ///   [`TransformOutcome::interval_normalized`];
    /// - an `interval` longer than the horizon is rejected — it would leave gaps
    ///   between windows;
    /// - every resolution in scope must agree on the derived `count` and
    ///   `initial_timestamp`, so one transform yields one forecast grid;
    /// - that grid must match any forecast already in the store at the same
    ///   `(resolution, interval)`.
    pub fn transform_single_time_series(
        &mut self,
        horizon: impl Into<Period>,
        interval: impl Into<Period>,
        owner_category: Option<OwnerCategory>,
        resolution: Option<Period>,
        policy: TransformPolicy,
    ) -> Result<TransformOutcome> {
        // A dry run writes nothing, so it is legal against a read-only store —
        // which is exactly where a caller wants to ask "would this work?".
        if self.read_only && !policy.dry_run {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let (horizon, requested_interval) = (horizon.into(), interval.into());

        // Validate the window parameters against the distinct static grids —
        // one `DISTINCT` query returning a row per resolution — instead of
        // per series. `horizon`/`interval` eligibility and the derived `count`
        // depend only on `(resolution, initial_timestamp, length)`, so this is
        // O(resolutions) no matter how many series are stored, and it doubles
        // as the per-resolution grid-uniformity check.
        let grids = self.static_grids(resolution, owner_category)?;
        let Some(plan) = TransformPlan::derive(&grids, horizon, requested_interval, policy)? else {
            // No SingleTimeSeries in scope: nothing to do, and nothing to fail.
            return Ok(TransformOutcome {
                transformed: 0,
                sources: 0,
                interval: requested_interval,
                interval_normalized: false,
                written: Vec::new(),
            });
        };
        let interval = plan.interval;

        // Under the uniform-grid policy the derived grid must also match any
        // forecast already stored at the same (resolution, interval): one
        // system holds one forecast grid, so a transform that would produce a
        // second one is rejected before any write.
        if policy.require_uniform_forecast_grid {
            for (&res, grid) in &plan.by_resolution {
                let existing_params =
                    self.get_forecast_parameters(Some(res), Some(grid.interval))?;
                plan.check_compatible_with(&existing_params)?;
            }
        }

        // Push the owner-category and resolution restrictions into SQL rather
        // than listing every SingleTimeSeries and discarding the misses: a store
        // whose components are transformed one resolution at a time should not
        // pay to hydrate the other resolutions' features on every call.
        let sources = self.metadata.list(
            &MetadataFilter {
                time_series_type: Some(TypeMatch::Exact(TimeSeriesType::SingleTimeSeries)),
                owner_category,
                resolution,
                ..Default::default()
            },
            &*self.backend,
        )?;

        // Series that already have a DeterministicSingleTimeSeries view *at this
        // interval* are skipped so the transform is idempotent (e.g. re-deriving
        // one series when others were transformed earlier, as during a component
        // copy) — but only when the existing view also has this `horizon`. The
        // identity (owner_id/owner_category plus name/resolution/interval/
        // features) does not include the horizon, so a same-identity view with a
        // *different* horizon cannot coexist with the requested one; silently
        // skipping it would leave the old horizon serving reads while reporting
        // success, so that case is an error instead. Interval is part of the
        // identity, so re-deriving the same series at a different interval is a
        // distinct view.
        //
        // Both dedup sets are read via `list_identities`, which returns the
        // stored `features_hash` column directly: the identity test needs the
        // hash, not the features themselves, so this skips hydrating (and
        // re-hashing) the features of every forecast already in the store.
        let existing_dst: HashMap<AssociationIdentity, Option<Period>> = self
            .metadata
            .list_identities(TimeSeriesType::DeterministicSingleTimeSeries)?
            .into_iter()
            .collect();

        // Families that already hold a real `Deterministic` forecast. A DST is a
        // synthetic view and is mutually exclusive with a `Deterministic` for one
        // family (owner, name, resolution, features, ignoring interval), so
        // deriving a DST over such a family is rejected.
        let existing_det: HashSet<SeriesFamily> = self
            .metadata
            .list_identities(TimeSeriesType::Deterministic)?
            .into_iter()
            .map(|(identity, _horizon)| SeriesFamily::from(identity))
            .collect();

        // Build every DST metadata row up front so a single ineligible series
        // aborts the whole transform before any write.
        let mut new_metas = Vec::with_capacity(sources.len());
        for src in &sources {
            let src_features_hash = crate::hash::features_hash(&src.features);
            let src_resolution_iso = src.resolution.map(|r| r.to_iso8601());
            // Reject deriving a DST over a family that already holds a real
            // Deterministic forecast (interval-independent).
            let det_family = SeriesFamily {
                owner_id: src.owner_id,
                owner_category: src.owner_category,
                name: src.name.clone(),
                resolution: src_resolution_iso.clone(),
                features_hash: src_features_hash,
            };
            if existing_det.contains(&det_family) {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "cannot derive DeterministicSingleTimeSeries for '{}': a Deterministic \
                     forecast of the same series already exists; they are mutually exclusive",
                    src.name
                )));
            }
            // No per-series arithmetic: `TransformPlan` already validated the
            // horizon and derived the window parameters from the catalog's
            // distinct grids, which cover exactly these sources. Re-deriving
            // them here would be a `divide_into` per series for an answer
            // already known.
            let resolution = required_resolution(src, "transform_single_time_series")?;
            let GridPlan { interval, count } = plan.for_resolution(resolution)?;
            // The interval is stored in whichever single-window encoding the
            // caller's policy selected — verbatim (`interval == horizon`, for
            // clients that map the empty interval to the horizon on write and
            // back on read) or the explicit zero interval
            // (InfrastructureSystems.jl's `Second(0)`). It is part of the
            // identity, so the stored form is what later lookups must use, and
            // the idempotency check below uses it too.
            let src_key = AssociationIdentity {
                owner_id: src.owner_id,
                owner_category: src.owner_category,
                name: src.name.clone(),
                resolution: src_resolution_iso,
                interval: Some(interval.to_iso8601()),
                features_hash: src_features_hash,
            };
            if let Some(existing_horizon) = existing_dst.get(&src_key) {
                if *existing_horizon == Some(horizon) {
                    // Same identity, same horizon: already derived; idempotent.
                    continue;
                }
                // Same identity but a different horizon: the two views cannot
                // coexist (horizon is not part of the identity), and silently
                // keeping the old one would misreport success.
                let describe = |h: &Option<Period>| match h {
                    Some(h) => h.to_iso8601(),
                    None => "-".to_string(),
                };
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "cannot derive DeterministicSingleTimeSeries for '{}' at interval {}: \
                     a view with horizon {} already exists (requested {}); remove it first \
                     or transform at a different interval",
                    src.name,
                    interval.to_iso8601(),
                    describe(existing_horizon),
                    horizon.to_iso8601(),
                )));
            }
            new_metas.push(derived_view_row(src.clone(), horizon, interval, count));
        }

        if policy.dry_run {
            // Every check has run against the rows that would be written; the
            // caller only wanted the verdict.
            return Ok(TransformOutcome {
                transformed: new_metas.len(),
                sources: sources.len(),
                interval,
                interval_normalized: plan.interval_normalized,
                written: Vec::new(),
            });
        }

        let tx = self.metadata.savepoint()?;
        // One cache for the whole batch: every derived row shares its source's
        // feature set, and sources overwhelmingly share sets with each other, so
        // the feature-set writes collapse to a handful regardless of how many
        // series are transformed.
        let mut feature_sets = SharedSetCache::default();
        let mut written = Vec::with_capacity(new_metas.len());
        for meta in &new_metas {
            match MetadataStore::insert_batched(&tx, meta, &mut feature_sets) {
                Ok(id) => written.push(TimeSeriesId(id)),
                Err(e) => {
                    drop(tx);
                    return Err(e);
                }
            }
        }
        tx.commit()?;
        Ok(TransformOutcome {
            transformed: new_metas.len(),
            sources: sources.len(),
            interval,
            interval_normalized: plan.interval_normalized,
            written,
        })
    }

    /// True iff at least one association matches `filter` — the owner-level
    /// counterpart of [`Self::has_time_series`], answering "does this
    /// component have any time series (of type T)?" without listing them.
    ///
    /// Same covering-index probe as the keyed check (one statement, nothing
    /// hydrated), so it is safe for hot loops. A `features` filter stays on
    /// indexes too: the requested set is probed as an exact set by hash first
    /// (one covering seek when the caller passes the complete feature set),
    /// with an indexed per-feature subset fallback for partial lists.
    pub fn has_any_time_series(&self, filter: ListFilter) -> Result<bool> {
        self.metadata.exists(&filter.into())
    }

    /// Whether the store holds no persistent content of any kind — no time
    /// series, and no associations in any catalog.
    ///
    /// Short-circuited `SELECT 1 ... LIMIT 1` existence probes, one per
    /// content table, so the cost is a handful of index seeks regardless of
    /// how much the store holds. Answering the same question from outside
    /// means a conjunction over the count APIs — eight `O(rows)` aggregate
    /// scans — that also goes stale the moment the schema grows a table.
    /// This is the store's own answer: adding a table means updating one
    /// function here rather than every binding and every consumer.
    ///
    /// Note that `BulkAdd::is_empty` is an unrelated predicate over buffered
    /// requests, not over store content.
    pub fn is_empty(&self) -> Result<bool> {
        self.metadata.is_empty()
    }

    pub fn get_resolutions(&self, time_series_type: Option<TimeSeriesType>) -> Result<Vec<Period>> {
        self.metadata.distinct_resolutions(time_series_type)
    }

    /// Distinct forecast intervals, optionally scoped to one time series type.
    /// The interval analog of [`Self::get_resolutions`]; ordered lexically by
    /// ISO-8601 text (mixed period kinds have no numeric order). Non-forecast
    /// types have no interval, so they return an empty list.
    pub fn get_intervals(&self, time_series_type: Option<TimeSeriesType>) -> Result<Vec<Period>> {
        self.metadata.distinct_intervals(time_series_type)
    }

    /// Every distinct [`TimeReference`] the catalog holds, sorted, plus whether
    /// any row left it unspecified.
    ///
    /// The audit surface for zone names: the store never gates on a zone
    /// existing (see [`TimeReference::validate`]), so a layer that *has* a tz
    /// database uses this to report a name it does not recognize — which makes
    /// a typo findable in one command rather than at some later read in some
    /// other language. One catalog query; the column is low-cardinality by
    /// nature.
    pub fn list_time_references(&self) -> Result<(Vec<TimeReference>, bool)> {
        self.metadata.distinct_time_references()
    }

    /// Distinct series names matching `filter`, sorted. A discovery projection
    /// over the authoritative filtered listing, so every filter (including the
    /// `features` subset match) is honored.
    pub fn list_names(&self, filter: ListFilter) -> Result<Vec<String>> {
        let mut names: Vec<String> = self
            .list_with_timestamps(filter)?
            .into_iter()
            .map(|m| m.name)
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// Distinct owner types matching `filter`, sorted. Same projection approach
    /// as [`Self::list_names`].
    pub fn list_owner_types(&self, filter: ListFilter) -> Result<Vec<String>> {
        let mut types: Vec<String> = self
            .list_with_timestamps(filter)?
            .into_iter()
            .map(|m| m.owner_type)
            .collect();
        types.sort();
        types.dedup();
        Ok(types)
    }

    /// Return the forecast parameters recorded in the store, optionally
    /// restricted to forecasts with a given `resolution` and/or `interval`.
    ///
    /// Looks for any metadata row whose type is a forecast type (and matches the
    /// filters) and returns its `horizon`, `interval`, `count`, and `resolution`.
    /// If none match, returns [`ForecastParameters::default()`]. When multiple
    /// match, returns the first one found (v0 stores a single coherent forecast
    /// configuration; callers that need per-type parameters should use
    /// [`Self::list_metadata`] directly). Both `resolution` and `interval`
    /// are pushed into the catalog query.
    pub fn get_forecast_parameters(
        &self,
        resolution: Option<Period>,
        interval: Option<Period>,
    ) -> Result<ForecastParameters> {
        use crate::metadata::MetadataFilter;
        // Check each forecast type in priority order.
        for ts_type in [
            TimeSeriesType::Deterministic,
            TimeSeriesType::DeterministicSingleTimeSeries,
            TimeSeriesType::Probabilistic,
            TimeSeriesType::Scenarios,
        ] {
            let rows = self.metadata.list(
                &MetadataFilter {
                    time_series_type: Some(TypeMatch::Exact(ts_type)),
                    resolution,
                    interval,
                    ..Default::default()
                },
                &*self.backend,
            )?;
            if let Some(row) = rows.into_iter().next() {
                return Ok(ForecastParameters {
                    horizon: row.horizon,
                    interval: row.interval,
                    count: row.count,
                    resolution: row.resolution,
                    initial_timestamp: row.initial_timestamp,
                });
            }
        }
        Ok(ForecastParameters::default())
    }

    /// Verify that, per resolution, all `SingleTimeSeries` share one
    /// `(initial_timestamp, length)` grid. Series at *different* resolutions
    /// legitimately have different grids (an hourly and a 5-minute profile over
    /// one year differ in length), so consistency is only required within a
    /// resolution.
    ///
    /// Returns one [`StaticConsistency`] row per resolution present (empty when
    /// the store has no `SingleTimeSeries`), ordered by resolution. `resolution`
    /// optionally scopes the check (and the returned rows) to one grid. Errors
    /// with [`TimeSeriesError::IntegrityError`] when any single resolution holds
    /// more than one distinct `(initial_timestamp, length)` pair. One `DISTINCT`
    /// query.
    pub fn check_static_consistency(
        &self,
        resolution: Option<Period>,
    ) -> Result<Vec<StaticConsistency>> {
        self.static_grids(resolution, None)
    }

    /// [`Self::check_static_consistency`] scoped to one owner category.
    ///
    /// `transform_single_time_series` needs this: it derives forecasts for one
    /// category, so a supplemental attribute's series on a different grid must
    /// not fail a component-only transform.
    fn static_grids(
        &self,
        resolution: Option<Period>,
        owner_category: Option<OwnerCategory>,
    ) -> Result<Vec<StaticConsistency>> {
        let rows = self
            .metadata
            .distinct_single_grids(resolution, owner_category)?;
        let mut out: Vec<StaticConsistency> = Vec::with_capacity(rows.len());
        for (res, ts, len) in rows {
            // Rows arrive ordered by resolution, so a divergent grid shows up
            // as two adjacent rows with the same resolution.
            if let Some(prev) = out.last()
                && prev.resolution == res
            {
                return Err(TimeSeriesError::IntegrityError(format!(
                    "SingleTimeSeries at resolution {} have more than one \
                     (initial_timestamp, length) pair: ({}, {}) vs ({}, {})",
                    res.to_iso8601(),
                    prev.initial_timestamp,
                    prev.length,
                    ts,
                    len,
                )));
            }
            out.push(StaticConsistency {
                resolution: res,
                initial_timestamp: ts,
                length: len as usize,
            });
        }
        Ok(out)
    }

    pub fn get_time_series_counts(&self) -> Result<TimeSeriesCounts> {
        let forecasts = self.metadata.count_by_type(TimeSeriesType::Deterministic)?
            + self
                .metadata
                .count_by_type(TimeSeriesType::DeterministicSingleTimeSeries)?
            + self.metadata.count_by_type(TimeSeriesType::Probabilistic)?
            + self.metadata.count_by_type(TimeSeriesType::Scenarios)?;
        Ok(TimeSeriesCounts {
            components_with_time_series: self.metadata.count_distinct_owners()?,
            static_time_series: self
                .metadata
                .count_by_type(TimeSeriesType::SingleTimeSeries)?
                + self
                    .metadata
                    .count_by_type(TimeSeriesType::NonSequentialTimeSeries)?,
            forecasts,
        })
    }

    /// Association count grouped by time series type. Replaces a binding-side
    /// scan-and-group with one catalog query.
    pub fn counts_by_type(&self) -> Result<Vec<(TimeSeriesType, i64)>> {
        self.metadata.counts_by_type()
    }

    /// Number of distinct stored arrays (content hashes); shared series count once.
    pub fn num_distinct_arrays(&self) -> Result<i64> {
        self.metadata.count_distinct_arrays()
    }

    /// Distinct owners per category and distinct stored arrays per kind (static
    /// vs forecast). Replaces a binding-side full scan that grouped owners and
    /// hashes in memory.
    pub fn time_series_counts_detailed(&self) -> Result<TimeSeriesCountsDetailed> {
        const STATIC: [TimeSeriesType; 2] = [
            TimeSeriesType::SingleTimeSeries,
            TimeSeriesType::NonSequentialTimeSeries,
        ];
        const FORECAST: [TimeSeriesType; 4] = [
            TimeSeriesType::Deterministic,
            TimeSeriesType::DeterministicSingleTimeSeries,
            TimeSeriesType::Probabilistic,
            TimeSeriesType::Scenarios,
        ];
        Ok(TimeSeriesCountsDetailed {
            components_with_time_series: self
                .metadata
                .count_distinct_owners_in_category(OwnerCategory::Component)?,
            supplemental_attributes_with_time_series: self
                .metadata
                .count_distinct_owners_in_category(OwnerCategory::SupplementalAttribute)?,
            static_time_series_count: self.metadata.count_distinct_arrays_for_types(&STATIC)?,
            forecast_count: self.metadata.count_distinct_arrays_for_types(&FORECAST)?,
        })
    }

    /// Distinct owner ids of `category` that have a time series, optionally
    /// restricted by type and/or resolution.
    pub fn list_owner_ids(
        &self,
        category: OwnerCategory,
        time_series_type: Option<TimeSeriesType>,
        resolution: Option<Period>,
    ) -> Result<Vec<i64>> {
        self.metadata
            .list_owner_ids(category, time_series_type, resolution)
    }

    /// Grouped static-series summary (one row per distinct owner/name/shape
    /// combination, with the association count). The binding builds the
    /// presentation table; the core does the grouping.
    pub fn static_summary(&self) -> Result<Vec<crate::metadata::StaticSummaryRow>> {
        self.metadata.static_summary()
    }

    /// Grouped forecast summary (one row per distinct owner/name/window
    /// configuration, with the association count).
    pub fn forecast_summary(&self) -> Result<Vec<crate::metadata::ForecastSummaryRow>> {
        self.metadata.forecast_summary()
    }

    // ---- Supplemental-attribute associations ------------------------------
    //
    // Which supplemental attributes are attached to which components. The store
    // holds the relationship only: the attributes and components themselves live
    // in the consumer's object graph. Attachments are independent of time series
    // in both directions — removing a component's series leaves its attachments
    // alone, and vice versa.

    /// Attach a supplemental attribute to a component, returning the catalog id
    /// the attachment was filed under. Fails with
    /// [`TimeSeriesError::DuplicateAssociation`] if that component already
    /// carries that attribute, whatever type names are supplied.
    ///
    /// The catalog assigns the id; `assoc.id` is ignored on the way in, so a row
    /// read back from one store and attached to another is filed under a fresh
    /// id there. This table's ids are independent of the other two catalogs'.
    pub fn add_supplemental_attribute_association(
        &mut self,
        assoc: SupplementalAttributeAssociation,
    ) -> Result<i64> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        let id = MetadataStore::insert_supplemental_attribute_association(&tx, &assoc)?;
        tx.commit()?;
        Ok(id)
    }

    /// Attach many in one all-or-nothing transaction, returning the id of each
    /// in input order. A duplicate anywhere in the batch rolls the whole batch
    /// back. This is the import half of the bulk round trip whose export is
    /// [`Self::list_supplemental_attribute_associations`] with a default filter.
    ///
    /// Returns ids rather than a count, which is `.len()` on the result.
    pub fn add_supplemental_attribute_associations(
        &mut self,
        assocs: Vec<SupplementalAttributeAssociation>,
    ) -> Result<Vec<i64>> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        let mut ids = Vec::with_capacity(assocs.len());
        for assoc in &assocs {
            ids.push(MetadataStore::insert_supplemental_attribute_association(
                &tx, assoc,
            )?);
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Whether any attachment matches `filter`.
    pub fn has_supplemental_attribute_association(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<bool> {
        self.metadata.has_supplemental_attribute_association(filter)
    }

    /// Full attachment rows matching `filter`, in insertion order. The default
    /// filter exports the whole table.
    pub fn list_supplemental_attribute_associations(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<Vec<SupplementalAttributeAssociation>> {
        self.metadata
            .list_supplemental_attribute_associations(filter)
    }

    /// Distinct attribute ids matching `filter`, ascending — the attributes
    /// attached to a component when `filter.component_id` is set.
    pub fn list_supplemental_attribute_ids(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<Vec<i64>> {
        self.metadata.list_supplemental_attribute_ids(filter)
    }

    /// Distinct component ids matching `filter`, ascending — the components
    /// carrying an attribute when `filter.attribute_id` is set.
    pub fn list_components_with_attributes(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<Vec<i64>> {
        self.metadata.list_components_with_attributes(filter)
    }

    /// Remove every attachment matching `filter`, returning the number removed.
    /// Matching nothing is `Ok(0)`: the store has no view of whether the caller
    /// expected a hit, so the count is the caller's to assert on.
    pub fn remove_supplemental_attribute_associations(
        &mut self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        let removed = MetadataStore::delete_supplemental_attribute_associations(&tx, filter)?;
        tx.commit()?;
        Ok(removed)
    }

    /// Move every attachment from component `old_id` to `new_id`, returning the
    /// rows updated. Fails with [`TimeSeriesError::DuplicateAssociation`] if
    /// `new_id` already carries one of the attributes being moved.
    pub fn replace_supplemental_attribute_component_id(
        &mut self,
        old_id: i64,
        new_id: i64,
    ) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        let updated =
            MetadataStore::replace_supplemental_attribute_component_id(&tx, old_id, new_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Number of attachments matching `filter`.
    pub fn count_supplemental_attribute_associations(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<i64> {
        self.metadata
            .count_supplemental_attribute_associations(filter)
    }

    /// Number of *distinct* attributes among the attachments matching `filter`.
    pub fn count_supplemental_attributes(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<i64> {
        self.metadata.count_supplemental_attributes(filter)
    }

    /// Number of *distinct* components among the attachments matching `filter`.
    pub fn count_components_with_attributes(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<i64> {
        self.metadata.count_components_with_attributes(filter)
    }

    /// Attachment counts grouped by attribute type.
    pub fn supplemental_attribute_counts_by_type(&self) -> Result<Vec<(String, i64)>> {
        self.metadata.supplemental_attribute_counts_by_type()
    }

    /// Attachment counts grouped by both type names, ordered by attribute type
    /// then component type.
    pub fn supplemental_attribute_summary(&self) -> Result<Vec<SupplementalAttributeSummaryRow>> {
        self.metadata.supplemental_attribute_summary()
    }

    // ---- Parent/child associations ----------------------------------------
    //
    // Directed edges between components — a generator (parent) connected to a
    // bus (child), say. Same independence from time series as attachments above.

    /// Record a parent/child edge. Fails with
    /// [`TimeSeriesError::DuplicateAssociation`] if that ordered pair is already
    /// related.
    /// Record a directed edge, returning the catalog id it was filed under. As
    /// on [`Self::add_supplemental_attribute_association`], the catalog assigns
    /// it and `assoc.id` is ignored, over this table's own id stream.
    pub fn add_parent_child_association(&mut self, assoc: ParentChildAssociation) -> Result<i64> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        let id = MetadataStore::insert_parent_child_association(&tx, &assoc)?;
        tx.commit()?;
        Ok(id)
    }

    /// Record many edges in one all-or-nothing transaction, returning the id of
    /// each in input order. The count is `.len()` on the result.
    pub fn add_parent_child_associations(
        &mut self,
        assocs: Vec<ParentChildAssociation>,
    ) -> Result<Vec<i64>> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        let mut ids = Vec::with_capacity(assocs.len());
        for assoc in &assocs {
            ids.push(MetadataStore::insert_parent_child_association(&tx, assoc)?);
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Whether any edge matches `filter`.
    pub fn has_parent_child_association(&self, filter: &ParentChildFilter) -> Result<bool> {
        self.metadata.has_parent_child_association(filter)
    }

    /// Full edge rows matching `filter`, in insertion order.
    pub fn list_parent_child_associations(
        &self,
        filter: &ParentChildFilter,
    ) -> Result<Vec<ParentChildAssociation>> {
        self.metadata.list_parent_child_associations(filter)
    }

    /// Distinct child ids matching `filter`, ascending — the children of a
    /// component when `filter.parent_id` is set.
    pub fn list_children(&self, filter: &ParentChildFilter) -> Result<Vec<i64>> {
        self.metadata.list_children(filter)
    }

    /// Distinct parent ids matching `filter`, ascending — the parents of a
    /// component when `filter.child_id` is set.
    pub fn list_parents(&self, filter: &ParentChildFilter) -> Result<Vec<i64>> {
        self.metadata.list_parents(filter)
    }

    /// Remove every edge matching `filter`, returning the number removed.
    /// Matching nothing is `Ok(0)`.
    pub fn remove_parent_child_associations(
        &mut self,
        filter: &ParentChildFilter,
    ) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        let removed = MetadataStore::delete_parent_child_associations(&tx, filter)?;
        tx.commit()?;
        Ok(removed)
    }

    /// Rewrite component `old_id` to `new_id` on both ends of every edge,
    /// returning the rows updated. Fails with
    /// [`TimeSeriesError::DuplicateAssociation`] if the rewrite would duplicate
    /// an edge `new_id` already has.
    pub fn replace_parent_child_component_id(&mut self, old_id: i64, new_id: i64) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        let updated = MetadataStore::replace_parent_child_component_id(&tx, old_id, new_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Number of edges matching `filter`.
    pub fn count_parent_child_associations(&self, filter: &ParentChildFilter) -> Result<i64> {
        self.metadata.count_parent_child_associations(filter)
    }

    /// Delete every stored timestamp vector no association references any more,
    /// returning how many went. The array-file half of what
    /// [`MetadataStore::sweep_orphan_feature_sets`] does for the catalog.
    ///
    /// Vectors are shared, so removing one series can never cascade into
    /// deleting the axis its cohort still sits on; the unreachable ones
    /// accumulate until a compaction reclaims them, exactly as unreachable
    /// arrays do. [`Self::clear_time_series`] is the exception, and goes through
    /// [`Self::orphaned_timestamp_vectors`] directly so its frees can be
    /// deferred under an open transaction.
    fn sweep_orphan_timestamp_vectors(&mut self) -> Result<usize> {
        let orphans = self.orphaned_timestamp_vectors()?;
        for hash in &orphans {
            self.backend.remove_timestamps(hash)?;
        }
        Ok(orphans.len())
    }

    /// Every timestamp vector the array file holds that no catalog row sits on.
    ///
    /// Sorted, so two calls on one store agree on the order and a caller's own
    /// reporting is stable.
    fn orphaned_timestamp_vectors(&self) -> Result<Vec<[u8; 32]>> {
        let (referenced, problems) = self.metadata.referenced_timestamp_hashes()?;
        // A row whose locator cannot be read might be the one holding a vector
        // alive; sweeping against a catalog that damaged would delete data the
        // repair needs. Refuse instead, and leave the diagnosis to
        // `verify_integrity`, whose job it is.
        if let Some(problem) = problems.first() {
            return Err(TimeSeriesError::IntegrityError(format!(
                "cannot sweep timestamp vectors: {problem}"
            )));
        }
        let mut orphans: Vec<[u8; 32]> = self
            .backend
            .timestamp_hashes()?
            .into_iter()
            .filter(|hash| !referenced.contains(hash))
            .collect();
        orphans.sort_unstable();
        Ok(orphans)
    }

    /// Reclaim space in both halves of the artifact.
    ///
    /// Both content-addressed shared sets are swept first: the feature sets no
    /// association references any more, out of the catalog, and the timestamp
    /// vectors none references any more, out of the array file.
    ///
    /// On the array side, for an on-disk store, this **rewrites the HDF5 file**:
    /// every array the catalog still references is written into a fresh sibling
    /// file, which then replaces the original. HDF5 cannot return freed space to
    /// the filesystem in place, so a rewrite is the only way a deletion actually
    /// shrinks the store. What the new file leaves behind is the freed packed
    /// slots (live columns are laid out contiguously again) and any dataset
    /// nothing references — an interrupted bulk add's leftovers, or a tombstone
    /// written by a version that did not unlink on removal. The `.sqlite` half
    /// is untouched: arrays are content-addressed, so a different physical
    /// layout is invisible to it.
    ///
    /// An in-memory store has no file to rewrite; there the array side is just
    /// the backend dropping its tombstone bookkeeping.
    ///
    /// # Single writer
    ///
    /// Replacing the file assumes this process is the store's only user, which
    /// is the store's model in general. A second process holding the old file
    /// open keeps reading the old inode on Unix (it will not see the compacted
    /// data, and its handle keeps the old bytes on disk until it closes); on
    /// Windows its lock makes the replacement fail, and the error surfaces with
    /// this store still open on the original file.
    pub fn compact(&mut self) -> Result<CompactionReport> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        // Compaction physically reclaims slots, which a rollback could still need
        // — an open transaction keeps removed arrays alive precisely so it can be
        // undone. Reclaiming them mid-transaction would make that impossible.
        if self.in_transaction() {
            return Err(TimeSeriesError::InvalidParameter(
                "cannot compact while a transaction is open; commit or roll back first".into(),
            ));
        }
        // Size the file before anything this call does can change it. The flush
        // below is part of that: HDF5 does hand back the blocks a removal freed
        // *at the end of the file*, truncating on flush, so measuring after it
        // would credit the caller with nothing for space this call reclaimed.
        // What a caller observes is `stat` before the call against `stat`
        // after, and that is what the report should say.
        let bytes_before = self
            .file_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len());
        // Checkpoint the catalog WAL and flush the arrays so both halves are
        // complete on disk before the rewrite reads from them.
        self.flush()?;
        // Sweep the catalog first: the rewrite's liveness scan should see the
        // post-sweep catalog.
        let tx = self.metadata.savepoint()?;
        let feature_sets_reclaimed = MetadataStore::sweep_orphan_feature_sets(&tx)?;
        tx.commit()?;
        // The timestamp vectors are in the array file, not the catalog, so their
        // sweep is a diff rather than a `DELETE`: whatever the backend holds and
        // no row still names. Done before the rewrite below, which copies only
        // what survives it.
        let timestamp_sets_reclaimed = self.sweep_orphan_timestamp_vectors()?;

        let Some(path) = self.file_path.clone() else {
            let mut report = self.backend.compact()?;
            report.feature_sets_reclaimed = feature_sets_reclaimed;
            report.timestamp_sets_reclaimed = timestamp_sets_reclaimed;
            return Ok(report);
        };

        let before = self.backend.stats();

        // Sibling of the original so the rename stays within one filesystem and
        // is therefore atomic, and uniquely named so a concurrent writer on a
        // filesystem where the HDF5 lock is silently unavailable cannot stage
        // through the same path — see `temp_tag`. A crash mid-rewrite leaves the
        // original intact plus this temp file, which is left for the caller to
        // remove.
        let tmp = repack_temp_path(&path, &temp_tag());
        match std::fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        // Carry the existing stamp into the rewrite rather than minting one.
        // Compaction replaces only the HDF5 half and leaves the catalog
        // untouched, so a fresh stamp here would manufacture exactly the
        // mismatch the stamp exists to detect.
        let generation = self.backend.generation();
        let rewritten = (|| -> Result<()> {
            let mut backend = Hdf5Backend::create(&tmp, self.compression())?;
            if let Some(generation) = &generation {
                backend.set_generation(generation)?;
            }
            self.materialize_into(&mut backend)?;
            backend.flush()?;
            drop(backend);
            Ok(())
        })();
        if let Err(e) = rewritten {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }

        // HDF5 keeps a byte-range lock on an open file, so the live handle has
        // to go before the original is replaced (required on Windows, correct
        // everywhere). The placeholder backend is never observed: nothing else
        // runs between the swap and the reopen.
        drop(std::mem::replace(
            &mut self.backend,
            Box::new(MemoryBackend::new()) as Box<dyn StorageBackend>,
        ));
        let renamed = std::fs::rename(&tmp, &path);
        if renamed.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        // Reopen before surfacing a rename failure, so a failed compaction
        // leaves the store usable instead of stranded on the placeholder.
        self.backend = open_backend(&path, self.read_only)?;
        renamed?;

        // The `stat` after, taken on the replaced file rather than on the temp
        // copy, so it pairs with `bytes_before` on the same path. Both stats
        // fall back the same way: a failure to size a file reports nothing
        // reclaimed. Defaulting this one to 0 instead would credit the caller
        // with having reclaimed the entire file.
        let bytes_after = std::fs::metadata(&path).map_or(bytes_before, |m| m.len());
        let after = self.backend.stats();
        Ok(CompactionReport {
            slots_reclaimed: before.free_packed_slots,
            datasets_dropped: before.data_datasets.saturating_sub(after.data_datasets),
            feature_sets_reclaimed,
            timestamp_sets_reclaimed,
            bytes_reclaimed: bytes_before.saturating_sub(bytes_after),
        })
    }

    /// Recompute every stored array's content hash and report the ones that
    /// disagree with the hash recorded alongside them.
    ///
    /// # Scope: the content the catalog points at
    ///
    /// A persisted store is two artifacts — the HDF5 file and its companion
    /// `<path>.sqlite` catalog. This takes the catalog as the statement of what
    /// *should* be there and checks the HDF5 half against it: it collects every
    /// array and every explicit time axis the catalog references, reads each one
    /// back, rehashes it, and compares. So it does cross-reference the two —
    /// what it never does is check the catalog against *itself*, and an empty
    /// report is not a statement that the store as a whole is sound. In
    /// particular these are invisible to it:
    ///
    /// - a catalog row whose `dtype`, `element_shape`, or `length` misdescribes
    ///   the array it points at — the hash addresses the array's own content, so
    ///   an array that matches its hash still passes while the row lies about it;
    /// - a missing catalog: opening read-write with the `.sqlite` half deleted
    ///   silently recreates it empty, and the resulting store — zero time series,
    ///   every array still on disk and now unreachable — verifies clean, because
    ///   a catalog that references nothing is a clean bill of health here;
    /// - anything about a stored array or axis the catalog does *not* reference:
    ///   the sweep never reaches it, whatever state it is in.
    ///
    /// What it does catch is the corruption it is named for: a stored value
    /// perturbed behind its recorded hash — an array's or a time axis's alike —
    /// something the catalog names and the file does not hold (a truncated
    /// catalog, or one paired with the wrong HDF5 file, shows up this way), and
    /// a read failure on either.
    ///
    /// For catalog-side checks use the purpose-built calls instead:
    /// [`Self::check_static_consistency`] verifies that every series at a given
    /// resolution agrees on the grid, and [`Self::compact`] reports the
    /// unreachable arrays and feature sets a delete left behind (both of which
    /// are expected states, not corruption — see
    /// `docs/src/reference/file-format.md`).
    pub fn verify_integrity(&self) -> Result<IntegrityReport> {
        let (referenced, mut errors) = self.metadata.referenced_arrays()?;
        let arrays: Vec<([u8; 32], Dtype)> = referenced
            .into_iter()
            .map(|(hash, element_type)| (hash, element_type.physical_dtype()))
            .collect();
        let mut report = self.backend.verify(&arrays)?;
        // Catalog-side problems lead: a row too malformed to name an array is
        // why the array-side sweep skipped it.
        errors.append(&mut report.errors);
        // The explicit time axes are in this same file and are checked the same
        // way, for the same reason: a vector content-addresses its own values,
        // so reading it back and rehashing is what catches one perturbed behind
        // an unchanged dataset name — presence alone would report such a store
        // clean while every read of that cohort silently returned the altered
        // axis. Sorted first, so a report does not depend on hash-set iteration
        // order.
        let (referenced, mut problems) = self.metadata.referenced_timestamp_hashes()?;
        errors.append(&mut problems);
        let mut referenced: Vec<[u8; 32]> = referenced.into_iter().collect();
        referenced.sort_unstable();
        for hash in referenced {
            match self.backend.get_timestamps(&hash) {
                Ok(timestamps) => {
                    let recomputed = crate::hash::timestamps_hash(&timestamps);
                    if recomputed != hash {
                        errors.push(format!(
                            "timestamp vector hash mismatch: stored={} computed={}",
                            crate::hash::hash_hex(&hash),
                            crate::hash::hash_hex(&recomputed),
                        ));
                    }
                }
                Err(TimeSeriesError::NotFound) => errors.push(format!(
                    "dangling reference: the catalog references timestamp vector {} but the \
                     array file does not hold it",
                    crate::hash::hash_hex(&hash),
                )),
                Err(e) => errors.push(format!(
                    "read error for timestamp vector {}: {e}",
                    crate::hash::hash_hex(&hash)
                )),
            }
        }
        Ok(IntegrityReport { errors })
    }

    pub fn flush(&mut self) -> Result<()> {
        // Checkpoint the catalog's WAL so the `.sqlite` file is complete on
        // its own: after a flush the two on-disk artifacts can be copied as a
        // pair (`Self::persist_to` relies on this via the `flush` it opens
        // with).
        self.metadata.checkpoint()?;
        self.backend.flush()
    }

    /// Persist this store's data to `path` (the HDF5 arrays) and its companion
    /// `<path>.sqlite` (the catalog). Works for every combination of backend and
    /// [`CatalogMode`]: an on-disk store's file is copied, an in-memory one's
    /// arrays are materialized, and the catalog is written from wherever it
    /// lives. Existing target files are replaced.
    ///
    /// Because arrays are content-addressed, copying every array by hash plus the
    /// full metadata database reproduces all time series — static, forecast, and
    /// non-sequential — without reconstructing per-type semantics.
    ///
    /// Saving an [`CatalogMode::Attached`] store onto its own path is a no-op:
    /// the destination already *is* this store, and the flush above made it
    /// durable. The [`CatalogMode::InMemory`] counterpart is not — there the
    /// arrays are already at `path` and the save is what writes the catalog
    /// beside them, which is the scratch-directory workflow.
    ///
    /// # Atomicity, and its limit
    ///
    /// Both halves are written to temporary siblings, fsynced, and only then
    /// renamed into place, so a crash before the first rename leaves the
    /// destination untouched. The pair is stamped with a fresh generation, so a
    /// crash *between* the two renames is caught: the halves disagree and
    /// [`Store::open`] fails with
    /// [`TimeSeriesError::MismatchedArtifact`](crate::TimeSeriesError::MismatchedArtifact)
    /// rather than reading a store that quietly contradicts itself.
    ///
    /// What this does **not** give you is a destination that survives a failed
    /// save. The renames replace the target, so a crash between them destroys
    /// whatever pair was there before. That is a deliberate trade: it converts
    /// silent corruption into loud, detectable loss. Callers must not assume the
    /// destination is intact after a failed `persist_to` — recover by saving
    /// again from this store, which is still live and unchanged.
    pub fn persist_to(&mut self, path: &Path) -> Result<()> {
        // An open transaction means the catalog holds uncommitted rows that a
        // rollback would take back. Writing them out would persist a state the
        // caller has not committed to.
        if self.in_transaction() {
            return Err(TimeSeriesError::InvalidParameter(
                "cannot persist while a transaction is open; commit or roll back first".into(),
            ));
        }
        // The renames below replace whatever is at `path`. Another handle in
        // this process holding it open would keep reading the file that was
        // renamed away -- the same hazard `StoreInUse` refuses at open -- so
        // the destination is held for the whole save, not just probed: an
        // open landing between a probe and the rename would be replaced under
        // all the same. This store's own path is exempt: that handle is `self`.
        let _destination = if self
            .file_path
            .as_deref()
            .is_none_or(|src| !same_file(src, path))
        {
            Some(OpenGuard::acquire(path)?)
        } else {
            None
        };
        self.flush()?;

        // Saving an attached catalog onto its own artifact is already done: the
        // `flush` above checkpointed the catalog and flushed HDF5, so both
        // halves are durable, paired, and exactly what a staged save would
        // produce. Running the staged path anyway would rename a fresh catalog
        // over the file this store's own connection still has open — which
        // orphans that connection and leaves its `-wal` to be recovered against
        // a database it does not belong to, corrupting the store.
        //
        // This is a no-op only for `Attached`. An in-memory catalog saving to
        // its own HDF5 path is the scratch-directory workflow `CatalogMode`
        // exists for, and has real work to do: the sidecar does not exist yet.
        if self.catalog == CatalogMode::Attached
            && self
                .file_path
                .as_deref()
                .is_some_and(|src| same_file(src, path))
        {
            return Ok(());
        }

        let sqlite_path = catalog_sqlite_path(path);
        // Unique per save, so a concurrent save to this same destination cannot
        // clear this one's in-flight temp out from under it — see `temp_tag`.
        let tag = temp_tag();
        let tmp_h5 = persist_temp_path(path, &tag);
        let tmp_sqlite = persist_temp_path(&sqlite_path, &tag);
        // `VACUUM INTO` refuses an existing target. With a unique tag nothing
        // should be here, so this only covers a tag that repeated against a
        // leftover.
        remove_if_exists(&tmp_h5)?;
        remove_if_exists(&tmp_sqlite)?;

        // A fresh stamp per save is what makes an interrupted save detectable.
        // Reusing the source's would leave a re-save to the same destination
        // (the modify-then-save-again flow) with matching stamps on mismatched
        // content. It is written to the temporaries only: stamping the live
        // catalog would unpair it from its own HDF5 file.
        let generation = mint_generation();

        let staged = match self.file_path.clone() {
            // Arrays live in this process. Read them out of the live backend
            // into a new file; the catalog is the liveness source, so this
            // writes exactly the referenced set.
            None => self.stage_persist_from_memory(&tmp_h5, &tmp_sqlite, &generation),
            // Arrays are already a file — copy it rather than rewriting it, so
            // the saved layout matches the live one.
            //
            // HDF5 keeps a byte-range lock on an open file, which on Windows
            // makes both `fs::copy` (ERROR_LOCK_VIOLATION) and a rename over the
            // source fail. So the handle goes away for the whole write-and-swap
            // and is reopened at the end — including on the failure path, so a
            // failed save leaves the store usable rather than stranded on the
            // placeholder. The placeholder is never observed: nothing else runs
            // in between.
            Some(src) => {
                // Read before the handle goes away: `fs::copy` clones the
                // source's `data_format_version` along with everything else, so
                // a source still owing a re-stamp (an `InMemory` open, where the
                // catalog migrated in RAM and the array file was deliberately
                // left alone) would publish an old stamp over a current catalog
                // — and nothing at the destination would ever discharge it.
                let owed = self.backend.pending_format_upgrade();
                drop(std::mem::replace(
                    &mut self.backend,
                    Box::new(MemoryBackend::new()) as Box<dyn StorageBackend>,
                ));
                let staged = std::fs::copy(&src, &tmp_h5)
                    .map_err(TimeSeriesError::from)
                    .and_then(|_| stamp_staged_copy(&tmp_h5, owed))
                    .and_then(|_| self.stage_persist_catalog(&tmp_h5, &tmp_sqlite, &generation));
                let swapped = staged
                    .and_then(|()| Self::swap_into_place(&tmp_h5, path, &tmp_sqlite, &sqlite_path));
                self.backend = open_backend(&src, self.read_only)?;
                return swapped.inspect_err(|_| Self::clear_temps(&tmp_h5, &tmp_sqlite));
            }
        };

        staged
            .and_then(|()| Self::swap_into_place(&tmp_h5, path, &tmp_sqlite, &sqlite_path))
            .inspect_err(|_| Self::clear_temps(&tmp_h5, &tmp_sqlite))
    }

    /// Write only the **array half** to `path`, leaving no catalog beside it.
    ///
    /// The mirror of [`Self::persist_catalog`], which writes only the other
    /// half, and the write-side counterpart of
    /// [`Self::open_without_catalog`]: together they are how a consumer ships an
    /// artifact as arrays plus a document of its own, with the catalog's rows
    /// carried in that document rather than in a `.sqlite` nobody reads.
    /// Everything the catalog holds is already in such a document — every row's
    /// `association_id`, and its `data_hash` pointer into the file written here.
    ///
    /// Which arrays land follows the backend, exactly as [`Self::persist_to`]
    /// does: an in-memory store is materialized, so only the arrays the catalog
    /// still references are written, while an on-disk store's file is copied
    /// whole — dead slots and unlinked datasets included, since HDF5 does not
    /// reclaim that space in place. [`Self::compact`] is what drops them; run it
    /// first when the bundle's size matters.
    ///
    /// Atomic, unlike `persist_to`. There is one file to publish, so there is
    /// one rename, and the interrupted-between-two-renames case that the
    /// generation stamp exists to catch cannot arise. The file still carries a
    /// fresh stamp, which `open_without_catalog` copies onto the catalog it
    /// mints, so the rebuilt pair agrees.
    ///
    /// Refuses to write over an existing catalog's partner: a `<path>.sqlite`
    /// beside the destination would be paired with the file this replaces, and
    /// publishing new arrays under it produces exactly the dangling-rows
    /// artifact [`TimeSeriesError::StoreExists`] guards against elsewhere.
    pub fn persist_arrays_to(&mut self, path: &Path) -> Result<()> {
        if self.in_transaction() {
            return Err(TimeSeriesError::InvalidParameter(
                "cannot persist while a transaction is open; commit or roll back first".into(),
            ));
        }
        // Writing the live arrays onto the file this store is reading them from
        // would be a rename over its own open handle, and there is no catalog
        // here to make the result meaningful anyway.
        if self
            .file_path
            .as_deref()
            .is_some_and(|src| same_file(src, path))
        {
            return Err(TimeSeriesError::InvalidParameter(
                "cannot persist a store's arrays onto its own array file".into(),
            ));
        }
        // The rename below replaces whatever array file is at `path`. As in
        // `persist_to`, a handle in this process holding it open would keep
        // reading the file that was renamed away, so the destination is held
        // for the whole save rather than probed.
        let _destination = OpenGuard::acquire(path)?;
        let sqlite_path = catalog_sqlite_path(path);
        if sqlite_path.exists() {
            return Err(TimeSeriesError::StoreExists {
                path: sqlite_path.display().to_string(),
            });
        }
        self.flush()?;

        let tag = temp_tag();
        let tmp_h5 = persist_temp_path(path, &tag);
        remove_if_exists(&tmp_h5)?;
        let generation = mint_generation();

        let staged = (|| -> Result<()> {
            match self.file_path.clone() {
                // Arrays live in this process: read them out of the live
                // backend, which writes exactly the referenced set.
                None => {
                    let mut backend = Hdf5Backend::create(&tmp_h5, self.compression())?;
                    self.materialize_into(&mut backend)?;
                    backend.flush()?;
                    drop(backend);
                }
                // Already a file — copy it, so the saved layout matches the
                // live one. The same HDF5 byte-range lock `persist_to` works
                // around applies, so the handle is dropped for the copy and
                // reopened afterwards, on the failure path too.
                Some(src) => {
                    let owed = self.backend.pending_format_upgrade();
                    drop(std::mem::replace(
                        &mut self.backend,
                        Box::new(MemoryBackend::new()) as Box<dyn StorageBackend>,
                    ));
                    let copied = std::fs::copy(&src, &tmp_h5)
                        .map_err(TimeSeriesError::from)
                        .and_then(|_| stamp_staged_copy(&tmp_h5, owed));
                    self.backend = open_backend(&src, self.read_only)?;
                    copied?;
                }
            }
            crate::storage::hdf5::stamp_generation(&tmp_h5, &generation)?;
            sync_file(&tmp_h5)
        })();

        staged
            .and_then(|()| {
                sync_parent_dir(path)?;
                std::fs::rename(&tmp_h5, path)?;
                sync_parent_dir(path)
            })
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp_h5);
            })
    }

    /// Write an in-memory catalog out to this store's *own* `<path>.sqlite`,
    /// pairing it with the HDF5 file already sitting there.
    ///
    /// [`Self::persist_to`] aimed at another path has to copy the arrays;
    /// this writes only the catalog half, because the arrays are already
    /// exactly where they belong. That is what makes [`CatalogMode::InMemory`]
    /// usable as the thing it is good for — skipping per-commit journaling
    /// during a single-process bulk load — without paying a full copy of the
    /// array file to land the result. It is also what a single command-per-
    /// process tool needs: an in-memory catalog that is never written before
    /// the process exits is not "not yet durable", it is *gone*, and the arrays
    /// it named become unreachable.
    ///
    /// The staged catalog is stamped with whatever generation the HDF5 half
    /// already carries (including none, for an artifact predating stamping), so
    /// the pair agrees. It is staged, fsynced, and renamed, like
    /// `persist_to`'s catalog half — but there is only one rename here, so
    /// unlike `persist_to` this *is* atomic.
    ///
    /// The catalog stays in memory afterwards. This is a checkpoint, not a mode
    /// switch: later mutations are again only in RAM until the next call.
    ///
    /// For a [`CatalogMode::Attached`] store the catalog already is that file,
    /// so this is [`Self::flush`]. Errors on a store with no HDF5 file, which
    /// has no half to pair a catalog with.
    pub fn persist_catalog(&mut self) -> Result<()> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        if self.in_transaction() {
            return Err(TimeSeriesError::InvalidParameter(
                "cannot persist the catalog while a transaction is open; commit or roll back first"
                    .into(),
            ));
        }
        if self.catalog == CatalogMode::Attached {
            return self.flush();
        }
        let Some(path) = self.file_path.clone() else {
            return Err(TimeSeriesError::InvalidParameter(
                "an in-memory store has no HDF5 file for a catalog to pair with; use persist_to"
                    .into(),
            ));
        };
        // The arrays have to be on disk before the catalog that names them: a
        // catalog referencing an array still sitting in a write buffer is the
        // dangling-reference state this whole pairing exists to prevent.
        self.backend.flush()?;

        let sqlite_path = catalog_sqlite_path(&path);
        let tmp_sqlite = persist_temp_path(&sqlite_path, &temp_tag());
        remove_if_exists(&tmp_sqlite)?;

        let staged = (|| -> Result<()> {
            self.metadata.backup_to(&tmp_sqlite)?;
            {
                let staged = MetadataStore::open_path(&tmp_sqlite, false)?;
                // Match the file this catalog is being paired with rather than
                // minting a stamp: nothing about the HDF5 half changed, so a
                // fresh generation would manufacture the exact mismatch the
                // stamp exists to detect.
                if let Some(generation) = self.backend.generation() {
                    staged.set_generation(&generation)?;
                }
                staged.checkpoint()?;
            }
            sync_file(&tmp_sqlite)?;
            sync_parent_dir(&sqlite_path)?;
            // Sidecars of the catalog being replaced, for the reason spelled
            // out in `swap_into_place`: SQLite would recover a stale `-wal` over
            // the database landing in its place.
            remove_if_exists(&sqlite_sidecar(&sqlite_path, "-wal"))?;
            remove_if_exists(&sqlite_sidecar(&sqlite_path, "-shm"))?;
            std::fs::rename(&tmp_sqlite, &sqlite_path)?;
            sync_parent_dir(&sqlite_path)
        })();
        staged.inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp_sqlite);
        })?;

        // The migrated catalog is now the one on disk, so the re-stamp that an
        // `InMemory` open deliberately deferred can finally be discharged. This
        // is the same three-step ordering `open_with_catalog` uses, spread over
        // a session instead of an open: the catalog lands durably first, and
        // only then does the array file claim the newer format.
        //
        // After the catalog, never before. A failure here leaves the safe
        // direction — a current catalog under an older array stamp, which an
        // older build reads without ever writing a row it does not understand.
        if self.backend.pending_format_upgrade() {
            self.backend.finish_format_upgrade()?;
        }
        Ok(())
    }

    /// Write both halves of a save for a store whose arrays live in memory.
    fn stage_persist_from_memory(
        &mut self,
        tmp_h5: &Path,
        tmp_sqlite: &Path,
        generation: &str,
    ) -> Result<()> {
        let mut backend = Hdf5Backend::create(tmp_h5, self.compression())?;
        self.materialize_into(&mut backend)?;
        backend.flush()?;
        drop(backend);
        self.stage_persist_catalog(tmp_h5, tmp_sqlite, generation)
    }

    /// Stamp the staged HDF5 file, write the catalog beside it, stamp that too,
    /// and fsync both — everything that has to be durable before the renames.
    fn stage_persist_catalog(
        &self,
        tmp_h5: &Path,
        tmp_sqlite: &Path,
        generation: &str,
    ) -> Result<()> {
        crate::storage::hdf5::stamp_generation(tmp_h5, generation)?;
        sync_file(tmp_h5)?;

        // `VACUUM INTO` reads through to committed WAL content, so this captures
        // the catalog whether it is a file or `:memory:`. The stamp goes on
        // afterwards, into the copy, leaving the live catalog's own alone.
        self.metadata.backup_to(tmp_sqlite)?;
        {
            let staged = MetadataStore::open_path(tmp_sqlite, false)?;
            staged.set_generation(generation)?;
            staged.checkpoint()?;
        }
        sync_file(tmp_sqlite)
    }

    /// Move both staged halves into place. The window between the two renames is
    /// what the generation stamp exists to cover — see [`Self::persist_to`].
    fn swap_into_place(tmp_h5: &Path, h5: &Path, tmp_sqlite: &Path, sqlite: &Path) -> Result<()> {
        sync_parent_dir(h5)?;
        std::fs::rename(tmp_h5, h5)?;
        // A `-wal`/`-shm` pair beside the destination belongs to the catalog
        // being replaced — a writer that crashed there left one behind. SQLite
        // would recover it over the database landing in its place, resurrecting
        // the old catalog's pages, so the save silently would not take (and,
        // with the stamp, the artifact then fails to open at all). Clearing them
        // *before* the rename keeps the crash window harmless: what it can
        // interrupt is the replacement of a catalog that is already outlived by
        // the HDF5 half renamed above, and which the stamp already flags.
        remove_if_exists(&sqlite_sidecar(sqlite, "-wal"))?;
        remove_if_exists(&sqlite_sidecar(sqlite, "-shm"))?;
        std::fs::rename(tmp_sqlite, sqlite)?;
        sync_parent_dir(h5)
    }

    fn clear_temps(tmp_h5: &Path, tmp_sqlite: &Path) {
        let _ = std::fs::remove_file(tmp_h5);
        let _ = std::fs::remove_file(tmp_sqlite);
    }

    /// Write every array the catalog still references into `backend`, choosing
    /// each one's physical layout from scratch.
    ///
    /// The catalog is the liveness source: an array no association names is
    /// simply never read, so it does not make it into `backend`. That is what
    /// makes this both the materialization step of [`Self::persist_to`] for an
    /// in-memory store and the rewrite step of [`Self::compact`] for an on-disk
    /// one — in both cases the destination ends up holding the live set and
    /// nothing else.
    fn materialize_into(&mut self, backend: &mut Hdf5Backend) -> Result<()> {
        // Plan each distinct array's layout before writing: packed is only
        // valid for arrays that every referencing association reads as a
        // series along axis 0 (the static types). Dense forecasts must stay
        // standalone — the forecast window read path rejects packed arrays.
        let mut plans: HashMap<[u8; 32], ArrayPlan> = HashMap::new();
        // The time axes the irregular rows sit on, collected per *row* rather
        // than per array: a hash shared between a regular and an irregular
        // series keeps only one plan, and it may be the regular one.
        let mut axes: HashSet<[u8; 32]> = HashSet::new();
        for meta in self.list_with_timestamps(ListFilter::default())? {
            let plan = ArrayPlan {
                layout: array_layout_for(meta.time_series_type),
                pool: pool_key_of(&meta),
            };
            if let PackGroup::Irregular(hash) = plan.pool.3 {
                axes.insert(hash);
            }
            plans
                .entry(meta.data_hash)
                // A hash shared across keys must use a standalone layout if
                // any referencing key is standalone (the window read rejects
                // packed); the first non-packed layout wins and sticks.
                .and_modify(|existing| {
                    if existing.layout.is_packed() {
                        existing.layout = plan.layout;
                    }
                })
                .or_insert(plan);
        }
        // Same bet as the write path: a pool only pays once several arrays
        // share it, and here the whole store is in hand, so cohort sizes are
        // exact rather than batch-local.
        let mut cohort: HashMap<PoolKey, usize> = HashMap::new();
        for plan in plans.values().filter(|p| p.layout.is_packed()) {
            *cohort.entry(plan.pool.clone()).or_default() += 1;
        }
        // Create each pool at exactly its cohort width before writing anything
        // into it. Left to grow on demand, a pool reserves room for a thousand
        // columns and pays 64 bytes of hash companion for every unfilled one —
        // enough that rewriting a bulk-written store would make the file bigger
        // instead of smaller.
        for (pool, &count) in &cohort {
            // An irregular cohort of one is written standalone below, so it
            // needs no pool at all.
            if matches!(pool.3, PackGroup::Irregular(_)) && count < 2 {
                continue;
            }
            let mut remaining = count;
            while remaining > 0 {
                let created =
                    backend.reserve_pack_group(pool.0, &pool.1, pool.2, pool.3, remaining)?;
                // A pool that cannot hold a single column would loop forever;
                // `resolve_dataset_cols` clamps to at least 1, so this only
                // guards against a future change to that floor.
                if created == 0 {
                    break;
                }
                remaining = remaining.saturating_sub(created);
            }
        }
        // The time axes travel with the rows that sit on them: they are
        // content-addressed data in this same file, and a rewrite that dropped
        // one would leave every `NonSequentialTimeSeries` on it unreadable. Only
        // the referenced ones are copied, which is what makes the rewrite a
        // sweep of the rest.
        for hash in &axes {
            backend.put_timestamps(hash, &self.backend.get_timestamps(hash)?)?;
        }
        for (hash, plan) in &plans {
            let mut layout = plan.layout;
            if matches!(plan.pool.3, PackGroup::Irregular(_))
                && cohort.get(&plan.pool).copied().unwrap_or(0) < 2
            {
                layout = ArrayLayout::Standalone;
            }
            let element_type = self.metadata.element_type_for_hash(hash)?;
            let array = self
                .backend
                .get_array(hash, element_type.physical_dtype())?;
            backend.put_array(hash, &array, plan.pool.3, layout)?;
        }
        Ok(())
    }
}

/// A buffered bulk-add session returned by [`Store::bulk_add`]. Requests are
/// accumulated in memory via [`Self::push`] / [`Self::add`] and written together
/// by [`Self::commit`], which packs each shape group into batch-sized datasets
/// so writes fill whole chunks. Dropping the guard without calling `commit`
/// discards every buffered request and writes nothing.
pub struct BulkAdd<'a> {
    store: &'a mut Store,
    items: Vec<AddRequest>,
    committed: bool,
}

impl BulkAdd<'_> {
    /// Buffer one prebuilt request. No validation or I/O happens here; both are
    /// deferred to [`Self::commit`], which is all-or-nothing.
    pub fn push(&mut self, request: AddRequest) -> &mut Self {
        self.items.push(request);
        self
    }

    /// Buffer one request from its parts (convenience over [`Self::push`]).
    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &mut self,
        owner_id: i64,
        owner_type: &str,
        owner_category: OwnerCategory,
        data: TimeSeriesData,
        features: Features,
    ) -> &mut Self {
        self.push(AddRequest {
            owner_id,
            owner_type: owner_type.to_string(),
            owner_category,
            data,
            features,
        })
    }

    /// The number of requests buffered so far.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether no requests have been buffered.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Flush the buffer: write all arrays as batch-sized blocks and insert every
    /// association in one transaction, returning the ids in push order. On any
    /// error nothing is committed and staged arrays are rolled back.
    pub fn commit(mut self) -> Result<Vec<TimeSeriesId>> {
        self.committed = true;
        let items = std::mem::take(&mut self.items);
        self.store.flush_bulk_add(items)
    }
}

impl Drop for BulkAdd<'_> {
    fn drop(&mut self) {
        if !self.committed && !self.items.is_empty() {
            tracing::debug!(
                discarded = self.items.len(),
                "BulkAdd dropped without commit; buffered requests discarded"
            );
        }
    }
}

/// The persistence inputs derived from one [`AddRequest`]: the array content
/// hash, the resolution that keys the packed pool, whether the array is packed
/// (vs. standalone), the metadata row, and the resulting key. Shared by the
/// per-item write path ([`Store::add_time_series_bulk`]) and the buffered
/// block-write path ([`Store::bulk_add`]).
struct RequestParts {
    hash: [u8; 32],
    /// The packed pool this array belongs in — its resolution for a regular
    /// series, its interned timestamp vector for an irregular one. Carried even
    /// when `layout` is standalone (where the backend ignores it), so the write
    /// paths can group by it before deciding.
    group: PackGroup,
    layout: ArrayLayout,
    meta: TimeSeriesMetadata,
}

/// The physical storage layout for a time-series type's backing array.
///
/// Both static types pack: their arrays are read a timestamp at a time across
/// every series, which is exactly what the timestamp-major packed chunking
/// serves. They differ only in what pools them — see [`PackGroup`] — and an
/// irregular series can still be demoted to standalone when nothing shares its
/// time axis (see [`resolve_irregular_layouts`]).
///
/// The count-axis choices for dense forecasts mirror the forecast reader's
/// [`WindowRead::Dense`](crate::reader) slicing (`Deterministic` → axis 1,
/// `Probabilistic` / `Scenarios` → axis 2), so writes and reads agree on which
/// axis the windows lie along.
/// Refuse a pair of halves whose generation stamps disagree.
///
/// Stamps that disagree mean these files came from different saves — most
/// likely a `persist_to` interrupted between its two renames. Comparing the
/// `Option`s directly makes a lone stamp a mismatch too, which is the point:
/// every path that writes a stamp writes both halves together
/// (`Store::create`, `persist_to`, and `compact`, which carries the existing
/// one across), so exactly one stamped half is a half swapped out on its own.
/// Only *both* unstamped is legitimate — an artifact that predates stamping —
/// and that compares equal.
/// Bring a staged HDF5 copy up to the current format version, when the source
/// it was copied from still owed a re-stamp.
///
/// `persist_to`'s copy branch clones the file byte for byte, so the stamp comes
/// with it. The destination's catalog is the migrated one, so leaving the old
/// stamp there would publish exactly the pair this whole mechanism defers to
/// avoid — and unlike the source, the destination has no later writable
/// `Attached` open guaranteed to fix it up.
///
/// The handle must not outlive this call: HDF5 holds a byte-range lock and
/// `swap_into_place` renames this file out from under it on Windows.
fn stamp_staged_copy(tmp_h5: &Path, owed: bool) -> Result<()> {
    if !owed {
        return Ok(());
    }
    let mut staged = open_backend(tmp_h5, false)?;
    staged.finish_format_upgrade()
}

fn check_generation_pair(h5: Option<String>, sqlite: Option<String>) -> Result<()> {
    if h5 != sqlite {
        return Err(TimeSeriesError::MismatchedArtifact {
            h5: h5.unwrap_or_else(|| UNSTAMPED.into()),
            sqlite: sqlite.unwrap_or_else(|| UNSTAMPED.into()),
        });
    }
    Ok(())
}

fn array_layout_for(ts_type: TimeSeriesType) -> ArrayLayout {
    match ts_type {
        TimeSeriesType::SingleTimeSeries | TimeSeriesType::DeterministicSingleTimeSeries => {
            ArrayLayout::Packed
        }
        TimeSeriesType::NonSequentialTimeSeries => ArrayLayout::Packed,
        TimeSeriesType::Deterministic => ArrayLayout::StandaloneWindowed { count_axis: 1 },
        TimeSeriesType::Probabilistic | TimeSeriesType::Scenarios => {
            ArrayLayout::StandaloneWindowed { count_axis: 2 }
        }
    }
}

/// Derive the [`RequestParts`] for one request, validating where required
/// (`NonSequentialTimeSeries` timestamps). The static types are packed; the
/// forecasts are stored standalone.
/// Write the explicit time axis of one request into the array file, before the
/// association row that names it exists.
///
/// A no-op for every type but `NonSequentialTimeSeries`, whose
/// [`PackGroup::Irregular`] cohort key *is* the vector's content hash. `seen`
/// collapses the repeats a cohort produces — a batch of ten thousand series on
/// one axis writes it once — and a vector the store already holds is not written
/// again. `staged` collects only what this call physically wrote, so a failure
/// downstream can undo exactly that.
fn stage_timestamp_vector(
    backend: &mut dyn StorageBackend,
    group: PackGroup,
    timestamps: Option<&[chrono::DateTime<chrono::Utc>]>,
    seen: &mut SharedSetCache,
    staged: &mut StagedWrites,
) -> Result<()> {
    let (PackGroup::Irregular(hash), Some(timestamps)) = (group, timestamps) else {
        return Ok(());
    };
    if seen.note_timestamps(hash) && backend.put_timestamps(&hash, timestamps)? {
        staged.timestamps.push(hash);
    }
    Ok(())
}

fn build_request_parts(item: &AddRequest) -> Result<RequestParts> {
    // Every write funnels through here (per-column adds and buffered bulk adds
    // alike), which makes it the one place the reserved-feature-name rule has
    // to hold.
    validate_features(&item.features)?;
    let element_type = resolve_element_type(item)?;
    validate_time_reference(&item.data)?;
    validate_data(&item.data)?;
    let (hash, group, layout, meta) = match &item.data {
        TimeSeriesData::SingleTimeSeries(single) => {
            let hash = array_hash(&single.data);
            (
                hash,
                PackGroup::Regular(single.resolution),
                array_layout_for(TimeSeriesType::SingleTimeSeries),
                TimeSeriesMetadata {
                    owner_id: item.owner_id,
                    owner_type: item.owner_type.clone(),
                    owner_category: item.owner_category,
                    time_series_type: TimeSeriesType::SingleTimeSeries,
                    name: single.name.clone(),
                    data_hash: hash,
                    initial_timestamp: Some(single.initial_timestamp),
                    resolution: Some(single.resolution),
                    length: Some(single.length),
                    horizon: None,
                    interval: None,
                    count: None,
                    timestamps: None,
                    features: item.features.clone(),
                    units: item.data.units().map(str::to_owned),
                    quantity_kind: item.data.quantity_kind().map(str::to_owned),
                    unit_system: item.data.unit_system(),
                    time_reference: item.data.time_reference().cloned(),
                    component_field: item.data.component_field().map(str::to_owned),
                    percentiles: None,
                    element_type,
                    element_shape: single.data.element_shape().to_vec(),
                    application_data: item.data.application_data().map(str::to_owned),
                    id: None,
                },
            )
        }
        TimeSeriesData::NonSequentialTimeSeries(non_sequential) => {
            let hash = array_hash(&non_sequential.data);
            (
                hash,
                // An irregular series has no resolution to pool by; its cohort
                // is every series on the same explicit time axis, which the
                // catalog already content-addresses.
                PackGroup::Irregular(crate::hash::timestamps_hash(&non_sequential.timestamps)),
                array_layout_for(TimeSeriesType::NonSequentialTimeSeries),
                TimeSeriesMetadata {
                    owner_id: item.owner_id,
                    owner_type: item.owner_type.clone(),
                    owner_category: item.owner_category,
                    time_series_type: TimeSeriesType::NonSequentialTimeSeries,
                    name: non_sequential.name.clone(),
                    data_hash: hash,
                    initial_timestamp: None,
                    resolution: None,
                    length: Some(non_sequential.length),
                    horizon: None,
                    interval: None,
                    count: None,
                    timestamps: Some(non_sequential.timestamps.clone()),
                    features: item.features.clone(),
                    units: item.data.units().map(str::to_owned),
                    quantity_kind: item.data.quantity_kind().map(str::to_owned),
                    unit_system: item.data.unit_system(),
                    time_reference: item.data.time_reference().cloned(),
                    component_field: item.data.component_field().map(str::to_owned),
                    percentiles: None,
                    element_type,
                    element_shape: non_sequential.data.element_shape().to_vec(),
                    application_data: item.data.application_data().map(str::to_owned),
                    id: None,
                },
            )
        }
        // Dense forecast types are stored as standalone arrays in their
        // native shape. `DeterministicSingleTimeSeries` is not added
        // directly; it is derived from a stored `SingleTimeSeries` via
        // [`Store::transform_single_time_series`].
        TimeSeriesData::Deterministic(det) => {
            validate_deterministic(det)?;
            (
                array_hash(&det.data),
                PackGroup::Regular(det.resolution),
                array_layout_for(TimeSeriesType::Deterministic),
                forecast_metadata(
                    item,
                    TimeSeriesType::Deterministic,
                    &det.name,
                    det.initial_timestamp,
                    det.resolution,
                    det.horizon,
                    det.interval,
                    det.count,
                    &det.data,
                    element_type,
                    None,
                ),
            )
        }
        TimeSeriesData::Probabilistic(prob) => {
            validate_probabilistic(prob)?;
            (
                array_hash(&prob.data),
                PackGroup::Regular(prob.resolution),
                array_layout_for(TimeSeriesType::Probabilistic),
                forecast_metadata(
                    item,
                    TimeSeriesType::Probabilistic,
                    &prob.name,
                    prob.initial_timestamp,
                    prob.resolution,
                    prob.horizon,
                    prob.interval,
                    prob.count,
                    &prob.data,
                    element_type,
                    Some(prob.percentiles.clone()),
                ),
            )
        }
        TimeSeriesData::Scenarios(scen) => {
            validate_scenarios(scen)?;
            (
                array_hash(&scen.data),
                PackGroup::Regular(scen.resolution),
                array_layout_for(TimeSeriesType::Scenarios),
                forecast_metadata(
                    item,
                    TimeSeriesType::Scenarios,
                    &scen.name,
                    scen.initial_timestamp,
                    scen.resolution,
                    scen.horizon,
                    scen.interval,
                    scen.count,
                    &scen.data,
                    element_type,
                    None,
                ),
            )
        }
    };
    Ok(RequestParts {
        hash,
        group,
        layout,
        meta,
    })
}

/// What a packed pool is keyed by: the array's physical shape plus the time
/// axis it lies on. Two arrays land in the same HDF5 dataset iff these match.
type PoolKey = (Dtype, Vec<usize>, usize, PackGroup);

/// One distinct array's placement, as planned by [`Store::persist_to`] before it
/// rewrites an in-memory store to disk.
struct ArrayPlan {
    layout: ArrayLayout,
    pool: PoolKey,
}

/// The pool a stored row's array belongs to, read off its catalog metadata.
///
/// An irregular row is pooled by its timestamp vector and a regular one by its
/// resolution. A row with neither — a shape no static or forecast type produces
/// — takes an arbitrary resolution: it can only be standalone, where the group
/// is ignored, and reads locate arrays by content hash regardless.
fn pool_key_of(meta: &TimeSeriesMetadata) -> PoolKey {
    let group = match (&meta.timestamps, meta.resolution) {
        (Some(timestamps), _) => PackGroup::Irregular(crate::hash::timestamps_hash(timestamps)),
        (None, Some(resolution)) => PackGroup::Regular(resolution),
        (None, None) => PackGroup::Regular(Period::fixed(chrono::Duration::nanoseconds(1))),
    };
    (
        meta.element_type.physical_dtype(),
        meta.element_shape.clone(),
        meta.length.unwrap_or(0),
        group,
    )
}

fn pool_key(array: &TypedArray, group: PackGroup) -> PoolKey {
    (
        array.dtype,
        array.element_shape().to_vec(),
        array.length(),
        group,
    )
}

/// Settle each irregular request's layout, demoting the ones whose time axis
/// nothing else shares back to standalone.
///
/// Packing is what makes a timestamp-major sweep across components cheap, and it
/// is the right default for irregular series precisely because they tend to
/// arrive in cohorts on one event timeline. But it is a bet that the pool will
/// be more than one column wide: a packed dataset spreads a single array across
/// `length` chunks, so a cohort of one pays far more per-chunk overhead than the
/// one standalone dataset it replaces. This settles the bet from what is
/// knowable at write time — how many requests in this batch share the axis, plus
/// whether the store already holds a pool for it.
///
/// Getting it "wrong" costs space, never correctness: reads resolve an array by
/// content hash and handle either layout, and a group can hold columns of both
/// (see `Hdf5Backend::read_index_locked`). So a cohort that arrives one series
/// at a time simply leaves its first member standalone and packs the rest.
fn resolve_irregular_layouts(
    backend: &dyn StorageBackend,
    items: &[AddRequest],
    parts: &mut [RequestParts],
) {
    let mut in_batch: HashMap<PoolKey, usize> = HashMap::new();
    for (item, part) in items.iter().zip(parts.iter()) {
        if matches!(part.group, PackGroup::Irregular(_)) {
            *in_batch
                .entry(pool_key(request_array(item), part.group))
                .or_default() += 1;
        }
    }
    for (item, part) in items.iter().zip(parts.iter_mut()) {
        if !matches!(part.group, PackGroup::Irregular(_)) {
            continue;
        }
        let array = request_array(item);
        let key = pool_key(array, part.group);
        let shared = in_batch.get(&key).copied().unwrap_or(0) > 1
            || backend.has_pack_group(key.0, &key.1, key.2, key.3);
        if !shared {
            part.layout = ArrayLayout::Standalone;
        }
    }
}

/// The value array backing a request, regardless of time-series type.
/// Push the arrays a write put into the backend out of libhdf5's caches
/// before the catalog row naming them commits.
///
/// The catalog commit is durable on its own (WAL, synchronous), but the
/// HDF5 half was only flushed at [`Store::flush`], `persist_*`, or
/// `compact`, so a crash between an add's commit and the next of those
/// left a committed row naming an array the file never received -- the
/// dangling reference `verify_integrity` reports, produced by a call that
/// had returned `Ok`. The order matters and the reverse was already right:
/// an array with no row is space `compact` reclaims, a row with no array
/// is a series that cannot be read.
///
/// Inside a cross-operation transaction the nested commit is not durable
/// either, so the flush waits for [`Store::commit_transaction`] to do it once
/// for the whole span. The in-memory backend's flush is a no-op.
///
/// A call that staged nothing skips it. Arrays are content-addressed, so an add
/// whose hash the backend already holds returns from `put_array` before writing
/// a byte (`Ok(false)`), and its row names an array some earlier call already
/// flushed — there is no dirty buffer for this one to push. `staged` is per
/// call and lists exactly what this one put into the backend, so an empty pair
/// is precisely that case. It matters because the flush is not free: a caller
/// adding series one at a time pays an fsync-shaped cost per call, and a
/// re-add of data the store already holds paid it for nothing. A call that
/// *did* write still flushes — that is the guarantee above, and the way to
/// amortize it across many writes is [`Store::begin_transaction`] or a bulk
/// add, both of which flush once for the whole span.
fn flush_arrays_before_commit(
    backend: &mut dyn StorageBackend,
    staged: &StagedWrites,
    in_transaction: bool,
) -> Result<()> {
    if in_transaction || (staged.arrays.is_empty() && staged.timestamps.is_empty()) {
        return Ok(());
    }
    backend.flush()
}

fn request_array(item: &AddRequest) -> &TypedArray {
    data_array(&item.data)
}

/// The catalog row for a `DeterministicSingleTimeSeries` view of `src`, as
/// [`Store::transform_single_time_series`] writes it: the source's own
/// descriptors under the forecast geometry the plan derived.
///
/// `id` is cleared rather than inherited: a view is its own row, and `..src`
/// would otherwise carry the *source's* id — `Some` on anything read back from
/// the catalog — straight into a primary-key collision.
fn derived_view_row(
    src: TimeSeriesMetadata,
    horizon: Period,
    interval: Period,
    count: usize,
) -> TimeSeriesMetadata {
    TimeSeriesMetadata {
        time_series_type: TimeSeriesType::DeterministicSingleTimeSeries,
        horizon: Some(horizon),
        interval: Some(interval),
        count: Some(count),
        id: None,
        ..src
    }
}

/// Insert one association, enforcing the Deterministic/DeterministicSingleTimeSeries
/// mutual exclusion: a DST is a synthetic view of a SingleTimeSeries, so a family
/// may hold one or the other but never both. A DST is only ever created by
/// [`Store::transform_single_time_series`], so the only overlap reachable here is
/// adding a `Deterministic` when a DST already exists.
fn insert_association(
    tx: &rusqlite::Connection,
    meta: &TimeSeriesMetadata,
    cache: &mut SharedSetCache,
) -> Result<i64> {
    check_forecast_family_free(tx, meta, "add")?;
    MetadataStore::insert_batched(tx, meta, cache)
}

/// The type that may not share an abstract-deterministic family with `ty`, or
/// `None` for a type that has no such counterpart.
fn exclusive_counterpart(ty: TimeSeriesType) -> Option<TimeSeriesType> {
    match ty {
        TimeSeriesType::Deterministic => Some(TimeSeriesType::DeterministicSingleTimeSeries),
        TimeSeriesType::DeterministicSingleTimeSeries => Some(TimeSeriesType::Deterministic),
        _ => None,
    }
}

/// Reject an association that would put both `Deterministic` and
/// `DeterministicSingleTimeSeries` in one family, whichever of the two is
/// arriving. `verb` names the operation in the error.
///
/// Every path that writes an association row has to consult this, not just the
/// add path. The catalog's unique index keys on `time_series_type` and so cannot
/// enforce the rule, and `copy_time_series`, `replace_owner` and
/// `rename_time_series` all move an existing row to a *new* family identity —
/// which is exactly the operation that can put the pair together. They wrote
/// through `MetadataStore::insert`/`replace_owner`/`rename` and skipped the
/// check entirely, so all three could reach a state the rest of the code treats
/// as impossible: `resolve_forecast_key` then reports the family as ambiguous
/// forever (both candidates share resolution *and* interval, so no filter
/// narrows it), and `transform_single_time_series` refuses to run again.
///
/// The check is bidirectional here because these paths can move *either* member
/// into the other's family, where a plain add can only ever bring the
/// `Deterministic` — a DST is only ever minted by
/// [`Store::transform_single_time_series`].
fn check_forecast_family_free(
    tx: &rusqlite::Connection,
    meta: &TimeSeriesMetadata,
    verb: &str,
) -> Result<()> {
    let Some(counterpart) = exclusive_counterpart(meta.time_series_type) else {
        return Ok(());
    };
    let conflict = crate::metadata::forecast_family_conflict(
        tx,
        meta.owner_id,
        meta.owner_category,
        &meta.name,
        meta.resolution,
        &crate::hash::features_hash(&meta.features),
        counterpart,
    )?;
    if conflict {
        return Err(TimeSeriesError::InvalidParameter(format!(
            "cannot {verb} {} '{}': a {} of the same series already exists; \
             they are mutually exclusive",
            meta.time_series_type.as_str(),
            meta.name,
            counterpart.as_str(),
        )));
    }
    Ok(())
}

/// Every invariant a write must hold, for all five addable types, checked at the
/// one boundary [`build_request_parts`] gives them.
///
/// The static types were already validated here; the dense forecasts trusted
/// their constructors, which is not a boundary — the fields are `pub` and the
/// types derive `Deserialize`, so a struct literal or a `serde_json::from_str`
/// reaches the store having met nothing. The result was the failure mode
/// [`validate_single`]'s comment describes for the static path: a
/// `Deterministic` whose horizon was not a whole multiple of its resolution, or
/// whose resolution was zero, was *written* and then failed on every read with
/// an `IntegrityError` blaming the store for what the caller passed.
///
/// Reads deliberately do not go through here. They re-run the same shape and
/// period checks via the constructors (which is how a genuinely corrupt row is
/// still caught, as an `IntegrityError`), but they do not apply the millisecond
/// rule, so an artifact written before it keeps reading back exactly.
/// The descriptive attributes of a stored row, as the read path hands them back
/// to a reconstructed series. One place, so a new descriptor cannot be filled in
/// on some read paths and not others.
fn descriptors_of(meta: &TimeSeriesMetadata) -> Descriptors {
    Descriptors {
        element_type: meta.element_type,
        units: meta.units.clone(),
        quantity_kind: meta.quantity_kind.clone(),
        unit_system: meta.unit_system,
        time_reference: meta.time_reference.clone(),
        component_field: meta.component_field.clone(),
        application_data: meta.application_data.clone(),
    }
}

/// Refuse a selection that spans both coherence groups.
///
/// The partition is [`TimeReference::Zoneless`] versus everything else — three
/// named states, two groups, with the rows that left the reference unset in the
/// second one. Mixing `Utc`, `FixedOffset` and `Zone` in one selection is fine:
/// all three name instants, and a bound or a shared axis is instants.
fn reject_mixed_zoning(metas: &[TimeSeriesMetadata], what: &str) -> Result<()> {
    let mut zoneless: Option<&TimeSeriesMetadata> = None;
    let mut zoned: Option<&TimeSeriesMetadata> = None;
    for meta in metas {
        let slot = if matches!(meta.time_reference, Some(TimeReference::Zoneless)) {
            &mut zoneless
        } else {
            &mut zoned
        };
        slot.get_or_insert(meta);
        if let (Some(a), Some(b)) = (zoneless, zoned) {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "{what}: the selection mixes zoneless series with series that record \
                 instants, and the two cannot share one time bound. {:?} on owner {} is \
                 zoneless; {:?} on owner {} is not. Split the call, or narrow it with \
                 ListFilter::zoneless.",
                a.name, a.owner_id, b.name, b.owner_id
            )));
        }
    }
    Ok(())
}

/// Validate the declared timestamp spelling, and warn where a spelling and a
/// calendar period disagree about which calendar they mean.
///
/// The validation is shape only — see [`TimeReference::validate`]. It runs here
/// rather than in a constructor because the native Rust path has no binding to
/// catch a hand-built reference, and this is the funnel every write passes
/// through.
///
/// The warning covers [`Period::Months`] on a zoned series. A month period is
/// calendar arithmetic and the store steps it on the *UTC* calendar
/// ([`Period::add_to`] via chrono's `checked_add_months`), which a caller who
/// spelled their timestamps in `America/Denver` will read as an hour of drift at
/// every DST transition and a day at a month boundary. Local-frame stepping is
/// refused — it is the local → instant direction the core never runs, and it
/// would let a spelling decide which instants a series contains — so this is
/// documented behavior plus a warning that makes it findable before it is filed
/// as a bug. A caller who wants months on a local calendar wants an explicit
/// instant per value: [`NonSequentialTimeSeries`].
fn validate_time_reference(data: &TimeSeriesData) -> Result<()> {
    let Some(reference) = data.time_reference() else {
        return Ok(());
    };
    reference.validate()?;
    // Only a reference that can *disagree* with the UTC calendar is worth
    // warning about. `is_zoned()` is true for `Utc` too, which made every UTC
    // series with a monthly period warn about DST drift against the calendar it
    // is already stepping on -- a warning that cannot come true, on the most
    // common spelling there is, which teaches the reader to ignore the ones
    // that can. `Zoneless` is wall clocks held as if UTC, so it steps on its own
    // calendar as well.
    if !matches!(
        reference,
        TimeReference::FixedOffset(_) | TimeReference::Zone(_)
    ) {
        return Ok(());
    }
    let calendar_periods: [(&str, Option<Period>); 3] = match data {
        TimeSeriesData::SingleTimeSeries(single) => [
            ("resolution", Some(single.resolution)),
            ("", None),
            ("", None),
        ],
        TimeSeriesData::NonSequentialTimeSeries(_) => [("", None), ("", None), ("", None)],
        TimeSeriesData::Deterministic(d) => [
            ("resolution", Some(d.resolution)),
            ("horizon", Some(d.horizon)),
            ("interval", Some(d.interval)),
        ],
        TimeSeriesData::Probabilistic(p) => [
            ("resolution", Some(p.resolution)),
            ("horizon", Some(p.horizon)),
            ("interval", Some(p.interval)),
        ],
        TimeSeriesData::Scenarios(sc) => [
            ("resolution", Some(sc.resolution)),
            ("horizon", Some(sc.horizon)),
            ("interval", Some(sc.interval)),
        ],
    };
    for (field, period) in calendar_periods {
        if period.is_some_and(|p| p.is_irregular()) {
            tracing::warn!(
                series = data.name(),
                field,
                time_reference = %reference,
                "a calendar period on a series spelled at an offset or in a named zone \
                 steps on the UTC calendar, not the reference's: the reference is a \
                 spelling, not a grid. Expect up to a day of drift at a month boundary, \
                 and -- for a named zone -- an hour at each DST transition. For a \
                 local-clock grid, use NonSequentialTimeSeries with explicit timestamps."
            );
        }
    }
    Ok(())
}

fn validate_data(data: &TimeSeriesData) -> Result<()> {
    let invalid = TimeSeriesError::InvalidParameter;
    validate_array_geometry(data_array(data), data.time_series_type())?;
    match data {
        TimeSeriesData::SingleTimeSeries(single) => validate_single(single),
        TimeSeriesData::NonSequentialTimeSeries(non_sequential) => {
            validate_non_sequential(non_sequential)
        }
        TimeSeriesData::Deterministic(det) => {
            require_ms(det.initial_timestamp, "Deterministic")?;
            det.validate().map_err(invalid)
        }
        TimeSeriesData::Probabilistic(prob) => {
            require_ms(prob.initial_timestamp, "Probabilistic")?;
            prob.validate().map_err(invalid)
        }
        TimeSeriesData::Scenarios(scen) => {
            require_ms(scen.initial_timestamp, "Scenarios")?;
            scen.validate().map_err(invalid)
        }
    }
}

/// Check that an array's buffer and shape describe the same thing, and that
/// every per-step element dimension is wider than zero.
///
/// The buffer check is [`TypedArray::check_bytes`]: `TypedArray::new` runs it,
/// but the fields are `pub` and the type derives `Deserialize`, so a
/// hand-built or deserialized array reaches the store having met nothing. Every
/// type-level check ([`validate_single`], the forecast `validate`s,
/// [`ElementType::validate_array`]) reasons about the *shape*, and every backend
/// indexes the buffer by a stride derived from it, so a buffer that disagrees
/// with its shape was either copied as a prefix and then hashed whole — a
/// persisted row whose hash does not match its bytes — or indexed past its end.
///
/// A zero-width element dimension (`[24, 0]`) describes no bytes at all, which
/// the buffer check accepts. Nothing can be read from such a series, and a
/// `DeterministicSingleTimeSeries` derived from one indexed past its empty
/// buffer; the on-disk backend refused it with an opaque chunk-layout error and
/// the in-memory one did not refuse it at all. The time axis (`shape[0]`) may
/// still be zero: an empty series is a stored fact.
fn validate_array_geometry(array: &TypedArray, ts_type: TimeSeriesType) -> Result<()> {
    array
        .check_bytes()
        .map_err(|e| TimeSeriesError::InvalidParameter(format!("{}: {e}", ts_type.as_str())))?;
    // Only the per-step element dims: the layout axes in front (time; for a
    // forecast also horizon and count, then a percentile or scenario axis)
    // may be empty -- an empty series and a zero-window forecast are stored
    // facts. A shape too short to hold the layout, including the rank-0
    // scalar that `length()` would read as an empty series, is refused by
    // [`ElementType::validate_array`] before this runs.
    let element_dims = array.shape.get(ts_type.leading_dims()..).unwrap_or(&[]);
    if element_dims.contains(&0) {
        return Err(TimeSeriesError::InvalidParameter(format!(
            "{}: array shape {:?} has a zero-width element dimension; every dimension after \
             the {} layout axes must be at least 1",
            ts_type.as_str(),
            array.shape,
            ts_type.leading_dims()
        )));
    }
    Ok(())
}

/// The array a [`TimeSeriesData`] carries, whatever its type.
fn data_array(data: &TimeSeriesData) -> &TypedArray {
    match data {
        TimeSeriesData::SingleTimeSeries(single) => &single.data,
        TimeSeriesData::NonSequentialTimeSeries(non_sequential) => &non_sequential.data,
        TimeSeriesData::Deterministic(det) => &det.data,
        TimeSeriesData::Probabilistic(prob) => &prob.data,
        TimeSeriesData::Scenarios(scen) => &scen.data,
    }
}

/// [`crate::timestamps::require_millisecond_precision`] as a
/// [`TimeSeriesError`], labeled with the type whose `initial_timestamp` it is.
fn require_ms(t: chrono::DateTime<chrono::Utc>, label: &str) -> Result<()> {
    crate::timestamps::require_millisecond_precision(t, || format!("{label} initial_timestamp"))
        .map_err(TimeSeriesError::InvalidParameter)
}

/// Check that a `SingleTimeSeries` describes its own array.
///
/// `length` is a public field that `SingleTimeSeries::new` derives from the
/// array, so the two agree at construction — but nothing keeps them agreeing
/// afterwards. Replacing `data` (or deserializing a hand-written payload, which
/// is a supported round trip) leaves a `length` that describes an array the
/// series no longer holds, and the catalog row is built from that field. Without
/// this check the store persists a row that misdescribes its own bytes: reads
/// return a series whose `length` and `data.length()` disagree, the mismatch
/// survives `flush`/`persist_to`/`compact`, and every consumer that trusts the
/// catalog — `check_static_consistency`, `transform_single_time_series`,
/// `build_static_reader` — works off the wrong grid.
///
/// [`validate_non_sequential`] enforces the equivalent rule.
fn validate_single(series: &SingleTimeSeries) -> Result<()> {
    require_ms(series.initial_timestamp, "SingleTimeSeries")?;
    if series.length != series.data.length() {
        return Err(TimeSeriesError::InvalidParameter(format!(
            "SingleTimeSeries declares length {} but its array holds {} time steps",
            series.length,
            series.data.length()
        )));
    }
    // The resolution has to be a period the store can actually represent, which
    // `Period::is_positive` defines as a whole number of milliseconds greater
    // than zero. `SingleTimeSeries::new` is infallible, so until this check
    // every forecast constructor rejected such a period while the static path
    // waved it through, and the result was a series nothing could read back:
    //
    //   * a negative resolution built a `StaticReader` whose timeline ran
    //     *backwards*, and whose every `index_at` then failed on its own
    //     timestamps;
    //   * zero repeated one instant `length` times;
    //   * a sub-millisecond resolution encoded as `PT0S` and read back as zero,
    //     so a sliced read failed on a zero-length step.
    //
    // All three were writable and none was usable, which is the worst place to
    // draw the line. Callers wanting a finer grid should scale their unit — a
    // 500 µs series is a 500-unit series with the unit recorded in `units`.
    if !series.resolution.is_positive() {
        return Err(TimeSeriesError::InvalidParameter(format!(
            "SingleTimeSeries resolution {} is not a positive whole number of milliseconds; \
             the store cannot represent it",
            series.resolution.to_iso8601()
        )));
    }
    Ok(())
}

fn validate_non_sequential(series: &NonSequentialTimeSeries) -> Result<()> {
    if series.timestamps.len() != series.data.length() || series.length != series.data.length() {
        return Err(TimeSeriesError::InvalidParameter(
            "timestamp count, length, and data length must match".into(),
        ));
    }
    if series.timestamps.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(TimeSeriesError::InvalidParameter(
            "timestamps must be strictly increasing".into(),
        ));
    }
    // Every timestamp, not just the first: the millisecond rule is what keeps
    // this vector strictly increasing for a consumer that reads it back through
    // a millisecond boundary (the C ABI, and Julia through it). Two timestamps
    // less than a millisecond apart are distinct here and identical there.
    for (i, t) in series.timestamps.iter().enumerate() {
        crate::timestamps::require_millisecond_precision(*t, || {
            format!("NonSequentialTimeSeries timestamp {i}")
        })
        .map_err(TimeSeriesError::InvalidParameter)?;
    }
    Ok(())
}

/// Recast a [`validate_forecast_shape`] mismatch as an invalid parameter: at
/// the add boundary the disagreement is in caller-supplied input, not in data
/// the store already holds, so it should read the way [`validate_single`] and
/// [`validate_non_sequential`] already do rather than as an integrity error.
fn as_invalid_parameter(e: TimeSeriesError) -> TimeSeriesError {
    match e {
        TimeSeriesError::IntegrityError(msg) => TimeSeriesError::InvalidParameter(msg),
        other => other,
    }
}

/// Validate that `det.data`'s shape agrees with its declared `resolution` /
/// `horizon` / `count`: `[H, count, *E]` where `H = horizon / resolution`.
/// `Deterministic::new` already enforces this at construction, but every
/// field is `pub`, so a caller can still hand the store a struct whose `data`
/// was swapped out afterward without rebuilding it — the add path re-checks
/// rather than trusting the constructor was the one that built it.
fn validate_deterministic(det: &Deterministic) -> Result<()> {
    let h = compute_h(det.horizon, det.resolution).map_err(TimeSeriesError::InvalidParameter)?;
    validate_forecast_shape(&det.data, &[h, det.count], "Deterministic")
        .map_err(as_invalid_parameter)
}

/// The [`validate_deterministic`] equivalent for `Probabilistic`: shape must
/// be `[P, H, count, *E]` where `P` is the percentile count.
fn validate_probabilistic(prob: &Probabilistic) -> Result<()> {
    let h = compute_h(prob.horizon, prob.resolution).map_err(TimeSeriesError::InvalidParameter)?;
    let p = prob.percentiles.len();
    validate_forecast_shape(&prob.data, &[p, h, prob.count], "Probabilistic")
        .map_err(as_invalid_parameter)
}

/// The [`validate_deterministic`] equivalent for `Scenarios`: shape must be
/// `[scenario_count, H, count, *E]`.
fn validate_scenarios(scen: &Scenarios) -> Result<()> {
    let h = compute_h(scen.horizon, scen.resolution).map_err(TimeSeriesError::InvalidParameter)?;
    validate_forecast_shape(
        &scen.data,
        &[scen.scenario_count, h, scen.count],
        "Scenarios",
    )
    .map_err(as_invalid_parameter)
}

/// Build the metadata row for a dense forecast (`Deterministic` /
/// `Probabilistic` / `Scenarios`) added via [`Store::add_time_series_bulk`].
/// The array is stored standalone in its native shape; `percentiles` is `Some`
/// only for `Probabilistic`.
#[allow(clippy::too_many_arguments)]
fn forecast_metadata(
    item: &AddRequest,
    time_series_type: TimeSeriesType,
    name: &str,
    initial_timestamp: chrono::DateTime<chrono::Utc>,
    resolution: Period,
    horizon: Period,
    interval: Period,
    count: usize,
    data: &TypedArray,
    element_type: ElementType,
    percentiles: Option<Vec<f64>>,
) -> TimeSeriesMetadata {
    TimeSeriesMetadata {
        owner_id: item.owner_id,
        owner_type: item.owner_type.clone(),
        owner_category: item.owner_category,
        time_series_type,
        name: name.to_owned(),
        data_hash: array_hash(data),
        initial_timestamp: Some(initial_timestamp),
        resolution: Some(resolution),
        length: Some(data.length()),
        horizon: Some(horizon),
        interval: Some(interval),
        count: Some(count),
        timestamps: None,
        features: item.features.clone(),
        units: item.data.units().map(str::to_owned),
        quantity_kind: item.data.quantity_kind().map(str::to_owned),
        unit_system: item.data.unit_system(),
        time_reference: item.data.time_reference().cloned(),
        component_field: item.data.component_field().map(str::to_owned),
        percentiles,
        element_type,
        element_shape: data.element_shape().to_vec(),
        application_data: item.data.application_data().map(str::to_owned),
        id: None,
    }
}

/// The element type a request writes — the one the series carries, which a
/// constructor resolved to plain scalars of the array's dtype unless the caller
/// declared otherwise.
///
/// Always validated against the array, so the store never persists a row that
/// misdescribes its own bytes. That also catches a series whose `data` was
/// replaced after construction without updating its element type: the check
/// reports the disagreement rather than silently re-deriving one.
fn resolve_element_type(item: &AddRequest) -> Result<ElementType> {
    let declared = item.data.element_type();
    declared.validate_array(
        request_array(item),
        item.data.time_series_type().leading_dims(),
    )?;
    Ok(declared)
}

/// Open the array backend for an existing store file. The `storage_backend`
/// root attribute identifies files written by [`Hdf5Backend`]; files without it
/// (including stores written by the removed netcdf backend) are rejected with
/// an actionable error instead of being misread.
///
/// Reachability is checked first, and separately, so a typo'd path or an
/// unreadable directory is reported as the missing file it is rather than as a
/// store of the wrong kind. The `io::Error` kind is carried through, so a
/// missing path and a permission-denied one stay distinguishable.
///
/// Past that, the sniff is three-way rather than a boolean, because "this is
/// not one of our files" and "this file would not open" call for opposite
/// advice. Only the first can be a netcdf-era store — netcdf4 is HDF5
/// underneath, so such a file *opens* and merely lacks our attribute. A file
/// that will not open at all is something else entirely, and the most common
/// something else is a store another process is holding: HDF5 takes an
/// exclusive lock and does not set `O_CLOEXEC`, so even an unrelated forked
/// child can keep one alive. Telling that user to re-create the store is advice
/// to destroy a healthy artifact, so libhdf5's own complaint is passed through
/// instead of being overwritten by a guess.
fn open_backend(path: &Path, read_only: bool) -> Result<Box<dyn StorageBackend>> {
    use crate::storage::hdf5::BackendSniff;

    if let Err(e) = std::fs::metadata(path) {
        return Err(TimeSeriesError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot open store {}: {e}", path.display()),
        )));
    }
    match crate::storage::hdf5::sniff_hdf5_backend_file(path) {
        BackendSniff::Ours => {}
        BackendSniff::NotOurs => {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "{} is not an infrastore hdf5 store (stores written by the removed \
                 netcdf backend are no longer supported; re-create the store to \
                 migrate)",
                path.display()
            )));
        }
        BackendSniff::Unopenable(why) => {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "{} could not be opened as an HDF5 file, so whether it is an infrastore \
                 store is unknown. If another process has it open for writing, close that \
                 one first — HDF5 takes an exclusive lock on a store it is writing. \
                 libhdf5 reported: {why}",
                path.display()
            )));
        }
    }
    Ok(Box::new(Hdf5Backend::open(path, read_only)?))
}

/// The SQLite catalog path paired with an HDF5 data path: `<path>.sqlite`.
///
/// Public because the two files are one logical artifact that must be moved,
/// copied, and deleted together, so a tool that reports or manipulates store
/// paths needs the same derivation the store itself uses rather than its own
/// copy of the rule.
pub fn catalog_sqlite_path(data_path: &Path) -> PathBuf {
    let mut p = data_path.to_path_buf();
    let new_name = match p.file_name().and_then(|n| n.to_str()) {
        Some(name) => format!("{name}.sqlite"),
        None => "metadata.sqlite".to_string(),
    };
    p.set_file_name(new_name);
    p
}

/// Sibling temp path a `persist_to` half is staged at. A sibling, not a temp
/// directory, so the rename that follows stays within one filesystem and is
/// therefore atomic.
///
/// `tag` makes the name unique per staging — see [`temp_tag`]. Both halves of
/// one save share a tag, so an interrupted save's leftovers are identifiable as
/// a pair.
fn persist_temp_path(target: &Path, tag: &str) -> PathBuf {
    let mut p = target.to_path_buf();
    let new_name = match p.file_name().and_then(|n| n.to_str()) {
        Some(name) => format!("{name}.persist-{tag}"),
        None => format!("store.persist-{tag}"),
    };
    p.set_file_name(new_name);
    p
}

/// A short, effectively-unique tag for a staged temporary file's name.
///
/// Minted the same way as a generation stamp, because the property needed is
/// the same: two stagings — in one process or in two — must not choose the same
/// path.
///
/// The obvious deterministic name (`<target>.persist`) is a corruption vector
/// rather than a convenience. Nothing holds a lock on a `persist_to`
/// *destination*, so two processes saving to one path would each clear the
/// other's in-flight temp while `stamp_generation` and the rename that follows
/// still resolve that name — publishing a partially written file as a finished
/// save, with stamps that can still agree. `compact` is nominally protected by
/// the HDF5 lock on the store it rewrites, but that lock is best-effort and
/// silently absent on the network filesystems this runs on, so it stages the
/// same way.
///
/// The cost is that a crashed staging is not swept: leftovers accumulate as
/// `<target>.persist-<tag>` / `<store>.h5.repack-<tag>` siblings. They cannot
/// be swept safely, because a temp belonging to a live concurrent save is
/// indistinguishable from an abandoned one. Callers may delete them once no
/// save is in flight.
fn temp_tag() -> String {
    let mut tag = mint_generation();
    tag.truncate(16);
    tag
}

/// Placeholder for a missing generation stamp in a
/// [`TimeSeriesError::MismatchedArtifact`] message. Not a value ever written to
/// an artifact — a stamped half always carries a hex stamp — so it cannot be
/// confused with one.
const UNSTAMPED: &str = "none";

/// Refuse to create a store where either half of an artifact already lives.
///
/// Creating truncates the HDF5 file (`H5F_ACC_TRUNC`) but *opens* the catalog
/// beside it, so a create over an existing artifact leaves an empty array file
/// paired with the old catalog's rows — and stamps both halves with one fresh
/// generation, so the pair agrees and reopens cleanly, reporting every series
/// still present while every array is a dangling reference. Nothing short of
/// [`Store::verify_integrity`] notices. It takes no crash to reach: a build
/// script re-run against a path that already holds a save is enough.
///
/// Checking both halves matters. The catalog alone is enough to poison a fresh
/// HDF5 file, and an HDF5 file alone (a `CatalogMode::InMemory` scratch store
/// abandoned before its first save) is enough to poison a fresh catalog.
fn reject_existing_artifact(path: &Path) -> Result<()> {
    let sqlite = catalog_sqlite_path(path);
    let existing = if path.exists() {
        Some(path.to_path_buf())
    } else if sqlite.exists() {
        Some(sqlite)
    } else {
        None
    };
    match existing {
        Some(p) => Err(TimeSeriesError::StoreExists {
            path: p.display().to_string(),
        }),
        None => Ok(()),
    }
}

/// SQLite's `-wal` / `-shm` sidecar beside a database file. The suffix is
/// appended to the whole filename, extension included, so `set_extension` is the
/// wrong tool here.
fn sqlite_sidecar(sqlite: &Path, suffix: &str) -> PathBuf {
    let mut name = sqlite.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Whether two paths name the same file. Falls back to comparing the paths as
/// written when either cannot be canonicalized — a destination that does not
/// exist yet is the common case, and it is by definition not the source.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// fsync a just-written file, so the rename that follows cannot publish a name
/// pointing at data the kernel has not committed.
///
/// The handle is opened for writing because Windows' `FlushFileBuffers` requires
/// write access and fails a read-only handle with `ERROR_ACCESS_DENIED`; on Unix
/// either would do. Both callers pass a staged temporary they just wrote, so
/// nothing else holds the file.
fn sync_file(path: &Path) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .sync_all()?;
    Ok(())
}

/// fsync the directory holding `path`, so the renames themselves survive a
/// crash and not just the file contents they publish.
#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Windows exposes no directory handle to fsync; NTFS orders the metadata
/// operations itself, so there is nothing to force here.
#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> Result<()> {
    Ok(())
}

/// Where [`Store::compact`] builds the rewritten HDF5 file before swapping it
/// over the original: a sibling of `data_path`, so the two are on one
/// filesystem and the swap is a plain atomic rename. `tag` makes the name unique
/// per compaction — see [`temp_tag`], including why a leftover from an
/// interrupted compaction is now left in place rather than swept by the next one.
fn repack_temp_path(data_path: &Path, tag: &str) -> PathBuf {
    let mut p = data_path.to_path_buf();
    let new_name = match p.file_name().and_then(|n| n.to_str()) {
        Some(name) => format!("{name}.repack-{tag}"),
        None => format!("store.h5.repack-{tag}"),
    };
    p.set_file_name(new_name);
    p
}

// ---------------------------------------------------------------------------
// Forecast read-path helpers
// ---------------------------------------------------------------------------

/// Slice a contiguous range `[w0, w1)` along `axis` of a row-major array.
///
/// This is a strided gather: axis `a` is not necessarily the leading axis, so
/// the bytes for each "outer" block are not contiguous in the source buffer.
///
/// - `outer = product(shape[0..axis])` — number of outer blocks.
/// - `inner_bytes = product(shape[axis+1..]) * dtype.size()` — bytes per
///   element in the axis-stride.
/// - For each outer block `o`, the source bytes for windows `[w0, w1)` live at
///   `o * axis_len * inner_bytes + w0 * inner_bytes .. + w1 * inner_bytes`.
///
/// The returned array has the same dtype and all the same shape dims except
/// `shape[axis]` which becomes `w1 - w0`.
pub(crate) fn slice_count_axis(arr: &TypedArray, axis: usize, w0: usize, w1: usize) -> TypedArray {
    assert!(
        axis < arr.shape.len(),
        "axis {axis} out of bounds for shape {:?}",
        arr.shape
    );
    assert!(w0 <= w1, "w0 ({w0}) must be <= w1 ({w1})");
    let axis_len = arr.shape[axis];
    assert!(w1 <= axis_len, "w1 ({w1}) > axis_len ({axis_len})");

    let outer: usize = arr.shape[..axis].iter().product();
    let inner_bytes: usize = arr.shape[axis + 1..].iter().product::<usize>() * arr.dtype.size();
    let window_bytes = (w1 - w0) * inner_bytes;

    let mut out_bytes = Vec::with_capacity(outer * window_bytes);
    for o in 0..outer {
        let block_start = o * axis_len * inner_bytes;
        let src_start = block_start + w0 * inner_bytes;
        let src_end = block_start + w1 * inner_bytes;
        out_bytes.extend_from_slice(&arr.bytes[src_start..src_end]);
    }

    let mut new_shape = arr.shape.clone();
    new_shape[axis] = w1 - w0;

    TypedArray {
        dtype: arr.dtype,
        shape: new_shape,
        bytes: out_bytes,
    }
}

/// Resolve the window range `[w0, w1)` from an optional `time_range`.
///
/// Implements the IS.jl rule: `start_time` must be the first timestamp of a
/// window (`initial_timestamp + k·interval`), `end` is exclusive. Returns
/// `(w0, w1, first_window_initial_timestamp)`.
///
/// On success, `w0 <= w1 <= count`. A `start` aligned to the grid but at or
/// beyond `count` (i.e. past the last stored window) is rejected with
/// [`TimeSeriesError::InvalidParameter`] rather than returning an empty
/// selection. A zero-width range (`end == start`) over an in-range `start`
/// legitimately selects nothing and returns `(0, 0, start)`.
fn resolve_windows(
    initial: chrono::DateTime<chrono::Utc>,
    _resolution: Period,
    _horizon: Period,
    interval: Period,
    count: usize,
    time_range: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
) -> Result<(usize, usize, chrono::DateTime<chrono::Utc>)> {
    let window_start = |k: usize| -> Result<chrono::DateTime<chrono::Utc>> {
        interval
            .add_to(initial, k as i64)
            .ok_or_else(|| TimeSeriesError::IntegrityError("window timestamp overflow".into()))
    };
    match time_range {
        None => Ok((0, count, initial)),
        Some((start, end)) => {
            if end < start {
                return Err(TimeSeriesError::InvalidParameter("end < start".into()));
            }
            if !interval.is_positive() {
                // A single-window forecast may carry a zero interval (there is
                // no second window to step to); its only window starts at
                // `initial`, which is also the only valid `start`.
                if count == 1 && interval.is_zero() {
                    if start != initial {
                        return Err(TimeSeriesError::InvalidParameter(
                            "forecast start_time must align to a window boundary \
                             (initial_timestamp + k·interval)"
                                .into(),
                        ));
                    }
                    return if initial < end {
                        Ok((0, 1, initial))
                    } else {
                        Ok((0, 0, initial))
                    };
                }
                return Err(TimeSeriesError::InvalidParameter(
                    "forecast interval must be positive".into(),
                ));
            }
            // `start` must be a window boundary: `initial + k·interval`
            // (calendar-aware for monthly intervals).
            let start_k = interval.steps_between(initial, start).map_err(|_| {
                TimeSeriesError::InvalidParameter(
                    "forecast start_time must align to a window boundary \
                     (initial_timestamp + k·interval)"
                        .into(),
                )
            })?;

            // A start aligned to the grid but at or beyond the window count
            // refers to windows that do not exist; reject it rather than
            // silently returning an empty selection.
            if start_k >= count {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "forecast start_time is past the last window (resolves to window \
                     index {start_k}, but only {count} window(s) are stored)"
                )));
            }

            // The selection — the k in [0, count) whose window start lies in
            // [start, end) — is contiguous, because window starts increase
            // monotonically. `start` is on the grid, so `start_k` is already the
            // range's first index; all that is left is its end, the first window
            // at or past `end`. Binary search rather than a walk: `count` can be
            // a year of windows, and each step here is calendar arithmetic.
            let mut lo = start_k;
            let mut hi = count;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if window_start(mid)? < end {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            let w1 = lo;

            // Empty selection: report the initial timestamp at the requested start.
            if w1 <= start_k {
                return Ok((0, 0, window_start(start_k)?));
            }

            Ok((start_k, w1, window_start(start_k)?))
        }
    }
}

/// The exact-identity `MetadataFilter` for one `KeyIdentity` — an existence
/// probe that pins the whole feature set by hash rather than matching a subset.
/// Internal: an identity is how the store files a row, not how a caller
/// addresses one.
fn identity_filter(key: &KeyIdentity) -> MetadataFilter {
    MetadataFilter {
        owner_id: Some(key.owner_id),
        owner_category: Some(key.owner_category),
        time_series_type: Some(TypeMatch::Exact(key.time_series_type)),
        name: Some(key.name.clone()),
        resolution: key.resolution,
        interval: key.interval,
        features: None,
        features_hash: Some(crate::hash::features_hash(&key.features)),
        owner_type: None,
        name_glob: None,
        component_field: None,
        zoneless: None,
        ids: None,
    }
}

// --- Metadata field accessors that return IntegrityError on None ---

fn required_initial(
    meta: &crate::types::metadata::TimeSeriesMetadata,
    label: &str,
) -> Result<chrono::DateTime<chrono::Utc>> {
    meta.initial_timestamp.ok_or_else(|| {
        TimeSeriesError::IntegrityError(format!("{label} missing initial_timestamp"))
    })
}

fn required_resolution(
    meta: &crate::types::metadata::TimeSeriesMetadata,
    label: &str,
) -> Result<Period> {
    meta.resolution
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("{label} missing resolution")))
}

fn required_horizon(
    meta: &crate::types::metadata::TimeSeriesMetadata,
    label: &str,
) -> Result<Period> {
    meta.horizon
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("{label} missing horizon")))
}

fn required_interval(
    meta: &crate::types::metadata::TimeSeriesMetadata,
    label: &str,
) -> Result<Period> {
    meta.interval
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("{label} missing interval")))
}

fn required_count(meta: &crate::types::metadata::TimeSeriesMetadata, label: &str) -> Result<usize> {
    meta.count
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("{label} missing count")))
}

fn required_length(
    meta: &crate::types::metadata::TimeSeriesMetadata,
    label: &str,
) -> Result<usize> {
    meta.length
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("{label} missing length")))
}

/// Validate that the leading shape dims of `arr` match `expected_prefix`,
/// returning an `IntegrityError` if not. Trailing element dims are allowed.
fn validate_forecast_shape(arr: &TypedArray, expected_prefix: &[usize], label: &str) -> Result<()> {
    if arr.shape.len() < expected_prefix.len() {
        return Err(TimeSeriesError::IntegrityError(format!(
            "{label}: stored shape {:?} has fewer dims than expected prefix {expected_prefix:?}",
            arr.shape
        )));
    }
    for (i, (&got, &exp)) in arr.shape.iter().zip(expected_prefix.iter()).enumerate() {
        if got != exp {
            return Err(TimeSeriesError::IntegrityError(format!(
                "{label}: stored shape {:?} mismatch at dim {i}: expected {exp}, got {got}",
                arr.shape
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod slice_count_axis_tests {
    use super::*;

    fn f64_arr(shape: Vec<usize>, vals: &[f64]) -> TypedArray {
        TypedArray::from_f64(shape, vals)
    }

    #[test]
    fn axis0() {
        // Shape [4]: axis 0 = leading axis, equivalent to leading-axis slicing.
        // vals = [10, 20, 30, 40] (f64).
        let arr = f64_arr(vec![4], &[10.0, 20.0, 30.0, 40.0]);
        let sliced = slice_count_axis(&arr, 0, 1, 3);
        assert_eq!(sliced.shape, vec![2]);
        assert_eq!(sliced.to_f64_vec().unwrap(), vec![20.0, 30.0]);
    }

    #[test]
    fn axis1_of_3d() {
        // Simulate Deterministic shape [H=2, C=4, E=1]: [2, 4, 1].
        // vals[s][w][e] = s*100 + w*10 + e
        let vals: Vec<f64> = (0..2_usize)
            .flat_map(|s| {
                (0..4_usize)
                    .flat_map(move |w| (0..1_usize).map(move |e| (s * 100 + w * 10 + e) as f64))
            })
            .collect();
        let arr = f64_arr(vec![2, 4, 1], &vals);

        // Select windows w=1..3 along axis 1.
        let sliced = slice_count_axis(&arr, 1, 1, 3);
        assert_eq!(sliced.shape, vec![2, 2, 1]);

        // Expected: s=0, w=1: [10.0], s=0, w=2: [20.0], s=1, w=1: [110.0], s=1, w=2: [120.0]
        let expected = vec![10.0, 20.0, 110.0, 120.0];
        assert_eq!(sliced.to_f64_vec().unwrap(), expected);
    }

    #[test]
    fn axis2_of_4d() {
        // Simulate Probabilistic/Scenarios shape [P=2, H=2, C=3]: [2, 2, 3].
        // vals[p][s][w] = p*1000 + s*100 + w*10
        let vals: Vec<f64> = (0..2_usize)
            .flat_map(|p| {
                (0..2_usize).flat_map(move |s| {
                    (0..3_usize).map(move |w| (p * 1000 + s * 100 + w * 10) as f64)
                })
            })
            .collect();
        let arr = f64_arr(vec![2, 2, 3], &vals);

        // Select windows w=0..2 (first two) along axis 2.
        let sliced = slice_count_axis(&arr, 2, 0, 2);
        assert_eq!(sliced.shape, vec![2, 2, 2]);

        // p=0, s=0: [0, 10]; p=0, s=1: [100, 110]; p=1, s=0: [1000, 1010]; p=1, s=1: [1100, 1110]
        let expected = vec![0.0, 10.0, 100.0, 110.0, 1000.0, 1010.0, 1100.0, 1110.0];
        assert_eq!(sliced.to_f64_vec().unwrap(), expected);
    }

    #[test]
    fn full_range_is_identity() {
        // Slicing the full range should return identical bytes.
        let vals: Vec<f64> = (0..12).map(|i| i as f64).collect();
        let arr = f64_arr(vec![3, 4], &vals);
        let sliced = slice_count_axis(&arr, 1, 0, 4);
        assert_eq!(sliced.shape, arr.shape);
        assert_eq!(sliced.bytes, arr.bytes);
    }

    #[test]
    fn empty_range() {
        let arr = f64_arr(vec![2, 4], &[0.0; 8]);
        let sliced = slice_count_axis(&arr, 1, 2, 2);
        assert_eq!(sliced.shape, vec![2, 0]);
        assert!(sliced.bytes.is_empty());
    }
}

#[cfg(test)]
mod resolve_windows_tests {
    use super::*;
    use chrono::{DateTime, Duration, TimeZone, Utc};

    // A forecast grid of 4 windows spaced 12h apart at hours 0, 12, 24, 36.
    fn t(h: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap() + Duration::hours(h)
    }

    fn rw(start_h: i64, end_h: i64) -> Result<(usize, usize, DateTime<Utc>)> {
        let interval = Period::Fixed(Duration::hours(12));
        let res = Period::Fixed(Duration::hours(1));
        let horizon = Period::Fixed(Duration::hours(6));
        resolve_windows(
            t(0),
            res,
            horizon,
            interval,
            4,
            Some((t(start_h), t(end_h))),
        )
    }

    #[test]
    fn full_and_partial_selection() {
        let interval = Period::Fixed(Duration::hours(12));
        let res = Period::Fixed(Duration::hours(1));
        let horizon = Period::Fixed(Duration::hours(6));
        // `None` selects every window.
        assert_eq!(
            resolve_windows(t(0), res, horizon, interval, 4, None).unwrap(),
            (0, 4, t(0))
        );
        // Middle range [12h, 36h) -> windows 1 and 2.
        assert_eq!(rw(12, 36).unwrap(), (1, 3, t(12)));
        // The exact last window (index 3) is in range.
        assert_eq!(rw(36, 48).unwrap(), (3, 4, t(36)));
    }

    #[test]
    fn zero_width_in_range_is_empty() {
        // An in-range start with `end == start` legitimately selects nothing.
        assert_eq!(rw(12, 12).unwrap(), (0, 0, t(12)));
    }

    #[test]
    fn start_past_last_window_errors() {
        // Hour 48 resolves to window index 4, but only 4 windows (0..=3) exist.
        assert!(matches!(
            rw(48, 60),
            Err(TimeSeriesError::InvalidParameter(_))
        ));
    }

    /// The pre-optimization walk over every window: the binary search has to
    /// agree with it everywhere, since the range it picks is what a caller's
    /// `time_range` read returns.
    fn linear_scan(
        window_start: impl Fn(usize) -> DateTime<Utc>,
        count: usize,
        range: (DateTime<Utc>, DateTime<Utc>),
    ) -> (usize, usize) {
        let (start, end) = range;
        let mut w0 = count;
        let mut w1 = 0usize;
        for k in 0..count {
            let ws = window_start(k);
            if ws >= start && ws < end {
                if w0 == count {
                    w0 = k;
                }
                w1 = k + 1;
            }
        }
        if w0 == count { (0, 0) } else { (w0, w1) }
    }

    #[test]
    fn fixed_interval_search_matches_the_linear_scan() {
        let res = Period::Fixed(Duration::hours(1));
        let horizon = Period::Fixed(Duration::hours(6));
        let interval = Period::Fixed(Duration::hours(12));
        let count = 7usize;
        let at = |k: usize| t(k as i64 * 12);

        for start_k in 0..count {
            let start = at(start_k);
            // Every end from the start itself to well past the last window,
            // hourly — so both on-boundary and interior ends are covered.
            for end_h in 0..=96 {
                let end = start + Duration::hours(end_h);
                let range = Some((start, end));
                let (w0, w1, first) =
                    resolve_windows(t(0), res, horizon, interval, count, range).unwrap();
                assert_eq!(
                    (w0, w1),
                    linear_scan(at, count, (start, end)),
                    "start={start_k} end=+{end_h}h"
                );
                // Aligned start, so the reported first timestamp is the start
                // itself whether or not the selection came back empty.
                assert_eq!(first, start, "start={start_k} end=+{end_h}h");
            }
        }
    }

    #[test]
    fn monthly_interval_search_matches_the_linear_scan() {
        // Calendar months are the case plain arithmetic cannot shortcut.
        let initial = Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap();
        let res = Period::Fixed(Duration::hours(1));
        let horizon = Period::Fixed(Duration::days(1));
        let interval = Period::Months(1);
        let count = 12usize;
        let at = |k: usize| interval.add_to(initial, k as i64).unwrap();

        for start_k in 0..count {
            let start = at(start_k);
            for end_k in start_k..=count + 1 {
                for offset in [Duration::zero(), Duration::days(1), Duration::hours(-1)] {
                    let end = at(end_k) + offset;
                    if end < start {
                        continue;
                    }
                    let range = Some((start, end));
                    let (w0, w1, first) =
                        resolve_windows(initial, res, horizon, interval, count, range).unwrap();
                    assert_eq!(
                        (w0, w1),
                        linear_scan(at, count, (start, end)),
                        "start={start_k} end={end_k}{offset:?}"
                    );
                    assert_eq!(first, start, "start={start_k} end={end_k}{offset:?}");
                }
            }
        }
    }

    #[test]
    fn misaligned_or_reversed_errors() {
        // 1h off the 12h window grid.
        assert!(matches!(
            rw(1, 24),
            Err(TimeSeriesError::InvalidParameter(_))
        ));
        // end < start.
        assert!(matches!(
            rw(24, 12),
            Err(TimeSeriesError::InvalidParameter(_))
        ));
    }
}

#[cfg(test)]
mod pending_format_upgrade_tests {
    //! Closing the loop the `InMemory` open deliberately leaves open.
    //!
    //! A writable open at an upgradable stamp owes a re-stamp.
    //! `open_with_catalog` discharges it immediately for `CatalogMode::Attached`
    //! — but not for `InMemory`, where the migration happened to a *copy* of the
    //! catalog and the file on disk is still stale until it is persisted.
    //! Something has to discharge it once that catalog does land, or the array
    //! file keeps its old stamp for the life of the store and eventually falls
    //! off the bottom of the upgrade window.
    //!
    //! None of this is reachable through the shipped constants, which is why
    //! every test here scopes a wider window.

    use super::*;
    use crate::version::test_bounds;

    const OLD: &str = "1.3.0";
    const MIN: &str = "1.2.0";
    const CUR: &str = "1.5.0";

    fn stamp_of(path: &Path) -> String {
        let file = hdf5_metno::File::open(path).expect("open h5");
        file.attr("data_format_version")
            .and_then(|a| a.read_scalar::<hdf5_metno::types::VarLenUnicode>())
            .map(|v| v.to_string())
            .expect("stamp")
    }

    /// A store whose array file is stamped `OLD` and whose catalog is at the
    /// current revision, built under the real constants and then backdated.
    fn backdated_store(dir: &Path) -> PathBuf {
        let path = dir.join("s.h5");
        {
            let mut store = Store::create(Some(&path), false).expect("create");
            store.flush().expect("flush");
        }
        let file = hdf5_metno::File::open_rw(&path).expect("open rw");
        let attr = file.attr("data_format_version").expect("attr");
        attr.write_scalar(&OLD.parse::<hdf5_metno::types::VarLenUnicode>().unwrap())
            .expect("backdate");
        drop(file);
        path
    }

    /// `persist_catalog` is the moment an `InMemory` store's migrated catalog
    /// becomes the one on disk, so it is the moment the deferred re-stamp is
    /// finally owed to nobody but this call.
    #[test]
    fn persist_catalog_discharges_the_deferred_restamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = backdated_store(dir.path());
        assert_eq!(stamp_of(&path), OLD);

        test_bounds::with(MIN, CUR, || {
            let mut store =
                Store::open_with_catalog(&path, false, CatalogMode::InMemory).expect("open");
            // Deliberately still owed: the catalog on disk has not moved yet.
            assert!(store.backend.pending_format_upgrade());
            assert_eq!(stamp_of(&path), OLD, "the open must not re-stamp");

            store.persist_catalog().expect("persist catalog");
            assert!(!store.backend.pending_format_upgrade());
        });
        assert_eq!(
            stamp_of(&path),
            CUR,
            "the catalog landed, so the stamp moves"
        );
    }

    /// `persist_to`'s copy branch clones the array file byte for byte, stamp
    /// included, while writing a *migrated* catalog beside it. Without the
    /// stamping step the destination is a fresh artifact born already stale,
    /// and nothing there would ever fix it.
    #[test]
    fn persist_to_stamps_the_copy_it_publishes() {
        let dir = tempfile::tempdir().unwrap();
        let path = backdated_store(dir.path());
        let dest = dir.path().join("dest.h5");

        test_bounds::with(MIN, CUR, || {
            let mut store =
                Store::open_with_catalog(&path, false, CatalogMode::InMemory).expect("open");
            assert!(store.backend.pending_format_upgrade());
            store.persist_to(&dest).expect("persist to");
        });

        assert_eq!(
            stamp_of(&dest),
            CUR,
            "the published copy carries the new stamp"
        );
        // The source is untouched: its own catalog on disk never migrated, so
        // it still legitimately owes the re-stamp.
        assert_eq!(stamp_of(&path), OLD);
    }

    /// The `Attached` path already discharges at open, so persisting must not
    /// depend on it and must stay a no-op here.
    #[test]
    fn an_attached_open_has_nothing_left_to_discharge() {
        let dir = tempfile::tempdir().unwrap();
        let path = backdated_store(dir.path());

        test_bounds::with(MIN, CUR, || {
            let mut store =
                Store::open_with_catalog(&path, false, CatalogMode::Attached).expect("open");
            assert!(!store.backend.pending_format_upgrade());
            store.persist_catalog().expect("persist catalog");
        });
        assert_eq!(stamp_of(&path), CUR);
    }
}
