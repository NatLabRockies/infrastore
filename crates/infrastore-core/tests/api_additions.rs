//! Tests for the Phase-1 additive API surface: `AddRequest`/`Store::add`,
//! bulk/filtered delete, time-sliced bulk read, discovery enumerations, rename,
//! and serde coverage. All additive — no on-disk format change.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    AddRequest, Deterministic, FeatureValue, Features, KeyIdentity, ListFilter, OwnerCategory,
    Period, SingleTimeSeries, TimeSeriesData, TimeSeriesError, TimeSeriesKey, TimeSeriesMetadata,
    TimeSeriesType, TypedArray, UnitSystem, create_store,
};

mod common;
use common::{for_each_backend, for_each_backend_mut};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
}

fn sts(name: &str, base: f64, length: usize) -> SingleTimeSeries {
    let values: Vec<f64> = (0..length).map(|i| base + i as f64).collect();
    SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![length], &values),
        name,
    )
}

fn det(name: &str, base: f64) -> Deterministic {
    // H=2, count=3, interval 1h.
    let vals: Vec<f64> = (0..6).map(|i| base + i as f64).collect();
    Deterministic::new(
        t0(),
        Duration::hours(1),
        Duration::hours(2),
        Duration::hours(1),
        3,
        TypedArray::from_f64(vec![2, 3], &vals),
        name,
    )
    .unwrap()
}

// ---- 1.1 AddRequest builder + Store::add ----------------------------------

#[test]
fn store_add_preserves_application_data() {
    let mut store = create_store(None, true).unwrap();
    let mut features: Features = BTreeMap::new();
    features.insert("scenario".into(), FeatureValue::Str("base".into()));

    let key = store
        .add(
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4))
                    .with_units("MW")
                    .with_application_data("QuadraticFunctionData"),
            )
            .with_features(features.clone()),
        )
        .unwrap();

    let meta = store.get_metadata(key.identity()).unwrap();
    assert_eq!(
        meta.application_data.as_deref(),
        Some("QuadraticFunctionData")
    );
    assert_eq!(meta.units.as_deref(), Some("MW"));
    assert_eq!(meta.features, features);
}

#[test]
fn bulk_push_preserves_application_data() {
    let mut store = create_store(None, true).unwrap();
    let keys = {
        let mut bulk = store.bulk_add();
        bulk.push(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("a", 1.0, 4)).with_application_data("TypeA"),
        ));
        bulk.push(AddRequest::new(
            2,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("b", 2.0, 4)).with_application_data("TypeB"),
        ));
        bulk.commit().unwrap()
    };
    assert_eq!(keys.len(), 2);
    assert_eq!(
        store
            .get_metadata(keys[0].identity())
            .unwrap()
            .application_data
            .as_deref(),
        Some("TypeA")
    );
    assert_eq!(
        store
            .get_metadata(keys[1].identity())
            .unwrap()
            .application_data
            .as_deref(),
        Some("TypeB")
    );
}

// ---- reserved feature names ------------------------------------------------

fn reserved_features(name: &str) -> Features {
    let mut features: Features = BTreeMap::new();
    features.insert("model_year".into(), FeatureValue::Int(2030));
    features.insert(name.into(), FeatureValue::Str("shadowed".into()));
    features
}

fn assert_reserved_err(err: TimeSeriesError, name: &str) {
    match err {
        TimeSeriesError::InvalidParameter(msg) => {
            assert!(msg.contains(name), "{name} should be named in {msg:?}")
        }
        other => panic!("{name}: expected InvalidParameter, got {other:?}"),
    }
}

/// Every write entry point rejects a feature that shadows a time-series or key
/// field, and rejects it before writing anything.
#[test]
fn write_paths_reject_reserved_feature_names() {
    let request = |name: &str, series: &str| {
        AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts(series, 10.0, 4)),
        )
        .with_features(reserved_features(name))
    };

    for name in [
        "name",
        "resolution",
        "initial_timestamp",
        "owner_id",
        "application_data",
    ] {
        let mut store = create_store(None, true).unwrap();

        assert_reserved_err(store.add(request(name, "load")).unwrap_err(), name);
        assert_reserved_err(
            store
                .add_time_series(
                    1,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4)),
                    reserved_features(name),
                )
                .unwrap_err(),
            name,
        );
        assert_reserved_err(
            store
                .add_time_series_bulk(vec![request(name, "load")])
                .unwrap_err(),
            name,
        );
        let err = {
            let mut bulk = store.bulk_add();
            bulk.push(request(name, "load"));
            bulk.commit().unwrap_err()
        };
        assert_reserved_err(err, name);

        assert!(
            store.list_keys(ListFilter::new()).unwrap().is_empty(),
            "{name}: a rejected add must not write anything"
        );
    }
}

/// A rejected item aborts the whole batch, including the valid items alongside
/// it — the same all-or-nothing contract every other bulk failure has.
#[test]
fn a_reserved_feature_name_rolls_back_the_whole_batch() {
    let mut store = create_store(None, true).unwrap();
    let valid = AddRequest::new(
        1,
        "Generator",
        OwnerCategory::Component,
        TimeSeriesData::SingleTimeSeries(sts("good", 1.0, 4)),
    );
    let offending = AddRequest::new(
        2,
        "Generator",
        OwnerCategory::Component,
        TimeSeriesData::SingleTimeSeries(sts("bad", 2.0, 4)),
    )
    .with_features(reserved_features("horizon"));

    assert_reserved_err(
        store
            .add_time_series_bulk(vec![valid, offending])
            .unwrap_err(),
        "horizon",
    );
    assert!(store.list_keys(ListFilter::new()).unwrap().is_empty());
}

/// The rule is exact-match: an ordinary feature that merely resembles a field
/// name still goes in, and reads back unchanged.
#[test]
fn near_miss_feature_names_are_accepted() {
    let mut store = create_store(None, true).unwrap();
    let mut features: Features = BTreeMap::new();
    features.insert("Name".into(), FeatureValue::Str("load".into()));
    features.insert("resolution_hours".into(), FeatureValue::Int(1));
    features.insert("model_year".into(), FeatureValue::Int(2030));

    let key = store
        .add(
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4)),
            )
            .with_features(features.clone()),
        )
        .unwrap();
    assert_eq!(
        store.get_metadata(key.identity()).unwrap().features,
        features
    );
}

// ---- 1.5 bulk / filtered delete -------------------------------------------

#[test]
fn remove_by_filter_empty_match_is_ok_zero() {
    let mut store = create_store(None, true).unwrap();
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4)),
        ))
        .unwrap();
    let removed = store
        .remove_by_filter(ListFilter::new().owner_id(999))
        .unwrap();
    assert_eq!(removed, 0);
    assert_eq!(store.list_keys(ListFilter::new()).unwrap().len(), 1);
}

// ---- 1.6 time-sliced bulk read --------------------------------------------

#[test]
fn bulk_read_range_matches_per_key_get_time_series() {
    let mut store = create_store(None, true).unwrap();
    let k1 = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 100.0, 8)),
        ))
        .unwrap();
    let k2 = store
        .add(AddRequest::new(
            2,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 200.0, 8)),
        ))
        .unwrap();

    let range = (t0() + Duration::hours(2), t0() + Duration::hours(5));
    let keys = [k1.identity(), k2.identity()];

    let sliced = store.bulk_read_range(&keys, Some(range)).unwrap();
    for (i, k) in keys.iter().enumerate() {
        let per_key = store.get_time_series(k, Some(range)).unwrap();
        assert_eq!(
            sliced[i], per_key,
            "sliced bulk differs from per-key at {i}"
        );
    }

    // None behaves exactly like bulk_read.
    let full = store.bulk_read_range(&keys, None).unwrap();
    assert_eq!(full, store.bulk_read(&keys).unwrap());
}

// ---- 1.7 discovery enumerations -------------------------------------------

// ---- 1.8 rename ------------------------------------------------------------

#[test]
fn rename_missing_key_is_not_found() {
    let mut store = create_store(None, true).unwrap();
    let missing = KeyIdentity {
        owner_id: 1,
        owner_category: OwnerCategory::Component,
        time_series_type: TimeSeriesType::SingleTimeSeries,
        name: "nope".into(),
        resolution: Some(Period::fixed(Duration::hours(1))),
        interval: None,
        features: Features::new(),
    };
    assert!(matches!(
        store.rename_time_series(&missing, "x"),
        Err(TimeSeriesError::NotFound)
    ));
}

// ---- 1.9 serde coverage ----------------------------------------------------

#[test]
fn period_serializes_as_iso8601_string() {
    assert_eq!(
        serde_json::to_string(&Period::fixed(Duration::hours(1))).unwrap(),
        "\"PT1H\""
    );
    assert_eq!(
        serde_json::to_string(&Period::Months(1)).unwrap(),
        "\"P1M\""
    );
    let back: Period = serde_json::from_str("\"P1Y\"").unwrap();
    assert_eq!(back, Period::Months(12));
}

#[test]
fn metadata_and_data_json_round_trip() {
    let mut store = create_store(None, true).unwrap();
    let key = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4))
                .with_units("MW")
                .with_application_data("QuadraticFunctionData"),
        ))
        .unwrap();

    let meta = store.get_metadata(key.identity()).unwrap();
    let json = serde_json::to_string(&meta).unwrap();
    let back: TimeSeriesMetadata = serde_json::from_str(&json).unwrap();
    assert_eq!(meta, back);

    // Each TimeSeriesData variant round-trips.
    let data = store.get_time_series(key.identity(), None).unwrap();
    let d_json = serde_json::to_string(&data).unwrap();
    let d_back: TimeSeriesData = serde_json::from_str(&d_json).unwrap();
    assert_eq!(data, d_back);

    let forecast = TimeSeriesData::Deterministic(det("f", 0.0));
    let f_json = serde_json::to_string(&forecast).unwrap();
    assert_eq!(
        forecast,
        serde_json::from_str::<TimeSeriesData>(&f_json).unwrap()
    );
}

/// serde must spell `UnitSystem` exactly as `as_str` does.
///
/// A serde→serde round trip cannot see a divergence here, but every *other*
/// surface can: the SQLite column, the proto, the C ABI, Python, Julia, and the
/// CLI descriptor all carry the `as_str` spelling and parse it back with
/// `UnitSystem::parse`. If the derive emitted the variant names instead, a value
/// serialized out of the core and handed to any of them would be rejected as an
/// unknown unit system. Both directions are pinned, since only deserialization
/// is what a foreign string actually hits.
#[test]
fn unit_system_serde_matches_its_as_str_spelling() {
    for variant in [UnitSystem::NaturalUnits, UnitSystem::ComponentBase] {
        let json = serde_json::to_string(&variant).unwrap();
        assert_eq!(json, format!("\"{}\"", variant.as_str()));
        assert_eq!(
            serde_json::from_str::<UnitSystem>(&json).unwrap(),
            variant,
            "the as_str spelling must deserialize back"
        );
        assert_eq!(UnitSystem::parse(variant.as_str()), Some(variant));
    }
}

// ===========================================================================
// Backend parity
//
// Everything above runs against the in-memory backend only. Rename, bulk /
// filtered delete, discovery, and copy all touch the *array* side as well as
// the catalog — reclaiming a slot, re-resolving a shared hash — so their
// in-memory result is not evidence about the persisted one. Each case below
// re-runs through `common::for_each_backend_mut`, which for HDF5 flushes,
// closes, and reopens read-write before the mutation, so the state being
// mutated came off disk.
//
// Series stay 3–4 steps long to keep the disk variants fast.
// ===========================================================================

fn add_sts(store: &mut infrastore_core::Store, owner: i64, name: &str, base: f64) -> TimeSeriesKey {
    store
        .add(AddRequest::new(
            owner,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts(name, base, 4)),
        ))
        .unwrap()
}

#[test]
fn rename_moves_the_association() {
    for_each_backend_mut(
        |store| add_sts(store, 1, "old", 10.0),
        |store, key, backend| {
            let new_key = store.rename_time_series(key.identity(), "new").unwrap();
            assert_eq!(new_key.name(), "new", "{backend}");
            assert!(
                matches!(
                    store.get_metadata(key.identity()),
                    Err(TimeSeriesError::NotFound)
                ),
                "{backend}: the old key must be gone"
            );
            // The renamed series still reads its original values: a rename must
            // not disturb the array it points at.
            let got = store.get_time_series(new_key.identity(), None).unwrap();
            assert_eq!(
                got.as_single().unwrap().data.to_f64_vec().unwrap(),
                vec![10.0, 11.0, 12.0, 13.0],
                "{backend}"
            );
            assert_eq!(store.num_distinct_arrays().unwrap(), 1, "{backend}");
        },
    );
}

#[test]
fn rename_collision_is_duplicate() {
    for_each_backend_mut(
        |store| {
            let a = add_sts(store, 1, "a", 1.0);
            add_sts(store, 1, "b", 2.0);
            a
        },
        |store, a, backend| {
            let err = store.rename_time_series(a.identity(), "b").unwrap_err();
            assert!(
                matches!(err, TimeSeriesError::DuplicateTimeSeries),
                "{backend}: got {err:?}"
            );
            // The failed rename left both series intact.
            let mut names = store.list_names(ListFilter::new()).unwrap();
            names.sort();
            assert_eq!(names, vec!["a", "b"], "{backend}");
        },
    );
}

#[test]
fn renaming_one_sharer_of_an_array_leaves_the_other_readable() {
    // Two owners with identical values share one content-addressed array.
    // Renaming one must not repoint or reclaim the shared array.
    for_each_backend_mut(
        |store| {
            let a = add_sts(store, 1, "shared", 5.0);
            let b = add_sts(store, 2, "shared", 5.0);
            (a, b)
        },
        |store, (a, b), backend| {
            assert_eq!(store.num_distinct_arrays().unwrap(), 1, "{backend}");
            let renamed = store.rename_time_series(a.identity(), "renamed").unwrap();
            assert_eq!(store.num_distinct_arrays().unwrap(), 1, "{backend}");

            let expected = vec![5.0, 6.0, 7.0, 8.0];
            for (key, who) in [(&renamed, "renamed"), (b, "untouched")] {
                let got = store.get_time_series(key.identity(), None).unwrap();
                assert_eq!(
                    got.as_single().unwrap().data.to_f64_vec().unwrap(),
                    expected,
                    "{backend}/{who}"
                );
            }
            // Both still reference the same array.
            let meta = store.get_metadata(renamed.identity()).unwrap();
            let (sts_refs, dst_refs) = store.count_array_references(&meta.data_hash).unwrap();
            assert_eq!((sts_refs, dst_refs), (2, 0), "{backend}");
        },
    );
}

#[test]
fn remove_by_filter_removes_matching_and_reclaims_arrays() {
    for_each_backend_mut(
        |store| {
            for owner in 1..=3 {
                add_sts(store, owner, "load", owner as f64 * 10.0);
            }
        },
        |store, (), backend| {
            assert_eq!(store.num_distinct_arrays().unwrap(), 3, "{backend}");
            let removed = store
                .remove_by_filter(ListFilter::new().owner_id(2))
                .unwrap();
            assert_eq!(removed, 1, "{backend}");
            assert_eq!(
                store.list_keys(ListFilter::new()).unwrap().len(),
                2,
                "{backend}"
            );
            assert_eq!(store.num_distinct_arrays().unwrap(), 2, "{backend}");
            // The survivors still read correctly after the reclaim.
            for owner in [1i64, 3] {
                let keys = store.list_keys(ListFilter::new().owner_id(owner)).unwrap();
                let got = store.get_time_series(keys[0].identity(), None).unwrap();
                let base = owner as f64 * 10.0;
                assert_eq!(
                    got.as_single().unwrap().data.to_f64_vec().unwrap(),
                    vec![base, base + 1.0, base + 2.0, base + 3.0],
                    "{backend}: owner {owner}"
                );
            }
        },
    );
}

#[test]
fn remove_bulk_rolls_back_on_missing_key() {
    for_each_backend_mut(
        |store| add_sts(store, 1, "load", 10.0),
        |store, k1, backend| {
            let missing = KeyIdentity {
                owner_id: 999,
                owner_category: OwnerCategory::Component,
                time_series_type: TimeSeriesType::SingleTimeSeries,
                name: "nope".into(),
                resolution: Some(Period::fixed(Duration::hours(1))),
                interval: None,
                features: Features::new(),
            };
            let err = store
                .remove_time_series_bulk(&[k1.identity(), &missing])
                .unwrap_err();
            assert!(matches!(err, TimeSeriesError::NotFound), "{backend}");
            // All-or-nothing: the valid key and its array both survive.
            assert_eq!(
                store.list_keys(ListFilter::new()).unwrap().len(),
                1,
                "{backend}"
            );
            assert_eq!(store.num_distinct_arrays().unwrap(), 1, "{backend}");
            assert!(
                store.get_time_series(k1.identity(), None).is_ok(),
                "{backend}"
            );
        },
    );
}

#[test]
fn remove_bulk_reclaims_a_shared_array_only_when_the_last_reference_goes() {
    for_each_backend_mut(
        |store| {
            let k1 = add_sts(store, 1, "load", 5.0);
            let k2 = add_sts(store, 2, "load", 5.0);
            (k1, k2)
        },
        |store, (k1, k2), backend| {
            assert_eq!(store.num_distinct_arrays().unwrap(), 1, "{backend}");

            // Removing one leaves the array alive for the other.
            assert_eq!(
                store.remove_time_series_bulk(&[k1.identity()]).unwrap(),
                1,
                "{backend}"
            );
            assert_eq!(store.num_distinct_arrays().unwrap(), 1, "{backend}");
            assert!(
                store.get_time_series(k2.identity(), None).is_ok(),
                "{backend}"
            );

            // Removing the last reference reclaims it.
            assert_eq!(
                store.remove_time_series_bulk(&[k2.identity()]).unwrap(),
                1,
                "{backend}"
            );
            assert_eq!(store.num_distinct_arrays().unwrap(), 0, "{backend}");
        },
    );
}

#[test]
fn discovery_enumerations() {
    for_each_backend(
        |store| {
            add_sts(store, 1, "load", 1.0);
            store
                .add(AddRequest::new(
                    2,
                    "Bus",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(sts("voltage", 2.0, 4)),
                ))
                .unwrap();
            store
                .add(AddRequest::new(
                    3,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::Deterministic(det("gen_forecast", 0.0)),
                ))
                .unwrap();
        },
        |store, (), backend| {
            // Intervals: only the forecast carries one (1h).
            assert_eq!(
                store.get_intervals(None).unwrap(),
                vec![Period::fixed(Duration::hours(1))],
                "{backend}"
            );
            assert!(
                store
                    .get_intervals(Some(TimeSeriesType::SingleTimeSeries))
                    .unwrap()
                    .is_empty(),
                "{backend}"
            );
            assert_eq!(
                store
                    .get_intervals(Some(TimeSeriesType::Deterministic))
                    .unwrap(),
                vec![Period::fixed(Duration::hours(1))],
                "{backend}"
            );

            // Names: distinct, sorted.
            assert_eq!(
                store.list_names(ListFilter::new()).unwrap(),
                vec!["gen_forecast", "load", "voltage"],
                "{backend}"
            );
            assert_eq!(
                store
                    .list_names(ListFilter::new().owner_type("Generator"))
                    .unwrap(),
                vec!["gen_forecast", "load"],
                "{backend}"
            );

            // Owner types: distinct, sorted.
            assert_eq!(
                store.list_owner_types(ListFilter::new()).unwrap(),
                vec!["Bus", "Generator"],
                "{backend}"
            );

            // Resolutions.
            assert_eq!(
                store.get_resolutions(None).unwrap(),
                vec![Period::fixed(Duration::hours(1))],
                "{backend}"
            );
        },
    );
}

#[test]
fn discovery_enumerations_on_an_empty_store() {
    for_each_backend(
        |_store| (),
        |store, (), backend| {
            assert!(store.get_intervals(None).unwrap().is_empty(), "{backend}");
            assert!(store.get_resolutions(None).unwrap().is_empty(), "{backend}");
            assert!(
                store.list_names(ListFilter::new()).unwrap().is_empty(),
                "{backend}"
            );
            assert!(
                store
                    .list_owner_types(ListFilter::new())
                    .unwrap()
                    .is_empty(),
                "{backend}"
            );
            assert!(
                store.list_keys(ListFilter::new()).unwrap().is_empty(),
                "{backend}"
            );
            assert_eq!(store.num_distinct_arrays().unwrap(), 0, "{backend}");
        },
    );
}

#[test]
fn copy_time_series_shares_the_array() {
    for_each_backend_mut(
        |store| add_sts(store, 1, "load", 7.0),
        |store, src, backend| {
            let copy = store
                .copy_time_series(src.identity(), 2, "Generator", Some("load_copy"))
                .unwrap();
            assert_eq!(copy.name(), "load_copy", "{backend}");

            // No array data was duplicated.
            assert_eq!(store.num_distinct_arrays().unwrap(), 1, "{backend}");
            let src_meta = store.get_metadata(src.identity()).unwrap();
            let copy_meta = store.get_metadata(copy.identity()).unwrap();
            assert_eq!(copy_meta.data_hash, src_meta.data_hash, "{backend}");
            assert_eq!(copy_meta.owner_id, 2, "{backend}");

            // Both read the same values.
            let expected = vec![7.0, 8.0, 9.0, 10.0];
            for key in [src, &copy] {
                let got = store.get_time_series(key.identity(), None).unwrap();
                assert_eq!(
                    got.as_single().unwrap().data.to_f64_vec().unwrap(),
                    expected,
                    "{backend}"
                );
            }

            // Copying onto an existing identity is a duplicate.
            let err = store
                .copy_time_series(src.identity(), 2, "Generator", Some("load_copy"))
                .unwrap_err();
            assert!(
                matches!(err, TimeSeriesError::DuplicateTimeSeries),
                "{backend}: got {err:?}"
            );

            // Removing the source leaves the copy readable (shared array kept).
            store.remove_time_series(src.identity()).unwrap();
            assert_eq!(store.num_distinct_arrays().unwrap(), 1, "{backend}");
            let got = store.get_time_series(copy.identity(), None).unwrap();
            assert_eq!(
                got.as_single().unwrap().data.to_f64_vec().unwrap(),
                expected,
                "{backend}"
            );
        },
    );
}

// ===========================================================================
// Error and accessor paths with no coverage
// ===========================================================================

/// A `KeyIdentity` that matches nothing.
fn missing_identity() -> KeyIdentity {
    KeyIdentity {
        owner_id: 999,
        owner_category: OwnerCategory::Component,
        time_series_type: TimeSeriesType::SingleTimeSeries,
        name: "nope".into(),
        resolution: Some(Period::fixed(Duration::hours(1))),
        interval: None,
        features: Features::new(),
    }
}

#[test]
fn reading_a_missing_key_is_not_found() {
    let mut store = create_store(None, true).unwrap();
    let missing = missing_identity();
    assert!(matches!(
        store.get_time_series(&missing, None),
        Err(TimeSeriesError::NotFound)
    ));
    // A time_range does not change the classification.
    assert!(matches!(
        store.get_time_series(&missing, Some((t0(), t0() + Duration::hours(2)))),
        Err(TimeSeriesError::NotFound)
    ));
    assert!(matches!(
        store.get_metadata(&missing),
        Err(TimeSeriesError::NotFound)
    ));
    assert!(!store.has_time_series(&missing).unwrap());
    assert!(matches!(
        store.remove_time_series(&missing),
        Err(TimeSeriesError::NotFound)
    ));
    // bulk_read is all-or-nothing on a missing member.
    assert!(matches!(
        store.bulk_read(&[&missing]),
        Err(TimeSeriesError::NotFound)
    ));
}

#[test]
fn reading_with_the_wrong_time_series_type_is_not_found() {
    // The stored row is a SingleTimeSeries. `time_series_type` is part of the
    // identity, so naming a different type addresses a series that does not
    // exist — it is a lookup miss, not a decode error.
    let mut store = create_store(None, true).unwrap();
    let key = add_sts(&mut store, 1, "load", 10.0);

    for wrong in [
        TimeSeriesType::NonSequentialTimeSeries,
        TimeSeriesType::Deterministic,
        TimeSeriesType::DeterministicSingleTimeSeries,
        TimeSeriesType::Probabilistic,
        TimeSeriesType::Scenarios,
    ] {
        let mut ident = key.identity().clone();
        ident.time_series_type = wrong;
        assert!(
            matches!(
                store.get_time_series(&ident, None),
                Err(TimeSeriesError::NotFound)
            ),
            "reading as {wrong:?} should miss"
        );
        assert!(!store.has_time_series(&ident).unwrap(), "{wrong:?}");
    }

    // The correct type still reads.
    assert!(store.get_time_series(key.identity(), None).is_ok());
}

#[test]
fn has_any_time_series_answers_owner_level_existence() {
    let mut store = create_store(None, true).unwrap();
    add_sts(&mut store, 1, "load", 10.0);

    let by_owner = |id| {
        ListFilter::new()
            .owner_id(id)
            .owner_category(OwnerCategory::Component)
    };
    assert!(store.has_any_time_series(by_owner(1)).unwrap());
    assert!(!store.has_any_time_series(by_owner(2)).unwrap());
    // Category is part of the owner identity: the same id in the other
    // category is a different owner.
    assert!(
        !store
            .has_any_time_series(
                ListFilter::new()
                    .owner_id(1)
                    .owner_category(OwnerCategory::SupplementalAttribute)
            )
            .unwrap()
    );

    // A type restriction narrows the probe.
    assert!(
        store
            .has_any_time_series(by_owner(1).time_series_type(TimeSeriesType::SingleTimeSeries))
            .unwrap()
    );
    assert!(
        !store
            .has_any_time_series(by_owner(1).time_series_type(TimeSeriesType::Deterministic))
            .unwrap()
    );

    // The empty filter asks "any association at all".
    assert!(store.has_any_time_series(ListFilter::new()).unwrap());
    store.clear_time_series(None).unwrap();
    assert!(!store.has_any_time_series(ListFilter::new()).unwrap());
}

#[test]
fn existence_probes_distinguish_features() {
    let mut store = create_store(None, true).unwrap();
    let mut high: Features = BTreeMap::new();
    high.insert("scenario".into(), FeatureValue::Str("high".into()));
    let mut low: Features = BTreeMap::new();
    low.insert("scenario".into(), FeatureValue::Str("low".into()));

    let key = store
        .add(
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4)),
            )
            .with_features(high.clone()),
        )
        .unwrap();

    // The keyed probe matches the feature set by content hash, so a key that
    // differs only in features is a miss.
    assert!(store.has_time_series(key.identity()).unwrap());
    let mut wrong = key.identity().clone();
    wrong.features = Features::new();
    assert!(!store.has_time_series(&wrong).unwrap());
    wrong.features = low.clone();
    assert!(!store.has_time_series(&wrong).unwrap());

    // The filtered probe's `features` predicate is a subset match. A complete
    // set is answered by the exact-hash fast path; a wrong value falls through
    // to the SQL subset probe and still misses.
    let by_owner = || {
        ListFilter::new()
            .owner_id(1)
            .owner_category(OwnerCategory::Component)
    };
    assert!(
        store
            .has_any_time_series(by_owner().features(high))
            .unwrap()
    );
    assert!(!store.has_any_time_series(by_owner().features(low)).unwrap());
    assert!(
        store
            .has_any_time_series(by_owner().features(Features::new()))
            .unwrap()
    );
}

#[test]
fn has_any_time_series_feature_subset_probe() {
    let mut store = create_store(None, true).unwrap();
    let mut stored: Features = BTreeMap::new();
    stored.insert("scenario".into(), FeatureValue::Str("high".into()));
    stored.insert("model_year".into(), FeatureValue::Int(2030));
    stored.insert("validated".into(), FeatureValue::Bool(true));
    store
        .add(
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4)),
            )
            .with_features(stored.clone()),
        )
        .unwrap();

    let by_owner = || {
        ListFilter::new()
            .owner_id(1)
            .owner_category(OwnerCategory::Component)
    };
    let feats = |pairs: &[(&str, FeatureValue)]| -> Features {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    };

    // Complete set: exact-hash fast path.
    assert!(
        store
            .has_any_time_series(by_owner().features(stored.clone()))
            .unwrap()
    );
    // Partial lists exercise the SQL subset fallback: every stored pair and
    // every 2-subset must match.
    for pair in [
        ("scenario", FeatureValue::Str("high".into())),
        ("model_year", FeatureValue::Int(2030)),
        ("validated", FeatureValue::Bool(true)),
    ] {
        assert!(
            store
                .has_any_time_series(by_owner().features(feats(&[pair])))
                .unwrap()
        );
    }
    assert!(
        store
            .has_any_time_series(by_owner().features(feats(&[
                ("scenario", FeatureValue::Str("high".into())),
                ("validated", FeatureValue::Bool(true)),
            ])))
            .unwrap()
    );

    // Wrong value, wrong key, and one-good-one-bad all miss.
    for bad in [
        feats(&[("scenario", FeatureValue::Str("low".into()))]),
        feats(&[("nonexistent", FeatureValue::Str("high".into()))]),
        feats(&[
            ("scenario", FeatureValue::Str("high".into())),
            ("model_year", FeatureValue::Int(2031)),
        ]),
    ] {
        assert!(!store.has_any_time_series(by_owner().features(bad)).unwrap());
    }

    // Value matching is kind-strict, like the in-memory subset filter:
    // Int(2030) is not Str("2030") or Float(2030.0).
    assert!(
        !store
            .has_any_time_series(
                by_owner().features(feats(&[("model_year", FeatureValue::Str("2030".into()))]))
            )
            .unwrap()
    );
    assert!(
        !store
            .has_any_time_series(
                by_owner().features(feats(&[("model_year", FeatureValue::Float(2030.0))]))
            )
            .unwrap()
    );

    // A subset probe scoped by the other filter columns still honors them.
    assert!(
        !store
            .has_any_time_series(
                ListFilter::new()
                    .owner_id(2)
                    .owner_category(OwnerCategory::Component)
                    .features(feats(&[("scenario", FeatureValue::Str("high".into()))]))
            )
            .unwrap()
    );
}

#[test]
fn empty_key_lists_are_no_ops_not_errors() {
    let mut store = create_store(None, true).unwrap();
    add_sts(&mut store, 1, "load", 10.0);

    assert!(store.bulk_read(&[]).unwrap().is_empty());
    assert!(store.bulk_read_range(&[], None).unwrap().is_empty());
    assert!(
        store
            .bulk_read_range(&[], Some((t0(), t0() + Duration::hours(2))))
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.remove_time_series_bulk(&[]).unwrap(), 0);
    // Nothing was disturbed.
    assert_eq!(store.list_keys(ListFilter::new()).unwrap().len(), 1);
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);
}

#[test]
fn get_array_by_hash_miss_is_not_found() {
    let mut store = create_store(None, true).unwrap();
    let key = add_sts(&mut store, 1, "load", 10.0);
    let meta = store.get_metadata(key.identity()).unwrap();

    // The real hash resolves.
    let arr = store.get_array_by_hash(&meta.data_hash).unwrap();
    assert_eq!(arr.to_f64_vec().unwrap(), vec![10.0, 11.0, 12.0, 13.0]);

    // An all-zero hash never exists.
    assert!(store.get_array_by_hash(&[0u8; 32]).is_err());
    // Counting references to a hash that is not stored is zero, not an error.
    assert_eq!(store.count_array_references(&[0u8; 32]).unwrap(), (0, 0));
}

// ---- replace_owner ---------------------------------------------------------

#[test]
fn replace_owner_moves_every_series_of_that_owner() {
    for_each_backend_mut(
        |store| {
            add_sts(store, 1, "load", 10.0);
            add_sts(store, 1, "voltage", 20.0);
            // A different owner that must be left alone.
            add_sts(store, 2, "load", 30.0);
        },
        |store, (), backend| {
            let moved = store.replace_owner(1, 7, OwnerCategory::Component).unwrap();
            assert_eq!(moved, 2, "{backend}");

            assert!(
                store
                    .list_keys(ListFilter::new().owner_id(1))
                    .unwrap()
                    .is_empty(),
                "{backend}: the old owner has nothing left"
            );
            let mut names: Vec<String> = store
                .list_keys(ListFilter::new().owner_id(7))
                .unwrap()
                .iter()
                .map(|k| k.name().to_string())
                .collect();
            names.sort();
            assert_eq!(names, vec!["load", "voltage"], "{backend}");

            // Owner 2 untouched, and its values still read.
            let other = store.list_keys(ListFilter::new().owner_id(2)).unwrap();
            assert_eq!(other.len(), 1, "{backend}");
            let got = store.get_time_series(other[0].identity(), None).unwrap();
            assert_eq!(
                got.as_single().unwrap().data.to_f64_vec().unwrap(),
                vec![30.0, 31.0, 32.0, 33.0],
                "{backend}"
            );

            // Arrays are shared by hash, so no array work happened.
            assert_eq!(store.num_distinct_arrays().unwrap(), 3, "{backend}");
        },
    );
}

#[test]
fn replace_owner_for_an_owner_with_no_series_is_zero() {
    let mut store = create_store(None, true).unwrap();
    add_sts(&mut store, 1, "load", 10.0);
    assert_eq!(
        store
            .replace_owner(42, 43, OwnerCategory::Component)
            .unwrap(),
        0
    );
    // Wrong category also matches nothing.
    assert_eq!(
        store
            .replace_owner(1, 43, OwnerCategory::SupplementalAttribute)
            .unwrap(),
        0
    );
    // The original owner still has its series.
    assert_eq!(
        store
            .list_keys(ListFilter::new().owner_id(1))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn replace_owner_onto_itself_is_a_no_op() {
    let mut store = create_store(None, true).unwrap();
    let key = add_sts(&mut store, 1, "load", 10.0);
    let moved = store.replace_owner(1, 1, OwnerCategory::Component).unwrap();
    assert_eq!(moved, 1, "the row is rewritten with the same value");
    assert!(store.get_time_series(key.identity(), None).is_ok());
}

#[test]
fn replace_owner_into_a_colliding_identity_is_a_duplicate() {
    // Owner 1 and owner 2 both have a series named "load" with the same
    // resolution and features, so moving 1 -> 2 would create two rows with one
    // identity. The unique index rejects it and the error surfaces as a typed
    // `DuplicateTimeSeries`, not a raw SQLite failure; because the whole call
    // runs in one transaction, nothing moves.
    let mut store = create_store(None, true).unwrap();
    let k1 = add_sts(&mut store, 1, "load", 10.0);
    let k2 = add_sts(&mut store, 2, "load", 20.0);

    let err = store
        .replace_owner(1, 2, OwnerCategory::Component)
        .unwrap_err();
    assert!(
        matches!(err, TimeSeriesError::DuplicateTimeSeries),
        "expected DuplicateTimeSeries, got {err:?}"
    );

    // Both series survive intact with their own values.
    for (key, base) in [(&k1, 10.0f64), (&k2, 20.0)] {
        let got = store.get_time_series(key.identity(), None).unwrap();
        assert_eq!(
            got.as_single().unwrap().data.to_f64_vec().unwrap(),
            vec![base, base + 1.0, base + 2.0, base + 3.0],
        );
    }
    assert_eq!(store.list_keys(ListFilter::new()).unwrap().len(), 2);
}

#[test]
fn replace_owner_is_rejected_on_a_read_only_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        let mut store = infrastore_core::create_store(Some(path.as_path()), false).unwrap();
        add_sts(&mut store, 1, "load", 10.0);
        store.flush().unwrap();
    }
    let mut store = infrastore_core::open_store(path.as_path(), true).unwrap();
    assert!(matches!(
        store.replace_owner(1, 2, OwnerCategory::Component),
        Err(TimeSeriesError::ReadOnlyStore)
    ));
}

// ---- read_only() / file_path() across all three store states --------------

#[test]
fn read_only_and_path_accessors_report_each_store_state() {
    // 1. In-memory: writable, no path.
    let mem = create_store(None, true).unwrap();
    assert!(!mem.read_only());
    assert_eq!(mem.file_path(), None);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");

    // 2. Created on disk: writable, path is the file it was created at.
    {
        let mut store = infrastore_core::create_store(Some(path.as_path()), false).unwrap();
        assert!(!store.read_only());
        assert_eq!(store.file_path(), Some(path.as_path()));
        add_sts(&mut store, 1, "load", 10.0);
        store.flush().unwrap();
    }

    // 3. Reopened read-write, then read-only.
    let rw = infrastore_core::open_store(path.as_path(), false).unwrap();
    assert!(!rw.read_only());
    assert_eq!(rw.file_path(), Some(path.as_path()));
    drop(rw);

    let ro = infrastore_core::open_store(path.as_path(), true).unwrap();
    assert!(ro.read_only());
    assert_eq!(ro.file_path(), Some(path.as_path()));
}

#[test]
fn a_read_only_store_rejects_every_write_entry_point() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let key = {
        let mut store = infrastore_core::create_store(Some(path.as_path()), false).unwrap();
        let key = add_sts(&mut store, 1, "load", 10.0);
        store.flush().unwrap();
        key
    };
    let mut store = infrastore_core::open_store(path.as_path(), true).unwrap();

    let is_ro = |r: infrastore_core::Result<()>| matches!(r, Err(TimeSeriesError::ReadOnlyStore));

    assert!(is_ro(
        store
            .add(AddRequest::new(
                2,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(sts("new", 1.0, 4)),
            ))
            .map(|_| ())
    ));
    assert!(is_ro(store.remove_time_series(key.identity())));
    assert!(is_ro(
        store.remove_time_series_bulk(&[key.identity()]).map(|_| ())
    ));
    assert!(is_ro(store.remove_by_filter(ListFilter::new()).map(|_| ())));
    assert!(is_ro(store.clear_time_series(None).map(|_| ())));
    assert!(is_ro(
        store
            .replace_owner(1, 2, OwnerCategory::Component)
            .map(|_| ())
    ));
    assert!(is_ro(
        store
            .copy_time_series(key.identity(), 2, "Generator", None)
            .map(|_| ())
    ));
    assert!(is_ro(
        store
            .rename_time_series(key.identity(), "other")
            .map(|_| ())
    ));
    assert!(is_ro(
        store
            .transform_single_time_series(
                Duration::hours(2),
                Duration::hours(1),
                None,
                None,
                Default::default()
            )
            .map(|_| ())
    ));

    // Reads still work, and nothing changed.
    assert!(store.get_time_series(key.identity(), None).is_ok());
    assert_eq!(store.list_keys(ListFilter::new()).unwrap().len(), 1);
}

/// The descriptive attributes — `element_type`, `units`, `quantity_kind`,
/// `unit_system`, `component_field`, `application_data` — live on the series,
/// not on the request,
/// so a read hands back what a write declared. Exercised against both backends,
/// so a persist/reopen cycle is covered too.
#[test]
fn series_descriptors_round_trip_on_the_struct() {
    for_each_backend_mut(
        |store| {
            let labeled = store
                .add(AddRequest::new(
                    1,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4))
                        .with_units("MW")
                        .with_quantity_kind("ActivePower")
                        .with_unit_system(UnitSystem::ComponentBase)
                        .with_component_field("max_active_power")
                        .with_application_data("Profile"),
                ))
                .unwrap();
            let bare = store
                .add(AddRequest::new(
                    2,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(sts("bare", 1.0, 4)),
                ))
                .unwrap();
            (labeled, bare)
        },
        |store, (labeled, bare), backend| {
            // The catalog row records them...
            let meta = store.get_metadata(labeled.identity()).unwrap();
            assert_eq!(meta.units.as_deref(), Some("MW"), "{backend}");
            assert_eq!(
                meta.quantity_kind.as_deref(),
                Some("ActivePower"),
                "{backend}"
            );
            assert_eq!(
                meta.unit_system,
                Some(UnitSystem::ComponentBase),
                "{backend}"
            );
            assert_eq!(
                meta.component_field.as_deref(),
                Some("max_active_power"),
                "{backend}"
            );
            assert_eq!(
                meta.application_data.as_deref(),
                Some("Profile"),
                "{backend}"
            );

            // ...and a read puts them back on the series itself.
            let data = store.get_time_series(labeled.identity(), None).unwrap();
            assert_eq!(data.units(), Some("MW"), "{backend}");
            assert_eq!(data.quantity_kind(), Some("ActivePower"), "{backend}");
            assert_eq!(
                data.unit_system(),
                Some(UnitSystem::ComponentBase),
                "{backend}"
            );
            assert_eq!(
                data.component_field(),
                Some("max_active_power"),
                "{backend}"
            );
            assert_eq!(data.application_data(), Some("Profile"), "{backend}");
            assert_eq!(data.element_type(), meta.element_type, "{backend}");

            // A slice is the same values over a shorter window, so it keeps them.
            let sliced = store
                .get_time_series(labeled.identity(), Some((t0(), t0() + Duration::hours(2))))
                .unwrap();
            assert_eq!(sliced.units(), Some("MW"), "{backend}");
            assert_eq!(sliced.quantity_kind(), Some("ActivePower"), "{backend}");
            assert_eq!(
                sliced.unit_system(),
                Some(UnitSystem::ComponentBase),
                "{backend}"
            );
            assert_eq!(
                sliced.component_field(),
                Some("max_active_power"),
                "{backend}"
            );
            assert_eq!(sliced.application_data(), Some("Profile"), "{backend}");

            // A bulk read takes the packed fast path, which builds its own
            // struct; it must agree with the per-key read rather than drop them.
            let ids = [labeled.identity()];
            let bulk = store.bulk_read(&ids).unwrap();
            assert_eq!(bulk[0].units(), Some("MW"), "{backend}");
            assert_eq!(bulk[0].quantity_kind(), Some("ActivePower"), "{backend}");
            assert_eq!(
                bulk[0].unit_system(),
                Some(UnitSystem::ComponentBase),
                "{backend}"
            );
            assert_eq!(
                bulk[0].component_field(),
                Some("max_active_power"),
                "{backend}"
            );
            assert_eq!(bulk[0].application_data(), Some("Profile"), "{backend}");

            // Unset stays unset -- the store never invents a label. In
            // particular an undeclared `unit_system` reads back as `None`, not
            // as `NaturalUnits`: nobody said these values were in natural
            // units, and pretending otherwise would be a claim the writer
            // never made.
            let plain = store.get_time_series(bare.identity(), None).unwrap();
            assert_eq!(plain.units(), None, "{backend}");
            assert_eq!(plain.quantity_kind(), None, "{backend}");
            assert_eq!(plain.unit_system(), None, "{backend}");
            assert_eq!(plain.component_field(), None, "{backend}");
            assert_eq!(plain.application_data(), None, "{backend}");
            assert_eq!(
                store.get_metadata(bare.identity()).unwrap().unit_system,
                None,
                "{backend}"
            );
        },
    );
}

/// Forecast types carry the same attributes through the same path.
#[test]
fn forecast_descriptors_round_trip_on_the_struct() {
    for_each_backend_mut(
        |store| {
            store
                .add(AddRequest::new(
                    1,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::Deterministic(det("fc", 1.0))
                        .with_units("MW")
                        .with_quantity_kind("ActivePower")
                        .with_unit_system(UnitSystem::NaturalUnits)
                        .with_component_field("rating"),
                ))
                .unwrap()
        },
        |store, key, backend| {
            let data = store.get_time_series(key.identity(), None).unwrap();
            assert_eq!(data.units(), Some("MW"), "{backend}");
            assert_eq!(data.quantity_kind(), Some("ActivePower"), "{backend}");
            assert_eq!(
                data.unit_system(),
                Some(UnitSystem::NaturalUnits),
                "{backend}"
            );
            assert_eq!(data.component_field(), Some("rating"), "{backend}");
        },
    );
}

/// `component_field` describes the values, so it sits outside the key and
/// outside both content hashes. Two series that differ only in it are the same
/// series said twice — and one array shared by owners that call it by different
/// field names is still stored once.
#[test]
fn component_field_is_descriptive_not_identity() {
    let mut store = create_store(None, true).unwrap();

    let key = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4))
                .with_component_field("max_active_power"),
        ))
        .unwrap();

    // Same identity, different field name: a duplicate, not a second series.
    let duplicate = store.add(AddRequest::new(
        1,
        "Generator",
        OwnerCategory::Component,
        TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4)).with_component_field("rating"),
    ));
    assert!(matches!(
        duplicate,
        Err(TimeSeriesError::DuplicateTimeSeries)
    ));

    // A different owner with the same values under a different field name
    // still content-addresses to the same array.
    let other = store
        .add(AddRequest::new(
            2,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4)).with_component_field("rating"),
        ))
        .unwrap();
    let first = store.get_metadata(key.identity()).unwrap();
    let second = store.get_metadata(other.identity()).unwrap();
    assert_eq!(first.data_hash, second.data_hash);
    assert_eq!(first.component_field.as_deref(), Some("max_active_power"));
    assert_eq!(second.component_field.as_deref(), Some("rating"));
}

/// A `DeterministicSingleTimeSeries` is a view of its source, so it inherits the
/// source's descriptors — including `component_field`, which describes the same
/// values seen through a forecast window.
#[test]
fn transformed_view_inherits_component_field() {
    let mut store = create_store(None, true).unwrap();
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 24))
                .with_component_field("max_active_power"),
        ))
        .unwrap();

    store
        .transform_single_time_series(
            Duration::hours(4),
            Duration::hours(1),
            None,
            None,
            Default::default(),
        )
        .unwrap();

    let derived = store
        .list_time_series(
            ListFilter::new().time_series_type(TimeSeriesType::DeterministicSingleTimeSeries),
        )
        .unwrap();
    assert_eq!(derived.len(), 1);
    assert_eq!(
        derived[0].component_field.as_deref(),
        Some("max_active_power")
    );
}
