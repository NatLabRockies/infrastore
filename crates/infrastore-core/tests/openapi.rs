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
    Dtype, ElementType, FeatureValue, Features, ListFilter, OwnerCategory, Period, Scenarios,
    SingleTimeSeries, Store, SupplementalAttributeAssociation, TimeReference, TimeSeriesData,
    TimeSeriesError, TimeSeriesId, TransformPolicy, TypedArray, UnitSystem, create_store,
};

// ---- shared helpers ---------------------------------------------------------

fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
}

/// Add and drop `n` throwaway rows, so the next id `store` assigns clears `n`.
/// See `full_surface_source` for why a document's ids have to start high.
fn advance_ids(store: &mut Store, n: usize) {
    for i in 0..n {
        let name = format!("__spacer{i}");
        let data = SingleTimeSeries::new(
            ts(2020, 1, 1, 0, 0, 0),
            Duration::hours(1),
            zeros(vec![2]),
            &name,
        );
        let spacer = store
            .add(infrastore_core::AddRequest::new(
                -1,
                "Spacer",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(data),
            ))
            .expect("spacer should add");
        store
            .remove_by_ids(&[spacer])
            .expect("spacer should remove");
    }
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

    // The schema requires `association_id`, so the fixtures carry one — but its
    // *value* is the store's own bookkeeping, depending on how many rows were
    // written before it. Comparing it would make these fixtures disagree with
    // the same rows exported from any differently-ordered store, so the
    // presence is asserted and the value dropped on both sides. The round trip
    // of the value itself is covered in `association_ids.rs`.
    let strip = |rows: &[serde_json::Value]| -> Vec<serde_json::Value> {
        rows.iter()
            .map(|row| {
                let mut row = row.clone();
                let object = row.as_object_mut().expect("each row is an object");
                assert!(
                    object.remove("association_id").is_some(),
                    "every exported row must carry its association_id",
                );
                row
            })
            .collect()
    };
    let content = strip(&rows);

    for name in [
        "single_time_series",
        "non_sequential_time_series",
        "deterministic",
        "deterministic_single_time_series",
        "probabilistic",
        "scenarios",
    ] {
        let want = strip(std::slice::from_ref(&fixture(name)))
            .pop()
            .expect("one row");
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
                    .remove("association_id")
                    .expect("every exported row carries its association_id");
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
            .list_metadata(ListFilter::new())
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
            .list_metadata(ListFilter::new())
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
        },
        infrastore_core::AddRequest {
            owner_id: 1,
            owner_type: "Generator".to_string(),
            owner_category: OwnerCategory::Component,
            data: TimeSeriesData::SingleTimeSeries(broken),
            features: Features::new(),
        },
    ];
    let err = store.add_time_series_bulk(items).unwrap_err();
    assert!(matches!(err, TimeSeriesError::InvalidParameter(_)), "{err}");
    assert!(
        store
            .list_metadata(ListFilter::new())
            .expect("list should succeed")
            .is_empty(),
        "a rejected bulk add must leave the catalog untouched, including the rows before the \
         offending one"
    );
}

// ---- full-surface time-series round trip ------------------------------------
//
// The golden tests above pin the *export* spelling of all six types, and the
// id tests in `association_ids.rs` pin one `Deterministic` row through an
// import. Neither covers the rest of the descriptive surface on the way back
// in: before these, no test had ever seen `units`, `quantity_kind`,
// `unit_system`, `component_field`, `application_data`, `percentiles`, a
// non-`f64` dtype, a non-scalar `element_type`, a calendar `Period`, a
// sub-second `initial_timestamp`, or three of the four `TimeReference`
// spellings survive `import_time_series_associations_openapi`. `Probabilistic`
// and `Scenarios` rows had never been imported at all.

/// One fully-populated row per importable [`TimeSeriesData`] variant, as
/// `(owner_id, owner_type, data, features)`.
///
/// Used twice: once to build the source store (under the real owners, with
/// explicit ids) and once to stock the import target with the same *arrays*
/// under owners of its own — arrays are content-addressed, so re-adding the
/// identical `TypedArray` under a different owner is exactly "the artifact
/// brought the values". Only `owner_id`/`owner_type` differ between the two
/// uses, so the hashes cannot drift apart.
///
/// The `DeterministicSingleTimeSeries` row is deliberately absent: it is
/// derived by [`Store::transform_single_time_series`] from the first entry
/// here, and shares that entry's array.
fn full_surface_rows() -> Vec<(i64, &'static str, TimeSeriesData, Features)> {
    // The DST source, and the one row carrying every descriptive field plus
    // all four `FeatureValue` kinds at once.
    let single = SingleTimeSeries::new(
        ts(2030, 1, 1, 0, 0, 0),
        Duration::hours(1),
        TypedArray::from_f64(vec![24], &(0..24).map(|i| i as f64).collect::<Vec<_>>()),
        "max_active_power",
    )
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits)
    .with_component_field("max_active_power")
    .with_application_data(r#"{"module":"PowerSystems"}"#)
    .with_time_reference(TimeReference::Zone("America/Denver".into()));

    // A tuple element type over a non-`f64` dtype, a fixed offset, the other
    // unit system, and an `initial_timestamp` with a millisecond remainder —
    // the branch of `format_initial_timestamp` that renders three fractional
    // digits, which every fixture's whole-second timestamp misses.
    let tupled = SingleTimeSeries::new(
        ts(2030, 6, 15, 12, 0, 0) + Duration::milliseconds(250),
        Duration::minutes(15),
        TypedArray::from_slice(vec![12, 2], &(0..24i32).collect::<Vec<_>>())
            .expect("i32 tuple array should build"),
        "reactive_power",
    )
    .with_element_type(ElementType::Tuple {
        arity: 2,
        dtype: Dtype::I32,
    })
    .with_units("pu")
    .with_quantity_kind("ReactivePower")
    .with_unit_system(UnitSystem::ComponentBase)
    .with_component_field("max_reactive_power")
    .with_time_reference(TimeReference::FixedOffset(-420));

    // The only calendar `Period` on the wire (`P1M`), a `bool` dtype, and the
    // wall-clock spelling.
    let sparse = SingleTimeSeries::new(
        ts(2030, 1, 1, 0, 0, 0),
        Period::months(1),
        TypedArray::from_slice(
            vec![12],
            &[
                true, false, true, false, true, false, true, false, true, false, true, false,
            ],
        )
        .expect("bool array should build"),
        "monthly_outage",
    )
    .with_time_reference(TimeReference::Zoneless);

    // horizon PT4H / resolution PT1H = 4 steps, count 6, per-step [2] for the
    // linear-function element type.
    let deterministic = infrastore_core::Deterministic::new(
        ts(2030, 1, 1, 0, 0, 0),
        Duration::hours(1),
        Duration::hours(4),
        Duration::hours(1),
        6,
        TypedArray::from_f64(
            vec![4, 6, 2],
            &(0..48).map(|i| i as f64).collect::<Vec<_>>(),
        ),
        "cost_forecast",
    )
    .expect("deterministic should construct")
    .with_element_type(ElementType::LinearFunction)
    .with_units("USD/MWh")
    .with_quantity_kind("EnergyPrice")
    .with_unit_system(UnitSystem::ComponentBase)
    .with_component_field("operation_cost")
    .with_application_data(r#"{"cost":"linear"}"#)
    .with_time_reference(TimeReference::Utc);

    // 3 percentiles, horizon PT1H / resolution PT15M = 4 steps, count 8,
    // per-step [3] for the quadratic-function element type.
    let probabilistic = infrastore_core::Probabilistic::new(
        ts(2030, 6, 15, 0, 0, 0),
        Duration::minutes(15),
        Duration::hours(1),
        Duration::hours(1),
        8,
        vec![5.0, 50.0, 95.0],
        TypedArray::from_f64(
            vec![3, 4, 8, 3],
            &(0..288).map(|i| i as f64).collect::<Vec<_>>(),
        ),
        "power_forecast",
    )
    .expect("probabilistic should construct")
    .with_element_type(ElementType::QuadraticFunction)
    .with_units("MW")
    .with_quantity_kind("ActivePower")
    .with_unit_system(UnitSystem::NaturalUnits)
    .with_component_field("max_active_power")
    .with_application_data(r#"{"ensemble":"weather"}"#)
    .with_time_reference(TimeReference::Utc);

    // scenario_count 5, horizon PT4H / resolution PT1H = 4 steps, count 6,
    // scalar `f32` with no per-step dims — and every optional descriptor left
    // unset, `time_reference` included, so the import has to keep "absent"
    // absent rather than filling in a default.
    let scenarios = Scenarios::new(
        ts(2030, 6, 15, 0, 0, 0),
        Duration::hours(1),
        Duration::hours(4),
        Duration::hours(1),
        6,
        5,
        TypedArray::from_slice(
            vec![5, 4, 6],
            &(0..120).map(|i| i as f32).collect::<Vec<_>>(),
        )
        .expect("f32 array should build"),
        "scenario_power",
    )
    .expect("scenarios should construct");

    vec![
        (
            7,
            "ThermalStandard",
            TimeSeriesData::SingleTimeSeries(single),
            features_of(&[
                ("scenario", FeatureValue::Str("high_load".into())),
                ("year", FeatureValue::Int(2030)),
                ("weight", FeatureValue::Float(0.5)),
                ("vintage", FeatureValue::Bool(true)),
            ]),
        ),
        (
            8,
            "RenewableDispatch",
            TimeSeriesData::SingleTimeSeries(tupled),
            Features::new(),
        ),
        (
            11,
            "Bus",
            TimeSeriesData::SingleTimeSeries(sparse),
            Features::new(),
        ),
        (
            7,
            "ThermalStandard",
            TimeSeriesData::Deterministic(deterministic),
            features_of(&[
                ("vintage", FeatureValue::Bool(true)),
                ("weight", FeatureValue::Float(0.5)),
            ]),
        ),
        (
            9,
            "RenewableDispatch",
            TimeSeriesData::Probabilistic(probabilistic),
            features_of(&[("model", FeatureValue::Str("ensemble".into()))]),
        ),
        (
            9,
            "RenewableDispatch",
            TimeSeriesData::Scenarios(scenarios),
            Features::new(),
        ),
    ]
}

/// The source store: the six rows above, plus the
/// `DeterministicSingleTimeSeries` derived from the first.
///
/// The counter is run up before anything real is added, so the ids land high.
/// An import refuses any id at or below the target catalog's high-water mark (a
/// deleted id must not be re-filable by hand), and the target is stocked with
/// anchor rows first, so source ids starting at 1 would collide with them. Ids
/// are assigned rather than chosen, so the way to place a document's rows above
/// an importing store's mark is to advance the exporter's counter.
fn full_surface_source() -> Store {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    advance_ids(&mut store, 1000);
    let mut rows = full_surface_rows().into_iter();

    // The DST source goes in alone and is transformed before anything else is
    // added: `transform_single_time_series` sweeps every SingleTimeSeries in
    // scope, so the two later statics must not be present yet.
    let (owner_id, owner_type, data, features) = rows.next().expect("at least one row");
    store
        .add(
            infrastore_core::AddRequest::new(owner_id, owner_type, OwnerCategory::Component, data)
                .with_features(features),
        )
        .expect("the DST source should add");
    store
        .transform_single_time_series(
            Duration::hours(2),
            Duration::hours(1),
            None,
            None,
            TransformPolicy::default(),
        )
        .expect("transform should derive one DeterministicSingleTimeSeries row");

    // The derived view took the next id; the rest continue from there.
    for (owner_id, owner_type, data, features) in rows {
        store
            .add(
                infrastore_core::AddRequest::new(
                    owner_id,
                    owner_type,
                    OwnerCategory::Component,
                    data,
                )
                .with_features(features),
            )
            .expect("fixture row should add");
    }
    store
}

/// A store holding every array the source's rows name, under owners of its
/// own, so only the rows are missing. `owner_type` is `"Anchor"` throughout,
/// which is how the assertions tell the pre-existing rows from the imported
/// ones in the target's own export.
fn full_surface_anchor_target() -> Store {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    for (index, (_, _, data, _)) in full_surface_rows().into_iter().enumerate() {
        store
            .add(infrastore_core::AddRequest::new(
                900 + index as i64,
                "Anchor",
                OwnerCategory::Component,
                data,
            ))
            .expect("anchor row should add");
    }
    store
}

/// Every optional field the wire form can carry must actually appear in an
/// export of the full-surface store — otherwise the round-trip assertion below
/// would pass vacuously on a field that silently stopped being written.
#[test]
fn the_full_surface_export_spells_every_optional_field() {
    let source = full_surface_source();
    let json = source
        .export_time_series_associations_openapi(&ListFilter::new())
        .expect("export should succeed");

    for spelling in [
        // All four `TimeReference` variants.
        r#""time_reference":"America/Denver""#,
        r#""time_reference":"utc""#,
        r#""time_reference":"zoneless""#,
        r#""time_reference":"-07:00""#,
        // Both unit systems, in the schema's SCREAMING_CASE.
        r#""unit_system":"NATURAL_UNITS""#,
        r#""unit_system":"COMPONENT_BASE""#,
        // The remaining descriptive fields.
        r#""units":"USD/MWh""#,
        r#""quantity_kind":"ReactivePower""#,
        r#""component_field":"operation_cost""#,
        r#""application_data":"{\"ensemble\":\"weather\"}""#,
        // Element types beyond the `f64` scalar default.
        r#""element_type":"tuple(2,i32)""#,
        r#""element_type":"linear_function""#,
        r#""element_type":"quadratic_function""#,
        r#""element_type":"bool""#,
        r#""element_type":"f32""#,
        // A calendar period, which no fixture carries.
        r#""resolution":"P1M""#,
        // The millisecond branch of `format_initial_timestamp`.
        r#""initial_timestamp":"2030-06-15T12:00:00.250Z""#,
        // Per-type geometry.
        r#""percentiles":[5.0,50.0,95.0]"#,
        r#""scenario_count":5"#,
        // All four `FeatureValue` kinds, in the plain scalar spelling.
        r#""scenario":"high_load""#,
        r#""year":2030"#,
        r#""weight":0.5"#,
        r#""vintage":true"#,
    ] {
        assert!(
            json.contains(spelling),
            "export is missing {spelling}; export was {json}"
        );
    }

    // A descriptor left unset is *absent*, never `null` — the distinction the
    // module docs call out for `unit_system`, and the one an importer that
    // fills in defaults would erase. The `Scenarios` row sets none of them.
    assert!(
        !json.contains("null"),
        "no field is ever written as null: {json}"
    );
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("export is a JSON array");
    let scenarios = rows
        .iter()
        .find(|row| row["name"] == "scenario_power")
        .expect("the Scenarios row is in the export");
    for absent in [
        "units",
        "quantity_kind",
        "unit_system",
        "component_field",
        "application_data",
        "time_reference",
    ] {
        assert!(
            scenarios.get(absent).is_none(),
            "{absent} is unset on the Scenarios row, so its object must omit it: {scenarios}"
        );
    }
}

/// The whole surface survives `export -> import -> export`, byte for byte, and
/// every row comes back as the identical [`TimeSeriesMetadata`].
///
/// This is the time-series analogue of
/// `supplemental_attribute_export_import_round_trips_byte_equal`, and it is the
/// assertion that keeps a newly-added descriptive column from being exported
/// but dropped on the way back in.
#[test]
fn the_full_surface_round_trips_byte_equal_with_identical_metadata() {
    let source = full_surface_source();
    let exported = source
        .export_time_series_associations_openapi(&ListFilter::new())
        .expect("export should succeed");

    let mut target = full_surface_anchor_target();
    let anchors = target
        .list_metadata(ListFilter::new())
        .expect("listing should succeed")
        .len();
    assert_eq!(
        target
            .import_time_series_associations_openapi(&exported)
            .expect("import should succeed"),
        7,
        "six added rows plus the derived DeterministicSingleTimeSeries",
    );

    // Re-export the target and drop the anchor rows: what is left is the same
    // rows in the same identity order, so the two documents must be identical.
    let reexported = target
        .export_time_series_associations_openapi(&ListFilter::new())
        .expect("re-export should succeed");
    let all_rows: Vec<serde_json::Value> =
        serde_json::from_str(&reexported).expect("re-export is a JSON array");
    assert_eq!(all_rows.len(), anchors + 7);
    let imported_rows: Vec<serde_json::Value> = all_rows
        .into_iter()
        .filter(|row| row["owner_type"] != "Anchor")
        .collect();
    assert_eq!(
        serde_json::to_string(&imported_rows).expect("rows re-serialize"),
        exported,
        "the document a store re-exports must be the document it imported",
    );

    // And the rows themselves, not just their wire spelling: every column the
    // catalog holds, compared by value, resolved through the id the document
    // carried.
    let originals = source
        .list_metadata(ListFilter::new())
        .expect("listing should succeed");
    assert_eq!(originals.len(), 7);
    for original in originals {
        let id = original.id.expect("a catalog row always carries its id");
        let imported = target
            .get_metadata_by_id(id)
            .expect("lookup should succeed")
            .unwrap_or_else(|| panic!("id {id} did not survive the import"));
        assert_eq!(imported, original, "row {id} changed across the round trip");
    }
}

// ---- the wire-form paths an infrastore-produced document never takes --------

/// A document from a producer that predates `array_shape` still imports, and
/// for a *static* row it reconstructs the catalog's shape exactly: `length`
/// and the per-step `element_shape` are both on the schema, and a static
/// series has no axes between them.
#[test]
fn an_import_without_array_shape_is_exact_for_a_static_row() {
    let source = full_surface_source();
    let exported = source
        .export_time_series_associations_openapi(&ListFilter::new())
        .expect("export should succeed");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&exported).expect("export is a JSON array");

    // The tuple-typed static: per-step dims [2], so `element_shape` is the one
    // field that has to carry them.
    let mut row = rows
        .iter()
        .find(|r| r["name"] == "reactive_power")
        .expect("the tuple-typed static is in the export")
        .clone();
    let original = source
        .get_metadata_by_id(TimeSeriesId(row["association_id"].as_i64().expect("an id")))
        .expect("lookup should succeed")
        .expect("the row exists");
    assert_eq!(original.element_shape, vec![2]);
    assert_eq!(original.length, Some(12));

    let stripped = row.as_object_mut().expect("a row is an object");
    assert!(stripped.remove("array_shape").is_some());
    let document = serde_json::to_string(&vec![row]).expect("the row re-serializes");

    let mut target = full_surface_anchor_target();
    assert_eq!(
        target
            .import_time_series_associations_openapi(&document)
            .expect("import should succeed"),
        1
    );
    let imported = target
        .get_metadata_by_id(original.id.expect("an id"))
        .expect("lookup should succeed")
        .expect("the row landed");
    assert_eq!(
        imported, original,
        "a static row's native shape is recoverable from length + element_shape alone",
    );
}

/// The same document shape for a *forecast* is a documented best effort, not an
/// identity: the schema carries no `length` for a `Deterministic`, and the
/// window axes between the time axis and the per-step dims are the caller's
/// layout convention rather than anything the store can rederive. The row still
/// lands — it is a legal document — but with the shape columns the schema could
/// express, which is what `array_shape` exists to avoid.
#[test]
fn an_import_without_array_shape_is_lossy_for_a_forecast_row() {
    let source = full_surface_source();
    let exported = source
        .export_time_series_associations_openapi(&ListFilter::new())
        .expect("export should succeed");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&exported).expect("export is a JSON array");

    let mut row = rows
        .iter()
        .find(|r| r["name"] == "cost_forecast")
        .expect("the deterministic row is in the export")
        .clone();
    let original = source
        .get_metadata_by_id(TimeSeriesId(row["association_id"].as_i64().expect("an id")))
        .expect("lookup should succeed")
        .expect("the row exists");
    // What the catalog holds: the native `[4, 6, 2]` minus its first axis.
    assert_eq!(original.length, Some(4));
    assert_eq!(original.element_shape, vec![6, 2]);

    let stripped = row.as_object_mut().expect("a row is an object");
    assert!(stripped.remove("array_shape").is_some());
    let document = serde_json::to_string(&vec![row]).expect("the row re-serializes");

    let mut target = full_surface_anchor_target();
    assert_eq!(
        target
            .import_time_series_associations_openapi(&document)
            .expect("import should succeed"),
        1
    );
    let imported = target
        .get_metadata_by_id(original.id.expect("an id"))
        .expect("lookup should succeed")
        .expect("the row landed");
    assert_ne!(
        imported, original,
        "the forecast shape cannot survive intact"
    );
    // Only the two shape columns differ; everything descriptive is intact.
    assert_eq!(
        imported.length, None,
        "no `length` on a Deterministic's schema"
    );
    assert_eq!(
        imported.element_shape,
        vec![2],
        "only the per-step dims the schema's element_shape carries",
    );
    assert_eq!(imported.units, original.units);
    assert_eq!(imported.time_reference, original.time_reference);
    assert_eq!(imported.count, original.count);
}

/// `uri` is a locator with no required format and `data_hash` is the content
/// hash, so a document from another producer may spell them differently. The
/// import resolves the array by `data_hash` first and falls back to `uri` only
/// when the row omits it — a distinction nothing could observe on an
/// infrastore-produced document, where the two are the same string.
#[test]
fn an_import_resolves_the_array_by_data_hash_before_uri() {
    let source = full_surface_source();
    let exported = source
        .export_time_series_associations_openapi(&ListFilter::new())
        .expect("export should succeed");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&exported).expect("export is a JSON array");
    let row = rows
        .iter()
        .find(|r| r["name"] == "monthly_outage")
        .expect("the sparse static is in the export")
        .clone();
    let original = source
        .get_metadata_by_id(TimeSeriesId(row["association_id"].as_i64().expect("an id")))
        .expect("lookup should succeed")
        .expect("the row exists");

    let with_uri = |uri: Option<&str>, data_hash: Option<&str>| -> String {
        let mut row = row.clone();
        let object = row.as_object_mut().expect("a row is an object");
        match uri {
            Some(value) => {
                object.insert("uri".into(), serde_json::Value::from(value));
            }
            None => {
                object.remove("uri");
            }
        }
        match data_hash {
            Some(value) => {
                object.insert("data_hash".into(), serde_json::Value::from(value));
            }
            None => {
                object.remove("data_hash");
            }
        }
        serde_json::to_string(&vec![row]).expect("the row re-serializes")
    };
    let hash = row["data_hash"].as_str().expect("a hex hash").to_string();

    // An opaque locator plus the real hash: resolved, and the row is identical.
    let mut target = full_surface_anchor_target();
    assert_eq!(
        target
            .import_time_series_associations_openapi(&with_uri(
                Some("s3://bucket/arrays/monthly_outage.h5"),
                Some(&hash),
            ))
            .expect("import should succeed"),
        1
    );
    assert_eq!(
        target
            .get_metadata_by_id(original.id.expect("an id"))
            .expect("lookup should succeed")
            .expect("the row landed"),
        original,
    );

    // No `data_hash` at all: the schema makes it optional, so a hash-shaped
    // `uri` is the fallback.
    let mut target = full_surface_anchor_target();
    assert_eq!(
        target
            .import_time_series_associations_openapi(&with_uri(Some(&hash), None))
            .expect("import should succeed"),
        1
    );
    assert_eq!(
        target
            .get_metadata_by_id(original.id.expect("an id"))
            .expect("lookup should succeed")
            .expect("the row landed"),
        original,
    );

    // A malformed `data_hash` is a document error, named as such — it is never
    // silently skipped in favor of the `uri` beside it.
    let mut target = full_surface_anchor_target();
    let err = target
        .import_time_series_associations_openapi(&with_uri(Some(&hash), Some("not-a-hash")))
        .expect_err("a malformed data_hash is refused");
    match err {
        TimeSeriesError::InvalidParameter(msg) => {
            assert!(msg.contains("data_hash"), "{msg}");
            assert!(msg.contains("64-character hex"), "{msg}");
        }
        other => panic!("expected InvalidParameter, got {other:?}"),
    }

    // Neither field names an array: refused with the message that says so,
    // rather than a dangling row.
    let mut target = full_surface_anchor_target();
    let err = target
        .import_time_series_associations_openapi(&with_uri(
            Some("s3://bucket/arrays/monthly_outage.h5"),
            None,
        ))
        .expect_err("a locator-only row is refused");
    match err {
        TimeSeriesError::InvalidParameter(msg) => {
            assert!(msg.contains("neither data_hash nor uri"), "{msg}");
        }
        other => panic!("expected InvalidParameter, got {other:?}"),
    }
}
