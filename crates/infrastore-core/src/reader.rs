//! Timestamp-oriented readers.
//!
//! Simulations consume the store with a `for` loop over every timestamp: at
//! each timestamp they want the value of *every* time series at that instant.
//! [`StaticReader`] serves exactly that access pattern for `SingleTimeSeries`,
//! which is why those arrays are kept in the compacted/packed on-disk format
//! (one timestamp across all columns of a packed dataset is a single hyperslab).
//!
//! # Design (locked 2026-06-25)
//!
//! * **One resolution per reader.** The build filter must pin a resolution.
//! * **Static and forecast are separate readers.** [`StaticReader`] serves
//!   `SingleTimeSeries`; [`ForecastReader`] serves dense forecasts (one type per
//!   reader, with `Deterministic` abstract over `DeterministicSingleTimeSeries`).
//! * **Columnar batches.** Results are grouped by `(dtype, element_shape)` — the
//!   same partition the packed datasets already use — so each group is a dense
//!   `[num_columns, *element_shape]` typed buffer.
//! * **No presence mask.** A single-resolution reader is built over series that
//!   share one grid (`initial_timestamp` + `length`); [`build_groups`] validates
//!   this and errors on divergence, so every column has a value at every valid
//!   timestamp.
//! * **Buffer reuse.** Each group owns its output buffer; [`Store::static_read`]
//!   overwrites it in place, so a tight read loop allocates nothing per step.
//! * **Off-grid timestamps are a hard error** (see [`StaticReader::index_at`]).
//! * **`NonSequentialTimeSeries` is excluded** (irregular, no resolution).
//!
//! The reader is a *passive plan*: it holds the resolved layout + reusable
//! buffers but does not borrow the [`Store`]. Reads go through
//! [`Store::static_read`], which fills the buffers; afterwards the caller walks
//! [`StaticReader::groups`] to read the bytes. This keeps the type free of
//! self-referential borrows, which matters for the FFI handle.

use std::collections::HashMap;
use std::ops::Range;

use chrono::{DateTime, Utc};

use crate::error::{Result, TimeSeriesError};
use crate::storage::common::window_block_cols;
use crate::types::array::{Dtype, Element};
use crate::types::element_type::ElementType;
use crate::types::key::TimeSeriesKey;
use crate::types::metadata::TimeSeriesMetadata;
use crate::types::period::Period;
use crate::types::time_series::{TimeSeriesType, compute_h};

/// A prepared reader returning the value of every matching `SingleTimeSeries`
/// at one timestamp. Build with [`Store::build_static_reader`], drive with
/// [`Store::static_read`], then read results via [`Self::groups`].
#[derive(Debug)]
pub struct StaticReader {
    /// Shared master grid for every series in the reader.
    initial_timestamp: DateTime<Utc>,
    resolution: Period,
    length: usize,
    /// Columnar groups, in a deterministic order (by dtype code then shape).
    groups: Vec<StaticGroup>,
    /// Timestamp of the last successful [`Store::static_read`], for diagnostics.
    last_read: Option<DateTime<Utc>>,
}

/// One `(dtype, element_shape)` batch. After a read, [`Self::values`] holds a
/// row-major, little-endian `[num_columns, *element_shape]` buffer whose column
/// `j` corresponds to `keys()[j]`.
#[derive(Debug)]
pub struct StaticGroup {
    element_type: ElementType,
    element_shape: Vec<usize>,
    /// Identity of each column, in buffer order. Returned once; stable for the
    /// reader's lifetime.
    keys: Vec<TimeSeriesKey>,
    /// Content hash of each column's array, parallel to `keys`. Drives the read.
    hashes: Vec<[u8; 32]>,
    /// Reused output buffer: `num_columns * element_count * dtype.size()` bytes.
    buf: Vec<u8>,
    /// Whether `buf` holds data from the most recent read.
    filled: bool,
}

impl StaticGroup {
    /// Physical dtype of the group's bytes, derived from [`Self::element_type`].
    pub fn dtype(&self) -> Dtype {
        self.element_type.physical_dtype()
    }

    /// What the group's elements mean. Columns are grouped by this *and*
    /// [`Self::element_shape`], so one group is uniform in both.
    pub fn element_type(&self) -> ElementType {
        self.element_type
    }

    /// Per-step element shape (trailing dims after the column axis); empty =
    /// scalar per series.
    pub fn element_shape(&self) -> &[usize] {
        &self.element_shape
    }

    /// Column identities, in buffer order.
    pub fn keys(&self) -> &[TimeSeriesKey] {
        &self.keys
    }

    pub fn num_columns(&self) -> usize {
        self.keys.len()
    }

    /// Raw `[num_columns, *element_shape]` bytes from the most recent read.
    /// Empty until [`Store::static_read`] has run at least once.
    pub fn values(&self) -> &[u8] {
        if self.filled { &self.buf } else { &[] }
    }

    /// Decode the most-recent read buffer as a `Vec<T>` in row-major
    /// `[num_columns, *element_shape]` order (dtype-checked; errors if `T` is not
    /// the group's dtype). A copy-based decode, so it is sound regardless of
    /// buffer alignment; use [`Self::values`] for the zero-copy byte view. Empty
    /// until [`Store::static_read`] has run at least once.
    pub fn values_to_vec<T: Element>(&self) -> Result<Vec<T>> {
        if self.dtype() != T::DTYPE {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "group dtype is {}, requested {}",
                self.dtype().as_str(),
                T::DTYPE.as_str()
            )));
        }
        Ok(self
            .values()
            .chunks_exact(T::DTYPE.size())
            .map(T::from_le_bytes)
            .collect())
    }

    /// Number of elements per column (product of `element_shape`; 1 for scalar).
    fn elements_per_column(&self) -> usize {
        self.element_shape.iter().product::<usize>().max(1)
    }

    /// Drive a backend read into this group's reusable buffer. The closure
    /// receives the column hashes (in key order), the dtype they share, and the
    /// output buffer to fill; splitting the borrow across the fields lets the
    /// backend read hashes and write the buffer of the *same* group without
    /// aliasing. The group carries the dtype because a backend no longer infers
    /// one — it comes from the catalog, via the group's `element_type`.
    pub(crate) fn fill<F>(&mut self, read: F) -> Result<()>
    where
        F: FnOnce(&[[u8; 32]], Dtype, &mut Vec<u8>) -> Result<()>,
    {
        let dtype = self.element_type.physical_dtype();
        read(&self.hashes, dtype, &mut self.buf)?;
        self.filled = true;
        Ok(())
    }
}

impl StaticReader {
    pub fn initial_timestamp(&self) -> DateTime<Utc> {
        self.initial_timestamp
    }

    pub fn resolution(&self) -> Period {
        self.resolution
    }

    /// Number of timestamps on the grid (`[0, length)`).
    pub fn length(&self) -> usize {
        self.length
    }

    pub fn groups(&self) -> &[StaticGroup] {
        &self.groups
    }

    pub(crate) fn groups_mut(&mut self) -> &mut [StaticGroup] {
        &mut self.groups
    }

    pub(crate) fn mark_read(&mut self, at: DateTime<Utc>) {
        self.last_read = Some(at);
    }

    /// Map a wall-clock timestamp to its 0-based array index on the reader's
    /// grid. Errors (never clamps) if `at` is before the grid start, not aligned
    /// to the resolution, or past the end — the simulation contract is that it
    /// only ever reads valid grid points.
    pub fn index_at(&self, at: DateTime<Utc>) -> Result<usize> {
        index_on_grid(
            self.initial_timestamp,
            self.resolution,
            self.length,
            at,
            "grid",
        )
    }

    /// The wall-clock timestamp at 0-based grid index `index`
    /// (`initial_timestamp + index · resolution`), calendar-aware for a
    /// `Period::Months` grid. The inverse of [`Self::index_at`]. Errors if
    /// `index >= length` or the date arithmetic overflows.
    pub fn timestamp_at(&self, index: usize) -> Result<DateTime<Utc>> {
        timestamp_on_grid(
            self.initial_timestamp,
            self.resolution,
            self.length,
            index,
            "grid",
        )
    }

    /// Iterate every timestamp on the reader's grid, `[0, length)` in order.
    /// The canonical simulation loop is:
    ///
    /// ```no_run
    /// # use infrastore_core::{Store, ListFilter};
    /// # use chrono::Duration;
    /// # fn run(store: &mut Store) -> infrastore_core::Result<()> {
    /// let mut reader = store.build_static_reader(ListFilter::new().resolution(Duration::hours(1)))?;
    /// // Collect first so the iterator's borrow of `reader` ends before the
    /// // `&mut reader` read below.
    /// let timeline: Vec<_> = reader.timestamps().collect();
    /// for t in timeline {
    ///     store.static_read(&mut reader, t)?;
    ///     // ... walk reader.groups() for the columnar bytes at `t` ...
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn timestamps(&self) -> impl Iterator<Item = DateTime<Utc>> + '_ {
        let (initial, resolution) = (self.initial_timestamp, self.resolution);
        (0..self.length).map(move |k| {
            resolution
                .add_to(initial, k as i64)
                .expect("timestamp on the reader grid is representable")
        })
    }
}

/// Map `at` to a 0-based index on a regular grid `initial + k·step` for
/// `k ∈ [0, len)`. Errors (never clamps) if `at` precedes `initial`, is not
/// aligned to `step`, or lands at/after `len`. Shared by [`StaticReader`]
/// (step = resolution, len = length) and [`ForecastReader`] (step = interval,
/// len = window count). `what` names the grid in error messages.
fn index_on_grid(
    initial: DateTime<Utc>,
    step: Period,
    len: usize,
    at: DateTime<Utc>,
    what: &str,
) -> Result<usize> {
    // A single-window forecast may carry a zero interval; its grid has exactly
    // one point, `initial`.
    if step.is_zero() && len == 1 {
        if at == initial {
            return Ok(0);
        }
        return Err(TimeSeriesError::InvalidParameter(format!(
            "timestamp {at} is off the single-point {what} grid at {initial}"
        )));
    }
    // `steps_between` is calendar-aware and rejects off-grid / pre-origin
    // timestamps; here we add the extent bound.
    let idx = step.steps_between(initial, at)?;
    if idx >= len {
        return Err(TimeSeriesError::InvalidParameter(format!(
            "timestamp {at} (index {idx}) is past the {what} extent ({len})"
        )));
    }
    Ok(idx)
}

/// The timestamp at 0-based `index` on a regular grid `initial + index·step`.
/// The inverse of [`index_on_grid`]: bounds-checks `index < len` and errors on
/// date-arithmetic overflow. `what` names the grid in error messages.
fn timestamp_on_grid(
    initial: DateTime<Utc>,
    step: Period,
    len: usize,
    index: usize,
    what: &str,
) -> Result<DateTime<Utc>> {
    if index >= len {
        return Err(TimeSeriesError::InvalidParameter(format!(
            "index {index} is past the {what} extent ({len})"
        )));
    }
    step.add_to(initial, index as i64).ok_or_else(|| {
        TimeSeriesError::InvalidParameter(format!(
            "timestamp at index {index} on the {what} grid is out of range"
        ))
    })
}

/// Resolve a set of `SingleTimeSeries` metadata rows into the reader's master
/// grid plus its `(dtype, element_shape)` groups.
///
/// Pure (no I/O) so it can be unit-tested without a backend. Validates that
/// every row shares one grid; this is what makes the per-read path mask-free.
pub(crate) fn build_groups(
    mut rows: Vec<TimeSeriesMetadata>,
) -> Result<(DateTime<Utc>, Period, usize, Vec<StaticGroup>)> {
    if rows.is_empty() {
        return Err(TimeSeriesError::InvalidParameter(
            "build_static_reader: no SingleTimeSeries match the filter".into(),
        ));
    }

    // Master grid from the first row; every other row must match it.
    let grid = grid_of(&rows[0])?;
    for r in &rows {
        let g = grid_of(r)?;
        if g != grid {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "StaticReader requires a uniform grid; series '{}' (owner {}) has grid {:?} \
                 but the reader grid is {:?}",
                r.name, r.owner_id, g, grid
            )));
        }
    }
    let (initial, resolution, length) = grid;

    // Deterministic layout: order by element type, then element shape, then key
    // identity, so column positions are stable across processes. Grouping on the
    // logical element type rather than the physical dtype keeps a group uniform
    // in meaning as well as in bytes — a quadratic-cost column never shares a
    // group with a plain 3-tuple column that happens to have the same layout.
    rows.sort_by(|a, b| {
        a.element_type
            .cmp(&b.element_type)
            .then_with(|| a.element_shape.cmp(&b.element_shape))
            .then_with(|| identity_sort_key(a).cmp(&identity_sort_key(b)))
    });

    let mut groups: Vec<StaticGroup> = Vec::new();
    for r in rows {
        let want = (r.element_type, r.element_shape.as_slice());
        let push_new = groups
            .last()
            .map(|g| (g.element_type, g.element_shape.as_slice()) != want)
            .unwrap_or(true);
        if push_new {
            groups.push(StaticGroup {
                element_type: r.element_type,
                element_shape: r.element_shape.clone(),
                keys: Vec::new(),
                hashes: Vec::new(),
                buf: Vec::new(),
                filled: false,
            });
        }
        let g = groups.last_mut().expect("group present");
        g.keys.push(TimeSeriesKey::from_metadata(&r)?);
        g.hashes.push(r.data_hash);
    }

    // Pre-size each reuse buffer so the read loop never reallocates.
    for g in &mut groups {
        let bytes = g.num_columns() * g.elements_per_column() * g.dtype().size();
        g.buf = vec![0u8; bytes];
        g.buf.clear();
    }

    Ok((initial, resolution, length, groups))
}

impl StaticReader {
    /// Assemble a reader from a resolved grid + groups. Used by
    /// [`Store::build_static_reader`].
    pub(crate) fn from_parts(
        initial_timestamp: DateTime<Utc>,
        resolution: Period,
        length: usize,
        groups: Vec<StaticGroup>,
    ) -> Self {
        Self {
            initial_timestamp,
            resolution,
            length,
            groups,
            last_read: None,
        }
    }
}

// ---- ForecastReader -------------------------------------------------------

/// A prepared reader returning the forecast window at one timestamp for every
/// matching dense forecast of a single type. Build with
/// [`Store::build_forecast_reader`], drive with [`Store::forecast_read`], then
/// read results via [`Self::entries`].
///
/// Dense forecasts (`Deterministic` / `Probabilistic` / `Scenarios`) are stored
/// as standalone, native-shape arrays, so unlike [`StaticReader`] the result is
/// a **per-key window list**, not columnar batches. One reader covers exactly
/// one forecast type; every entry shares the window timeline
/// (`initial_timestamp`, `interval`, `count`), validated at build — the
/// forecast analog of the static uniform-grid check.
#[derive(Debug)]
pub struct ForecastReader {
    time_series_type: TimeSeriesType,
    initial_timestamp: DateTime<Utc>,
    resolution: Period,
    interval: Period,
    count: usize,
    /// Deduplicated physical reads, one per unique `(array, read plan)`.
    /// [`Store::forecast_read`] fills these — one backend read each.
    slots: Vec<WindowSlot>,
    /// Per-key entries; each indexes into `slots`. Multiple entries may share a
    /// slot when their forecasts reference the same array and read plan.
    entries: Vec<ForecastEntry>,
    last_read: Option<DateTime<Utc>>,
}

/// How an entry's window is read from storage. Dense forecasts slice a
/// standalone array along its count axis; a `DeterministicSingleTimeSeries`
/// gathers a contiguous run from the packed underlying `SingleTimeSeries`, so
/// both yield an identical `[H, *E]` window.
///
/// `(hash, WindowRead)` is the dedup key for [`WindowSlot`]s: two forecasts that
/// reference the same array *and* slice it the same way read byte-identical
/// windows, so they share one physical read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum WindowRead {
    /// Dense forecast: slice the standalone array at the window index along
    /// `count_axis` (1 for Deterministic, 2 for Probabilistic/Scenarios).
    Dense { count_axis: usize },
    /// `DeterministicSingleTimeSeries`: read `horizon_steps` consecutive steps
    /// of the underlying array starting at `window · interval_steps`.
    Derived {
        interval_steps: usize,
        horizon_steps: usize,
    },
}

/// One deduplicated physical window read. After a read, [`Self::window`] holds
/// the row-major, little-endian bytes of a single window and
/// [`Self::window_shape`] its shape: `[H, *E]` (Deterministic /
/// DeterministicSingleTimeSeries), `[P, H, *E]` (Probabilistic),
/// `[scenarios, H, *E]` (Scenarios).
///
/// Every [`ForecastEntry`] whose array and read plan match shares one slot, so
/// [`Store::forecast_read`] reads each slot once regardless of how many
/// components reference it — the forecast analog of a packed static column read
/// once and gathered into many owners.
///
/// For dense forecasts the slot caches one storage chunk's worth of windows (a
/// block of [`Self::block_cols`] consecutive windows, matching the on-disk
/// [`window_block_cols`] chunking). Reads whose window falls in the cached block
/// are served from memory, so sweeping the timeline decompresses each block
/// once instead of once per window.
#[derive(Debug)]
pub struct WindowSlot {
    hash: [u8; 32],
    element_type: ElementType,
    /// Shape of a single window.
    window_shape: Vec<usize>,
    /// How to read this slot's window from storage.
    read: WindowRead,
    /// Total windows on the timeline; bounds the final (short) cache block.
    count: usize,
    /// Windows per cached block for the dense path — the storage chunk width
    /// along the count axis (1 for the derived path, which is not block-cached).
    block_cols: usize,
    /// Window index range currently held in [`Self::block`], if any.
    cached: Option<Range<usize>>,
    /// Raw bytes of the cached block: the array's rows for windows
    /// `cached`, in native (count-axis-interior) row-major order. Dense only.
    block: Vec<u8>,
    /// Reused single-window output buffer: `product(window_shape) * dtype.size()`
    /// bytes, gathered from [`Self::block`] (dense) or read directly (derived).
    buf: Vec<u8>,
    filled: bool,
}

impl WindowSlot {
    /// Physical dtype of the window bytes, derived from [`Self::element_type`].
    pub fn dtype(&self) -> Dtype {
        self.element_type.physical_dtype()
    }

    /// What the window's elements mean.
    pub fn element_type(&self) -> ElementType {
        self.element_type
    }

    pub fn window_shape(&self) -> &[usize] {
        &self.window_shape
    }

    /// Raw window bytes from the most recent read. Empty until
    /// [`Store::forecast_read`] has run at least once.
    pub fn window(&self) -> &[u8] {
        if self.filled { &self.buf } else { &[] }
    }

    /// Decode the most-recent window buffer as a `Vec<T>` in row-major
    /// [`Self::window_shape`] order (dtype-checked; errors if `T` is not the
    /// slot's dtype). Copy-based, so alignment-safe; use [`Self::window`] for the
    /// zero-copy byte view. Empty until [`Store::forecast_read`] has run.
    pub fn window_to_vec<T: Element>(&self) -> Result<Vec<T>> {
        if self.dtype() != T::DTYPE {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "slot dtype is {}, requested {}",
                self.dtype().as_str(),
                T::DTYPE.as_str()
            )));
        }
        Ok(self
            .window()
            .chunks_exact(T::DTYPE.size())
            .map(T::from_le_bytes)
            .collect())
    }

    /// Fill this slot's single-window buffer for window index `window`.
    ///
    /// Dense forecasts read the enclosing chunk block once via `read_block`
    /// (`hash, count_axis, block_start, block_len, out`) and gather the window
    /// from the cache; a repeat read inside the cached block does no I/O.
    /// The derived path reads its contiguous horizon run via `read_range`
    /// (`hash, start, len, out`).
    pub(crate) fn read_window<FBlock, FRange>(
        &mut self,
        window: usize,
        read_block: FBlock,
        read_range: FRange,
    ) -> Result<()>
    where
        FBlock: FnOnce(&[u8; 32], Dtype, usize, usize, usize, &mut Vec<u8>) -> Result<()>,
        FRange: FnOnce(&[u8; 32], Dtype, usize, usize, &mut Vec<u8>) -> Result<()>,
    {
        // The slot's element type comes from the catalog; the backend is told
        // the dtype rather than inferring one from what it stored.
        let dtype = self.element_type.physical_dtype();
        match self.read {
            WindowRead::Dense { count_axis } => {
                let cols = self.block_cols.max(1);
                let start = (window / cols) * cols;
                let end = (start + cols).min(self.count);
                if self.cached.as_ref() != Some(&(start..end)) {
                    read_block(
                        &self.hash,
                        dtype,
                        count_axis,
                        start,
                        end - start,
                        &mut self.block,
                    )?;
                    self.cached = Some(start..end);
                }
                gather_window(
                    &self.block,
                    &self.window_shape,
                    count_axis,
                    end - start,
                    window - start,
                    self.dtype().size(),
                    &mut self.buf,
                );
            }
            WindowRead::Derived {
                interval_steps,
                horizon_steps,
            } => {
                let start = window * interval_steps;
                read_range(&self.hash, dtype, start, horizon_steps, &mut self.buf)?;
            }
        }
        self.filled = true;
        Ok(())
    }
}

/// Gather a single window (local count-axis index `lw`) out of a cached block.
///
/// `block` holds `block_len` windows in the array's native row-major order (the
/// count axis interior at `count_axis`); `window_shape` is one window's shape
/// (the array shape with the count axis removed). The window's rows are strided
/// by `block_len` along `count_axis`, so they are copied one outer index at a
/// time into `out` (cleared first).
fn gather_window(
    block: &[u8],
    window_shape: &[usize],
    count_axis: usize,
    block_len: usize,
    lw: usize,
    elem_size: usize,
    out: &mut Vec<u8>,
) {
    let outer: usize = window_shape[..count_axis].iter().product();
    let inner_bytes: usize = window_shape[count_axis..].iter().product::<usize>() * elem_size;
    out.clear();
    out.reserve(outer * inner_bytes);
    for o in 0..outer {
        let start = (o * block_len + lw) * inner_bytes;
        out.extend_from_slice(&block[start..start + inner_bytes]);
    }
}

/// One forecast's identity, mapped to the [`WindowSlot`] that supplies its
/// window. Many entries can reference the same slot when they share an array and
/// read plan; reach the window bytes via [`ForecastReader::entry_slot`].
#[derive(Debug)]
pub struct ForecastEntry {
    key: TimeSeriesKey,
    /// Index into [`ForecastReader::slots`].
    slot: usize,
}

impl ForecastEntry {
    pub fn key(&self) -> &TimeSeriesKey {
        &self.key
    }

    /// Index of the [`WindowSlot`] backing this entry. Entries that share an
    /// array and read plan return the same index.
    pub fn slot(&self) -> usize {
        self.slot
    }
}

impl ForecastReader {
    pub fn time_series_type(&self) -> TimeSeriesType {
        self.time_series_type
    }

    pub fn initial_timestamp(&self) -> DateTime<Utc> {
        self.initial_timestamp
    }

    pub fn resolution(&self) -> Period {
        self.resolution
    }

    pub fn interval(&self) -> Period {
        self.interval
    }

    /// Number of windows on the forecast timeline (`[0, count)`).
    pub fn count(&self) -> usize {
        self.count
    }

    pub fn entries(&self) -> &[ForecastEntry] {
        &self.entries
    }

    /// The deduplicated window slots — [`Store::forecast_read`] performs exactly
    /// one backend read per slot per timestamp. `slots().len()` is the per-read
    /// I/O count regardless of how many entries reference each slot.
    pub fn slots(&self) -> &[WindowSlot] {
        &self.slots
    }

    pub(crate) fn slots_mut(&mut self) -> &mut [WindowSlot] {
        &mut self.slots
    }

    /// The [`WindowSlot`] backing entry `i`. Panics if `i >= entries().len()`;
    /// callers taking an external index should bounds-check `entries()` first.
    pub fn entry_slot(&self, i: usize) -> &WindowSlot {
        &self.slots[self.entries[i].slot]
    }

    pub(crate) fn mark_read(&mut self, at: DateTime<Utc>) {
        self.last_read = Some(at);
    }

    /// Window index for `at` on the forecast timeline (`initial + k·interval`).
    /// Hard error (never clamps) if off-grid.
    pub fn window_index(&self, at: DateTime<Utc>) -> Result<usize> {
        index_on_grid(
            self.initial_timestamp,
            self.interval,
            self.count,
            at,
            "forecast window",
        )
    }

    /// The initial timestamp of window `index` on the forecast timeline
    /// (`initial_timestamp + index · interval`). The inverse of
    /// [`Self::window_index`]. Errors if `index >= count` or the arithmetic
    /// overflows.
    pub fn timestamp_at(&self, index: usize) -> Result<DateTime<Utc>> {
        timestamp_on_grid(
            self.initial_timestamp,
            self.interval,
            self.count,
            index,
            "forecast window",
        )
    }

    /// Iterate every window start timestamp, `[0, count)` in order — the forecast
    /// analog of [`StaticReader::timestamps`], stepping by `interval`.
    pub fn timestamps(&self) -> impl Iterator<Item = DateTime<Utc>> + '_ {
        let (initial, interval) = (self.initial_timestamp, self.interval);
        (0..self.count).map(move |k| {
            interval
                .add_to(initial, k as i64)
                .expect("timestamp on the forecast timeline is representable")
        })
    }

    pub(crate) fn from_parts(
        time_series_type: TimeSeriesType,
        initial_timestamp: DateTime<Utc>,
        resolution: Period,
        interval: Period,
        count: usize,
        slots: Vec<WindowSlot>,
        entries: Vec<ForecastEntry>,
    ) -> Self {
        Self {
            time_series_type,
            initial_timestamp,
            resolution,
            interval,
            count,
            slots,
            entries,
            last_read: None,
        }
    }
}

/// Whether a stored forecast of concrete type `concrete` belongs in a reader
/// built for `reported` — the shared request rule, so a `Deterministic` reader
/// admits a `DeterministicSingleTimeSeries` (read into an identical `[H, *E]`
/// window) without restating it here.
fn type_accepted(reported: TimeSeriesType, concrete: TimeSeriesType) -> bool {
    reported.accepts(concrete)
}

/// Derive one entry's `(window_shape, read)` from its stored shape, concrete
/// type, and the shared window `count`.
fn entry_layout(
    m: &TimeSeriesMetadata,
    shape: &[usize],
    count: usize,
) -> Result<(Vec<usize>, WindowRead)> {
    let dense = |count_axis: usize| -> Result<(Vec<usize>, WindowRead)> {
        if count_axis >= shape.len() {
            return Err(TimeSeriesError::IntegrityError(format!(
                "forecast '{}' stored shape {shape:?} has no count axis {count_axis}",
                m.name
            )));
        }
        if shape[count_axis] != count {
            return Err(TimeSeriesError::IntegrityError(format!(
                "forecast '{}' window count {count} disagrees with stored axis length {}",
                m.name, shape[count_axis]
            )));
        }
        let window_shape = shape
            .iter()
            .enumerate()
            .filter_map(|(i, &d)| (i != count_axis).then_some(d))
            .collect();
        Ok((window_shape, WindowRead::Dense { count_axis }))
    };

    match m.time_series_type {
        // Stored `[H, count, *E]` / `[P, H, count, *E]` / `[scenarios, H, count, *E]`.
        TimeSeriesType::Deterministic => dense(1),
        TimeSeriesType::Probabilistic | TimeSeriesType::Scenarios => dense(2),
        // Derived from the packed underlying SingleTimeSeries `[total_len, *E]`:
        // window k is the contiguous slice `[k·interval_steps .. +H]`.
        TimeSeriesType::DeterministicSingleTimeSeries => {
            let missing = |f: &str| {
                TimeSeriesError::IntegrityError(format!(
                    "DeterministicSingleTimeSeries '{}' missing {f}",
                    m.name
                ))
            };
            let resolution = m.resolution.ok_or_else(|| missing("resolution"))?;
            let horizon = m.horizon.ok_or_else(|| missing("horizon"))?;
            let interval = m.interval.ok_or_else(|| missing("interval"))?;
            let h = compute_h(horizon, resolution).map_err(TimeSeriesError::IntegrityError)?;
            // A single-window view carries a zero interval; its one window
            // starts at index 0, so the step width is irrelevant.
            let interval_steps = if count == 1 && interval.is_zero() {
                0
            } else {
                resolution.divide_into(&interval).map_err(|_| {
                    TimeSeriesError::IntegrityError(format!(
                        "DeterministicSingleTimeSeries '{}' interval ({}) is not a multiple of \
                         resolution ({})",
                        m.name,
                        interval.to_iso8601(),
                        resolution.to_iso8601()
                    ))
                })?
            };
            let total_len = shape.first().copied().unwrap_or(0);
            let required = count.saturating_sub(1) * interval_steps + h;
            if required > total_len {
                return Err(TimeSeriesError::IntegrityError(format!(
                    "DeterministicSingleTimeSeries '{}' needs {required} underlying steps \
                     ((count-1)·interval_steps + H) but the array has {total_len}",
                    m.name
                )));
            }
            let mut window_shape = vec![h];
            window_shape.extend_from_slice(shape.get(1..).unwrap_or(&[]));
            Ok((
                window_shape,
                WindowRead::Derived {
                    interval_steps,
                    horizon_steps: h,
                },
            ))
        }
        other => Err(TimeSeriesError::InvalidParameter(format!(
            "ForecastReader handles dense forecast types and DeterministicSingleTimeSeries; \
             got {}",
            other.as_str()
        ))),
    }
}

/// `(initial_timestamp, resolution, interval, count)` of a forecast row.
fn forecast_timeline(m: &TimeSeriesMetadata) -> Result<(DateTime<Utc>, Period, Period, usize)> {
    let missing = |f: &str| {
        TimeSeriesError::IntegrityError(format!("{} missing {f}", m.time_series_type.as_str()))
    };
    Ok((
        m.initial_timestamp
            .ok_or_else(|| missing("initial_timestamp"))?,
        m.resolution.ok_or_else(|| missing("resolution"))?,
        m.interval.ok_or_else(|| missing("interval"))?,
        m.count.ok_or_else(|| missing("count"))?,
    ))
}

/// Resolve forecast metadata rows (paired with their stored array shapes) into
/// a [`ForecastReader`] reported as `reported`. Pure (no I/O). Validates a
/// uniform window timeline across all rows (mirroring [`build_groups`] for
/// static) and that each row's concrete type is acceptable for `reported`.
pub(crate) fn build_forecast_entries(
    reported: TimeSeriesType,
    mut items: Vec<(TimeSeriesMetadata, Vec<usize>)>,
) -> Result<ForecastReader> {
    if items.is_empty() {
        return Err(TimeSeriesError::InvalidParameter(
            "build_forecast_reader: no forecasts match the filter".into(),
        ));
    }

    // Deterministic entry order, by key identity.
    items.sort_by(|a, b| identity_sort_key(&a.0).cmp(&identity_sort_key(&b.0)));

    let timeline = forecast_timeline(&items[0].0)?;
    let (_, _, _, count) = timeline;
    let mut slots: Vec<WindowSlot> = Vec::new();
    // Dedup key: forecasts sharing an array *and* read plan read identical
    // windows, so they collapse to one slot (one backend read per timestamp).
    let mut slot_of: HashMap<([u8; 32], WindowRead), usize> = HashMap::new();
    let mut entries = Vec::with_capacity(items.len());
    for (m, shape) in items {
        if !type_accepted(reported, m.time_series_type) {
            return Err(TimeSeriesError::IntegrityError(format!(
                "ForecastReader for {} cannot hold {} '{}'",
                reported.as_str(),
                m.time_series_type.as_str(),
                m.name
            )));
        }
        let tl = forecast_timeline(&m)?;
        if tl != timeline {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "ForecastReader requires a uniform window timeline; forecast '{}' (owner {}) \
                 has timeline {:?} but the reader timeline is {:?}",
                m.name, m.owner_id, tl, timeline
            )));
        }
        // Validate every row's layout, even ones that land on an existing slot.
        let (window_shape, read) = entry_layout(&m, &shape, count)?;
        let slot = *slot_of.entry((m.data_hash, read)).or_insert_with(|| {
            let bytes = window_shape.iter().product::<usize>().max(1)
                * m.element_type.physical_dtype().size();
            let mut buf = vec![0u8; bytes];
            buf.clear();
            // Block-cache the dense path at the storage chunk width so a window
            // sweep decompresses each chunk once; the derived path reads a
            // contiguous run per window and is not block-cached.
            let block_cols = match read {
                WindowRead::Dense { count_axis } => {
                    window_block_cols(m.element_type.physical_dtype(), &shape, count_axis)
                }
                WindowRead::Derived { .. } => 1,
            };
            slots.push(WindowSlot {
                hash: m.data_hash,
                element_type: m.element_type,
                window_shape: window_shape.clone(),
                read,
                count,
                block_cols,
                cached: None,
                block: Vec::new(),
                buf,
                filled: false,
            });
            slots.len() - 1
        });
        entries.push(ForecastEntry {
            key: TimeSeriesKey::from_metadata(&m)?,
            slot,
        });
    }

    let (initial, resolution, interval, count) = timeline;
    Ok(ForecastReader::from_parts(
        reported, initial, resolution, interval, count, slots, entries,
    ))
}

/// `(initial_timestamp, resolution, length)` of a `SingleTimeSeries` row, or an
/// error if a required field is missing or the row is not a `SingleTimeSeries`.
fn grid_of(m: &TimeSeriesMetadata) -> Result<(DateTime<Utc>, Period, usize)> {
    if m.time_series_type != TimeSeriesType::SingleTimeSeries {
        return Err(TimeSeriesError::InvalidParameter(format!(
            "StaticReader handles SingleTimeSeries only; got {}",
            m.time_series_type.as_str()
        )));
    }
    let initial = m.initial_timestamp.ok_or_else(|| {
        TimeSeriesError::IntegrityError("SingleTimeSeries missing initial_timestamp".into())
    })?;
    let resolution = m.resolution.ok_or_else(|| {
        TimeSeriesError::IntegrityError("SingleTimeSeries missing resolution".into())
    })?;
    let length = m
        .length
        .ok_or_else(|| TimeSeriesError::IntegrityError("SingleTimeSeries missing length".into()))?;
    Ok((initial, resolution, length))
}

/// A cheap, total ordering key for deterministic column layout within a group.
fn identity_sort_key(m: &TimeSeriesMetadata) -> (i64, &'static str, &str) {
    (m.owner_id, m.owner_category.as_str(), m.name.as_str())
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use std::collections::HashMap;

    use crate::store::{ListFilter, Store};
    use crate::types::array::{Dtype, TypedArray};
    use crate::types::metadata::OwnerCategory;
    use crate::types::time_series::{
        Deterministic, Probabilistic, Scenarios, SingleTimeSeries, TimeSeriesData,
    };

    use super::*;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
    }

    fn add(store: &mut Store, owner_id: i64, name: &str, data: TypedArray) {
        let ts = SingleTimeSeries::new(t0(), Duration::hours(1), data, name);
        store
            .add_time_series(
                owner_id,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(ts),
                Default::default(),
                None,
            )
            .unwrap();
    }

    fn add_f64(store: &mut Store, owner_id: i64, name: &str, vals: &[f64]) {
        add(
            store,
            owner_id,
            name,
            TypedArray::from_f64(vec![vals.len()], vals),
        );
    }

    fn add_f64_nd(store: &mut Store, owner_id: i64, name: &str, shape: Vec<usize>, vals: &[f64]) {
        add(store, owner_id, name, TypedArray::from_f64(shape, vals));
    }

    fn add_i64(store: &mut Store, owner_id: i64, name: &str, vals: &[i64]) {
        let mut bytes = Vec::new();
        for v in vals {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let data = TypedArray::new(Dtype::I64, vec![vals.len()], bytes).unwrap();
        add(store, owner_id, name, data);
    }

    fn f64_cols(group: &StaticGroup) -> Vec<f64> {
        group
            .values()
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    #[test]
    fn reads_columnar_values_at_a_timestamp() {
        let mut store = Store::create(None, true).unwrap();
        add_f64(&mut store, 2, "load", &[20.0, 21.0, 22.0, 23.0]);
        add_f64(&mut store, 1, "load", &[10.0, 11.0, 12.0, 13.0]);
        add_i64(&mut store, 3, "count", &[100, 101, 102, 103]);

        let mut reader = store
            .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
            .unwrap();

        assert_eq!(reader.length(), 4);
        assert_eq!(reader.groups().len(), 2, "one f64 group, one i64 group");

        // Read at t0 + 2h -> index 2.
        store
            .static_read(&mut reader, t0() + Duration::hours(2))
            .unwrap();

        // Group 0: f64, columns ordered by owner_id (1 then 2).
        let g0 = &reader.groups()[0];
        assert_eq!(g0.dtype(), Dtype::F64);
        assert_eq!(g0.num_columns(), 2);
        assert_eq!(f64_cols(g0), vec![12.0, 22.0]);
        assert_eq!(g0.keys()[0].owner_id(), 1);
        assert_eq!(g0.keys()[1].owner_id(), 2);

        // Group 1: i64, single column.
        let g1 = &reader.groups()[1];
        assert_eq!(g1.dtype(), Dtype::I64);
        assert_eq!(g1.num_columns(), 1);
        let v = i64::from_le_bytes(g1.values().try_into().unwrap());
        assert_eq!(v, 102);

        // Buffers reuse: a second read at a different index overwrites in place.
        store
            .static_read(&mut reader, t0() + Duration::hours(3))
            .unwrap();
        assert_eq!(f64_cols(&reader.groups()[0]), vec![13.0, 23.0]);
    }

    #[test]
    fn off_grid_timestamp_is_an_error() {
        let mut store = Store::create(None, true).unwrap();
        add_f64(&mut store, 1, "load", &[10.0, 11.0, 12.0, 13.0]);
        let mut reader = store
            .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
            .unwrap();

        // Not aligned to the resolution.
        assert!(
            store
                .static_read(&mut reader, t0() + Duration::minutes(30))
                .is_err()
        );
        // Past the end (length 4 -> valid indices 0..=3).
        assert!(
            store
                .static_read(&mut reader, t0() + Duration::hours(4))
                .is_err()
        );
        // Before the start.
        assert!(
            store
                .static_read(&mut reader, t0() - Duration::hours(1))
                .is_err()
        );
    }

    #[test]
    fn divergent_grid_is_rejected_at_build() {
        let mut store = Store::create(None, true).unwrap();
        add_f64(&mut store, 1, "load", &[10.0, 11.0, 12.0, 13.0]);
        // Same resolution, different length -> divergent grid.
        add_f64(&mut store, 2, "load", &[20.0, 21.0]);
        let err = store
            .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
            .unwrap_err();
        assert!(matches!(err, TimeSeriesError::InvalidParameter(_)));
    }

    /// Populate `store` with the same mixed-dtype, mixed-shape set used to
    /// exercise both the default and on-disk read paths.
    fn populate(store: &mut Store) {
        add_f64(store, 2, "load", &[20.0, 21.0, 22.0, 23.0]);
        add_f64(store, 1, "load", &[10.0, 11.0, 12.0, 13.0]);
        add_i64(store, 3, "count", &[100, 101, 102, 103]);
        // f64 with a 2-element per-step shape -> its own group. Step t = [t0+.., ..].
        add_f64_nd(
            store,
            5,
            "pair",
            vec![4, 2],
            &[0.0, 0.5, 10.0, 10.5, 20.0, 20.5, 30.0, 30.5],
        );
    }

    /// On-disk store: drives `Hdf5Backend::read_index_into` (the one-hyperslab-
    /// per-dataset override) and cross-checks it byte-for-byte against the
    /// in-memory backend (the default per-hash path).
    #[test]
    fn disk_override_matches_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.h5");

        let mut disk = Store::create(Some(&path), false).unwrap();
        populate(&mut disk);
        let mut mem = Store::create(None, true).unwrap();
        populate(&mut mem);

        let filter = || ListFilter::new().resolution(Duration::hours(1));
        let mut r_disk = disk.build_static_reader(filter()).unwrap();
        let mut r_mem = mem.build_static_reader(filter()).unwrap();

        // Layout must be identical across backends.
        assert_eq!(r_disk.groups().len(), 3);
        assert_eq!(r_mem.groups().len(), 3);

        for idx in 0..4u32 {
            let at = t0() + Duration::hours(idx as i64);
            disk.static_read(&mut r_disk, at).unwrap();
            mem.static_read(&mut r_mem, at).unwrap();
            for (gd, gm) in r_disk.groups().iter().zip(r_mem.groups()) {
                assert_eq!(gd.dtype(), gm.dtype());
                assert_eq!(gd.element_shape(), gm.element_shape());
                assert_eq!(gd.keys(), gm.keys());
                assert_eq!(gd.values(), gm.values(), "mismatch at index {idx}");
            }
        }

        // Spot-check concrete values at index 2 on the on-disk (override) reader.
        disk.static_read(&mut r_disk, t0() + Duration::hours(2))
            .unwrap();
        // Group 0: f64 scalar, owners 1 then 2.
        assert_eq!(r_disk.groups()[0].element_shape(), &[] as &[usize]);
        assert_eq!(f64_cols(&r_disk.groups()[0]), vec![12.0, 22.0]);
        // Group 1: f64 shape [2], owner 5 -> step 2 = [20.0, 20.5].
        assert_eq!(r_disk.groups()[1].element_shape(), &[2]);
        assert_eq!(f64_cols(&r_disk.groups()[1]), vec![20.0, 20.5]);
        // Group 2: i64 scalar, owner 3.
        let g2 = &r_disk.groups()[2];
        assert_eq!(g2.dtype(), Dtype::I64);
        assert_eq!(i64::from_le_bytes(g2.values().try_into().unwrap()), 102);
    }

    /// After removals the surviving series sit at non-contiguous, higher column
    /// indices. Exercises the bounded row read (`width = max_col + 1`) and the
    /// per-column gather offsets for a non-zero column.
    #[test]
    fn override_handles_noncontiguous_high_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.h5");
        let mut store = Store::create(Some(&path), false).unwrap();

        // Four f64 scalar series -> columns 0,1,2,3 in add order.
        add_f64(&mut store, 1, "load", &[10.0, 11.0, 12.0, 13.0]); // col 0
        add_f64(&mut store, 2, "load", &[20.0, 21.0, 22.0, 23.0]); // col 1
        add_f64(&mut store, 3, "load", &[30.0, 31.0, 32.0, 33.0]); // col 2
        add_f64(&mut store, 4, "load", &[40.0, 41.0, 42.0, 43.0]); // col 3

        // Remove owners 2 and 3 -> survivors keep columns 0 and 3 (gap + high col).
        let keys = store.list_keys(ListFilter::new()).unwrap();
        for k in &keys {
            if k.owner_id() == 2 || k.owner_id() == 3 {
                store.remove_time_series(k.identity()).unwrap();
            }
        }

        let mut reader = store
            .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
            .unwrap();
        assert_eq!(reader.groups().len(), 1);
        assert_eq!(reader.groups()[0].num_columns(), 2);

        store
            .static_read(&mut reader, t0() + Duration::hours(2))
            .unwrap();
        let g = &reader.groups()[0];
        assert_eq!(g.keys()[0].owner_id(), 1);
        assert_eq!(g.keys()[1].owner_id(), 4);
        // col 0 @ idx 2 -> 12.0; col 3 @ idx 2 -> 42.0.
        assert_eq!(f64_cols(g), vec![12.0, 42.0]);
    }

    // ---- ForecastReader ---------------------------------------------------

    fn f64_window(s: &WindowSlot) -> Vec<f64> {
        s.window()
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    /// Add a dense `Deterministic` forecast: resolution 1h, horizon = `h` hours,
    /// interval 1h, `count` windows. `vals` is the row-major `[h, count, *E]`
    /// array.
    fn add_det(
        store: &mut Store,
        owner_id: i64,
        name: &str,
        h: usize,
        count: usize,
        element_shape: Vec<usize>,
        vals: &[f64],
    ) {
        let mut shape = vec![h, count];
        shape.extend_from_slice(&element_shape);
        let data = TypedArray::from_f64(shape, vals);
        let det = Deterministic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(h as i64),
            Duration::hours(1),
            count,
            data,
            name,
        )
        .unwrap();
        store
            .add_time_series(
                owner_id,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::Deterministic(det),
                Default::default(),
                None,
            )
            .unwrap();
    }

    fn forecast_filter() -> ListFilter {
        ListFilter::new()
            .time_series_type(TimeSeriesType::Deterministic)
            .resolution(Duration::hours(1))
    }

    /// On-disk forecast reader (drives the HDF5 hyperslab window read) cross-
    /// checked byte-for-byte against the in-memory default, plus concrete value
    /// assertions including a multi-dimensional per-step shape.
    #[test]
    fn forecast_reader_reads_windows() {
        // Scalar forecast: H=2, count=3. Row-major [s, k]; value = k*10 + s.
        let scalar = [0.0, 10.0, 20.0, 1.0, 11.0, 21.0];
        // Shaped forecast: H=2, count=3, E=[2]. Row-major [s, k, e].
        let shaped = [
            100.0, 101.0, 110.0, 111.0, 120.0, 121.0, // s=0
            200.0, 201.0, 210.0, 211.0, 220.0, 221.0, // s=1
        ];
        let populate = |store: &mut Store| {
            add_det(store, 1, "gen", 2, 3, vec![], &scalar);
            add_det(store, 2, "gen", 2, 3, vec![2], &shaped);
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.h5");
        let mut disk = Store::create(Some(&path), false).unwrap();
        populate(&mut disk);
        let mut mem = Store::create(None, true).unwrap();
        populate(&mut mem);

        let mut rd = disk.build_forecast_reader(forecast_filter()).unwrap();
        let mut rm = mem.build_forecast_reader(forecast_filter()).unwrap();
        assert_eq!(rd.entries().len(), 2);
        assert_eq!(rd.count(), 3);
        assert_eq!(rd.time_series_type(), TimeSeriesType::Deterministic);

        for k in 0..3u32 {
            let at = t0() + Duration::hours(k as i64);
            disk.forecast_read(&mut rd, at).unwrap();
            mem.forecast_read(&mut rm, at).unwrap();
            for i in 0..rd.entries().len() {
                assert_eq!(rd.entries()[i].key(), rm.entries()[i].key());
                assert_eq!(
                    rd.entry_slot(i).window_shape(),
                    rm.entry_slot(i).window_shape()
                );
                assert_eq!(
                    rd.entry_slot(i).window(),
                    rm.entry_slot(i).window(),
                    "mismatch at window {k}"
                );
            }
        }

        // Window at index 1 (t0 + 1h) on the on-disk (override) reader.
        disk.forecast_read(&mut rd, t0() + Duration::hours(1))
            .unwrap();
        // Entry 0: scalar, owner 1 -> window k=1 = [value(1,0), value(1,1)] = [10, 11].
        let s0 = rd.entry_slot(0);
        assert_eq!(rd.entries()[0].key().owner_id(), 1);
        assert_eq!(s0.window_shape(), &[2]); // [H]
        assert_eq!(f64_window(s0), vec![10.0, 11.0]);
        // Entry 1: shaped, owner 2 -> window k=1, shape [H, E] = [2, 2].
        let s1 = rd.entry_slot(1);
        assert_eq!(rd.entries()[1].key().owner_id(), 2);
        assert_eq!(s1.window_shape(), &[2, 2]);
        assert_eq!(f64_window(s1), vec![110.0, 111.0, 210.0, 211.0]);
    }

    /// The dense window cache reads its enclosing chunk block once and serves
    /// every window in it from memory: sweeping a `[H=2, count=5]` array with
    /// `block_cols = 2` hits the backend three times (blocks `[0,2) [2,4) [4,5)`),
    /// re-reading a cached window does no I/O, and every gathered window is
    /// correct across block boundaries.
    #[test]
    fn dense_window_cache_reads_each_block_once() {
        use std::cell::Cell;

        let (h, count, block_cols) = (2usize, 5usize, 2usize);
        // Model array value[hi][k] = k*10 + hi (native `[H, count]` row-major).
        let mut slot = WindowSlot {
            hash: [0u8; 32],
            element_type: ElementType::Scalar(Dtype::F64),
            window_shape: vec![h], // count axis removed
            read: WindowRead::Dense { count_axis: 1 },
            count,
            block_cols,
            cached: None,
            block: Vec::new(),
            buf: Vec::new(),
            filled: false,
        };
        let reads = Cell::new(0usize);
        let range_unused =
            |_: &[u8; 32], _: Dtype, _: usize, _: usize, _: &mut Vec<u8>| -> Result<()> {
                unreachable!()
            };

        for k in 0..count {
            slot.read_window(
                k,
                |_hash, _dtype, _axis, start, len, out| {
                    reads.set(reads.get() + 1);
                    out.clear();
                    // Emit the `[H, len]` block in native row-major order.
                    for hi in 0..h {
                        for kk in start..start + len {
                            out.extend_from_slice(&((kk * 10 + hi) as f64).to_le_bytes());
                        }
                    }
                    Ok(())
                },
                range_unused,
            )
            .unwrap();
            assert_eq!(
                f64_window(&slot),
                vec![(k * 10) as f64, (k * 10 + 1) as f64],
                "window {k}"
            );
        }
        assert_eq!(reads.get(), 3, "one backend read per 2-window block over 5");

        // Re-reading a window inside the currently cached block does no I/O.
        slot.read_window(
            count - 1,
            |_, _, _, _, _, _| -> Result<()> { panic!("cached block must not re-read") },
            range_unused,
        )
        .unwrap();
        assert_eq!(reads.get(), 3);
    }

    /// Components sharing one forecast array dedup to a single [`WindowSlot`], so
    /// a timestamp read hits the backend once no matter how many components
    /// reference it — while each component still resolves to its own (identical)
    /// window. This is the forecast analog of the static packed-column read.
    #[test]
    fn shared_forecast_reads_once_per_timestamp() {
        // Scalar forecast H=2, count=3; row-major [s, k], value = k*10 + s.
        let scalar = [0.0, 10.0, 20.0, 1.0, 11.0, 21.0];
        // A distinct array (offset by 5) for the non-shared owner.
        let other = [5.0, 15.0, 25.0, 6.0, 16.0, 26.0];

        let mut store = Store::create(None, true).unwrap();
        // Owners 1..=3 add byte-identical data (content-addressed -> one array);
        // owner 4 is distinct.
        add_det(&mut store, 1, "gen", 2, 3, vec![], &scalar);
        add_det(&mut store, 2, "gen", 2, 3, vec![], &scalar);
        add_det(&mut store, 3, "gen", 2, 3, vec![], &scalar);
        add_det(&mut store, 4, "gen", 2, 3, vec![], &other);

        let mut reader = store.build_forecast_reader(forecast_filter()).unwrap();
        // Four components, two unique arrays: four entries, two physical reads.
        assert_eq!(reader.entries().len(), 4);
        assert_eq!(
            reader.slots().len(),
            2,
            "shared forecast collapses to one slot per unique array"
        );

        store
            .forecast_read(&mut reader, t0() + Duration::hours(1))
            .unwrap();

        // Entries 0..=2 (owners 1-3) share one slot and identical window bytes.
        let shared_slot = reader.entries()[0].slot();
        let shared_window = reader.entry_slot(0).window().to_vec();
        for i in 0..3 {
            assert_eq!(reader.entries()[i].slot(), shared_slot);
            assert_eq!(reader.entry_slot(i).window(), shared_window.as_slice());
        }
        // Window k=1 of the shared array = [value(0,1), value(1,1)] = [10, 11].
        assert_eq!(f64_window(reader.entry_slot(0)), vec![10.0, 11.0]);

        // Owner 4 is a distinct slot with its own data (window k=1 = [15, 16]).
        assert_ne!(reader.entries()[3].slot(), shared_slot);
        assert_eq!(f64_window(reader.entry_slot(3)), vec![15.0, 16.0]);
    }

    #[test]
    fn forecast_reader_build_validation() {
        let mut store = Store::create(None, true).unwrap();
        add_det(&mut store, 1, "gen", 2, 3, vec![], &[0.0; 6]);

        // No forecast type in the filter.
        assert!(
            store
                .build_forecast_reader(ListFilter::new().resolution(Duration::hours(1)))
                .is_err()
        );
        // A static type is rejected.
        assert!(
            store
                .build_forecast_reader(
                    ListFilter::new()
                        .time_series_type(TimeSeriesType::SingleTimeSeries)
                        .resolution(Duration::hours(1)),
                )
                .is_err()
        );
        // Missing resolution.
        assert!(
            store
                .build_forecast_reader(
                    ListFilter::new().time_series_type(TimeSeriesType::Deterministic)
                )
                .is_err()
        );
    }

    #[test]
    fn forecast_reader_off_grid_is_an_error() {
        let mut store = Store::create(None, true).unwrap();
        add_det(&mut store, 1, "gen", 2, 3, vec![], &[0.0; 6]);
        let mut reader = store.build_forecast_reader(forecast_filter()).unwrap();
        // Unaligned to the interval.
        assert!(
            store
                .forecast_read(&mut reader, t0() + Duration::minutes(30))
                .is_err()
        );
        // Past the last window (count 3 -> valid 0..=2).
        assert!(
            store
                .forecast_read(&mut reader, t0() + Duration::hours(3))
                .is_err()
        );
    }

    /// A Deterministic reader is abstract: it includes DeterministicSingleTime-
    /// Series, read into windows byte-identical to a real Deterministic with the
    /// same windows. DST window k is the contiguous underlying slice
    /// `[k·interval_steps .. +H]`. Cross-checked disk vs memory.
    #[test]
    fn deterministic_reader_includes_dst_identically() {
        // Underlying SingleTimeSeries: total_len 6, resolution 1h.
        let sts = [10.0, 11.0, 12.0, 13.0, 14.0, 15.0];
        // Real Deterministic whose windows match the DST: value[s, k] = sts[k+s].
        // Shape [H=2, count=5], row-major [s, k].
        let det = [
            10.0, 11.0, 12.0, 13.0, 14.0, // s=0
            11.0, 12.0, 13.0, 14.0, 15.0, // s=1
        ];
        let populate = |store: &mut Store| {
            add_f64(store, 1, "load", &sts);
            add_det(store, 2, "gen", 2, 5, vec![], &det);
            // Derive a DST view for owner 1 from its SingleTimeSeries.
            store
                .transform_single_time_series(Duration::hours(2), Duration::hours(1), None, None)
                .unwrap();
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.h5");
        let mut disk = Store::create(Some(&path), false).unwrap();
        populate(&mut disk);
        let mut mem = Store::create(None, true).unwrap();
        populate(&mut mem);

        let mut rd = disk.build_forecast_reader(forecast_filter()).unwrap();
        let mut rm = mem.build_forecast_reader(forecast_filter()).unwrap();

        // DST (owner 1) + Deterministic (owner 2); the SingleTimeSeries is excluded.
        assert_eq!(rd.entries().len(), 2);
        assert_eq!(rd.count(), 5);
        assert_eq!(
            rd.entries()[0].key().time_series_type(),
            TimeSeriesType::DeterministicSingleTimeSeries
        );
        assert_eq!(
            rd.entries()[1].key().time_series_type(),
            TimeSeriesType::Deterministic
        );

        for k in 0..5u32 {
            let at = t0() + Duration::hours(k as i64);
            disk.forecast_read(&mut rd, at).unwrap();
            mem.forecast_read(&mut rm, at).unwrap();
            let dst = rd.entry_slot(0);
            let det_entry = rd.entry_slot(1);
            // DST read identically to Deterministic: same shape and bytes.
            assert_eq!(dst.window_shape(), &[2]);
            assert_eq!(dst.window_shape(), det_entry.window_shape());
            assert_eq!(
                dst.window(),
                det_entry.window(),
                "DST != Deterministic at window {k}"
            );
            // Concrete expectation: window k = [sts[k], sts[k+1]].
            assert_eq!(f64_window(dst), vec![sts[k as usize], sts[k as usize + 1]]);
            // On-disk (packed underlying hyperslab) == in-memory default.
            assert_eq!(dst.window(), rm.entry_slot(0).window());
            assert_eq!(det_entry.window(), rm.entry_slot(1).window());
        }
    }

    // ---- Oracle cross-checks against get_time_series ----------------------
    //
    // The strongest correctness check: a reader's bytes must equal what the
    // independent, separately-tested `get_time_series` path returns for the
    // same series at the same timestamp / window. These run on-disk so they
    // exercise the HDF5 hyperslab overrides.

    /// Encode `vals` into `dtype`'s little-endian bytes for a typed array.
    fn typed_from(dtype: Dtype, shape: Vec<usize>, vals: &[f64]) -> TypedArray {
        let mut bytes = Vec::new();
        for &v in vals {
            match dtype {
                Dtype::F64 => bytes.extend_from_slice(&v.to_le_bytes()),
                Dtype::F32 => bytes.extend_from_slice(&(v as f32).to_le_bytes()),
                Dtype::I64 => bytes.extend_from_slice(&(v as i64).to_le_bytes()),
                Dtype::I32 => bytes.extend_from_slice(&(v as i32).to_le_bytes()),
                Dtype::I16 => bytes.extend_from_slice(&(v as i16).to_le_bytes()),
                Dtype::I8 => bytes.extend_from_slice(&(v as i8).to_le_bytes()),
                Dtype::U64 => bytes.extend_from_slice(&(v as u64).to_le_bytes()),
                Dtype::U32 => bytes.extend_from_slice(&(v as u32).to_le_bytes()),
                Dtype::U16 => bytes.extend_from_slice(&(v as u16).to_le_bytes()),
                Dtype::U8 => bytes.extend_from_slice(&(v as u8).to_le_bytes()),
                Dtype::Bool => bytes.push(if v != 0.0 { 1 } else { 0 }),
            }
        }
        TypedArray::new(dtype, shape, bytes).unwrap()
    }

    fn distinct(base: f64, n: usize) -> Vec<f64> {
        (0..n).map(|k| base + k as f64).collect()
    }

    /// Every dtype × element-shape × timestamp: static reader bytes must equal
    /// `get_time_series` (full series, indexed at the same step).
    #[test]
    fn static_reader_matches_get_time_series_all_dtypes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.h5");
        let mut store = Store::create(Some(&path), false).unwrap();
        let length = 5usize;
        let res = Duration::hours(1);
        let dtypes = [
            Dtype::F64,
            Dtype::F32,
            Dtype::I64,
            Dtype::I32,
            Dtype::U64,
            Dtype::Bool,
        ];
        let shapes: [Vec<usize>; 3] = [vec![], vec![2], vec![2, 3]];
        let mut owner = 1i64;
        for &dt in &dtypes {
            for esh in &shapes {
                let ecount: usize = esh.iter().product::<usize>().max(1);
                // Distinct values per (timestep, element) to catch any stride bug.
                let vals = distinct(owner as f64 * 1000.0, length * ecount);
                let mut shape = vec![length];
                shape.extend_from_slice(esh);
                let ts = SingleTimeSeries::new(t0(), res, typed_from(dt, shape, &vals), "v");
                store
                    .add_time_series(
                        owner,
                        "Gen",
                        OwnerCategory::Component,
                        TimeSeriesData::SingleTimeSeries(ts),
                        Default::default(),
                        None,
                    )
                    .unwrap();
                owner += 1;
            }
        }

        let mut reader = store
            .build_static_reader(ListFilter::new().resolution(res))
            .unwrap();
        // Oracle: full series bytes per owner, via get_time_series.
        let mut full: HashMap<i64, Vec<u8>> = HashMap::new();
        for g in reader.groups() {
            for k in g.keys() {
                match store.get_time_series(k.identity(), None).unwrap() {
                    TimeSeriesData::SingleTimeSeries(s) => {
                        full.insert(k.owner_id(), s.data.bytes);
                    }
                    other => panic!("expected SingleTimeSeries, got {other:?}"),
                }
            }
        }
        // 6 dtypes × 3 shapes = 18 columns spread over groups.
        assert_eq!(
            reader
                .groups()
                .iter()
                .map(|g| g.num_columns())
                .sum::<usize>(),
            18
        );

        for i in 0..length {
            store
                .static_read(&mut reader, t0() + res * (i as i32))
                .unwrap();
            for g in reader.groups() {
                let eb = g.element_shape().iter().product::<usize>().max(1) * g.dtype().size();
                let vals = g.values();
                for (j, k) in g.keys().iter().enumerate() {
                    let col = &vals[j * eb..(j + 1) * eb];
                    let oracle = &full[&k.owner_id()];
                    assert_eq!(
                        col,
                        &oracle[i * eb..(i + 1) * eb],
                        "owner {} dtype {:?} shape {:?} index {i}",
                        k.owner_id(),
                        g.dtype(),
                        g.element_shape()
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_prob(
        store: &mut Store,
        owner: i64,
        name: &str,
        h: usize,
        count: usize,
        p: usize,
        eshape: Vec<usize>,
        vals: &[f64],
    ) {
        let mut shape = vec![p, h, count];
        shape.extend_from_slice(&eshape);
        let percentiles: Vec<f64> = (1..=p).map(|i| i as f64 / (p as f64 + 1.0)).collect();
        let prob = Probabilistic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(h as i64),
            Duration::hours(1),
            count,
            percentiles,
            TypedArray::from_f64(shape, vals),
            name,
        )
        .unwrap();
        store
            .add_time_series(
                owner,
                "Gen",
                OwnerCategory::Component,
                TimeSeriesData::Probabilistic(prob),
                Default::default(),
                None,
            )
            .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn add_scen(
        store: &mut Store,
        owner: i64,
        name: &str,
        h: usize,
        count: usize,
        sc: usize,
        eshape: Vec<usize>,
        vals: &[f64],
    ) {
        let mut shape = vec![sc, h, count];
        shape.extend_from_slice(&eshape);
        let scen = Scenarios::new(
            t0(),
            Duration::hours(1),
            Duration::hours(h as i64),
            Duration::hours(1),
            count,
            sc,
            TypedArray::from_f64(shape, vals),
            name,
        )
        .unwrap();
        store
            .add_time_series(
                owner,
                "Gen",
                OwnerCategory::Component,
                TimeSeriesData::Scenarios(scen),
                Default::default(),
                None,
            )
            .unwrap();
    }

    fn forecast_bytes(d: &TimeSeriesData) -> Vec<u8> {
        match d {
            TimeSeriesData::Deterministic(x) => x.data.bytes.clone(),
            TimeSeriesData::Probabilistic(x) => x.data.bytes.clone(),
            TimeSeriesData::Scenarios(x) => x.data.bytes.clone(),
            other => panic!("not a forecast: {other:?}"),
        }
    }

    /// Every forecast type (Deterministic, Probabilistic, Scenarios, DST) and
    /// every window: reader window bytes must equal a single-window
    /// `get_time_series` (whose count axis is length 1, so its bytes are exactly
    /// the window). Covers the count-axis-2 layouts (Prob/Scen) and multidim E.
    #[test]
    fn forecast_reader_matches_get_time_series_all_types() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.h5");
        let mut store = Store::create(Some(&path), false).unwrap();
        let res = Duration::hours(1);
        let ivl = Duration::hours(1);
        let (h, count) = (2usize, 4usize);

        add_det(
            &mut store,
            1,
            "d0",
            h,
            count,
            vec![],
            &distinct(1000.0, h * count),
        );
        add_det(
            &mut store,
            2,
            "d1",
            h,
            count,
            vec![2],
            &distinct(2000.0, h * count * 2),
        );
        add_prob(
            &mut store,
            3,
            "p0",
            h,
            count,
            3,
            vec![],
            &distinct(3000.0, 3 * h * count),
        );
        add_prob(
            &mut store,
            4,
            "p1",
            h,
            count,
            3,
            vec![2],
            &distinct(4000.0, 3 * h * count * 2),
        );
        add_scen(
            &mut store,
            5,
            "sc0",
            h,
            count,
            2,
            vec![],
            &distinct(5000.0, 2 * h * count),
        );
        add_scen(
            &mut store,
            6,
            "sc1",
            h,
            count,
            2,
            vec![2],
            &distinct(6000.0, 2 * h * count * 2),
        );
        // DST owner 7: underlying length (count-1)*interval_steps + H = 3*1 + 2 = 5.
        add_f64(&mut store, 7, "load", &distinct(7000.0, 5));
        store
            .transform_single_time_series(Duration::hours(h as i64), ivl, None, None)
            .unwrap();

        for ts_type in [
            TimeSeriesType::Deterministic, // abstract -> Deterministic + DST
            TimeSeriesType::Probabilistic,
            TimeSeriesType::Scenarios,
        ] {
            let mut reader = store
                .build_forecast_reader(ListFilter::new().time_series_type(ts_type).resolution(res))
                .unwrap();
            assert!(!reader.entries().is_empty());
            for w in 0..count {
                let t_w = t0() + ivl * (w as i32);
                store.forecast_read(&mut reader, t_w).unwrap();
                for i in 0..reader.entries().len() {
                    let key = reader.entries()[i].key();
                    let window = store
                        .get_time_series(key.identity(), Some((t_w, t_w + ivl)))
                        .unwrap();
                    assert_eq!(
                        reader.entry_slot(i).window(),
                        forecast_bytes(&window).as_slice(),
                        "type {ts_type:?} owner {} window {w}",
                        key.owner_id()
                    );
                }
            }
        }
    }

    /// More than DEFAULT_COLS_PER_DATASET series in one group spill into a second
    /// packed dataset; the row read must gather across the dataset boundary.
    /// Single `add_time_series` calls take the per-column path, which packs into a
    /// shared default-width dataset and spills once full (a managed bulk batch
    /// would instead size one dataset to the batch).
    #[test]
    fn static_reader_spans_spilled_datasets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spill.h5");
        let mut store = Store::create(Some(&path), false).unwrap();
        let res = Duration::hours(1);
        let length = 3usize;
        let n = 1001i64; // > DEFAULT_COLS_PER_DATASET (1000) -> two datasets
        for owner in 1..=n {
            let vals = distinct(owner as f64 * 10.0, length);
            let ts =
                SingleTimeSeries::new(t0(), res, TypedArray::from_f64(vec![length], &vals), "v");
            store
                .add_time_series(
                    owner,
                    "Gen",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(ts),
                    Default::default(),
                    None,
                )
                .unwrap();
        }

        let mut reader = store
            .build_static_reader(ListFilter::new().resolution(res))
            .unwrap();
        assert_eq!(reader.groups().len(), 1);
        assert_eq!(reader.groups()[0].num_columns(), n as usize);

        for i in 0..length {
            store
                .static_read(&mut reader, t0() + res * (i as i32))
                .unwrap();
            let g = &reader.groups()[0];
            let vals = g.values();
            for (j, k) in g.keys().iter().enumerate() {
                let got = f64::from_le_bytes(vals[j * 8..j * 8 + 8].try_into().unwrap());
                assert_eq!(got, k.owner_id() as f64 * 10.0 + i as f64);
            }
        }
    }

    #[test]
    fn timestamps_iterator_agrees_with_index_at() {
        let mut store = Store::create(None, true).unwrap();
        add_f64(&mut store, 1, "load", &[10.0, 11.0, 12.0, 13.0]);
        let reader = store
            .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
            .unwrap();

        let stamps: Vec<_> = reader.timestamps().collect();
        assert_eq!(stamps.len(), 4);
        for (i, t) in stamps.iter().enumerate() {
            assert_eq!(reader.index_at(*t).unwrap(), i);
            assert_eq!(reader.timestamp_at(i).unwrap(), *t);
        }
        // Out-of-bounds index errors.
        assert!(reader.timestamp_at(4).is_err());
    }

    #[test]
    fn timestamps_iterator_on_months_grid() {
        let mut store = Store::create(None, true).unwrap();
        // 3-month calendar grid from t0.
        let ts = SingleTimeSeries::new(
            t0(),
            Period::Months(1),
            TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]),
            "monthly",
        );
        store
            .add_time_series(
                1,
                "Gen",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(ts),
                Default::default(),
                None,
            )
            .unwrap();

        let reader = store
            .build_static_reader(ListFilter::new().resolution(Period::Months(1)))
            .unwrap();
        let stamps: Vec<_> = reader.timestamps().collect();
        assert_eq!(
            stamps,
            vec![
                t0(),
                t0() + Duration::days(31), // Jan -> Feb (2030-02-01)
                t0() + Duration::days(31 + 28),
            ]
        );
        for (i, t) in stamps.iter().enumerate() {
            assert_eq!(reader.index_at(*t).unwrap(), i);
        }
    }

    #[test]
    fn values_to_vec_typed_decode() {
        let mut store = Store::create(None, true).unwrap();
        add_f64(&mut store, 1, "load", &[10.0, 11.0, 12.0, 13.0]);
        add_i64(&mut store, 2, "count", &[100, 101, 102, 103]);
        let mut reader = store
            .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
            .unwrap();
        store
            .static_read(&mut reader, t0() + Duration::hours(2))
            .unwrap();

        let f = &reader.groups()[0];
        assert_eq!(f.values_to_vec::<f64>().unwrap(), vec![12.0]);
        // Wrong dtype errors.
        assert!(f.values_to_vec::<i64>().is_err());

        let i = &reader.groups()[1];
        assert_eq!(i.values_to_vec::<i64>().unwrap(), vec![102]);
    }

    #[test]
    fn window_to_vec_typed_decode() {
        let mut store = Store::create(None, true).unwrap();
        add_det(
            &mut store,
            1,
            "gen",
            2,
            3,
            vec![],
            &[0.0, 10.0, 20.0, 1.0, 11.0, 21.0],
        );
        let mut reader = store.build_forecast_reader(forecast_filter()).unwrap();
        store
            .forecast_read(&mut reader, t0() + Duration::hours(1))
            .unwrap();
        let slot = reader.entry_slot(0);
        // Window k=1 = [10, 11].
        assert_eq!(slot.window_to_vec::<f64>().unwrap(), vec![10.0, 11.0]);
        assert!(slot.window_to_vec::<i64>().is_err());
    }

    /// A reader pins one resolution: series at other resolutions are excluded
    /// (pulling them in would violate the uniform-grid invariant).
    #[test]
    fn static_reader_scopes_to_one_resolution() {
        let mut store = Store::create(None, true).unwrap();
        add_f64(&mut store, 1, "load", &[1.0, 2.0, 3.0, 4.0]); // 1h
        add_f64(&mut store, 2, "load", &[5.0, 6.0, 7.0, 8.0]); // 1h
        let two_hour = SingleTimeSeries::new(
            t0(),
            Duration::hours(2),
            TypedArray::from_f64(vec![4], &[9.0, 10.0, 11.0, 12.0]),
            "load",
        );
        store
            .add_time_series(
                3,
                "Gen",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(two_hour),
                Default::default(),
                None,
            )
            .unwrap();

        let reader = store
            .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
            .unwrap();
        let owners: Vec<i64> = reader
            .groups()
            .iter()
            .flat_map(|g| g.keys().iter().map(|k| k.owner_id()))
            .collect();
        assert_eq!(owners, vec![1, 2], "the 2h series must be excluded");
    }
}
