//! Tests for the OpenAPI-row JSON serde of the two association catalogs
//! (`crate::openapi`): the frozen wire contract (D3) and the time-series
//! reconcile (D4).
//!
//! The golden tests build a store whose rows reproduce the checked-in
//! fixtures at `conformance/openapi_row_fixtures/*.json` and assert the
//! export is value-equal to each one; the reconcile tests exercise the D4
//! policy matrix one cell at a time against a small dedicated store.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    FeatureValue, Features, ListFilter, OwnerCategory, ReconcilePolicy, Scenarios,
    SingleTimeSeries, Store, SupplementalAttributeAssociation, TimeSeriesData, TimeSeriesError,
    TransformPolicy, TypedArray, UnitSystem, create_store,
};

// ---- shared helpers ---------------------------------------------------------

fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
}

fn zeros(shape: Vec<usize>) -> TypedArray {
    let n: usize = shape.iter().product();
    TypedArray::from_f64(shape, &vec![0.0; n])
}

fn features_of(pairs: &[(&str, FeatureValue)]) -> Features {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

/// Load a checked-in fixture as a parsed [`serde_json::Value`].
fn fixture(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/../../conformance/openapi_row_fixtures/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading fixture {path}: {e}"));
    serde_json::from_str(&text).expect("fixture is valid JSON")
}

/// Build the store whose rows reproduce the six time-series fixtures, plus one
/// extra "sacrificial" `SingleTimeSeries` (owner 7, `max_active_power`, no
/// features or `component_field`) used only as the source
/// [`Store::transform_single_time_series`] derives the
/// `DeterministicSingleTimeSeries` fixture from — it is not itself one of the
/// six fixtures and is not asserted on directly.
///
/// Insertion order is deliberately fixed so the catalog's autoincrement `id`
/// column lands on known values: the DST source (1), the derived DST (2), then
/// the six fixtures in file order (3-8, skipping the SA fixture which lives in
/// a different table). The original hand-written fixtures used a different id
/// numbering (101-106); those were adjusted to the ids the store genuinely
/// assigns, per the task's fixture-correction allowance.
fn build_fixture_store() -> Store {
    let mut store = create_store(None, true).expect("in-memory store should initialize");

    // DST source: owner 7, "max_active_power", no features or component_field,
    // 24 hourly points from 2030-01-01 -- exactly enough for a 2h horizon / 1h
    // interval to derive 23 windows ((24 - 2) / 1 + 1 = 23), matching the
    // deterministic_single_time_series fixture. `transform_single_time_series`
    // clones every other descriptive column from the source verbatim
    // (`store.rs`'s `..src.clone()`), so units/quantity_kind/unit_system are
    // set here to match what the fixture expects on the derived row.
    let dst_source = SingleTimeSeries::new(
        ts(2030, 1, 1, 0, 0, 0),
        Duration::hours(1),
        zeros(vec![24]),
        "max_active_power",
    )
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits);
    store
        .add_time_series(
            7,
            "ThermalStandard",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(dst_source),
            Features::new(),
        )
        .expect("dst source should add");
    store
        .transform_single_time_series(
            Duration::hours(2),
            Duration::hours(1),
            None,
            None,
            TransformPolicy::default(),
        )
        .expect("transform should derive one DeterministicSingleTimeSeries row");

    // single_time_series.json
    let single = SingleTimeSeries::new(
        ts(2030, 1, 1, 0, 0, 0),
        Duration::hours(1),
        zeros(vec![8760]),
        "max_active_power",
    )
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits)
    .with_component_field("max_active_power");
    store
        .add_time_series(
            7,
            "ThermalStandard",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(single),
            features_of(&[
                ("scenario", FeatureValue::Str("high_load".into())),
                ("year", FeatureValue::Int(2030)),
            ]),
        )
        .expect("single_time_series fixture row should add");

    // non_sequential_time_series.json
    let timestamps: Vec<DateTime<Utc>> = (0..42)
        .map(|i| ts(2030, 3, 1, 0, 0, 0) + Duration::hours(i))
        .collect();
    let non_sequential = infrastore_core::NonSequentialTimeSeries::new(
        timestamps,
        TypedArray::from_slice(vec![42], &[false; 42]).expect("bool array should build"),
        "outage_events",
    )
    .expect("non_sequential fixture row should construct")
    .with_application_data(r#"{"module":"PowerSystems"}"#);
    store
        .add_time_series(
            12,
            "GeometricDistributionForcedOutage",
            OwnerCategory::SupplementalAttribute,
            TimeSeriesData::NonSequentialTimeSeries(non_sequential),
            Features::new(),
        )
        .expect("non_sequential_time_series fixture row should add");

    // deterministic.json: H = P1D / PT1H = 24, count = 365.
    let deterministic = infrastore_core::Deterministic::new(
        ts(2030, 1, 1, 0, 0, 0),
        Duration::hours(1),
        Duration::days(1),
        Duration::hours(1),
        365,
        zeros(vec![24, 365]),
        "max_active_power_forecast",
    )
    .expect("deterministic fixture row should construct")
    .with_units("pu")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::ComponentBase)
    .with_component_field("max_active_power");
    store
        .add_time_series(
            7,
            "ThermalStandard",
            OwnerCategory::Component,
            TimeSeriesData::Deterministic(deterministic),
            features_of(&[
                ("vintage", FeatureValue::Bool(true)),
                ("weight", FeatureValue::Float(0.5)),
            ]),
        )
        .expect("deterministic fixture row should add");

    // probabilistic.json: P = 3 percentiles, H = PT4H / PT15M = 16, count = 96,
    // per-step element shape [3] (coincidentally the same width as P).
    let probabilistic = infrastore_core::Probabilistic::new(
        ts(2030, 6, 15, 0, 0, 0),
        Duration::minutes(15),
        Duration::hours(4),
        Duration::hours(1),
        96,
        vec![5.0, 50.0, 95.0],
        zeros(vec![3, 16, 96, 3]),
        "power_forecast",
    )
    .expect("probabilistic fixture row should construct")
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits);
    store
        .add_time_series(
            9,
            "RenewableDispatch",
            OwnerCategory::Component,
            TimeSeriesData::Probabilistic(probabilistic),
            features_of(&[("model", FeatureValue::Str("ensemble".into()))]),
        )
        .expect("probabilistic fixture row should add");

    // scenarios.json: scenario_count = 5, H = PT4H / PT1H = 4, count = 24,
    // per-step element shape [5] (coincidentally the same width as
    // scenario_count).
    let scenarios = Scenarios::new(
        ts(2030, 6, 15, 0, 0, 0),
        Duration::hours(1),
        Duration::hours(4),
        Duration::hours(1),
        24,
        5,
        zeros(vec![5, 4, 24, 5]),
        "scenario_power",
    )
    .expect("scenarios fixture row should construct")
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits);
    store
        .add_time_series(
            9,
            "RenewableDispatch",
            OwnerCategory::Component,
            TimeSeriesData::Scenarios(scenarios),
            Features::new(),
        )
        .expect("scenarios fixture row should add");

    store
}

// ---- golden: time-series export --------------------------------------------

#[test]
fn export_reproduces_every_time_series_fixture() {
    let store = build_fixture_store();
    let json = store
        .export_time_series_associations_openapi("time_series.h5", &ListFilter::new())
        .expect("export should succeed");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("export is a JSON array");

    for name in [
        "single_time_series",
        "non_sequential_time_series",
        "deterministic",
        "deterministic_single_time_series",
        "probabilistic",
        "scenarios",
    ] {
        let want = fixture(name);
        assert!(
            rows.contains(&want),
            "export does not contain the {name} fixture row; export was {rows:#?}"
        );
    }
}

#[test]
fn export_sort_order_does_not_depend_on_insertion_order() {
    // Insert the same six rows as `build_fixture_store`, but shuffled: forecast
    // types before statics, and reversed within a couple of groups. The sorted
    // export must come out identical regardless.
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let scenarios = Scenarios::new(
        ts(2030, 6, 15, 0, 0, 0),
        Duration::hours(1),
        Duration::hours(4),
        Duration::hours(1),
        24,
        5,
        zeros(vec![5, 4, 24, 5]),
        "scenario_power",
    )
    .expect("scenarios should construct")
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits);
    store
        .add_time_series(
            9,
            "RenewableDispatch",
            OwnerCategory::Component,
            TimeSeriesData::Scenarios(scenarios),
            Features::new(),
        )
        .expect("scenarios should add");

    let single_a = SingleTimeSeries::new(
        ts(2030, 1, 1, 0, 0, 0),
        Duration::hours(1),
        zeros(vec![8760]),
        "max_active_power",
    )
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits)
    .with_component_field("max_active_power");
    store
        .add_time_series(
            7,
            "ThermalStandard",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(single_a),
            features_of(&[
                ("scenario", FeatureValue::Str("high_load".into())),
                ("year", FeatureValue::Int(2030)),
            ]),
        )
        .expect("single should add");

    let probabilistic = infrastore_core::Probabilistic::new(
        ts(2030, 6, 15, 0, 0, 0),
        Duration::minutes(15),
        Duration::hours(4),
        Duration::hours(1),
        96,
        vec![5.0, 50.0, 95.0],
        zeros(vec![3, 16, 96, 3]),
        "power_forecast",
    )
    .expect("probabilistic should construct")
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits);
    store
        .add_time_series(
            9,
            "RenewableDispatch",
            OwnerCategory::Component,
            TimeSeriesData::Probabilistic(probabilistic),
            features_of(&[("model", FeatureValue::Str("ensemble".into()))]),
        )
        .expect("probabilistic should add");

    let shuffled = store
        .export_time_series_associations_openapi("time_series.h5", &ListFilter::new())
        .expect("export should succeed");
    let shuffled_rows: Vec<serde_json::Value> =
        serde_json::from_str(&shuffled).expect("export is a JSON array");

    // Same three rows, inserted in ascending identity order this time.
    let mut ordered = create_store(None, true).expect("in-memory store should initialize");
    let single_b = SingleTimeSeries::new(
        ts(2030, 1, 1, 0, 0, 0),
        Duration::hours(1),
        zeros(vec![8760]),
        "max_active_power",
    )
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits)
    .with_component_field("max_active_power");
    ordered
        .add_time_series(
            7,
            "ThermalStandard",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(single_b),
            features_of(&[
                ("scenario", FeatureValue::Str("high_load".into())),
                ("year", FeatureValue::Int(2030)),
            ]),
        )
        .expect("single should add");
    let probabilistic_b = infrastore_core::Probabilistic::new(
        ts(2030, 6, 15, 0, 0, 0),
        Duration::minutes(15),
        Duration::hours(4),
        Duration::hours(1),
        96,
        vec![5.0, 50.0, 95.0],
        zeros(vec![3, 16, 96, 3]),
        "power_forecast",
    )
    .expect("probabilistic should construct")
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits);
    ordered
        .add_time_series(
            9,
            "RenewableDispatch",
            OwnerCategory::Component,
            TimeSeriesData::Probabilistic(probabilistic_b),
            features_of(&[("model", FeatureValue::Str("ensemble".into()))]),
        )
        .expect("probabilistic should add");
    let scenarios_b = Scenarios::new(
        ts(2030, 6, 15, 0, 0, 0),
        Duration::hours(1),
        Duration::hours(4),
        Duration::hours(1),
        24,
        5,
        zeros(vec![5, 4, 24, 5]),
        "scenario_power",
    )
    .expect("scenarios should construct")
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits);
    ordered
        .add_time_series(
            9,
            "RenewableDispatch",
            OwnerCategory::Component,
            TimeSeriesData::Scenarios(scenarios_b),
            Features::new(),
        )
        .expect("scenarios should add");

    let ordered_json = ordered
        .export_time_series_associations_openapi("time_series.h5", &ListFilter::new())
        .expect("export should succeed");
    let ordered_rows: Vec<serde_json::Value> =
        serde_json::from_str(&ordered_json).expect("export is a JSON array");

    // The `id` column tracks insertion order and legitimately differs between
    // the two stores; strip it before comparing the sort order itself.
    let without_ids = |rows: Vec<serde_json::Value>| -> Vec<serde_json::Value> {
        rows.into_iter()
            .map(|mut row| {
                row.as_object_mut().expect("row is an object").remove("id");
                row
            })
            .collect()
    };
    assert_eq!(without_ids(shuffled_rows), without_ids(ordered_rows));
}

// ---- golden: supplemental-attribute export/import round trip --------------

#[test]
fn export_reproduces_the_supplemental_attribute_fixture() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    store
        .add_supplemental_attribute_association(SupplementalAttributeAssociation {
            component_id: 7,
            component_type: "ThermalStandard".into(),
            attribute_id: 481,
            attribute_type: "GeometricDistributionForcedOutage".into(),
        })
        .expect("association should add");

    let json = store
        .export_supplemental_attribute_associations_openapi()
        .expect("export should succeed");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("export is an array");
    assert_eq!(rows, vec![fixture("supplemental_attribute_association")]);
}

#[test]
fn supplemental_attribute_export_import_round_trips_byte_equal() {
    let mut source = create_store(None, true).expect("in-memory store should initialize");
    source
        .add_supplemental_attribute_associations(vec![
            SupplementalAttributeAssociation {
                component_id: 1,
                component_type: "Generator".into(),
                attribute_id: 100,
                attribute_type: "GeographicInfo".into(),
            },
            SupplementalAttributeAssociation {
                component_id: 2,
                component_type: "Load".into(),
                attribute_id: 100,
                attribute_type: "GeographicInfo".into(),
            },
        ])
        .expect("associations should add");

    let exported = source
        .export_supplemental_attribute_associations_openapi()
        .expect("export should succeed");

    let mut target = create_store(None, true).expect("in-memory store should initialize");
    let inserted = target
        .import_supplemental_attribute_associations_openapi(&exported)
        .expect("import should succeed");
    assert_eq!(inserted, 2);

    let re_exported = target
        .export_supplemental_attribute_associations_openapi()
        .expect("export should succeed");
    assert_eq!(exported, re_exported);
}

// ---- error paths ------------------------------------------------------------

#[test]
fn sa_import_rejects_malformed_json() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let err = store
        .import_supplemental_attribute_associations_openapi("{not valid json")
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::Serde(_)));
}

#[test]
fn sa_import_rejects_unknown_fields() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let json = r#"[{"component_id":1,"component_type":"Generator","attribute_id":100,
        "attribute_type":"GeographicInfo","extra":"nope"}]"#;
    let err = store
        .import_supplemental_attribute_associations_openapi(json)
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::Serde(_)));
}

#[test]
fn sa_import_rolls_back_a_duplicate_within_the_batch() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let json = r#"[
        {"component_id":1,"component_type":"Generator","attribute_id":100,"attribute_type":"GeographicInfo"},
        {"component_id":1,"component_type":"Generator","attribute_id":100,"attribute_type":"GeographicInfo"}
    ]"#;
    let err = store
        .import_supplemental_attribute_associations_openapi(json)
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateAssociation(_)));
    let exported = store
        .export_supplemental_attribute_associations_openapi()
        .expect("export should succeed");
    assert_eq!(exported, "[]");
}

#[test]
fn reconcile_rejects_malformed_json() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let err = store
        .reconcile_time_series_associations_openapi(
            "{not valid json",
            ReconcilePolicy::Strict,
            None,
        )
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::Serde(_)));
}

#[test]
fn reconcile_rejects_unknown_fields() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let json = r#"[{"owner_id":1,"owner_type":"Generator","owner_category":"Component",
        "time_series_type":"SingleTimeSeries","name":"load","features":{},
        "element_type":"f64","element_shape":[],"bogus_field":true}]"#;
    let err = store
        .reconcile_time_series_associations_openapi(json, ReconcilePolicy::Strict, None)
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::Serde(_)));
}

// ---- reconcile: D4 policy matrix --------------------------------------------

/// One `SingleTimeSeries` row: owner 1 "Generator", name "load", 24 hourly
/// points from 2030-01-01, full descriptive set. The base every reconcile test
/// below either matches verbatim or perturbs one column of.
fn reconcile_fixture_store() -> Store {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let single = SingleTimeSeries::new(
        ts(2030, 1, 1, 0, 0, 0),
        Duration::hours(1),
        zeros(vec![24]),
        "load",
    )
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits)
    .with_component_field("load");
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(single),
            Features::new(),
        )
        .expect("base row should add");
    store
}

/// A clean row matching `reconcile_fixture_store`'s one row exactly.
fn clean_row_json() -> serde_json::Value {
    serde_json::json!({
        "owner_id": 1, "owner_type": "Generator", "owner_category": "Component",
        "time_series_type": "SingleTimeSeries", "name": "load", "features": {},
        "address": "store.h5", "element_type": "f64", "element_shape": [],
        "units": "MW", "quantity_kind": "ActivePower", "unit_system": "NATURAL_UNITS",
        "component_field": "load",
        "initial_timestamp": "2030-01-01T00:00:00Z", "resolution": "PT1H", "length": 24
    })
}

#[test]
fn reconcile_clean_match_is_a_no_op_under_either_policy() {
    for policy in [ReconcilePolicy::Strict, ReconcilePolicy::UpdateDescriptive] {
        let mut store = reconcile_fixture_store();
        let json = serde_json::to_string(&vec![clean_row_json()]).unwrap();
        let report = store
            .reconcile_time_series_associations_openapi(&json, policy, None)
            .unwrap_or_else(|e| panic!("{policy:?} should succeed on a clean match: {e}"));
        assert_eq!(report.matched, 1);
        assert_eq!(report.updated, 0);
        assert_eq!(report.missing_in_store, 0);
        assert_eq!(report.unmatched_in_store, 0);
        assert!(report.conflicts.is_empty());
    }
}

#[test]
fn reconcile_descriptive_drift_errors_under_strict() {
    let mut store = reconcile_fixture_store();
    let mut row = clean_row_json();
    row["units"] = serde_json::json!("kW");
    let json = serde_json::to_string(&vec![row]).unwrap();
    let err = store
        .reconcile_time_series_associations_openapi(&json, ReconcilePolicy::Strict, None)
        .unwrap_err();
    match err {
        TimeSeriesError::ReconcileConflict(msg) => {
            assert!(msg.contains("units"), "{msg}");
        }
        other => panic!("expected ReconcileConflict, got {other}"),
    }
}

#[test]
fn reconcile_descriptive_drift_is_applied_under_update_descriptive() {
    let mut store = reconcile_fixture_store();
    let mut row = clean_row_json();
    row["units"] = serde_json::json!("kW");
    row["component_field"] = serde_json::json!("net_load");
    let json = serde_json::to_string(&vec![row]).unwrap();
    let report = store
        .reconcile_time_series_associations_openapi(&json, ReconcilePolicy::UpdateDescriptive, None)
        .expect("update_descriptive should resolve descriptive drift");
    assert_eq!(report.matched, 1);
    assert_eq!(report.updated, 1);
    assert_eq!(report.unmatched_in_store, 0);
    assert_eq!(report.conflicts.len(), 1);

    // The rewrite is durable: exporting again reflects the JSON's values.
    let exported = store
        .export_time_series_associations_openapi("store.h5", &ListFilter::new())
        .expect("export should succeed");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&exported).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["units"], serde_json::json!("kW"));
    assert_eq!(rows[0]["component_field"], serde_json::json!("net_load"));
    // Geometry and identity are untouched by the rewrite.
    assert_eq!(rows[0]["length"], serde_json::json!(24));
}

#[test]
fn reconcile_geometry_drift_errors_under_both_policies() {
    for policy in [ReconcilePolicy::Strict, ReconcilePolicy::UpdateDescriptive] {
        let mut store = reconcile_fixture_store();
        let mut row = clean_row_json();
        row["length"] = serde_json::json!(25);
        let json = serde_json::to_string(&vec![row]).unwrap();
        let err = store
            .reconcile_time_series_associations_openapi(&json, policy, None)
            .unwrap_err();
        match err {
            TimeSeriesError::ReconcileConflict(msg) => {
                assert!(msg.contains("geometry drift"), "{policy:?}: {msg}");
                assert!(msg.contains("length"), "{policy:?}: {msg}");
            }
            other => panic!("{policy:?}: expected ReconcileConflict, got {other}"),
        }
    }
}

#[test]
fn reconcile_json_row_with_no_catalog_match_errors() {
    let mut store = reconcile_fixture_store();
    let mut row = clean_row_json();
    row["name"] = serde_json::json!("a_series_the_store_does_not_have");
    let json = serde_json::to_string(&vec![row]).unwrap();
    let err = store
        .reconcile_time_series_associations_openapi(&json, ReconcilePolicy::Strict, None)
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::ReconcileConflict(_)));
}

#[test]
fn reconcile_tolerates_and_counts_a_catalog_row_absent_from_the_json() {
    // An empty document against a non-empty catalog: nothing to match, but the
    // one catalog row is tolerated as a superset, not an error.
    let mut store = reconcile_fixture_store();
    let report = store
        .reconcile_time_series_associations_openapi("[]", ReconcilePolicy::Strict, None)
        .expect("an empty document should not fail on its own");
    assert_eq!(report.matched, 0);
    assert_eq!(report.unmatched_in_store, 1);
}

#[test]
fn reconcile_address_check_passes_when_it_matches() {
    let mut store = reconcile_fixture_store();
    let json = serde_json::to_string(&vec![clean_row_json()]).unwrap();
    let report = store
        .reconcile_time_series_associations_openapi(
            &json,
            ReconcilePolicy::Strict,
            Some("store.h5"),
        )
        .expect("matching address should not fail");
    assert_eq!(report.matched, 1);
}

#[test]
fn reconcile_address_check_fails_when_it_mismatches() {
    let mut store = reconcile_fixture_store();
    let json = serde_json::to_string(&vec![clean_row_json()]).unwrap();
    let err = store
        .reconcile_time_series_associations_openapi(
            &json,
            ReconcilePolicy::Strict,
            Some("other_store.h5"),
        )
        .unwrap_err();
    match err {
        TimeSeriesError::ReconcileConflict(msg) => {
            assert!(msg.contains("address"), "{msg}");
        }
        other => panic!("expected ReconcileConflict, got {other}"),
    }
}
