//! Tests for the OpenAPI-row JSON serde of the two association catalogs
//! (`crate::openapi`): the frozen wire contract, plus the add-boundary
//! rejection tests for a geometry mismatch between a series and its
//! association row (infrastore never reconciles the two — a mismatch fails
//! the addition instead).
//!
//! The golden tests build a store whose rows reproduce the checked-in
//! fixtures at `conformance/openapi_row_fixtures/*.json` and assert the
//! export is value-equal to each one.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    FeatureValue, Features, ListFilter, OwnerCategory, Scenarios, SingleTimeSeries, Store,
    SupplementalAttributeAssociation, TimeSeriesData, TimeSeriesError, TransformPolicy, TypedArray,
    UnitSystem, create_store,
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
        .export_time_series_associations_openapi(&ListFilter::new())
        .expect("export should succeed");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("export is a JSON array");

    // The fixtures are goldens of row *content*, so they carry no `id`: an id is
    // the store's own bookkeeping, and its value depends on how many rows were
    // written before it — pinning one would make this fixture disagree with the
    // same row exported from any differently-ordered store. That the export
    // emits an id at all is asserted below, and its round trip is covered in
    // `association_ids.rs`.
    let mut content: Vec<serde_json::Value> = rows.clone();
    for row in &mut content {
        let object = row.as_object_mut().expect("each row is an object");
        assert!(
            object.remove("id").is_some(),
            "every exported row must carry its catalog id",
        );
    }

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
            content.contains(&want),
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
        .export_time_series_associations_openapi(&ListFilter::new())
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
        .export_time_series_associations_openapi(&ListFilter::new())
        .expect("export should succeed");
    let ordered_rows: Vec<serde_json::Value> =
        serde_json::from_str(&ordered_json).expect("export is a JSON array");

    // The sort order should be identical regardless of insertion order — but
    // the ids are not, and must not be: an id records *when* a row was written,
    // so two stores built in different orders assign them differently. That is
    // the whole reason the id sits outside a series' identity, so this compares
    // the rows without it.
    let without_ids = |rows: Vec<serde_json::Value>| -> Vec<serde_json::Value> {
        rows.into_iter()
            .map(|mut row| {
                row.as_object_mut()
                    .expect("each row is an object")
                    .remove("id")
                    .expect("every exported row carries its id");
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
            id: None,
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
                id: None,
            },
            SupplementalAttributeAssociation {
                component_id: 2,
                component_type: "Load".into(),
                attribute_id: 100,
                attribute_type: "GeographicInfo".into(),
                id: None,
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

// ---- add-boundary rejection: geometry vs. association row -----------------
//
// Infrastore never reconciles a data array against its association row —
// see the module docs. A mismatch between the two is rejected at the add
// boundary instead, loudly and without writing anything.

#[test]
fn add_rejects_single_time_series_length_mismatch_and_leaves_store_untouched() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let mut single = SingleTimeSeries::new(
        ts(2030, 1, 1, 0, 0, 0),
        Duration::hours(1),
        zeros(vec![24]),
        "load",
    );
    // Declared length disagrees with the array itself, bypassing the
    // constructor's own derivation of `length` from `data`.
    single.length = 25;
    let err = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(single),
            Features::new(),
        )
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::InvalidParameter(_)), "{err}");
    assert!(
        store
            .list_time_series(ListFilter::new())
            .expect("list should succeed")
            .is_empty(),
        "a rejected add must leave the catalog untouched"
    );
}

#[test]
fn add_rejects_deterministic_shape_mismatch_and_leaves_store_untouched() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let mut deterministic = infrastore_core::Deterministic::new(
        ts(2030, 1, 1, 0, 0, 0),
        Duration::hours(1),
        Duration::days(1),
        Duration::hours(1),
        365,
        zeros(vec![24, 365]),
        "max_active_power_forecast",
    )
    .expect("deterministic should construct");
    // Declared `count` no longer agrees with the array's own window axis,
    // bypassing the constructor's own shape check.
    deterministic.count = 364;
    let err = store
        .add_time_series(
            7,
            "ThermalStandard",
            OwnerCategory::Component,
            TimeSeriesData::Deterministic(deterministic),
            Features::new(),
        )
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::InvalidParameter(_)), "{err}");
    assert!(
        store
            .list_time_series(ListFilter::new())
            .expect("list should succeed")
            .is_empty(),
        "a rejected add must leave the catalog untouched"
    );
}

#[test]
fn add_bulk_rejects_geometry_mismatch_and_leaves_the_whole_batch_untouched() {
    // A batch of two: a clean row and a mismatched one. The mismatch must
    // reject the whole batch, including the row that would otherwise have
    // added cleanly.
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let clean = SingleTimeSeries::new(
        ts(2030, 1, 1, 0, 0, 0),
        Duration::hours(1),
        zeros(vec![24]),
        "load",
    );
    let mut broken = SingleTimeSeries::new(
        ts(2030, 1, 1, 0, 0, 0),
        Duration::hours(1),
        zeros(vec![24]),
        "generation",
    );
    broken.length = 10;
    let items = vec![
        infrastore_core::AddRequest {
            owner_id: 1,
            owner_type: "Generator".to_string(),
            owner_category: OwnerCategory::Component,
            data: TimeSeriesData::SingleTimeSeries(clean),
            features: Features::new(),
            id: None,
        },
        infrastore_core::AddRequest {
            owner_id: 1,
            owner_type: "Generator".to_string(),
            owner_category: OwnerCategory::Component,
            data: TimeSeriesData::SingleTimeSeries(broken),
            features: Features::new(),
            id: None,
        },
    ];
    let err = store.add_time_series_bulk(items).unwrap_err();
    assert!(matches!(err, TimeSeriesError::InvalidParameter(_)), "{err}");
    assert!(
        store
            .list_time_series(ListFilter::new())
            .expect("list should succeed")
            .is_empty(),
        "a rejected bulk add must leave the catalog untouched, including the rows before the \
         offending one"
    );
}
