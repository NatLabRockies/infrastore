//! Reading an artifact back from its **arrays plus an OpenAPI document alone**,
//! with no `.sqlite` beside the HDF5 file.
//!
//! A consumer that already ships the catalog's association rows inside its own
//! JSON document (PowerSystems' `system.json` + `time_series.h5` bundle) carries
//! the SQLite half for nothing. These tests pin the two halves that make dropping
//! it work: [`Store::open_without_catalog`], which mints an empty catalog stamped
//! to match the arrays it finds, and the import that replays the document's rows
//! into it.
//!
//! All six types make the trip, `NonSequentialTimeSeries` included. Its axis is the
//! one thing the store cannot infer from the values — arrays are content-addressed,
//! so two irregular series with identical values on different axes share one stored
//! array, and only `timestamps_hash` tells them apart — so the wire form locates it
//! explicitly as `timestamps_uri`. The last tests pin that: the locator is required,
//! it must resolve, and a shared axis stays shared through the round trip.

mod common;

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    AddRequest, CatalogMode, Deterministic, ListFilter, NonSequentialTimeSeries, OwnerCategory,
    Probabilistic, Scenarios, SingleTimeSeries, Store, SupplementalAttributeAssociation,
    TimeSeriesData, TimeSeriesError, TimeSeriesMetadata, TimeSeriesType, TransformPolicy,
    TypedArray, UnitSystem,
};

fn ts(y: i32, mo: u32, d: u32, h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap()
}

fn zeros(shape: Vec<usize>) -> TypedArray {
    let n: usize = shape.iter().product();
    TypedArray::from_f64(shape, &vec![0.0; n])
}

/// A store carrying one row of every time-series type, plus two
/// supplemental-attribute attachments — the whole surface a document has to
/// carry back.
fn build_full_store(path: &std::path::Path) -> Store {
    let mut store = Store::create(Some(path), false).expect("store should create");

    // The `DeterministicSingleTimeSeries` source, and the derived view itself.
    let source = SingleTimeSeries::new(
        ts(2030, 1, 1, 0),
        Duration::hours(1),
        zeros(vec![24]),
        "max_active_power",
    )
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits);
    store
        .add(AddRequest::new(
            7,
            "ThermalStandard",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(source),
        ))
        .expect("dst source should add");
    store
        .transform_single_time_series(
            Duration::hours(2),
            Duration::hours(1),
            None,
            None,
            TransformPolicy::default(),
        )
        .expect("transform should derive a DeterministicSingleTimeSeries");

    let single = SingleTimeSeries::new(
        ts(2030, 1, 1, 0),
        Duration::hours(1),
        zeros(vec![48]),
        "load",
    )
    .with_component_field("max_active_power");
    store
        .add(AddRequest::new(
            8,
            "PowerLoad",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(single),
        ))
        .expect("single should add");

    // Two `NonSequentialTimeSeries` sharing one axis, and a third on its own, so the
    // round trip has to keep two distinct axes apart rather than collapse them.
    let shared: Vec<DateTime<Utc>> = (0..12)
        .map(|i| ts(2030, 3, 1, 0) + Duration::hours(i))
        .collect();
    for (owner, fill) in [(12, 1.0), (13, 3.0)] {
        let nsts = NonSequentialTimeSeries::new(
            shared.clone(),
            TypedArray::from_f64(vec![12], &[fill; 12]),
            "outage_events",
        )
        .expect("shared-axis nsts should construct")
        .with_application_data(r#"{"module":"PowerSystems"}"#);
        store
            .add(AddRequest::new(
                owner,
                "GeometricDistributionForcedOutage",
                OwnerCategory::SupplementalAttribute,
                TimeSeriesData::NonSequentialTimeSeries(nsts),
            ))
            .expect("shared-axis nsts should add");
    }
    let lone_axis: Vec<DateTime<Utc>> = (0..12)
        .map(|i| ts(2030, 9, 1, 0) + Duration::minutes(i * 7))
        .collect();
    let lone = NonSequentialTimeSeries::new(
        lone_axis,
        TypedArray::from_f64(vec![12], &[2.0; 12]),
        "maintenance",
    )
    .expect("lone-axis nsts should construct");
    store
        .add(AddRequest::new(
            14,
            "GeometricDistributionForcedOutage",
            OwnerCategory::SupplementalAttribute,
            TimeSeriesData::NonSequentialTimeSeries(lone),
        ))
        .expect("lone-axis nsts should add");

    let deterministic = Deterministic::new(
        ts(2030, 1, 1, 0),
        Duration::hours(1),
        Duration::days(1),
        Duration::hours(1),
        7,
        zeros(vec![24, 7]),
        "max_active_power_forecast",
    )
    .expect("deterministic should construct");
    store
        .add(AddRequest::new(
            9,
            "RenewableDispatch",
            OwnerCategory::Component,
            TimeSeriesData::Deterministic(deterministic),
        ))
        .expect("deterministic should add");

    let probabilistic = Probabilistic::new(
        ts(2030, 6, 15, 0),
        Duration::minutes(15),
        Duration::hours(4),
        Duration::hours(1),
        6,
        vec![5.0, 50.0, 95.0],
        zeros(vec![3, 16, 6]),
        "power_forecast",
    )
    .expect("probabilistic should construct");
    store
        .add(AddRequest::new(
            9,
            "RenewableDispatch",
            OwnerCategory::Component,
            TimeSeriesData::Probabilistic(probabilistic),
        ))
        .expect("probabilistic should add");

    let scenarios = Scenarios::new(
        ts(2030, 6, 15, 0),
        Duration::hours(1),
        Duration::hours(4),
        Duration::hours(1),
        6,
        5,
        zeros(vec![5, 4, 6]),
        "scenario_power",
    )
    .expect("scenarios should construct");
    store
        .add(AddRequest::new(
            9,
            "RenewableDispatch",
            OwnerCategory::Component,
            TimeSeriesData::Scenarios(scenarios),
        ))
        .expect("scenarios should add");

    store
        .add_supplemental_attribute_associations(vec![
            SupplementalAttributeAssociation {
                component_id: 7,
                component_type: "ThermalStandard".into(),
                attribute_id: 12,
                attribute_type: "Outage".into(),
                id: None,
            },
            SupplementalAttributeAssociation {
                component_id: 8,
                component_type: "PowerLoad".into(),
                attribute_id: 13,
                attribute_type: "Outage".into(),
                id: None,
            },
        ])
        .expect("attachments should add");

    store
}

/// Every row, ordered so two catalogs can be compared regardless of insertion
/// order. The `id` is deliberately kept: preserving it across the round trip is
/// the whole point of putting `association_id` on the wire.
fn sorted_rows(store: &Store) -> Vec<TimeSeriesMetadata> {
    let mut rows = store
        .list_metadata(ListFilter::new())
        .expect("listing should succeed");
    rows.sort_by_key(|row| row.id.expect("a stored row always carries its id").get());
    rows
}

/// Drop the catalog half (and its sidecars), leaving the arrays alone — the
/// "document plus arrays" bundle a consumer actually ships.
fn drop_catalog(path: &std::path::Path) {
    let sqlite = format!("{}.sqlite", path.display());
    std::fs::remove_file(&sqlite).expect("catalog should exist to be dropped");
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{sqlite}{suffix}"));
    }
}

#[test]
fn a_document_and_its_arrays_rebuild_the_whole_catalog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bundle.h5");

    let (ts_json, sa_json, before, values_before) = {
        let store = build_full_store(&path);
        let ts_json = store
            .export_time_series_associations_openapi(&ListFilter::new())
            .expect("time-series export should succeed");
        let sa_json = store
            .export_supplemental_attribute_associations_openapi()
            .expect("attachment export should succeed");
        let before = sorted_rows(&store);
        let values: Vec<TimeSeriesData> = before
            .iter()
            .map(|row| {
                store
                    .read_by_id(row.id.expect("stored row has an id"), Default::default())
                    .expect("every row should read before the round trip")
            })
            .collect();
        (ts_json, sa_json, before, values)
    };

    drop_catalog(&path);

    let mut store = Store::open_without_catalog(&path, CatalogMode::Attached)
        .expect("an artifact with no catalog should open for a rebuild");
    assert!(
        sorted_rows(&store).is_empty(),
        "a freshly minted catalog holds no rows",
    );

    let imported = store
        .import_time_series_associations_openapi(&ts_json)
        .expect("every exported row should import");
    assert_eq!(imported, before.len());
    store
        .import_supplemental_attribute_associations_openapi(&sa_json)
        .expect("attachment rows should import");

    assert_eq!(
        sorted_rows(&store),
        before,
        "the rebuilt catalog must be the one the document described",
    );
    for (row, expected) in sorted_rows(&store).iter().zip(&values_before) {
        let actual = store
            .read_by_id(row.id.expect("stored row has an id"), Default::default())
            .expect("every rebuilt row should read");
        assert_eq!(
            &actual, expected,
            "row {:?} read back differently",
            row.name
        );
    }
}

#[test]
fn a_rebuilt_catalog_reopens_as_an_ordinary_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bundle.h5");
    let ts_json = {
        let store = build_full_store(&path);
        store
            .export_time_series_associations_openapi(&ListFilter::new())
            .expect("export should succeed")
    };
    drop_catalog(&path);
    {
        let mut store = Store::open_without_catalog(&path, CatalogMode::Attached)
            .expect("catalogless open should succeed");
        store
            .import_time_series_associations_openapi(&ts_json)
            .expect("import should succeed");
    }
    // The generation stamp the mint copied off the arrays is what makes this
    // pass: a freshly minted stamp would pair a stamped catalog with an
    // array file claiming a different save.
    let reopened = Store::open(&path, false).expect("the rebuilt pair should open normally");
    assert_eq!(sorted_rows(&reopened).len(), 9);
}

#[test]
fn minting_a_catalog_over_an_existing_one_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bundle.h5");
    drop(build_full_store(&path));

    let Err(err) = Store::open_without_catalog(&path, CatalogMode::Attached) else {
        panic!("a catalog that is already there must not be discarded");
    };
    assert!(
        matches!(err, TimeSeriesError::StoreExists { .. }),
        "expected StoreExists, got {err}",
    );
}

/// The document locates an irregular series' axis rather than carrying it, so a
/// row that omits the locator is refused — the values alone cannot say which axis
/// the row is on (see `identical_values_on_two_axes_share_one_array`).
#[test]
fn an_irregular_row_without_its_axis_locator_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bundle.h5");
    let ts_json = irregular_bundle(&path);

    let mut rows: Vec<serde_json::Value> =
        serde_json::from_str(&ts_json).expect("export is a JSON array");
    assert!(
        rows[0]
            .as_object_mut()
            .expect("row is an object")
            .remove("timestamps_uri")
            .is_some(),
        "an exported irregular row must carry its axis locator",
    );
    let stripped = serde_json::to_string(&rows).expect("re-encode");

    drop_catalog(&path);
    let mut store = Store::open_without_catalog(&path, CatalogMode::Attached)
        .expect("catalogless open should succeed");
    let Err(err) = store.import_time_series_associations_openapi(&stripped) else {
        panic!("an irregular row must not be imported under a guessed time axis");
    };
    let message = err.to_string();
    assert!(
        message.contains("timestamps_uri"),
        "the refusal must name the missing locator, got: {message}",
    );
}

/// A locator naming an axis the store does not hold is a dangling reference, and
/// is refused for the same reason an absent array is.
#[test]
fn an_axis_locator_the_store_cannot_resolve_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bundle.h5");
    let ts_json = irregular_bundle(&path);

    let mut rows: Vec<serde_json::Value> =
        serde_json::from_str(&ts_json).expect("export is a JSON array");
    rows[0]["timestamps_uri"] = serde_json::Value::from("00".repeat(32));
    let rehomed = serde_json::to_string(&rows).expect("re-encode");

    drop_catalog(&path);
    let mut store = Store::open_without_catalog(&path, CatalogMode::Attached)
        .expect("catalogless open should succeed");
    let Err(err) = store.import_time_series_associations_openapi(&rehomed) else {
        panic!("an unresolvable axis must not be filed");
    };
    assert!(err.to_string().contains("does not hold"), "got: {err}",);
}

/// A cohort sharing one axis must come back sharing it, and the row on its own
/// axis must stay on its own — the locator is what keeps the two apart.
#[test]
fn a_shared_axis_survives_the_round_trip_intact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bundle.h5");
    let ts_json = {
        let store = build_full_store(&path);
        store
            .export_time_series_associations_openapi(&ListFilter::new())
            .expect("export should succeed")
    };
    drop_catalog(&path);
    let mut store = Store::open_without_catalog(&path, CatalogMode::Attached)
        .expect("catalogless open should succeed");
    store
        .import_time_series_associations_openapi(&ts_json)
        .expect("import should succeed");

    let mut axes: Vec<Vec<DateTime<Utc>>> = store
        .list_metadata(
            ListFilter::new()
                .time_series_type(infrastore_core::TimeSeriesType::NonSequentialTimeSeries),
        )
        .expect("listing should succeed")
        .iter()
        .map(|row| {
            match store
                .read_by_id(row.id.expect("stored row has an id"), Default::default())
                .expect("nsts should read")
            {
                TimeSeriesData::NonSequentialTimeSeries(series) => series.timestamps,
                other => panic!("expected a NonSequentialTimeSeries, got {other:?}"),
            }
        })
        .collect();
    assert_eq!(axes.len(), 3, "three irregular rows");
    axes.sort();
    axes.dedup();
    assert_eq!(
        axes.len(),
        2,
        "two distinct time axes, not one and not three"
    );
}

/// Why the axis has to be on the wire at all: one content-addressed array serves
/// both series, so the array cannot say which axis a row is on. If storage ever
/// stopped deduplicating across axes, `timestamps_uri` could become optional.
#[test]
fn identical_values_on_two_axes_share_one_array() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bundle.h5");
    let mut store = Store::create(Some(&path), false).expect("store should create");

    let values = || TypedArray::from_f64(vec![3], &[7.0, 8.0, 9.0]);
    let axis_a = vec![ts(2030, 1, 1, 0), ts(2030, 1, 1, 5), ts(2030, 1, 2, 9)];
    let axis_b = vec![ts(2031, 4, 1, 0), ts(2031, 4, 1, 6), ts(2031, 4, 3, 1)];
    for (owner, axis) in [(1, axis_a), (2, axis_b)] {
        let series =
            NonSequentialTimeSeries::new(axis, values(), "outage").expect("nsts should construct");
        store
            .add(AddRequest::new(
                owner,
                "Gen",
                OwnerCategory::Component,
                TimeSeriesData::NonSequentialTimeSeries(series),
            ))
            .expect("nsts should add");
    }

    let rows = sorted_rows(&store);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].data_hash, rows[1].data_hash,
        "two axes, one stored array — so the array cannot say which axis a row is on",
    );
}

/// A store at `path` holding one irregular series, and its exported rows.
fn irregular_bundle(path: &std::path::Path) -> String {
    let mut store = Store::create(Some(path), false).expect("store should create");
    let axis: Vec<DateTime<Utc>> = (0..12)
        .map(|i| ts(2030, 3, 1, 0) + Duration::hours(i))
        .collect();
    let series = NonSequentialTimeSeries::new(
        axis,
        TypedArray::from_f64(vec![12], &[1.0; 12]),
        "outage_events",
    )
    .expect("nsts should construct");
    store
        .add(AddRequest::new(
            12,
            "GeometricDistributionForcedOutage",
            OwnerCategory::SupplementalAttribute,
            TimeSeriesData::NonSequentialTimeSeries(series),
        ))
        .expect("nsts should add");
    store
        .export_time_series_associations_openapi(&ListFilter::new())
        .expect("export should succeed")
}

/// The write side: `persist_arrays_to` publishes a bundle of exactly one file,
/// which `open_without_catalog` then reads back with the document's rows. This
/// is the whole point of the pair — a consumer ships arrays plus its own JSON
/// and never carries a `.sqlite`.
#[test]
fn an_arrays_only_persist_round_trips_through_the_document() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source_path = dir.path().join("source.h5");
    let bundle = dir.path().join("bundle.h5");

    let (ts_json, sa_json, before) = {
        let mut store = build_full_store(&source_path);
        let ts_json = store
            .export_time_series_associations_openapi(&ListFilter::new())
            .expect("export should succeed");
        let sa_json = store
            .export_supplemental_attribute_associations_openapi()
            .expect("attachment export should succeed");
        let before = sorted_rows(&store);
        store
            .persist_arrays_to(&bundle)
            .expect("the array half should publish on its own");
        (ts_json, sa_json, before)
    };

    assert!(bundle.is_file(), "the arrays landed");
    assert!(
        !dir.path().join("bundle.h5.sqlite").exists(),
        "an arrays-only persist writes no catalog",
    );

    let mut restored = Store::open_without_catalog(&bundle, CatalogMode::Attached)
        .expect("the one-file bundle should open");
    restored
        .import_time_series_associations_openapi(&ts_json)
        .expect("every row should import");
    restored
        .import_supplemental_attribute_associations_openapi(&sa_json)
        .expect("attachment rows should import");

    assert_eq!(
        sorted_rows(&restored),
        before,
        "the bundle plus the document is the store it came from",
    );
}

/// The bundle is the live set: a series removed from the source leaves a dead
/// slot in the source file until `compact`, and `persist_arrays_to` writes only
/// what the catalog still names, so the restored store has nothing to reclaim.
#[test]
fn an_arrays_only_persist_leaves_dead_arrays_behind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source_path = dir.path().join("source.h5");
    let bundle = dir.path().join("bundle.h5");

    let mut store = build_full_store(&source_path);
    // A dense forecast: its array is a standalone dataset nothing else shares,
    // so removing it leaves a dead dataset in the source file.
    let victim = sorted_rows(&store)
        .into_iter()
        .find(|m| m.time_series_type == TimeSeriesType::Probabilistic)
        .and_then(|m| m.id)
        .expect("the full store holds a probabilistic forecast");
    store
        .remove_by_ids(&[victim])
        .expect("removing a stored series should succeed");
    let ts_json = store
        .export_time_series_associations_openapi(&ListFilter::new())
        .expect("export should succeed");
    let before = sorted_rows(&store);
    store
        .persist_arrays_to(&bundle)
        .expect("the array half should publish on its own");
    let source_report = store
        .compact()
        .expect("compacting the source should succeed");
    assert!(
        source_report.slots_reclaimed + source_report.datasets_dropped > 0,
        "the source still carried the removed series' array: {source_report:?}",
    );

    let mut restored = Store::open_without_catalog(&bundle, CatalogMode::Attached)
        .expect("the one-file bundle should open");
    restored
        .import_time_series_associations_openapi(&ts_json)
        .expect("every row should import");
    assert_eq!(sorted_rows(&restored), before, "the live rows round-trip");
    let report = restored
        .compact()
        .expect("compacting the restored bundle should succeed");
    assert_eq!(
        report.slots_reclaimed + report.datasets_dropped,
        0,
        "the bundle carried nothing the catalog does not name: {report:?}",
    );
}

/// A `.sqlite` beside the destination is paired with the file being replaced, so
/// publishing arrays under it would leave its rows dangling.
#[test]
fn an_arrays_only_persist_refuses_to_orphan_a_catalog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source_path = dir.path().join("source.h5");
    let occupied = dir.path().join("occupied.h5");
    drop(build_full_store(&occupied));

    let mut store = build_full_store(&source_path);
    let Err(err) = store.persist_arrays_to(&occupied) else {
        panic!("publishing arrays beside a live catalog must be refused");
    };
    assert!(
        matches!(err, TimeSeriesError::StoreExists { .. }),
        "expected StoreExists, got {err}",
    );
}
