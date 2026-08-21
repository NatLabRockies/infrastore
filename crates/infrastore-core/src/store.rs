//! High-level `Store` composing the storage backend and metadata store.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Result, TimeSeriesError};
use crate::hash::array_hash;
use crate::metadata::{
    AssociationIdentity, MetadataFilter, MetadataStore, ParentChildAssociation, ParentChildFilter,
    SeriesFamily, SharedSetCache, SupplementalAttributeAssociation, SupplementalAttributeFilter,
    SupplementalAttributeSummaryRow, TypeMatch, references_to_in_tx, typed_references_to_in_tx,
};
use crate::reader::{ForecastReader, StaticReader};
use crate::storage::{
    ArrayLayout, ArrayLocation, CompactionReport, Compression, Hdf5Backend, IntegrityReport,
    MemoryBackend, PackGroup, StorageBackend,
};
use crate::types::array::{Dtype, TypedArray};
use crate::types::element_type::ElementType;
use crate::types::key::{
    ForecastTimeSeriesKey, KeyIdentity, NonSequentialTimeSeriesKey, SingleTimeSeriesKey,
    TimeSeriesKey,
};
use crate::types::metadata::{Features, OwnerCategory, TimeSeriesMetadata, validate_features};
use crate::types::period::Period;
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
    pub resolution: Option<Period>,
    pub interval: Option<Period>,
    pub features: Option<Features>,
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
    pub fn resolution(mut self, r: impl Into<Period>) -> Self {
        self.resolution = Some(r.into());
        self
    }
    pub fn interval(mut self, i: impl Into<Period>) -> Self {
        self.interval = Some(i.into());
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
            resolution: value.resolution,
            interval: value.interval,
            features: value.features,
            features_hash: None,
        }
    }
}

/// Single item in a bulk add.
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
    /// True when the requested `interval` equalled the horizon and that horizon
    /// spanned the whole series, so the interval was normalized to zero.
    pub interval_normalized: bool,
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
    /// `staged_hashes.len()` as each nesting level was opened, so a rollback can
    /// tell which writes belong to the level it is unwinding. Without it an
    /// inner rollback unwound the catalog but left its arrays in the file: the
    /// outer commit only consults `pending_free`, so the bytes stayed with no
    /// row referencing them — invisible to `verify_integrity`, which walks only
    /// catalog-referenced arrays, and reclaimable only by `compact`.
    marks: Vec<usize>,
    /// Arrays that a removal inside this transaction left unreferenced. The free
    /// is deferred to the outermost commit: while the transaction is open the
    /// bytes must survive, because a rollback restores the catalog rows that
    /// point at them.
    pending_free: HashSet<[u8; 32]>,
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
            });
        }
        let file_path = path.ok_or_else(|| {
            TimeSeriesError::InvalidParameter("path is required when in_memory=false".into())
        })?;
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
        let sqlite = catalog_sqlite_path(path);
        // Sidecars before the database they belong to: a `-wal` outliving its
        // database is the one ordering SQLite would try to recover from.
        remove_if_exists(&sqlite_sidecar(&sqlite, "-wal"))?;
        remove_if_exists(&sqlite_sidecar(&sqlite, "-shm"))?;
        remove_if_exists(&sqlite)?;
        remove_if_exists(path)?;
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
                // so this still never writes to it.
                MetadataStore::open_path(&src_sqlite, true)?.backup_to(&dest_sqlite)?;
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
        let sqlite_path = catalog_sqlite_path(path);
        // The HDF5 half opens FIRST, and the order is load-bearing: `open_backend`
        // is where `data_format_version` is checked, and opening the catalog
        // writable runs `schema::DDL`, which can only be applied to a catalog of
        // the current format. The DDL is idempotent but not version-agnostic —
        // `idx_component_field` names a column added in 0.16.0, so applying it to
        // an older catalog fails with a raw `no such column`, pre-empting the
        // `IncompatibleFormat` the version stamp exists to produce. Checking the
        // version before touching the catalog keeps that error the one a caller
        // sees, and as a bonus stops a bad path from leaving a freshly created
        // empty `.sqlite` behind.
        //
        // A read-only store opens both halves read-only: the HDF5 side needs
        // no write permission (works on read-only media, shared HDF5 lock) and
        // its write paths error with `ReadOnlyStore` as a backstop behind the
        // `Store::add_*` / `remove_*` guards.
        let backend = open_backend(path, read_only)?;
        let metadata = match catalog {
            CatalogMode::Attached => MetadataStore::open_path(&sqlite_path, read_only)?,
            CatalogMode::InMemory => MetadataStore::open_path_into_memory(&sqlite_path, read_only)?,
        };
        // Stamps that disagree mean these files came from different saves — most
        // likely a `persist_to` interrupted between its two renames. Comparing
        // the `Option`s directly makes a lone stamp a mismatch too, which is the
        // point: every path that writes a stamp writes both halves together
        // (`Store::create`, `persist_to`, and `compact`, which carries the
        // existing one across), so exactly one stamped half is a half swapped
        // out on its own. Only *both* unstamped is legitimate — an artifact that
        // predates stamping — and that compares equal.
        let (h5, sqlite) = (backend.generation(), metadata.generation()?);
        if h5 != sqlite {
            return Err(TimeSeriesError::MismatchedArtifact {
                h5: h5.unwrap_or_else(|| UNSTAMPED.into()),
                sqlite: sqlite.unwrap_or_else(|| UNSTAMPED.into()),
            });
        }
        Ok(Self {
            backend,
            metadata,
            read_only,
            file_path: Some(path.to_path_buf()),
            catalog,
            txn: None,
        })
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Where this store's catalog lives. See [`CatalogMode`].
    pub fn catalog_mode(&self) -> CatalogMode {
        self.catalog
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
        txn.marks.push(txn.staged_hashes.len());
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
        // Decide what to free *before* releasing, while the transaction's view of
        // the catalog is still the one the commit is about to make permanent.
        let to_free = if depth == 0 {
            self.unreferenced(|t| std::mem::take(&mut t.pending_free).into_iter().collect())?
        } else {
            Vec::new()
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
        tracing::debug!(freed = to_free.len(), "transaction committed");
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
            // The catalog has unwound this level, so the arrays it wrote are as
            // unreachable as an outermost rollback's — free them on the same
            // terms rather than leaving them for the outer commit, which only
            // ever looks at `pending_free` and would strand them in the file.
            // `unreferenced` rechecks each one, so a hash that predates this
            // level, or that an enclosing level also wrote, is kept.
            let mark = {
                let txn = self.txn.as_mut().expect("checked above");
                txn.depth = depth;
                txn.marks.pop().unwrap_or(0)
            };
            let to_free = self.unreferenced(|t| t.staged_hashes.split_off(mark))?;
            for hash in &to_free {
                self.backend.remove_array(hash)?;
            }
            tracing::debug!(
                depth,
                removed = to_free.len(),
                "inner transaction rolled back"
            );
            return Ok(());
        }
        // The catalog is back to its pre-transaction state, so anything this
        // transaction wrote is now unreferenced and must go. Recheck rather than
        // trusting the staged list: an array can predate the transaction and have
        // been re-referenced by a rolled-back add.
        let to_free =
            self.unreferenced(|t| std::mem::take(&mut t.staged_hashes).into_iter().collect())?;
        // Deferred frees are abandoned: rollback restored the rows pointing at
        // those arrays, so the data must stay.
        self.txn = None;
        for hash in &to_free {
            self.backend.remove_array(hash)?;
        }
        tracing::debug!(removed = to_free.len(), "transaction rolled back");
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
    fn unreferenced(
        &mut self,
        take: impl FnOnce(&mut OpenTxn) -> Vec<[u8; 32]>,
    ) -> Result<Vec<[u8; 32]>> {
        let candidates = take(self.txn.as_mut().expect("caller checked a txn is open"));
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let tx = self.metadata.savepoint()?;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for hash in candidates {
            if seen.insert(hash) && references_to_in_tx(&tx, &hash)? == 0 {
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
    pub fn add_time_series(
        &mut self,
        owner_id: i64,
        owner_type: &str,
        owner_category: OwnerCategory,
        data: TimeSeriesData,
        features: Features,
    ) -> Result<TimeSeriesKey> {
        self.add_per_column(vec![AddRequest {
            owner_id,
            owner_type: owner_type.to_string(),
            owner_category,
            data,
            features,
        }])
        .map(|mut keys| keys.remove(0))
    }

    /// Add one time series from an [`AddRequest`]. Equivalent to
    /// [`Self::add_time_series`] — both preserve the series' `element_type`,
    /// `units`, `quantity_kind`, `unit_system`, `component_field`, and
    /// `application_data`, since those travel on the [`TimeSeriesData`] itself.
    /// Routed through the same per-column path.
    pub fn add(&mut self, request: AddRequest) -> Result<TimeSeriesKey> {
        self.add_per_column(vec![request])
            .map(|mut keys| keys.remove(0))
    }

    /// Bulk insert. All-or-nothing: any error rolls back every association and
    /// array put performed in this call.
    ///
    /// This is a managed batch, so it takes the block-write path
    /// ([`Self::bulk_add`] internals): packed series are packed into batch-sized
    /// datasets that fill whole chunks. A one-at-a-time un-managed loop should use
    /// [`Self::add_time_series`], which packs incrementally into shared datasets.
    #[tracing::instrument(skip(self, items), fields(count = items.len()))]
    pub fn add_time_series_bulk(&mut self, items: Vec<AddRequest>) -> Result<Vec<TimeSeriesKey>> {
        self.flush_bulk_add(items)
    }

    /// Per-column insert used by single [`Self::add_time_series`] calls: each
    /// packed array is dropped into the first free slot of a shared, default-width
    /// dataset (created on demand, spilling once full). This keeps incremental
    /// un-managed adds space-efficient and still grouped for read-by-timestamp,
    /// at the cost of a per-column read-modify-write under the timestamp-major
    /// chunking. All-or-nothing, like [`Self::add_time_series_bulk`].
    #[tracing::instrument(skip(self, items), fields(count = items.len()))]
    fn add_per_column(&mut self, items: Vec<AddRequest>) -> Result<Vec<TimeSeriesKey>> {
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

        // Stage backend writes so we can roll them back on metadata error.
        let mut staged_hashes: Vec<[u8; 32]> = Vec::with_capacity(items.len());
        let tx = self.metadata.savepoint()?;
        let mut keys = Vec::with_capacity(items.len());
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
                key,
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
            self.backend.put_array(&hash, data, group, layout)?;
            if !already_present {
                staged_hashes.push(hash);
            }

            match insert_association(&tx, &meta, &mut shared_sets) {
                Ok(()) => {
                    keys.push(key);
                }
                Err(e) => {
                    // Rollback metadata via Drop; also undo any array puts we
                    // staged in this call so the store returns to its prior state.
                    drop(tx);
                    for staged in &staged_hashes {
                        let _ = self.backend.remove_array(staged);
                    }
                    return Err(e);
                }
            }
        }

        tx.commit()?;
        // Hand the writes to an enclosing transaction, which owns undoing them
        // if it rolls back. Outside one this is a no-op: the call has already
        // unwound its own writes on every failing path above.
        for hash in staged_hashes {
            self.note_array_written(hash);
        }
        tracing::debug!(count = keys.len(), "transaction committed");
        Ok(keys)
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
    fn flush_bulk_add(&mut self, items: Vec<AddRequest>) -> Result<Vec<TimeSeriesKey>> {
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
        let mut staged_hashes: Vec<[u8; 32]> = Vec::new();

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
                    staged_hashes.push(p.hash);
                }
            }
        }
        for (pool, idxs) in &packed_groups {
            let hashes: Vec<[u8; 32]> = idxs.iter().map(|&i| parts[i].hash).collect();
            let arrays: Vec<&TypedArray> = idxs.iter().map(|&i| request_array(&items[i])).collect();
            let written = self.backend.put_packed_block(&hashes, &arrays, pool.3)?;
            for (j, &i) in idxs.iter().enumerate() {
                if written[j] {
                    staged_hashes.push(parts[i].hash);
                }
            }
        }

        // Insert associations in input order; roll the whole batch back on error.
        let tx = self.metadata.savepoint()?;
        let mut shared_sets = SharedSetCache::default();
        for p in &parts {
            if let Err(e) = insert_association(&tx, &p.meta, &mut shared_sets) {
                drop(tx);
                for staged in &staged_hashes {
                    let _ = self.backend.remove_array(staged);
                }
                return Err(e);
            }
        }
        tx.commit()?;
        for hash in staged_hashes {
            self.note_array_written(hash);
        }
        tracing::debug!(count = parts.len(), "bulk-add transaction committed");
        Ok(parts.into_iter().map(|p| p.key).collect())
    }

    #[tracing::instrument(skip(self, key), fields(owner = key.owner_id, name = %key.name))]
    pub fn remove_time_series(&mut self, key: &KeyIdentity) -> Result<()> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        let removed_hashes = MetadataStore::delete_by_key(&tx, key)?;
        if removed_hashes.is_empty() {
            return Err(TimeSeriesError::NotFound);
        }
        if key.time_series_type == TimeSeriesType::SingleTimeSeries {
            // Dropping the tx rolls the deletion back on error.
            Self::check_no_orphaned_dst(&tx, removed_hashes.iter().copied())?;
        }
        // For each removed association, drop the underlying array iff no other
        // association still references it.
        let mut to_drop = Vec::new();
        for h in &removed_hashes {
            if references_to_in_tx(&tx, h)? == 0 {
                to_drop.push(*h);
            }
        }
        tx.commit()?;
        for h in to_drop {
            self.free_or_defer(h)?;
        }
        Ok(())
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
    fn check_no_orphaned_dst(
        tx: &rusqlite::Connection,
        removed_sts_hashes: impl IntoIterator<Item = [u8; 32]>,
    ) -> Result<()> {
        let mut seen = HashSet::new();
        for h in removed_sts_hashes {
            if !seen.insert(h) {
                continue;
            }
            let dst =
                typed_references_to_in_tx(tx, &h, TimeSeriesType::DeterministicSingleTimeSeries)?;
            if dst > 0 && typed_references_to_in_tx(tx, &h, TimeSeriesType::SingleTimeSeries)? == 0
            {
                return Err(TimeSeriesError::InvalidParameter(
                    "cannot remove a SingleTimeSeries that backs a DeterministicSingleTimeSeries; \
                     remove the derived forecast first"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// Remove several time series in one all-or-nothing transaction, dropping
    /// each underlying array that no surviving association references (exactly
    /// like [`Self::remove_time_series`], and sharing its removal helper).
    /// Returns the number of associations removed.
    ///
    /// A key that matches nothing makes the whole batch fail with
    /// [`TimeSeriesError::NotFound`] and roll back — the batch either removes
    /// every requested series or none.
    pub fn remove_time_series_bulk(&mut self, keys: &[&KeyIdentity]) -> Result<usize> {
        self.remove_identities(keys, true)
    }

    /// Remove every time series matching `filter` in one all-or-nothing
    /// transaction, dropping newly unreferenced arrays like
    /// [`Self::remove_time_series`]. Returns the number of associations removed;
    /// an empty match is `Ok(0)`.
    pub fn remove_by_filter(&mut self, filter: ListFilter) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let identities: Vec<KeyIdentity> = self
            .list_keys(filter)?
            .into_iter()
            .map(|k| k.identity().clone())
            .collect();
        let refs: Vec<&KeyIdentity> = identities.iter().collect();
        // Keys come straight from `list_keys`, so each is guaranteed to match;
        // `require_all` is moot here but keeps one code path.
        self.remove_identities(&refs, false)
    }

    /// Shared removal core for the bulk paths: delete every key's association(s)
    /// in one transaction, then drop each array left unreferenced. With
    /// `require_all`, a key matching nothing aborts and rolls back the batch.
    fn remove_identities(&mut self, keys: &[&KeyIdentity], require_all: bool) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        let mut removed_hashes: Vec<[u8; 32]> = Vec::new();
        let mut removed_sts_hashes: Vec<[u8; 32]> = Vec::new();
        let mut count = 0usize;
        for key in keys {
            let removed = MetadataStore::delete_by_key(&tx, key)?;
            if removed.is_empty() && require_all {
                // Roll back (tx drops) so the batch is all-or-nothing.
                return Err(TimeSeriesError::NotFound);
            }
            count += removed.len();
            if key.time_series_type == TimeSeriesType::SingleTimeSeries {
                removed_sts_hashes.extend(removed.iter().copied());
            }
            removed_hashes.extend(removed);
        }
        // Checked after all deletes, so a batch removing a DST together with
        // its backing series passes regardless of order.
        Self::check_no_orphaned_dst(&tx, removed_sts_hashes)?;
        // Decide array drops after *all* deletes so a hash referenced only by
        // other rows removed in this same batch is reclaimed too. Dedup so a
        // hash removed via several keys is checked (and dropped) once.
        let mut to_drop = Vec::new();
        let mut seen = HashSet::new();
        for h in &removed_hashes {
            if seen.insert(*h) && references_to_in_tx(&tx, h)? == 0 {
                to_drop.push(*h);
            }
        }
        tx.commit()?;
        for h in to_drop {
            self.free_or_defer(h)?;
        }
        Ok(count)
    }

    /// Remove every time series for the owner `(owner_id, owner_category)`, or
    /// every time series in the store when `owner` is `None`. Returns the count
    /// removed.
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
    #[tracing::instrument(skip(self, src), fields(owner = src.owner_id, name = %src.name))]
    pub fn copy_time_series(
        &mut self,
        src: &KeyIdentity,
        dst_owner_id: i64,
        dst_owner_type: &str,
        new_name: Option<&str>,
    ) -> Result<TimeSeriesKey> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }

        let mut meta = self.metadata.get_by_key(src)?;
        meta.owner_id = dst_owner_id;
        meta.owner_type = dst_owner_type.to_string();
        if let Some(name) = new_name {
            meta.name = name.to_string();
        }

        let dst = KeyIdentity {
            owner_id: meta.owner_id,
            owner_category: meta.owner_category,
            time_series_type: meta.time_series_type,
            name: meta.name.clone(),
            resolution: meta.resolution,
            interval: meta.interval,
            features: meta.features.clone(),
        };
        if self.has_time_series(&dst)? {
            return Err(TimeSeriesError::DuplicateTimeSeries);
        }

        let tx = self.metadata.savepoint()?;
        check_forecast_family_free(&tx, &meta, "copy")?;
        MetadataStore::insert(&tx, &meta)?;
        tx.commit()?;

        TimeSeriesKey::from_metadata(&meta)
    }

    #[tracing::instrument(skip(self, key, time_range), fields(owner = key.owner_id, name = %key.name, has_time_range = time_range.is_some()))]
    pub fn get_time_series(
        &self,
        key: &KeyIdentity,
        time_range: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    ) -> Result<TimeSeriesData> {
        let meta = self.metadata.get_by_key(key)?;
        self.materialize_time_series(&meta, time_range)
    }

    /// Like [`Self::get_time_series`], but also returns the association's
    /// catalog row from the same single lookup. Callers that need both the
    /// reconstructed series and row-level detail (the FFI getters read the
    /// `application_data` payload alongside the data) would otherwise pay a second
    /// SQLite
    /// key lookup per read — at 100k-series scale that lookup is ~20% of a
    /// full read.
    pub fn get_time_series_with_metadata(
        &self,
        key: &KeyIdentity,
        time_range: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    ) -> Result<(TimeSeriesData, TimeSeriesMetadata)> {
        let meta = self.metadata.get_by_key(key)?;
        let data = self.materialize_time_series(&meta, time_range)?;
        Ok((data, meta))
    }

    /// Reconstruct the series described by `meta`, reading its array (or the
    /// requested `time_range` slice) from the backend.
    fn materialize_time_series(
        &self,
        meta: &TimeSeriesMetadata,
        time_range: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    ) -> Result<TimeSeriesData> {
        // The descriptors describe the series but live on the catalog row, not
        // in the array bytes. Filling them in here — once, for every variant —
        // is what makes a read round-trip what a write declared.
        let mut data = self.materialize_array(meta, time_range)?;
        data.set_descriptors(Descriptors {
            element_type: meta.element_type,
            units: meta.units.clone(),
            quantity_kind: meta.quantity_kind.clone(),
            unit_system: meta.unit_system,
            component_field: meta.component_field.clone(),
            application_data: meta.application_data.clone(),
        });
        Ok(data)
    }

    /// The array-reconstruction half of [`Self::materialize_time_series`]: builds
    /// the variant from the stored bytes and the row's shape/time fields. The
    /// descriptive attributes are left unset for the caller to fill in.
    fn materialize_array(
        &self,
        meta: &TimeSeriesMetadata,
        time_range: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    ) -> Result<TimeSeriesData> {
        tracing::debug!(ts_type = ?meta.time_series_type, "metadata loaded");
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
                // Element bytes per underlying step.
                let elem_shape: Vec<usize> = arr.shape[1..].to_vec();
                let elem_bytes: usize = elem_shape.iter().product::<usize>() * arr.dtype.size();
                let elem_factor = if elem_bytes == 0 {
                    arr.dtype.size()
                } else {
                    elem_bytes
                };

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

    pub fn list_time_series(&self, filter: ListFilter) -> Result<Vec<TimeSeriesMetadata>> {
        self.metadata.list(&filter.into())
    }

    /// List the [`TimeSeriesKey`] of every association matching `filter`. This is
    /// the key-centric counterpart of [`Self::list_time_series`]: each row is
    /// reduced to its identifying + descriptive key, dropping physical storage
    /// detail (`data_hash`, `dtype`, `application_data`, `percentiles`) which is read
    /// on demand via [`Self::get_metadata`]. The binding-facing listing path.
    pub fn list_keys(&self, filter: ListFilter) -> Result<Vec<TimeSeriesKey>> {
        self.metadata
            .list(&filter.into())?
            .iter()
            .map(TimeSeriesKey::from_metadata)
            .collect()
    }

    /// Like [`Self::list_keys`], but pairs each key with the 32-byte content hash
    /// of the array it resolves to. Rows that share a stored array carry the same
    /// hash, which is what lets a caller group time series by their underlying
    /// data: deduplicated identical arrays, and a `SingleTimeSeries` together with
    /// any `DeterministicSingleTimeSeries` derived from it. The hash is read
    /// straight off each metadata row, so this is still one catalog query.
    pub fn list_keys_with_hash(
        &self,
        filter: ListFilter,
    ) -> Result<Vec<(TimeSeriesKey, [u8; 32])>> {
        self.metadata
            .list(&filter.into())?
            .iter()
            .map(|m| Ok((TimeSeriesKey::from_metadata(m)?, m.data_hash)))
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
                let rows = self.list_time_series(filter)?;
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
                let timestamps = self.metadata.timestamps_for_hash(&hash)?;
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
        let index = reader.index_at(at)?;
        for group in reader.groups_mut() {
            group.fill(|hashes, dtype, out| {
                self.backend.read_index_into(hashes, dtype, index, out)
            })?;
        }
        reader.mark_read(at);
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
        for m in self.list_time_series(filter)? {
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
        let window = reader.window_index(at)?;
        // One read per *slot*: forecasts that share an array and read plan
        // (e.g. components referencing one shared forecast) collapse to a single
        // slot at build time, so the backend is hit once for all of them. Each
        // slot caches its enclosing chunk block, so stepping the timeline only
        // hits the backend when the window crosses a block boundary.
        let backend = &*self.backend;
        for slot in reader.slots_mut() {
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
        reader.mark_read(at);
        Ok(())
    }

    /// Read many full series at once, returning a [`TimeSeriesData`] per key in
    /// order. This is the bulk counterpart to [`Self::get_time_series`] for
    /// whole-series reads (e.g. exploration or plotting): packed `SingleTimeSeries`
    /// are read in one decompress-once pass per dataset via
    /// [`StorageBackend::read_arrays`], rather than re-reading every chunk once per
    /// series — the read-side complement to the timestamp-major layout, where a
    /// single full-series read is otherwise the slow direction. Other types get
    /// no batching benefit on the array side — they are standalone, or one
    /// column of a cohort dataset — so they are rebuilt one at a time, but from
    /// the metadata this call already loaded. No time-range slicing — each
    /// series is returned in full.
    #[tracing::instrument(skip(self, keys), fields(count = keys.len()))]
    pub fn bulk_read(&self, keys: &[&KeyIdentity]) -> Result<Vec<TimeSeriesData>> {
        let metas: Vec<TimeSeriesMetadata> = keys
            .iter()
            .map(|k| self.metadata.get_by_key(k))
            .collect::<Result<_>>()?;

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

        let mut out = Vec::with_capacity(keys.len());
        for meta in &metas {
            if meta.time_series_type == TimeSeriesType::SingleTimeSeries {
                let data = single_arrays.next().ok_or_else(|| {
                    TimeSeriesError::IntegrityError(
                        "bulk_read: fewer arrays returned than SingleTimeSeries keys".into(),
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

    /// Read many series at once, optionally sliced to `time_range`. With `None`
    /// this is exactly [`Self::bulk_read`] (whole series, with the packed
    /// batch-read optimization). With a range, each key is read through
    /// [`Self::get_time_series`], so the slice semantics match a per-key sliced
    /// read; the packed-batch fast path is not applied to sliced reads.
    #[tracing::instrument(skip(self, keys), fields(count = keys.len(), has_time_range = time_range.is_some()))]
    pub fn bulk_read_range(
        &self,
        keys: &[&KeyIdentity],
        time_range: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    ) -> Result<Vec<TimeSeriesData>> {
        match time_range {
            None => self.bulk_read(keys),
            Some(range) => keys
                .iter()
                .map(|k| self.get_time_series(k, Some(range)))
                .collect(),
        }
    }

    /// Look up the full metadata record for a key. Errors with `NotFound` if no
    /// association matches. Used by external bindings (e.g. the Julia
    /// `RustTimeSeriesStore`) to reconstruct a typed metadata object on read.
    pub fn get_metadata(&self, key: &KeyIdentity) -> Result<TimeSeriesMetadata> {
        self.metadata.get_by_key(key)
    }

    /// [`Self::get_metadata`] for many keys, in order. Errors with `NotFound` if
    /// any key is missing. The companion to [`Self::bulk_read`] for callers that
    /// need each series' metadata (its `element_type`, say) alongside the values.
    pub fn get_metadata_bulk(&self, keys: &[&KeyIdentity]) -> Result<Vec<TimeSeriesMetadata>> {
        keys.iter().map(|k| self.metadata.get_by_key(k)).collect()
    }

    /// Resolve a forecast addressed by attributes plus a requested
    /// [`TimeSeriesType`] to the [`TimeSeriesKey`] of the single matching
    /// association. The returned key's `time_series_type` is the concrete
    /// stored type that matched, which is how a caller inspects whether it got
    /// a real or a synthetic forecast.
    ///
    /// Matching follows [`TimeSeriesType::accepts`], so requesting
    /// `Deterministic` also resolves a stored `DeterministicSingleTimeSeries`.
    /// The two cannot coexist for one identity (see `insert_association`), so
    /// this never introduces ambiguity. The catalog — not the caller — decides
    /// which stored form satisfies the request.
    ///
    /// `resolution` and `interval` are optional filters on the identity. Leave
    /// either unset to match across it; supply it to disambiguate when several
    /// series share the other attributes (e.g. a day-ahead and a real-time
    /// forecast that differ only by interval).
    ///
    /// Errors:
    /// - [`TimeSeriesError::NotFound`] if nothing matches.
    /// - [`TimeSeriesError::InvalidParameter`] if the request is ambiguous (more
    ///   than one stored series matches); the caller must then narrow it with a
    ///   concrete type, a resolution, and/or an interval.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_forecast_key(
        &self,
        owner_id: i64,
        owner_category: OwnerCategory,
        name: &str,
        resolution: Option<Period>,
        interval: Option<Period>,
        features: Features,
        requested: TimeSeriesType,
    ) -> Result<TimeSeriesKey> {
        // List candidates sharing (owner, name, resolution, interval, features),
        // then keep those whose concrete type satisfies the request. A concrete
        // request with resolution+interval pinned resolves to one row via the
        // unique index; looser requests may match several and are reported as
        // ambiguous rather than silently picking one.
        let f_hash = crate::hash::features_hash(&features);
        let mut matches = self.metadata.list(&MetadataFilter {
            owner_id: Some(owner_id),
            owner_category: Some(owner_category),
            name: Some(name.to_string()),
            resolution,
            interval,
            features_hash: Some(f_hash),
            ..Default::default()
        })?;
        matches.retain(|m| m.features == features && requested.accepts(m.time_series_type));
        match matches.len() {
            0 => Err(TimeSeriesError::NotFound),
            1 => TimeSeriesKey::from_metadata(&matches.pop().unwrap()),
            _ => {
                // More than one candidate matches: with `resolution`/`interval`
                // unset this can be several forecasts at different resolutions or
                // intervals, so report the actual candidates rather than
                // asserting a single shape.
                let describe = |p: Option<Period>| match p {
                    Some(p) => p.to_iso8601(),
                    None => "-".to_string(),
                };
                let mut candidates: Vec<String> = matches
                    .iter()
                    .map(|m| {
                        format!(
                            "{} (resolution={}, interval={})",
                            m.time_series_type.as_str(),
                            describe(m.resolution),
                            describe(m.interval),
                        )
                    })
                    .collect();
                candidates.sort();
                Err(TimeSeriesError::InvalidParameter(format!(
                    "ambiguous forecast request for '{name}': {} candidates match \
                     ({}); narrow it with a concrete type, resolution, and/or interval",
                    candidates.len(),
                    candidates.join(", "),
                )))
            }
        }
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

    pub fn get_time_series_keys(
        &self,
        owner_id: i64,
        owner_category: OwnerCategory,
    ) -> Result<Vec<TimeSeriesKey>> {
        self.metadata.list_keys_for_owner(owner_id, owner_category)
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
        let sources = self.metadata.list(&MetadataFilter {
            time_series_type: Some(TypeMatch::Exact(TimeSeriesType::SingleTimeSeries)),
            owner_category,
            resolution,
            ..Default::default()
        })?;

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
            new_metas.push(TimeSeriesMetadata {
                time_series_type: TimeSeriesType::DeterministicSingleTimeSeries,
                horizon: Some(horizon),
                interval: Some(interval),
                count: Some(count),
                ..src.clone()
            });
        }

        if policy.dry_run {
            // Every check has run against the rows that would be written; the
            // caller only wanted the verdict.
            return Ok(TransformOutcome {
                transformed: new_metas.len(),
                sources: sources.len(),
                interval,
                interval_normalized: plan.interval_normalized,
            });
        }

        let tx = self.metadata.savepoint()?;
        // One cache for the whole batch: every derived row shares its source's
        // feature set, and sources overwhelmingly share sets with each other, so
        // the feature-set writes collapse to a handful regardless of how many
        // series are transformed.
        let mut feature_sets = SharedSetCache::default();
        for meta in &new_metas {
            if let Err(e) = MetadataStore::insert_batched(&tx, meta, &mut feature_sets) {
                drop(tx);
                return Err(e);
            }
        }
        tx.commit()?;
        Ok(TransformOutcome {
            transformed: new_metas.len(),
            sources: sources.len(),
            interval,
            interval_normalized: plan.interval_normalized,
        })
    }

    /// True iff an association with exactly this key identity exists.
    ///
    /// A covering-index probe (`SELECT 1 ... LIMIT 1` on the uniqueness
    /// index), safe for hot per-component loops: it fetches no row, hydrates
    /// no metadata, and runs one statement. The key's feature set is matched
    /// by its content hash — equal hash implies equal set, the same
    /// content-addressing contract `feature_sets` storage relies on.
    pub fn has_time_series(&self, key: &KeyIdentity) -> Result<bool> {
        self.metadata.exists(&MetadataFilter {
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

    /// Distinct series names matching `filter`, sorted. A discovery projection
    /// over the authoritative filtered listing, so every filter (including the
    /// `features` subset match) is honored.
    pub fn list_names(&self, filter: ListFilter) -> Result<Vec<String>> {
        let mut names: Vec<String> = self
            .list_time_series(filter)?
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
            .list_time_series(filter)?
            .into_iter()
            .map(|m| m.owner_type)
            .collect();
        types.sort();
        types.dedup();
        Ok(types)
    }

    /// Rename the series identified by `key` to `new_name`, returning the new
    /// key. Only the catalog association's name changes; the underlying array
    /// and its hash are untouched. Errors: [`TimeSeriesError::NotFound`] if `key`
    /// matches nothing, [`TimeSeriesError::DuplicateTimeSeries`] if a series with
    /// the new identity already exists, [`TimeSeriesError::ReadOnlyStore`] on a
    /// read-only store.
    pub fn rename_time_series(
        &mut self,
        key: &KeyIdentity,
        new_name: &str,
    ) -> Result<TimeSeriesKey> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        // A rename moves the row to a new family identity, which can put it
        // alongside the counterpart it is mutually exclusive with. Read the row
        // first so the check can be posed against the *destination* name.
        let mut probe = self.metadata.get_by_key(key)?;
        probe.name = new_name.to_string();

        let tx = self.metadata.savepoint()?;
        check_forecast_family_free(&tx, &probe, "rename to")?;
        let updated = MetadataStore::rename(&tx, key, new_name)?;
        if updated == 0 {
            // No matching row; tx drops (rolls back a no-op).
            return Err(TimeSeriesError::NotFound);
        }
        if updated > 1 {
            // A key names one series, so touching more than one row means the
            // predicate was wider than the caller asked for. Fail *before* the
            // commit: this used to commit a multi-row update and only then
            // discover the ambiguity on the follow-up lookup, reporting an error
            // for a rename that had already taken full effect. The `tx` drop
            // rolls it back.
            return Err(TimeSeriesError::IntegrityError(format!(
                "rename matched {updated} associations for a single key identity"
            )));
        }
        tx.commit()?;
        // Rebuild the key from the renamed row (same identity, new name).
        let new_identity = KeyIdentity {
            name: new_name.to_string(),
            ..key.clone()
        };
        let meta = self.metadata.get_by_key(&new_identity)?;
        TimeSeriesKey::from_metadata(&meta)
    }

    /// Return the forecast parameters recorded in the store, optionally
    /// restricted to forecasts with a given `resolution` and/or `interval`.
    ///
    /// Looks for any metadata row whose type is a forecast type (and matches the
    /// filters) and returns its `horizon`, `interval`, `count`, and `resolution`.
    /// If none match, returns [`ForecastParameters::default()`]. When multiple
    /// match, returns the first one found (v0 stores a single coherent forecast
    /// configuration; callers that need per-type parameters should use
    /// [`Self::list_time_series`] directly). Both `resolution` and `interval`
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
            let rows = self.metadata.list(&MetadataFilter {
                time_series_type: Some(TypeMatch::Exact(ts_type)),
                resolution,
                interval,
                ..Default::default()
            })?;
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

    /// Attach a supplemental attribute to a component. Fails with
    /// [`TimeSeriesError::DuplicateAssociation`] if that component already
    /// carries that attribute, whatever type names are supplied.
    pub fn add_supplemental_attribute_association(
        &mut self,
        assoc: SupplementalAttributeAssociation,
    ) -> Result<()> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        MetadataStore::insert_supplemental_attribute_association(&tx, &assoc)?;
        tx.commit()?;
        Ok(())
    }

    /// Attach many in one all-or-nothing transaction, returning the number
    /// inserted. A duplicate anywhere in the batch rolls the whole batch back.
    /// This is the import half of the bulk round trip whose export is
    /// [`Self::list_supplemental_attribute_associations`] with a default filter.
    pub fn add_supplemental_attribute_associations(
        &mut self,
        assocs: Vec<SupplementalAttributeAssociation>,
    ) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        for assoc in &assocs {
            MetadataStore::insert_supplemental_attribute_association(&tx, assoc)?;
        }
        tx.commit()?;
        Ok(assocs.len())
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
    pub fn add_parent_child_association(&mut self, assoc: ParentChildAssociation) -> Result<()> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        MetadataStore::insert_parent_child_association(&tx, &assoc)?;
        tx.commit()?;
        Ok(())
    }

    /// Record many edges in one all-or-nothing transaction, returning the number
    /// inserted.
    pub fn add_parent_child_associations(
        &mut self,
        assocs: Vec<ParentChildAssociation>,
    ) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.savepoint()?;
        for assoc in &assocs {
            MetadataStore::insert_parent_child_association(&tx, assoc)?;
        }
        tx.commit()?;
        Ok(assocs.len())
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

    /// Reclaim space in both halves of the artifact.
    ///
    /// On the catalog side this sweeps the content-addressed feature sets and
    /// timestamp vectors no association references any more.
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
        let timestamp_sets_reclaimed = MetadataStore::sweep_orphan_timestamp_sets(&tx)?;
        tx.commit()?;

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
    /// # Scope: the array half only
    ///
    /// A persisted store is two artifacts — the HDF5 file and its companion
    /// `<path>.sqlite` catalog — but this checks only the first. It reads each
    /// array the HDF5 side knows about, rehashes it, and compares. It does
    /// **not** open, parse, or cross-reference the catalog, so an empty report is
    /// not a statement that the store as a whole is sound. In particular these
    /// are all invisible to it:
    ///
    /// - a `data_hash` in the catalog that names no stored array (a truncated or
    ///   corrupted catalog, or a catalog paired with the wrong HDF5 file) —
    ///   every read of the affected key fails, but this reports no error;
    /// - a catalog row whose `dtype`, `element_shape`, or `length` misdescribes
    ///   the array it points at;
    /// - a missing catalog: opening read-write with the `.sqlite` half deleted
    ///   silently recreates it empty, and the resulting store — zero time series,
    ///   every array still on disk and now unreachable — verifies clean.
    ///
    /// What it does catch is the array-side corruption it is named for: a stored
    /// value perturbed behind its recorded hash, and a read failure on any
    /// indexed array.
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
                drop(std::mem::replace(
                    &mut self.backend,
                    Box::new(MemoryBackend::new()) as Box<dyn StorageBackend>,
                ));
                let staged = std::fs::copy(&src, &tmp_h5)
                    .map_err(TimeSeriesError::from)
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
        })
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
        for meta in self.list_time_series(ListFilter::default())? {
            let plan = ArrayPlan {
                layout: array_layout_for(meta.time_series_type),
                pool: pool_key_of(&meta),
            };
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
    /// association in one transaction, returning the keys in push order. On any
    /// error nothing is committed and staged arrays are rolled back.
    pub fn commit(mut self) -> Result<Vec<TimeSeriesKey>> {
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
    key: TimeSeriesKey,
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
fn build_request_parts(item: &AddRequest) -> Result<RequestParts> {
    // Every write funnels through here (per-column adds and buffered bulk adds
    // alike), which makes it the one place the reserved-feature-name rule has
    // to hold.
    validate_features(&item.features)?;
    let element_type = resolve_element_type(item)?;
    validate_data(&item.data)?;
    let (hash, group, layout, meta, key) = match &item.data {
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
                    component_field: item.data.component_field().map(str::to_owned),
                    percentiles: None,
                    element_type,
                    element_shape: single.data.element_shape().to_vec(),
                    application_data: item.data.application_data().map(str::to_owned),
                },
                TimeSeriesKey::Single(SingleTimeSeriesKey::new(
                    item.owner_id,
                    item.owner_category,
                    single.name.clone(),
                    single.resolution,
                    item.features.clone(),
                    single.initial_timestamp,
                    single.length,
                )),
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
                    component_field: item.data.component_field().map(str::to_owned),
                    percentiles: None,
                    element_type,
                    element_shape: non_sequential.data.element_shape().to_vec(),
                    application_data: item.data.application_data().map(str::to_owned),
                },
                TimeSeriesKey::NonSequential(NonSequentialTimeSeriesKey::new(
                    item.owner_id,
                    item.owner_category,
                    non_sequential.name.clone(),
                    item.features.clone(),
                    non_sequential.length,
                )),
            )
        }
        // Dense forecast types are stored as standalone arrays in their
        // native shape. `DeterministicSingleTimeSeries` is not added
        // directly; it is derived from a stored `SingleTimeSeries` via
        // [`Store::transform_single_time_series`].
        TimeSeriesData::Deterministic(det) => (
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
            forecast_key(
                item,
                TimeSeriesType::Deterministic,
                &det.name,
                det.resolution,
                det.initial_timestamp,
                det.horizon,
                det.interval,
                det.count,
            ),
        ),
        TimeSeriesData::Probabilistic(prob) => (
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
            forecast_key(
                item,
                TimeSeriesType::Probabilistic,
                &prob.name,
                prob.resolution,
                prob.initial_timestamp,
                prob.horizon,
                prob.interval,
                prob.count,
            ),
        ),
        TimeSeriesData::Scenarios(scen) => (
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
            forecast_key(
                item,
                TimeSeriesType::Scenarios,
                &scen.name,
                scen.resolution,
                scen.initial_timestamp,
                scen.horizon,
                scen.interval,
                scen.count,
            ),
        ),
    };
    Ok(RequestParts {
        hash,
        group,
        layout,
        meta,
        key,
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
fn request_array(item: &AddRequest) -> &TypedArray {
    match &item.data {
        TimeSeriesData::SingleTimeSeries(single) => &single.data,
        TimeSeriesData::NonSequentialTimeSeries(non_sequential) => &non_sequential.data,
        TimeSeriesData::Deterministic(det) => &det.data,
        TimeSeriesData::Probabilistic(prob) => &prob.data,
        TimeSeriesData::Scenarios(scen) => &scen.data,
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
) -> Result<()> {
    check_forecast_family_free(tx, meta, "add")?;
    MetadataStore::insert_batched(tx, meta, cache).map(|_| ())
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
fn validate_data(data: &TimeSeriesData) -> Result<()> {
    let invalid = TimeSeriesError::InvalidParameter;
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

/// [`crate::timestamps::require_millisecond_precision`] as a
/// [`TimeSeriesError`], labelled with the type whose `initial_timestamp` it is.
fn require_ms(t: chrono::DateTime<chrono::Utc>, label: &str) -> Result<()> {
    crate::timestamps::require_millisecond_precision(t, &format!("{label} initial_timestamp"))
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
/// The sibling [`validate_non_sequential`] has always enforced the equivalent
/// rule; this is the static path catching up.
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
        crate::timestamps::require_millisecond_precision(
            *t,
            &format!("NonSequentialTimeSeries timestamp {i}"),
        )
        .map_err(TimeSeriesError::InvalidParameter)?;
    }
    Ok(())
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
        component_field: item.data.component_field().map(str::to_owned),
        percentiles,
        element_type,
        element_shape: data.element_shape().to_vec(),
        application_data: item.data.application_data().map(str::to_owned),
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

/// Build the key returned for a dense forecast added via
/// [`Store::add_time_series_bulk`].
#[allow(clippy::too_many_arguments)]
fn forecast_key(
    item: &AddRequest,
    time_series_type: TimeSeriesType,
    name: &str,
    resolution: Period,
    initial_timestamp: chrono::DateTime<chrono::Utc>,
    horizon: Period,
    interval: Period,
    count: usize,
) -> TimeSeriesKey {
    TimeSeriesKey::Forecast(ForecastTimeSeriesKey::new(
        item.owner_id,
        item.owner_category,
        time_series_type,
        name.to_owned(),
        resolution,
        item.features.clone(),
        initial_timestamp,
        horizon,
        interval,
        count,
    ))
}

/// Open the array backend for an existing store file. The `storage_backend`
/// root attribute identifies files written by [`Hdf5Backend`]; files without it
/// (including stores written by the removed netcdf backend) are rejected with
/// an actionable error instead of being misread.
///
/// Reachability is checked first, and separately. `is_hdf5_backend_file` cannot
/// tell "this file is not an infrastore store" from "there is no file here" —
/// it answers `false` either way — so without this a typo'd path or an
/// unreadable directory would be reported as a netcdf-era store needing
/// migration, which is advice about a file that does not exist. The `io::Error`
/// kind is carried through, so a missing path and a permission-denied one stay
/// distinguishable.
fn open_backend(path: &Path, read_only: bool) -> Result<Box<dyn StorageBackend>> {
    if let Err(e) = std::fs::metadata(path) {
        return Err(TimeSeriesError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot open store {}: {e}", path.display()),
        )));
    }
    if !crate::storage::hdf5::is_hdf5_backend_file(path) {
        return Err(TimeSeriesError::InvalidParameter(format!(
            "{} is not an infrastore hdf5 store (stores written by the removed \
             netcdf backend are no longer supported; re-create the store to \
             migrate)",
            path.display()
        )));
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
/// The cost is that a crashed staging no longer gets swept by the next one:
/// leftovers accumulate as `<target>.persist-<tag>` / `<store>.h5.repack-<tag>`
/// siblings. They cannot be swept safely, because a temp belonging to a live
/// concurrent save is indistinguishable from an abandoned one. Callers may
/// delete them once no save is in flight.
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
