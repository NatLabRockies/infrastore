//! NetCDF persistence integration tests.
//!
//! These exercise the on-disk format and slot map: write to a real `.nc` file,
//! close, reopen, and verify what comes back. Also covers the spill-on-1001
//! and compaction tombstone behaviours documented in the spec.

use castore_core::{
    Compression, Deterministic, Features, ListFilter, NonSequentialTimeSeries, OwnerCategory,
    SingleTimeSeries, TimeSeriesData, TimeSeriesError, TimeSeriesKey, TimeSeriesType, TypedArray,
    create_store, create_store_with_compression, open_store,
};
use chrono::{Duration, TimeZone, Utc};

fn series(initial_year: i32, length: usize, base: f64) -> SingleTimeSeries {
    let initial_timestamp = Utc.with_ymd_and_hms(initial_year, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let values: Vec<f64> = (0..length).map(|i| base + i as f64).collect();
    let data = TypedArray::from_f64(vec![length], &values);
    SingleTimeSeries::new(initial_timestamp, resolution, data, "load")
}

#[test]
fn persistent_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let s = series(2024, 24, 100.0);
        store
            .add_time_series(
                42,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(s.clone()),
                Features::new(),
                Some("MW".into()),
            )
            .unwrap();
        store.flush().unwrap();
        // store dropped here, file closed
    }

    // Reopen and read back.
    let store = open_store(path.as_path(), true).unwrap();
    let keys = store
        .get_time_series_keys(42, OwnerCategory::Component)
        .unwrap();
    assert_eq!(keys.len(), 1);
    let got = store.get_time_series(keys[0].identity(), None).unwrap();
    let single = got.as_single().unwrap();
    assert_eq!(single.length, 24);
    assert_eq!(
        single.data.to_f64_vec().unwrap(),
        (0..24).map(|i| 100.0 + i as f64).collect::<Vec<_>>()
    );

    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "integrity errors: {:?}", report.errors);
}

/// Persisting an *on-disk* store copies both halves and leaves the source store
/// usable. `persist_to` has to close its NetCDF handle around the copy — HDF5
/// keeps a byte-range lock on an open file, which makes the copy fail on Windows
/// with ERROR_LOCK_VIOLATION — so this also covers the reopen after that swap.
#[test]
fn on_disk_persist_copies_and_leaves_the_source_usable() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("store.nc");
    let dest = dir.path().join("copy.nc");

    let mut store = create_store(Some(src.as_path()), false).unwrap();
    store
        .add_time_series(
            42,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(2024, 24, 100.0)),
            Features::new(),
            Some("MW".into()),
        )
        .unwrap();

    store.persist_to(&dest).unwrap();
    assert!(dest.exists(), "the destination .nc must exist");
    assert!(
        dir.path().join("copy.nc.sqlite").exists(),
        "the companion catalog must be copied too"
    );

    // The source store survived the close/reopen: it still reads, still verifies,
    // and still accepts writes.
    let keys = store
        .get_time_series_keys(42, OwnerCategory::Component)
        .unwrap();
    assert_eq!(keys.len(), 1);
    assert!(store.verify_integrity().unwrap().ok());
    store
        .add_time_series(
            43,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(2024, 24, 200.0)),
            Features::new(),
            Some("MW".into()),
        )
        .unwrap();
    drop(store);

    // The copy is a complete, independent store holding the pre-persist state.
    let copy = open_store(dest.as_path(), true).unwrap();
    let copied = copy
        .get_time_series_keys(42, OwnerCategory::Component)
        .unwrap();
    assert_eq!(copied.len(), 1);
    assert_eq!(
        copy.get_time_series(copied[0].identity(), None)
            .unwrap()
            .as_single()
            .unwrap()
            .data
            .to_f64_vec()
            .unwrap(),
        (0..24).map(|i| 100.0 + i as f64).collect::<Vec<_>>()
    );
    assert!(copy.verify_integrity().unwrap().ok());
}

/// An in-memory store must be persistable to disk: `persist_to` materializes its
/// arrays + metadata, and the reopened store reads the same data back.
#[test]
fn in_memory_persist_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    {
        let mut store = create_store(None, true).unwrap(); // in-memory
        let s = series(2024, 24, 100.0);
        store
            .add_time_series(
                42,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(s),
                Features::new(),
                Some("MW".into()),
            )
            .unwrap();
        store.persist_to(&path).unwrap();
        // in-memory store dropped; only the persisted files remain
    }

    let store = open_store(path.as_path(), true).unwrap();
    let keys = store
        .get_time_series_keys(42, OwnerCategory::Component)
        .unwrap();
    assert_eq!(keys.len(), 1);
    let got = store.get_time_series(keys[0].identity(), None).unwrap();
    assert_eq!(
        got.as_single().unwrap().data.to_f64_vec().unwrap(),
        (0..24).map(|i| 100.0 + i as f64).collect::<Vec<_>>()
    );
    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "integrity errors: {:?}", report.errors);
}

/// Persisting an in-memory store must preserve each array's storage layout:
/// dense forecasts and non-sequential series stay standalone (the forecast
/// window read path rejects packed arrays), while `SingleTimeSeries` stays
/// packed. Regression test: `persist_to` used to write every array packed,
/// which broke `forecast_read` on the reopened store.
#[test]
fn in_memory_persist_preserves_forecast_window_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    let t0 = Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap();

    {
        let mut store = create_store(None, true).unwrap();
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(2030, 24, 100.0)),
                Features::new(),
                None,
            )
            .unwrap();
        // H=2, count=3, scalar. Row-major [s, k]; value = k*10 + s.
        let det = Deterministic::new(
            t0,
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            3,
            TypedArray::from_f64(vec![2, 3], &[0.0, 10.0, 20.0, 1.0, 11.0, 21.0]),
            "fc",
        )
        .unwrap();
        store
            .add_time_series(
                2,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::Deterministic(det),
                Features::new(),
                None,
            )
            .unwrap();
        let stamps = vec![t0, t0 + Duration::minutes(7), t0 + Duration::days(3)];
        let ns = NonSequentialTimeSeries::new(
            stamps,
            TypedArray::from_f64(vec![3], &[1.5, 2.5, 3.5]),
            "events",
        )
        .unwrap();
        store
            .add_time_series(
                3,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::NonSequentialTimeSeries(ns),
                Features::new(),
                None,
            )
            .unwrap();
        store.persist_to(&path).unwrap();
    }

    let store = open_store(&path, false).unwrap();

    // Forecast window reads work on the reopened store.
    let mut reader = store
        .build_forecast_reader(
            ListFilter::new()
                .time_series_type(TimeSeriesType::Deterministic)
                .resolution(Duration::hours(1)),
        )
        .unwrap();
    store
        .forecast_read(&mut reader, t0 + Duration::hours(1))
        .unwrap();
    let window: Vec<f64> = reader
        .entry_slot(0)
        .window()
        .chunks_exact(8)
        .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(window, vec![10.0, 11.0]);

    // The static reader still sees the packed SingleTimeSeries.
    let mut static_reader = store
        .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
        .unwrap();
    store
        .static_read(&mut static_reader, t0 + Duration::hours(2))
        .unwrap();
    assert_eq!(static_reader.groups()[0].num_columns(), 1);

    // Whole-series reads work for every type.
    for owner in [1i64, 2, 3] {
        let keys = store
            .get_time_series_keys(owner, OwnerCategory::Component)
            .unwrap();
        assert_eq!(keys.len(), 1, "owner {owner}");
        store.get_time_series(keys[0].identity(), None).unwrap();
    }
    assert!(store.verify_integrity().unwrap().ok());
}

/// Every supported compression policy must round-trip transparently: data
/// written under one filter reads back identically, and appends after reopen
/// reuse the persisted policy.
#[test]
fn compression_policies_round_trip() {
    for compression in [
        Compression::None,
        Compression::Deflate {
            level: 9,
            shuffle: false,
        },
        Compression::Deflate {
            level: 1,
            shuffle: true,
        },
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.nc");

        {
            let mut store =
                create_store_with_compression(Some(path.as_path()), false, compression).unwrap();
            store
                .add_time_series(
                    7,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(series(2024, 24, 100.0)),
                    Features::new(),
                    None,
                )
                .unwrap();
            store.flush().unwrap();
        }

        // Reopen read-write and append a second series; this exercises the
        // restored-from-attribute compression path.
        {
            let mut store = open_store(path.as_path(), false).unwrap();
            // The policy is restored from the persisted file attribute.
            assert_eq!(store.compression(), compression, "{compression:?}");
            store
                .add_time_series(
                    8,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(series(2024, 24, 200.0)),
                    Features::new(),
                    None,
                )
                .unwrap();
            store.flush().unwrap();
        }

        let store = open_store(path.as_path(), true).unwrap();
        for (owner, base) in [(7i64, 100.0), (8, 200.0)] {
            let keys = store
                .get_time_series_keys(owner, OwnerCategory::Component)
                .unwrap();
            assert_eq!(keys.len(), 1, "{compression:?}");
            let got = store.get_time_series(keys[0].identity(), None).unwrap();
            assert_eq!(
                got.as_single().unwrap().data.to_f64_vec().unwrap(),
                (0..24).map(|i| base + i as f64).collect::<Vec<_>>(),
                "{compression:?}",
            );
        }
        let report = store.verify_integrity().unwrap();
        assert!(report.ok(), "integrity errors: {:?}", report.errors);
    }
}

/// A read-only open must not require write permission on either artifact:
/// stores on read-only media (or shared, permission-locked deployments) must
/// still be readable. Regression test: the NetCDF side used to open in append
/// mode regardless of `read_only`, which failed on write-protected files.
#[test]
fn read_only_open_works_on_write_protected_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(2024, 24, 100.0)),
                Features::new(),
                None,
            )
            .unwrap();
        store.flush().unwrap();
    }

    // Write-protect both halves of the artifact, as on read-only media.
    let mut protected = Vec::new();
    for file in [path.clone(), path.with_file_name("store.nc.sqlite")] {
        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&file, perms).unwrap();
        protected.push(file);
    }

    {
        let mut store = open_store(path.as_path(), true).unwrap();
        let keys = store
            .get_time_series_keys(1, OwnerCategory::Component)
            .unwrap();
        assert_eq!(keys.len(), 1);
        let got = store.get_time_series(keys[0].identity(), None).unwrap();
        assert_eq!(got.as_single().unwrap().length, 24);
        // Writes through a read-only store are rejected, not attempted.
        let err = store
            .add_time_series(
                2,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(2024, 24, 200.0)),
                Features::new(),
                None,
            )
            .unwrap_err();
        assert!(matches!(err, TimeSeriesError::ReadOnlyStore));
    }

    // Restore permissions so the tempdir can be cleaned up everywhere.
    for file in protected {
        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&file, perms).unwrap();
    }
}

/// DEFLATE levels outside 0–9 are rejected up front.
#[test]
fn invalid_compression_level_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    let err = create_store_with_compression(
        Some(path.as_path()),
        false,
        Compression::Deflate {
            level: 10,
            shuffle: true,
        },
    );
    assert!(err.is_err(), "level 10 should be rejected");
}

#[test]
fn deduplication_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let s = series(2024, 24, 7.0);
        for owner in [1i64, 2, 3] {
            store
                .add_time_series(
                    owner,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(s.clone()),
                    Features::new(),
                    None,
                )
                .unwrap();
        }
        store.flush().unwrap();
    }

    let store = open_store(path.as_path(), true).unwrap();
    // Three associations exist…
    assert_eq!(store.list_time_series(ListFilter::new()).unwrap().len(), 3);
    // …but verify_integrity reads each only once at the array level — the
    // single underlying column hashes correctly.
    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "errors: {:?}", report.errors);
}

#[test]
fn multiple_resolutions_separate_datasets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let data = TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]);

        for (i, res) in [
            Duration::hours(1),
            Duration::minutes(15),
            Duration::seconds(60),
        ]
        .into_iter()
        .enumerate()
        {
            let s = SingleTimeSeries::new(initial, res, data.clone(), "load");
            store
                .add_time_series(
                    (i + 1) as i64,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(s),
                    Features::new(),
                    None,
                )
                .unwrap();
        }
        store.flush().unwrap();
    }

    let store = open_store(path.as_path(), true).unwrap();
    let resolutions = store.get_resolutions(None).unwrap();
    assert_eq!(resolutions.len(), 3);
    let report = store.verify_integrity().unwrap();
    assert!(report.ok());
}

#[test]
fn time_range_slicing_through_netcdf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let data = TypedArray::from_f64(
        vec![10],
        &[10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0],
    );
    let s = SingleTimeSeries::new(initial, resolution, data, "load");

    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let key = store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(s.clone()),
                Features::new(),
                None,
            )
            .unwrap();
        store.flush().unwrap();
        key
    };

    let store = open_store(path.as_path(), true).unwrap();
    let start = initial + Duration::hours(3);
    let end = initial + Duration::hours(7);
    let got = store
        .get_time_series(key.identity(), Some((start, end)))
        .unwrap();
    let single = got.as_single().unwrap();
    assert_eq!(single.length, 4);
    assert_eq!(single.initial_timestamp, start);
    assert_eq!(
        single.data.to_f64_vec().unwrap(),
        vec![40.0, 50.0, 60.0, 70.0]
    );
}

#[test]
fn spill_into_new_dataset_past_capacity() {
    use castore_core::storage::netcdf::DEFAULT_COLS_PER_DATASET;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    // Need DEFAULT + 1 distinct arrays of identical (length, resolution) so they
    // compete for the same dataset family. Single `add_time_series` calls take the
    // per-column path, which packs into a shared default-width dataset and spills
    // once it fills. (A managed bulk batch would instead create one batch-sized
    // dataset and not spill.) To keep the test fast we use small length=4.
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let total = DEFAULT_COLS_PER_DATASET + 1;

    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        for i in 0..total {
            let vals = [i as f64, i as f64 + 1.0, i as f64 + 2.0, i as f64 + 3.0];
            let data = TypedArray::from_f64(vec![4], &vals);
            let s = SingleTimeSeries::new(initial, resolution, data, "load");
            store
                .add_time_series(
                    (i + 1) as i64,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(s),
                    Features::new(),
                    None,
                )
                .unwrap();
        }
        store.flush().unwrap();
    }

    // Reopen, sample the first and the last association, and verify integrity.
    let store = open_store(path.as_path(), true).unwrap();
    let counts = store.get_time_series_counts().unwrap();
    assert_eq!(counts.static_time_series as usize, total);

    // Quick spot-check: the very last one — which must have spilled — reads back.
    let keys = store
        .get_time_series_keys(total as i64, OwnerCategory::Component)
        .unwrap();
    assert_eq!(keys.len(), 1);
    let last = store.get_time_series(keys[0].identity(), None).unwrap();
    let single = last.as_single().unwrap();
    assert_eq!(
        single.data.to_f64_vec().unwrap(),
        vec![
            (total - 1) as f64,
            (total - 1) as f64 + 1.0,
            (total - 1) as f64 + 2.0,
            (total - 1) as f64 + 3.0,
        ]
    );

    let report = store.verify_integrity().unwrap();
    assert!(
        report.ok(),
        "errors after spill: {} (showing up to 5: {:?})",
        report.errors.len(),
        report.errors.iter().take(5).collect::<Vec<_>>()
    );
}

#[test]
fn bulk_add_session_writes_block_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bulk.nc");

    // A batch of distinct same-shape series plus one whose content duplicates an
    // earlier series (different owner) to exercise the block writer's dedup.
    let n = 50usize;
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let mut bulk = store.bulk_add();
        for i in 0..n {
            bulk.add(
                (i + 1) as i64,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(2024, 12, i as f64 * 10.0)),
                Features::new(),
                Some("MW".into()),
            );
        }
        // Duplicate the content of series 0 under a new owner.
        bulk.add(
            10_000,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(2024, 12, 0.0)),
            Features::new(),
            Some("MW".into()),
        );
        let keys = bulk.commit().unwrap();
        assert_eq!(keys.len(), n + 1);
        store.flush().unwrap();
    }

    // Reopen and verify every series reads back, including the deduped duplicate.
    let store = open_store(path.as_path(), true).unwrap();
    assert_eq!(
        store.get_time_series_counts().unwrap().static_time_series as usize,
        n + 1
    );
    for i in 0..n {
        let keys = store
            .get_time_series_keys((i + 1) as i64, OwnerCategory::Component)
            .unwrap();
        let got = store.get_time_series(keys[0].identity(), None).unwrap();
        let expected: Vec<f64> = (0..12).map(|t| i as f64 * 10.0 + t as f64).collect();
        assert_eq!(
            got.as_single().unwrap().data.to_f64_vec().unwrap(),
            expected
        );
    }
    // The duplicate-content owner reads back the same values as series 0.
    let dup_keys = store
        .get_time_series_keys(10_000, OwnerCategory::Component)
        .unwrap();
    let dup = store.get_time_series(dup_keys[0].identity(), None).unwrap();
    let expected0: Vec<f64> = (0..12).map(|t| t as f64).collect();
    assert_eq!(
        dup.as_single().unwrap().data.to_f64_vec().unwrap(),
        expected0
    );

    assert!(store.verify_integrity().unwrap().ok());
}

#[test]
fn bulk_add_dropped_without_commit_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("discard.nc");

    let mut store = create_store(Some(path.as_path()), false).unwrap();
    {
        let mut bulk = store.bulk_add();
        bulk.add(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(2024, 12, 1.0)),
            Features::new(),
            None,
        );
        assert_eq!(bulk.len(), 1);
        // Dropped here without commit: nothing should be written.
    }
    assert_eq!(
        store.get_time_series_counts().unwrap().static_time_series,
        0
    );
}

#[test]
fn bulk_read_matches_get_time_series_across_types() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bulkread.nc");
    let n = 30usize;
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let mut bulk = store.bulk_add();
        // A standalone series first, so the packed fast-path and the standalone
        // fallback interleave and bulk_read must keep input order.
        let stamps: Vec<_> = (0..8i64)
            .map(|j| initial + Duration::hours(j * 2))
            .collect();
        let ns_data: Vec<f64> = (0..8).map(|j| 1000.0 + j as f64).collect();
        let ns =
            NonSequentialTimeSeries::new(stamps, TypedArray::from_f64(vec![8], &ns_data), "load")
                .unwrap();
        bulk.add(
            0,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::NonSequentialTimeSeries(ns),
            Features::new(),
            None,
        );
        for i in 0..n {
            bulk.add(
                (i + 1) as i64,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(2024, 16, i as f64)),
                Features::new(),
                Some("MW".into()),
            );
        }
        bulk.commit().unwrap();
        store.flush().unwrap();
    }

    let store = open_store(path.as_path(), true).unwrap();
    let mut keys = Vec::new();
    for owner in 0..=(n as i64) {
        let k = store
            .get_time_series_keys(owner, OwnerCategory::Component)
            .unwrap();
        keys.push(k.into_iter().next().unwrap());
    }
    let ids: Vec<_> = keys.iter().map(|k| k.identity()).collect();

    let bulk = store.bulk_read(&ids).unwrap();
    assert_eq!(bulk.len(), n + 1);
    // Every bulk result equals the per-key get_time_series result, in order.
    for (i, data) in bulk.iter().enumerate() {
        let expected = store.get_time_series(ids[i], None).unwrap();
        match (data, &expected) {
            (TimeSeriesData::SingleTimeSeries(got), TimeSeriesData::SingleTimeSeries(want)) => {
                assert_eq!(got.data, want.data)
            }
            (
                TimeSeriesData::NonSequentialTimeSeries(got),
                TimeSeriesData::NonSequentialTimeSeries(want),
            ) => {
                assert_eq!(got.data, want.data);
                assert_eq!(got.timestamps, want.timestamps);
            }
            other => panic!("type mismatch at {i}: {other:?}"),
        }
    }
}

#[test]
fn compact_reports_tombstones_and_slot_is_reused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    let mut store = create_store(Some(path.as_path()), false).unwrap();
    // Three distinct arrays in the same family.
    let s1 = series(2024, 8, 1.0);
    let s2 = series(2024, 8, 100.0);
    let s3 = series(2024, 8, 200.0);

    let k1 = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s1),
            Features::new(),
            None,
        )
        .unwrap();
    let _k2 = store
        .add_time_series(
            2,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s2),
            Features::new(),
            None,
        )
        .unwrap();
    let _k3 = store
        .add_time_series(
            3,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s3),
            Features::new(),
            None,
        )
        .unwrap();

    // Remove the middle association — its underlying array is dropped because
    // no other association references it.
    store.remove_time_series(k1.identity()).unwrap();

    // compact() should report >=1 reclaimed slot. (The full dataset was
    // pre-allocated at MAX_COLS, so it'll actually report MAX_COLS-2.)
    let report = store.compact().unwrap();
    assert!(report.slots_reclaimed >= 1);

    // Adding a new array should reuse the freed column slot.
    let s4 = series(2024, 8, 500.0);
    store
        .add_time_series(
            4,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s4),
            Features::new(),
            None,
        )
        .unwrap();

    let counts = store.get_time_series_counts().unwrap();
    // s2, s3, s4 → 3 active.
    assert_eq!(counts.static_time_series, 3);

    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "errors: {:?}", report.errors);
}

#[test]
fn data_format_version_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    {
        let _ = create_store(Some(path.as_path()), false).unwrap();
    }
    // Open the file with the netcdf crate directly to read the attribute.
    let f = netcdf::open(&path).unwrap();
    let attr = f.attribute("data_format_version").expect("attr present");
    let value = attr.value().unwrap();
    let netcdf::AttributeValue::Str(s) = value else {
        panic!("expected str, got {value:?}");
    };
    assert_eq!(s, castore_core::DATA_FORMAT_VERSION);
}

#[test]
fn netcdf_roundtrips_multidim_element_tuples() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    // A (4, 3) array — a 3-tuple per step (e.g. quadratic curve coeffs).
    let values: Vec<f64> = (0..12).map(|i| i as f64).collect();
    let data = TypedArray::from_f64(vec![4, 3], &values);
    let s = SingleTimeSeries::new(initial, resolution, data.clone(), "load");

    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let key = store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(s),
                Features::new(),
                None,
            )
            .unwrap();
        store.flush().unwrap();
        key
    };

    let store = open_store(path.as_path(), true).unwrap();
    let got = store.get_time_series(key.identity(), None).unwrap();
    let single = got.as_single().unwrap();
    assert_eq!(single.data.shape, vec![4, 3]);
    assert_eq!(single.data, data);
    assert!(store.verify_integrity().unwrap().ok());
}

#[test]
fn golden_hash_pin() {
    // Pin the exact SHA-256 of a fixed input. Any change in the canonical
    // hash domain that perturbs this value is a format-breaking change and
    // must bump DATA_FORMAT_VERSION.
    use castore_core::{array_hash, hash_hex};
    let data = TypedArray::from_f64(vec![4], &[0.0, 1.0, 2.0, 3.0]);
    let h = array_hash(&data);
    let hex = hash_hex(&h);
    assert_eq!(
        hex, "f85b4f66e62c7c51f9c82d01eabed7fb5e3b5217a69296aaaff876e1144dd841",
        "golden hash drifted; bump DATA_FORMAT_VERSION if intentional",
    );
}

#[test]
fn non_sequential_persistent_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let timestamps = vec![
        initial,
        initial + Duration::minutes(7),
        initial + Duration::days(3),
    ];
    let data = TypedArray::from_f64(vec![3], &[1.5, 2.5, 3.5]);
    let key = {
        let mut store = create_store(Some(&path), false).unwrap();
        let series =
            NonSequentialTimeSeries::new(timestamps.clone(), data.clone(), "events").unwrap();
        let key = store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::NonSequentialTimeSeries(series),
                Features::new(),
                None,
            )
            .unwrap();
        store.flush().unwrap();
        key
    };

    let store = open_store(&path, true).unwrap();
    let got = store.get_time_series(key.identity(), None).unwrap();
    let irregular = got.as_non_sequential().unwrap();
    assert_eq!(irregular.timestamps, timestamps);
    assert_eq!(irregular.data, data);
    assert!(store.verify_integrity().unwrap().ok());
}

/// A store written in an older on-disk format is rejected on open with a clear
/// diagnostic, rather than being misread. `DATA_FORMAT_VERSION` is bumped only
/// for backward-incompatible changes, so any mismatch means this build cannot
/// read the file — there is no in-place upgrade.
#[test]
fn opening_a_store_from_an_older_format_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(2024, 8, 1.0)),
                Features::new(),
                None,
            )
            .unwrap();
        store.flush().unwrap();
    }

    // Backdate the recorded format version in place, simulating a store written
    // by an older build.
    {
        let mut f = netcdf::append(&path).unwrap();
        f.add_attribute("data_format_version", "0.9.0").unwrap();
    }

    let Err(err) = open_store(path.as_path(), true) else {
        panic!("expected an older-format store to be rejected");
    };
    match err {
        TimeSeriesError::IncompatibleFormat { found, expected } => {
            assert_eq!(found, "0.9.0");
            assert_eq!(expected, castore_core::DATA_FORMAT_VERSION);
        }
        other => panic!("expected IncompatibleFormat, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Failure-side persistence: a failing integrity report, torn artifacts, and
// version mismatches other than "older".
// ---------------------------------------------------------------------------

/// Build a small on-disk store and return `(dir, nc_path, key)`. The temp dir is
/// returned so the caller keeps it alive.
fn store_on_disk() -> (tempfile::TempDir, std::path::PathBuf, TimeSeriesKey) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let key = store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(2024, 8, 1.0)),
                Features::new(),
                None,
            )
            .unwrap();
        store.flush().unwrap();
        key
    };
    (dir, path, key)
}

fn sqlite_path_of(nc: &std::path::Path) -> std::path::PathBuf {
    let mut p = nc.as_os_str().to_owned();
    p.push(".sqlite");
    std::path::PathBuf::from(p)
}

/// The name of the packed data variable (not its `_h` companion, not a
/// standalone `arr_*`) inside the `time_series/single` group of a store file.
fn packed_data_variable(path: &std::path::Path) -> String {
    let f = netcdf::open(path).unwrap();
    // `File::group` returns `Result<Option<_>>`; `Group::group` returns `Option<_>`.
    let ts = f.group("time_series").unwrap().expect("time_series group");
    let single = ts.group("single").expect("single group");
    single
        .variables()
        .map(|v| v.name())
        .find(|n| !n.ends_with("_h") && !n.starts_with("arr_"))
        .expect("a packed data variable exists")
}

#[test]
fn verify_integrity_reports_a_hash_mismatch_when_stored_bytes_are_corrupted() {
    // `verify_integrity` recomputes each array's content hash and compares it
    // with the hash recorded alongside it in the file. Perturbing one stored
    // element without touching the recorded hash is exactly the corruption the
    // check exists to catch — and until now nothing anywhere produced a
    // *failing* report, so the error-reporting path was untested.
    let (_dir, path, _key) = store_on_disk();
    let dataset = packed_data_variable(&path);

    {
        let mut f = netcdf::append(&path).unwrap();
        let mut ts = f
            .group_mut("time_series")
            .unwrap()
            .expect("time_series group");
        let mut single = ts.group_mut("single").expect("single group");
        let mut var = single.variable_mut(&dataset).expect("data variable");
        // Flip row 0, column 0 to a value the recorded hash does not describe.
        var.put_value(-999.5f64, [0, 0]).unwrap();
    }

    let store = open_store(path.as_path(), true).unwrap();
    let report = store.verify_integrity().unwrap();
    assert!(
        !report.ok(),
        "corrupting stored bytes must produce a failing report"
    );
    assert_eq!(report.errors.len(), 1, "errors: {:?}", report.errors);
    assert!(
        report.errors[0].starts_with("hash mismatch: stored="),
        "unexpected diagnostic: {}",
        report.errors[0]
    );
}

#[test]
fn verify_integrity_does_not_inspect_the_sqlite_catalog() {
    // FINDING F3 (TEST_COVERAGE_PLAN.md §9): `Store::verify_integrity`
    // delegates straight to the NetCDF backend, which walks only its own
    // hash index. A `data_hash` corrupted in the SQLite catalog — the half that
    // maps a key to an array — is therefore NOT reported, even though every
    // read of that key now fails or returns the wrong array. Pinned as-is; the
    // fix (cross-checking catalog hashes against the backend index) is a
    // behavior change and out of scope here.
    let (_dir, path, key) = store_on_disk();

    {
        let conn = rusqlite::Connection::open(sqlite_path_of(&path)).unwrap();
        let n = conn
            .execute(
                "UPDATE time_series_associations SET data_hash = ?1",
                [&"0".repeat(64)],
            )
            .unwrap();
        assert_eq!(n, 1, "one association to corrupt");
    }

    let store = open_store(path.as_path(), true).unwrap();
    let report = store.verify_integrity().unwrap();
    assert!(
        report.ok(),
        "PIN: a catalog-side hash corruption is invisible to verify_integrity; \
         got errors {:?}",
        report.errors
    );
    // But the key genuinely no longer resolves to its array, which is what the
    // report failed to surface.
    assert!(
        store.get_time_series(key.identity(), None).is_err(),
        "the corrupted association must not still read successfully"
    );
}

#[test]
fn opening_a_store_whose_sqlite_half_is_missing_creates_an_empty_catalog() {
    // PIN: the two artifacts are one logical store, but nothing enforces that.
    // Deleting the catalog and opening read-write silently yields a store with
    // the arrays still on disk and *no* time series — a torn artifact reads as
    // an empty one rather than erroring.
    let (_dir, path, key) = store_on_disk();
    std::fs::remove_file(sqlite_path_of(&path)).unwrap();

    let store = open_store(path.as_path(), false).unwrap();
    assert!(
        store.list_keys(ListFilter::new()).unwrap().is_empty(),
        "PIN: the catalog is recreated empty, not restored"
    );
    assert!(matches!(
        store.get_metadata(key.identity()),
        Err(TimeSeriesError::NotFound)
    ));
    // The array is still physically present, so it is now unreachable garbage.
    assert!(store.verify_integrity().unwrap().ok());
}

#[test]
fn opening_a_store_whose_sqlite_half_is_missing_read_only_errors() {
    // A read-only open cannot create the catalog file, so unlike the read-write
    // case above it fails loudly rather than presenting an empty store. The
    // error comes from SQLite, so the diagnostic names the missing `.sqlite`
    // path rather than explaining that the store is torn.
    let (_dir, path, _key) = store_on_disk();
    std::fs::remove_file(sqlite_path_of(&path)).unwrap();

    let Err(err) = open_store(path.as_path(), true) else {
        panic!("a read-only open with no catalog must fail");
    };
    assert!(
        matches!(err, TimeSeriesError::Sqlite(_)),
        "expected a Sqlite error, got {err:?}"
    );
    assert!(err.to_string().contains(".sqlite"), "{err}");
}

#[test]
fn opening_a_zero_byte_netcdf_file_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    std::fs::write(&path, b"").unwrap();

    let Err(err) = open_store(path.as_path(), true) else {
        panic!("a zero-byte file is not a store");
    };
    // It is not a NetCDF file at all, so the failure comes from the HDF5 open,
    // not from the format-version check.
    assert!(
        !matches!(err, TimeSeriesError::IncompatibleFormat { .. }),
        "expected an open failure, got {err:?}"
    );
    assert!(!err.to_string().is_empty());
}

#[test]
fn opening_a_truncated_netcdf_file_is_rejected() {
    // A file with a plausible prefix but no valid trailer.
    let (_dir, path, _key) = store_on_disk();
    let bytes = std::fs::read(&path).unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    let truncated = dir2.path().join("truncated.nc");
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();

    assert!(
        open_store(truncated.as_path(), true).is_err(),
        "a truncated store file must not open"
    );
}

#[test]
fn opening_a_directory_as_a_store_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let subdir = dir.path().join("not_a_store.nc");
    std::fs::create_dir(&subdir).unwrap();

    let Err(err) = open_store(subdir.as_path(), true) else {
        panic!("a directory is not a store");
    };
    assert!(!err.to_string().is_empty());
}

#[test]
fn opening_a_nonexistent_path_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does_not_exist.nc");
    assert!(open_store(missing.as_path(), true).is_err());
}

#[test]
fn opening_a_store_from_a_newer_format_is_rejected() {
    // The version check is exact equality in both directions: a store written
    // by a *newer* build is just as unreadable as an older one, and must say so
    // rather than being parsed hopefully.
    let (_dir, path, _key) = store_on_disk();
    {
        let mut f = netcdf::append(&path).unwrap();
        f.add_attribute("data_format_version", "99.0.0").unwrap();
    }

    let Err(err) = open_store(path.as_path(), true) else {
        panic!("expected a newer-format store to be rejected");
    };
    match err {
        TimeSeriesError::IncompatibleFormat { found, expected } => {
            assert_eq!(found, "99.0.0");
            assert_eq!(expected, castore_core::DATA_FORMAT_VERSION);
        }
        other => panic!("expected IncompatibleFormat, got {other:?}"),
    }
}

#[test]
fn opening_a_store_with_no_format_attribute_is_rejected_as_unspecified() {
    // A file that predates the attribute entirely reports `found:
    // "unspecified"`. Build one from scratch with the netcdf crate: a store
    // created by this build always carries the attribute, and the crate has no
    // attribute-removal API.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.nc");
    {
        let _f = netcdf::create(&path).unwrap();
    }

    // Opened read-write so the companion catalog is created; a read-only open
    // would fail on the absent catalog first (see
    // `the_catalog_half_is_opened_before_the_format_check`).
    let Err(err) = open_store(path.as_path(), false) else {
        panic!("expected a store with no format attribute to be rejected");
    };
    match err {
        TimeSeriesError::IncompatibleFormat { found, expected } => {
            assert_eq!(found, "unspecified");
            assert_eq!(expected, castore_core::DATA_FORMAT_VERSION);
        }
        other => panic!("expected IncompatibleFormat, got {other:?}"),
    }
}

#[test]
fn the_catalog_half_is_opened_before_the_format_check() {
    // PIN the ordering inside `Store::open`: the SQLite catalog is opened
    // first, so when *both* halves are wrong the caller sees the catalog error,
    // not the more informative format-version error. Worth knowing when reading
    // a bug report: a `Sqlite(CannotOpen)` does not rule out a version
    // mismatch underneath it.
    let (_dir, path, _key) = store_on_disk();
    {
        let mut f = netcdf::append(&path).unwrap();
        f.add_attribute("data_format_version", "99.0.0").unwrap();
    }
    std::fs::remove_file(sqlite_path_of(&path)).unwrap();

    let Err(err) = open_store(path.as_path(), true) else {
        panic!("expected an error");
    };
    assert!(
        matches!(err, TimeSeriesError::Sqlite(_)),
        "PIN: the catalog error wins over the format error; got {err:?}"
    );

    // Opened read-write the catalog is created, and the format check then fires.
    assert!(matches!(
        open_store(path.as_path(), false),
        Err(TimeSeriesError::IncompatibleFormat { .. })
    ));
}
