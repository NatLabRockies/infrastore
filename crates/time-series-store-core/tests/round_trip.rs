//! End-to-end round-trip tests for the in-memory Store. These exercise the
//! full Store API surface defined in M0.

use std::collections::BTreeMap;

use chrono::{Duration, TimeZone, Utc};
use time_series_store_core::{
    FeatureValue, Features, KeyIdentity, ListFilter, NonSequentialTimeSeries, OwnerCategory,
    Period, SingleTimeSeries, TimeSeriesData, TimeSeriesError, TimeSeriesType, TypedArray,
    create_store, open_store,
};

fn series(initial_year: i32, length: usize, base: f64) -> SingleTimeSeries {
    let initial_timestamp = Utc.with_ymd_and_hms(initial_year, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let values: Vec<f64> = (0..length).map(|i| base + i as f64).collect();
    let data = TypedArray::from_f64(vec![length], &values);
    SingleTimeSeries::new(initial_timestamp, resolution, data, "load")
}

fn features_with_year(year: i64) -> Features {
    let mut f: Features = BTreeMap::new();
    f.insert("model_year".into(), FeatureValue::Int(year));
    f
}

#[test]
fn monthly_calendar_resolution_round_trips_on_disk_and_reader() {
    // A SingleTimeSeries on a calendar (irregular) monthly grid: 12 months from
    // 2024-01-15. Exercises the ISO-8601 dataset-name + SQLite encoding for
    // `Period::Months`, plus calendar-aware reader index math.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("monthly.nc");
    let initial = Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap();
    let values: Vec<f64> = (0..12).map(|i| 100.0 + i as f64).collect();

    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let s = SingleTimeSeries::new(
            initial,
            Period::Months(1),
            TypedArray::from_f64(vec![12], &values),
            "monthly_load",
        );
        store
            .add_time_series(
                7,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(s),
                Features::new(),
                None,
            )
            .unwrap();
        store.flush().unwrap();
    }

    // Reopen read-only: the resolution survives the ISO round trip as a calendar
    // period (not a fixed ms span).
    let store = open_store(path.as_path(), true).unwrap();
    let keys = store.list_keys(ListFilter::new()).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].resolution(), Some(Period::Months(1)));

    // StaticReader over the monthly grid: month index 3 is 2024-04-15.
    let mut reader = store
        .build_static_reader(ListFilter::new().resolution(Period::Months(1)))
        .unwrap();
    assert_eq!(reader.resolution(), Period::Months(1));
    store
        .static_read(
            &mut reader,
            Utc.with_ymd_and_hms(2024, 4, 15, 0, 0, 0).unwrap(),
        )
        .unwrap();
    let g = &reader.groups()[0];
    let v = f64::from_le_bytes(g.values()[0..8].try_into().unwrap());
    assert_eq!(v, 103.0);

    // Off-grid (mid-month, not a calendar step) is a hard error.
    assert!(
        store
            .static_read(
                &mut reader,
                Utc.with_ymd_and_hms(2024, 4, 20, 0, 0, 0).unwrap()
            )
            .is_err()
    );
}

#[test]
fn add_and_get_round_trip() {
    let mut store = create_store(None, true).unwrap();
    let s = series(2024, 24, 100.0);

    let key = store
        .add_time_series(
            42,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            Some("MW".into()),
        )
        .unwrap();

    let got = store.get_time_series(key.identity(), None).unwrap();
    let single = got.as_single().unwrap();
    assert_eq!(single.data, s.data);
    assert_eq!(single.length, 24);
    assert_eq!(single.initial_timestamp, s.initial_timestamp);
    assert_eq!(single.resolution, s.resolution);
}

#[test]
fn duplicate_key_rejected() {
    let mut store = create_store(None, true).unwrap();
    let s = series(2024, 12, 1.0);

    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
        )
        .unwrap();

    let err = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
        )
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateTimeSeries));
}

#[test]
fn features_disambiguate_keys() {
    let mut store = create_store(None, true).unwrap();
    let s1 = series(2024, 12, 1.0);
    let s2 = series(2024, 12, 100.0);

    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s1.clone()),
            features_with_year(2030),
            None,
        )
        .unwrap();
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s2.clone()),
            features_with_year(2035),
            None,
        )
        .unwrap();

    let all = store
        .list_time_series(ListFilter::new().owner_id(1))
        .unwrap();
    assert_eq!(all.len(), 2);

    // Subset filter — only the 2035 row.
    let only_2035 = store
        .list_time_series(
            ListFilter::new()
                .owner_id(1)
                .features(features_with_year(2035)),
        )
        .unwrap();
    assert_eq!(only_2035.len(), 1);
    assert_eq!(only_2035[0].features, features_with_year(2035));
}

#[test]
fn deduplication_via_content_addressing() {
    // Two associations with identical data should share one underlying array.
    let mut store = create_store(None, true).unwrap();
    let s = series(2024, 24, 7.0);

    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
        )
        .unwrap();
    store
        .add_time_series(
            2,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
        )
        .unwrap();

    let counts = store.get_time_series_counts().unwrap();
    assert_eq!(counts.static_time_series, 2);
    assert_eq!(counts.components_with_time_series, 2);

    // Two SingleTimeSeries associations, but they share one content-addressed array.
    assert_eq!(
        store.counts_by_type().unwrap(),
        vec![(TimeSeriesType::SingleTimeSeries, 2)]
    );
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);

    // Detailed counts: two distinct Component owners, no supplemental attributes,
    // one shared static array, no forecasts.
    let detailed = store.time_series_counts_detailed().unwrap();
    assert_eq!(detailed.components_with_time_series, 2);
    assert_eq!(detailed.supplemental_attributes_with_time_series, 0);
    assert_eq!(detailed.static_time_series_count, 1);
    assert_eq!(detailed.forecast_count, 0);
    let mut owners = store
        .list_owner_ids(OwnerCategory::Component, None, None)
        .unwrap();
    owners.sort_unstable();
    assert_eq!(owners, vec![1, 2]);

    // The two associations collapse to one static-summary group with count 2.
    let summary = store.static_summary().unwrap();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0].count, 2);
    assert_eq!(summary[0].name, "load");
    assert_eq!(summary[0].time_step_count, Some(24));
    assert!(store.forecast_summary().unwrap().is_empty());

    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "integrity errors: {:?}", report.errors);
}

#[test]
fn remove_keeps_array_when_other_refs_exist() {
    let mut store = create_store(None, true).unwrap();
    let s = series(2024, 12, 1.0);

    let k1 = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
        )
        .unwrap();
    let k2 = store
        .add_time_series(
            2,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
        )
        .unwrap();

    // Remove first association — array still referenced by k2, so k2 must work.
    store.remove_time_series(k1.identity()).unwrap();
    let still_there = store.get_time_series(k2.identity(), None).unwrap();
    assert_eq!(still_there.as_single().unwrap().data, s.data);

    // Remove second — array is now unreferenced. The store doesn't expose
    // the dropped-array fact directly, but verify_integrity should still pass.
    store.remove_time_series(k2.identity()).unwrap();
    let report = store.verify_integrity().unwrap();
    assert!(report.ok());
}

#[test]
fn bulk_add_atomic_rollback() {
    use time_series_store_core::AddRequest;

    let mut store = create_store(None, true).unwrap();
    let s_ok = series(2024, 12, 1.0);
    let s_dup = series(2024, 12, 100.0);

    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s_ok.clone()),
            Features::new(),
            None,
        )
        .unwrap();

    // Bulk: first item succeeds, second collides with the existing (1,"load",None,{}).
    let bulk = vec![
        AddRequest {
            owner_id: 2,
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            data: TimeSeriesData::SingleTimeSeries(s_ok.clone()),
            features: Features::new(),
            units: None,

            logical_type: None,
        },
        AddRequest {
            owner_id: 1,
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            data: TimeSeriesData::SingleTimeSeries(s_dup.clone()),
            features: Features::new(),
            units: None,

            logical_type: None,
        },
    ];
    let err = store.add_time_series_bulk(bulk).unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateTimeSeries));

    // The first item must have been rolled back: owner 2 has nothing.
    let rows = store
        .list_time_series(ListFilter::new().owner_id(2))
        .unwrap();
    assert!(rows.is_empty(), "rollback failed: {:?}", rows);
}

#[test]
fn time_range_slicing() {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let data = TypedArray::from_f64(vec![6], &[10.0, 20.0, 30.0, 40.0, 50.0, 60.0]);
    let s = SingleTimeSeries::new(initial, resolution, data, "load");

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

    // Hours 2..5 (i.e. samples 30, 40, 50).
    let start = initial + Duration::hours(2);
    let end = initial + Duration::hours(5);
    let got = store
        .get_time_series(key.identity(), Some((start, end)))
        .unwrap();
    let single = got.as_single().unwrap();
    assert_eq!(single.length, 3);
    assert_eq!(single.initial_timestamp, start);
    assert_eq!(single.data.to_f64_vec().unwrap(), vec![30.0, 40.0, 50.0]);
}

#[test]
fn clear_by_owner() {
    let mut store = create_store(None, true).unwrap();
    let s = series(2024, 12, 1.0);

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
    assert_eq!(
        store.get_time_series_counts().unwrap().static_time_series,
        3
    );
    let removed = store
        .clear_time_series(Some((2, OwnerCategory::Component)))
        .unwrap();
    assert_eq!(removed, 1);
    let remaining = store
        .list_time_series(ListFilter::new().time_series_type(TimeSeriesType::SingleTimeSeries))
        .unwrap();
    assert_eq!(remaining.len(), 2);
}

#[test]
fn read_only_blocks_writes() {
    use std::path::Path;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    // Create a writable store at `path` (catalog sqlite) and add a row.
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let s = series(2024, 12, 1.0);
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(s),
                Features::new(),
                None,
            )
            .unwrap();
    }

    // Reopen read-only.
    let mut ro = time_series_store_core::open_store(path.as_path() as &Path, true).unwrap();
    let s = series(2024, 12, 1.0);
    let err = ro
        .add_time_series(
            2,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s),
            Features::new(),
            None,
        )
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::ReadOnlyStore));
}

#[test]
fn distinct_resolutions_returned_sorted() {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let data = TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]);

    for (i, resolution) in [
        Duration::hours(1),
        Duration::minutes(15),
        Duration::hours(4),
    ]
    .into_iter()
    .enumerate()
    {
        let s = SingleTimeSeries::new(initial, resolution, data.clone(), "load");
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

    let resolutions = store.get_resolutions(None).unwrap();
    assert_eq!(
        resolutions,
        vec![
            Duration::minutes(15),
            Duration::hours(1),
            Duration::hours(4)
        ]
    );
}

#[test]
fn non_sequential_round_trip_and_time_slice() {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let timestamps = vec![
        initial,
        initial + Duration::hours(3),
        initial + Duration::hours(4),
        initial + Duration::days(2),
    ];
    let data = TypedArray::from_f64(vec![4], &[10.0, 20.0, 30.0, 40.0]);
    let series = NonSequentialTimeSeries::new(timestamps.clone(), data, "availability").unwrap();
    let key = store
        .add_time_series(
            7,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::NonSequentialTimeSeries(series),
            Features::new(),
            Some("MW".into()),
        )
        .unwrap();

    assert_eq!(
        key.time_series_type(),
        TimeSeriesType::NonSequentialTimeSeries
    );
    assert_eq!(key.resolution(), None);
    let metadata = store.get_metadata(key.identity()).unwrap();
    assert_eq!(metadata.timestamps, Some(timestamps.clone()));
    assert_eq!(metadata.resolution, None);

    let got = store
        .get_time_series(
            key.identity(),
            Some((initial + Duration::hours(2), initial + Duration::days(1))),
        )
        .unwrap();
    let irregular = got.as_non_sequential().unwrap();
    assert_eq!(irregular.timestamps, timestamps[1..3]);
    assert_eq!(irregular.data.to_f64_vec().unwrap(), vec![20.0, 30.0]);

    let counts = store.get_time_series_counts().unwrap();
    assert_eq!(counts.static_time_series, 1);
    assert!(
        store
            .get_resolutions(Some(TimeSeriesType::NonSequentialTimeSeries))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn non_sequential_validates_timestamps() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let data = TypedArray::from_f64(vec![2], &[1.0, 2.0]);
    assert!(NonSequentialTimeSeries::new(vec![initial], data.clone(), "test").is_err());
    assert!(NonSequentialTimeSeries::new(vec![initial, initial], data, "test").is_err());
}

#[test]
fn duplicate_non_sequential_key_is_rejected() {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    for values in [[1.0, 2.0], [3.0, 4.0]] {
        let series = NonSequentialTimeSeries::new(
            vec![initial, initial + Duration::hours(1)],
            TypedArray::from_f64(vec![2], &values),
            "events",
        )
        .unwrap();
        let result = store.add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::NonSequentialTimeSeries(series),
            Features::new(),
            None,
        );
        if values[0] == 1.0 {
            result.unwrap();
        } else {
            assert!(matches!(
                result.unwrap_err(),
                TimeSeriesError::DuplicateTimeSeries
            ));
        }
    }
}

#[test]
fn list_keys_with_hash_groups_shared_arrays() {
    use std::collections::HashMap;

    let mut store = create_store(None, true).unwrap();
    // Two owners with identical data: deduplicated to one stored array (one hash).
    for owner in [1, 2] {
        store
            .add_time_series(
                owner,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(2024, 24, 0.0)),
                Features::new(),
                None,
            )
            .unwrap();
    }
    // A third owner with distinct data: a different hash.
    store
        .add_time_series(
            3,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(2024, 24, 100.0)),
            Features::new(),
            None,
        )
        .unwrap();

    let rows = store.list_keys_with_hash(ListFilter::new()).unwrap();
    assert_eq!(rows.len(), 3);

    // Each row's hash agrees with the per-key get_metadata hash.
    for (key, hash) in &rows {
        let meta = store.get_metadata(key.identity()).unwrap();
        assert_eq!(&meta.data_hash, hash);
    }

    // Group by hash: owners 1 and 2 share one array, owner 3 is alone.
    let mut groups: HashMap<[u8; 32], Vec<i64>> = HashMap::new();
    for (key, hash) in &rows {
        groups.entry(*hash).or_default().push(key.owner_id());
    }
    assert_eq!(groups.len(), 2);
    let mut shared: Vec<Vec<i64>> = groups.values().filter(|v| v.len() > 1).cloned().collect();
    assert_eq!(shared.len(), 1);
    shared[0].sort();
    assert_eq!(shared[0], vec![1, 2]);
}

// ---- copy_time_series -------------------------------------------------------

/// The identity of the single association owned by `owner`.
fn only_key(store: &time_series_store_core::Store, owner: i64) -> KeyIdentity {
    let keys = store
        .get_time_series_keys(owner, OwnerCategory::Component)
        .unwrap();
    assert_eq!(keys.len(), 1);
    keys[0].identity().clone()
}

#[test]
fn copy_time_series_shares_the_array_and_renames() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(2024, 24, 0.0)),
            Features::new(),
            None,
        )
        .unwrap();

    let src = only_key(&store, 1);
    let copied = store
        .copy_time_series(&src, 2, "HybridSystem", Some("Generator__load"))
        .unwrap();

    // The copy is a new association on the new owner under the new name...
    assert_eq!(copied.identity().owner_id, 2);
    assert_eq!(copied.identity().name, "Generator__load");
    // ...and the source is untouched (a copy, not a move).
    assert!(store.has_time_series(&src).unwrap());

    // No array duplication: both associations point at the same content hash.
    let src_meta = store.get_metadata(&src).unwrap();
    let dst_meta = store.get_metadata(copied.identity()).unwrap();
    assert_eq!(src_meta.data_hash, dst_meta.data_hash);
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);
    assert_eq!(dst_meta.owner_type, "HybridSystem");
}

#[test]
fn copy_time_series_preserves_deterministic_single_type() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(2024, 24, 0.0)),
            Features::new(),
            None,
        )
        .unwrap();
    // Derive the DeterministicSingleTimeSeries view over the stored SingleTimeSeries.
    store
        .transform_single_time_series(Duration::hours(4), Duration::hours(1), None, None)
        .unwrap();

    let dst_src = store
        .get_time_series_keys(1, OwnerCategory::Component)
        .unwrap()
        .into_iter()
        .find(|k| k.identity().time_series_type == TimeSeriesType::DeterministicSingleTimeSeries)
        .expect("transform should have produced a DeterministicSingleTimeSeries");

    let copied = store
        .copy_time_series(
            dst_src.identity(),
            2,
            "HybridSystem",
            Some("Generator__load"),
        )
        .unwrap();

    // The whole point: the copy stays a DeterministicSingleTimeSeries rather than
    // being materialized into a dense Deterministic, and still shares the array.
    let meta = store.get_metadata(copied.identity()).unwrap();
    assert_eq!(
        meta.time_series_type,
        TimeSeriesType::DeterministicSingleTimeSeries
    );
    assert_eq!(
        meta.data_hash,
        store.get_metadata(dst_src.identity()).unwrap().data_hash
    );
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);
}

#[test]
fn copy_time_series_rejects_a_duplicate_destination() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(2024, 24, 0.0)),
            Features::new(),
            None,
        )
        .unwrap();
    let src = only_key(&store, 1);

    store.copy_time_series(&src, 2, "Generator", None).unwrap();
    // Copying again onto the same owner+name collides with the first copy.
    let err = store
        .copy_time_series(&src, 2, "Generator", None)
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateTimeSeries));
}

// ---------------------------------------------------------------------------
// Content-addressed feature sets
//
// Feature rows live in `feature_sets`, keyed by the SHA-256 of the feature map,
// and are SHARED by every association whose `features_hash` matches. These pin
// the properties that sharing makes non-obvious: that one association's deletion
// cannot take another's features with it, and that the now-unreachable sets are
// reclaimable rather than leaked.
// ---------------------------------------------------------------------------

/// Deleting one of several associations that share a feature set must not
/// disturb the survivors' features. Under the old per-association feature rows
/// with `ON DELETE CASCADE` this was trivially true; with shared rows it is a
/// real invariant, and a stray cascade would silently blank the survivors.
#[test]
fn deleting_one_sharer_leaves_the_others_features_intact() {
    let mut store = create_store(None, true).unwrap();
    let features = features_with_year(2030);

    // Three owners, one identical feature set: one stored `feature_sets` group.
    for owner in 1..=3i64 {
        store
            .add_time_series(
                owner,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(2024, 8, owner as f64)),
                features.clone(),
                None,
            )
            .unwrap();
    }

    store
        .remove_time_series(&KeyIdentity {
            owner_id: 2,
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::SingleTimeSeries,
            name: "load".to_string(),
            resolution: Some(Period::from(Duration::hours(1))),
            interval: None,
            features: features.clone(),
        })
        .unwrap();

    for owner in [1i64, 3] {
        let keys = store
            .get_time_series_keys(owner, OwnerCategory::Component)
            .unwrap();
        assert_eq!(keys.len(), 1, "owner {owner} should still have its series");
        assert_eq!(
            keys[0].features(),
            &features,
            "owner {owner} lost its features when a co-sharer was deleted"
        );
    }
}

/// A feature set outlives the last association that referenced it (sets are
/// shared, so deletion cannot cascade), and `compact` is what reclaims it.
#[test]
fn compact_reclaims_feature_sets_left_unreachable_by_deletion() {
    let mut store = create_store(None, true).unwrap();
    let features = features_with_year(2031);
    let key = KeyIdentity {
        owner_id: 1,
        owner_category: OwnerCategory::Component,
        time_series_type: TimeSeriesType::SingleTimeSeries,
        name: "load".to_string(),
        resolution: Some(Period::from(Duration::hours(1))),
        interval: None,
        features: features.clone(),
    };

    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(2024, 8, 1.0)),
            features.clone(),
            None,
        )
        .unwrap();

    // Nothing to reclaim while the set is still referenced.
    assert_eq!(store.compact().unwrap().feature_sets_reclaimed, 0);

    store.remove_time_series(&key).unwrap();

    // The set is now unreachable: one key/value row is swept.
    let report = store.compact().unwrap();
    assert_eq!(
        report.feature_sets_reclaimed, 1,
        "the orphaned feature set should be reclaimed"
    );
    // Idempotent: nothing left to sweep.
    assert_eq!(store.compact().unwrap().feature_sets_reclaimed, 0);
}

/// A derived `DeterministicSingleTimeSeries` has the same features as the
/// `SingleTimeSeries` it came from, so it reuses the stored set and writes no new
/// feature rows. This is the property that makes `transform_single_time_series`
/// stop scaling with feature count.
#[test]
fn transform_reuses_the_sources_feature_set() {
    let mut store = create_store(None, true).unwrap();
    let features = features_with_year(2032);

    for owner in 1..=5i64 {
        store
            .add_time_series(
                owner,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(2024, 8, owner as f64)),
                features.clone(),
                None,
            )
            .unwrap();
    }
    let n = store
        .transform_single_time_series(Duration::hours(4), Duration::hours(1), None, None)
        .unwrap();
    assert_eq!(n, 5);

    // Every DST shares its source's set, so no set is orphaned and each derived
    // series still reads back its features.
    assert_eq!(store.compact().unwrap().feature_sets_reclaimed, 0);
    for owner in 1..=5i64 {
        let keys = store
            .get_time_series_keys(owner, OwnerCategory::Component)
            .unwrap();
        assert_eq!(keys.len(), 2, "owner {owner}: an STS and its derived DST");
        assert!(
            keys.iter().all(|k| k.features() == &features),
            "owner {owner}: a derived DST lost the source's features"
        );
    }
}
