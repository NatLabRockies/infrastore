//! Timestamp-oriented readers.
//!
//! Simulations consume the store with a `for` loop over every timestamp: at
//! each timestamp they want the value of *every* time series at that instant.
//! [`StaticReader`] serves exactly that access pattern for `SingleTimeSeries`,
//! which is why those arrays are kept in the compacted/packed NetCDF format
//! (one timestamp across all columns of a packed dataset is a single hyperslab).
//!
//! # Design (locked 2026-06-25)
//!
//! * **One resolution per reader.** The build filter must pin a resolution.
//! * **Static and forecast are separate readers.** This module is the static
//!   side; forecasts get a sibling `ForecastReader` (not yet sketched).
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

use chrono::{DateTime, Duration, Utc};

use crate::error::{Result, TimeSeriesError};
use crate::types::array::Dtype;
use crate::types::key::TimeSeriesKey;
use crate::types::metadata::TimeSeriesMetadata;
use crate::types::time_series::TimeSeriesType;

/// A prepared reader returning the value of every matching `SingleTimeSeries`
/// at one timestamp. Build with [`Store::build_static_reader`], drive with
/// [`Store::static_read`], then read results via [`Self::groups`].
#[derive(Debug)]
pub struct StaticReader {
    /// Shared master grid for every series in the reader.
    initial_timestamp: DateTime<Utc>,
    resolution: Duration,
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
    dtype: Dtype,
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
    pub fn dtype(&self) -> Dtype {
        self.dtype
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

    /// Number of elements per column (product of `element_shape`; 1 for scalar).
    fn elements_per_column(&self) -> usize {
        self.element_shape.iter().product::<usize>().max(1)
    }

    /// Drive a backend read into this group's reusable buffer. The closure
    /// receives the column hashes (in key order) and the output buffer to fill;
    /// splitting the borrow across the two fields lets the backend read hashes
    /// and write the buffer of the *same* group without aliasing.
    pub(crate) fn fill<F>(&mut self, read: F) -> Result<()>
    where
        F: FnOnce(&[[u8; 32]], &mut Vec<u8>) -> Result<()>,
    {
        read(&self.hashes, &mut self.buf)?;
        self.filled = true;
        Ok(())
    }
}

impl StaticReader {
    pub fn initial_timestamp(&self) -> DateTime<Utc> {
        self.initial_timestamp
    }

    pub fn resolution(&self) -> Duration {
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
        let res_ms = self.resolution.num_milliseconds();
        if res_ms <= 0 {
            return Err(TimeSeriesError::IntegrityError(
                "StaticReader resolution must be positive".into(),
            ));
        }
        let delta = (at - self.initial_timestamp).num_milliseconds();
        if delta < 0 {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "timestamp {at} is before the reader's initial timestamp {}",
                self.initial_timestamp
            )));
        }
        if delta % res_ms != 0 {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "timestamp {at} is not aligned to resolution {res_ms} ms"
            )));
        }
        let idx = (delta / res_ms) as usize;
        if idx >= self.length {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "timestamp {at} (index {idx}) is past the grid length {}",
                self.length
            )));
        }
        Ok(idx)
    }
}

/// Resolve a set of `SingleTimeSeries` metadata rows into the reader's master
/// grid plus its `(dtype, element_shape)` groups.
///
/// Pure (no I/O) so it can be unit-tested without a backend. Validates that
/// every row shares one grid; this is what makes the per-read path mask-free.
pub(crate) fn build_groups(
    mut rows: Vec<TimeSeriesMetadata>,
) -> Result<(DateTime<Utc>, Duration, usize, Vec<StaticGroup>)> {
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

    // Deterministic layout: order by dtype, then element shape, then key
    // identity, so column positions are stable across processes.
    rows.sort_by(|a, b| {
        a.dtype
            .code()
            .cmp(&b.dtype.code())
            .then_with(|| a.element_shape.cmp(&b.element_shape))
            .then_with(|| identity_sort_key(a).cmp(&identity_sort_key(b)))
    });

    let mut groups: Vec<StaticGroup> = Vec::new();
    for r in rows {
        let want = (r.dtype, r.element_shape.as_slice());
        let push_new = groups
            .last()
            .map(|g| (g.dtype, g.element_shape.as_slice()) != want)
            .unwrap_or(true);
        if push_new {
            groups.push(StaticGroup {
                dtype: r.dtype,
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
        let bytes = g.num_columns() * g.elements_per_column() * g.dtype.size();
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
        resolution: Duration,
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

/// `(initial_timestamp, resolution, length)` of a `SingleTimeSeries` row, or an
/// error if a required field is missing or the row is not a `SingleTimeSeries`.
fn grid_of(m: &TimeSeriesMetadata) -> Result<(DateTime<Utc>, Duration, usize)> {
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
    use chrono::TimeZone;

    use crate::store::{ListFilter, Store};
    use crate::types::array::{Dtype, TypedArray};
    use crate::types::metadata::OwnerCategory;
    use crate::types::time_series::{SingleTimeSeries, TimeSeriesData};

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
    /// exercise both the default and NetCDF read paths.
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

    /// On-disk store: drives `NetCdfBackend::read_index_into` (the one-hyperslab-
    /// per-dataset override) and cross-checks it byte-for-byte against the
    /// in-memory backend (the default per-hash path).
    #[test]
    fn netcdf_override_matches_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.nc");

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
        let path = dir.path().join("store.nc");
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
}
