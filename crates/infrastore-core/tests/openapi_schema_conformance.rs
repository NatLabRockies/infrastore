//! Validates the checked-in OpenAPI row fixtures
//! (`conformance/openapi_row_fixtures/`) against the SiennaSchemas wire
//! contract that the `power-openapi-models` SDK carries.
//!
//! Each time-series fixture is checked against the `TimeSeriesAssociation`
//! `oneOf` and must parse into the SDK type its `time_series_type` names; the
//! supplemental-attribute-association fixture is checked against
//! `SupplementalAttributeAssociation`. A failure here means a fixture drifted
//! from the wire contract, or the SDK moved to a schema release the fixtures
//! do not match.

use std::path::{Path, PathBuf};

use infrastore_core::types::metadata::RESERVED_FEATURE_NAMES;
use power_openapi_models::infrastructure_core::SupplementalAttributeAssociation;
use power_openapi_models::timeseries::TimeSeriesAssociation;
use power_openapi_models::validate::Validator;
use serde_json::Value;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../conformance/openapi_row_fixtures")
}

fn read_json(path: &Path) -> Value {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("parsing {} as JSON: {e}", path.display()))
}

fn fixture(name: &str) -> Value {
    read_json(&fixtures_dir().join(name))
}

fn assert_conforms(definition: &str, instance: &Value, fixture_name: &str) {
    let validator = Validator::new(definition).expect("the SDK bundle has the definition");
    let violations = validator.violations(instance);
    assert!(
        violations.is_empty(),
        "{fixture_name} does not conform to {definition} at SiennaSchemas {}: {violations:?}",
        power_openapi_models::SCHEMA_VERSION,
    );
}

/// One fixture file per time-series row type.
const TIME_SERIES_FIXTURES: &[&str] = &[
    "single_time_series.json",
    "non_sequential_time_series.json",
    "deterministic.json",
    "deterministic_single_time_series.json",
    "probabilistic.json",
    "scenarios.json",
];

fn parse_row(fixture_name: &str) -> TimeSeriesAssociation {
    serde_json::from_value(fixture(fixture_name))
        .unwrap_or_else(|e| panic!("{fixture_name} does not parse into the SDK type: {e}"))
}

#[test]
fn each_time_series_fixture_conforms_to_the_association_one_of() {
    for fixture_name in TIME_SERIES_FIXTURES {
        assert_conforms(
            "TimeSeriesAssociation",
            &fixture(fixture_name),
            fixture_name,
        );
        parse_row(fixture_name);
    }
}

#[test]
fn supplemental_attribute_association_fixture_conforms() {
    let name = "supplemental_attribute_association.json";
    let instance = fixture(name);
    assert_conforms("SupplementalAttributeAssociation", &instance, name);
    serde_json::from_value::<SupplementalAttributeAssociation>(instance)
        .unwrap_or_else(|e| panic!("{name} does not parse into the SDK type: {e}"));
}

/// The reserved feature names are maintained by hand in two repositories --
/// [`RESERVED_FEATURE_NAMES`] here and `TimeSeriesFeatures.propertyNames` in
/// the schema. Nothing ties the two together, which is what this test catches.
///
/// The schema reserves every field of a time-series row. So the names are the
/// keys of the six fixture rows, re-serialized through the SDK types so that a
/// defaulted field such as `interval_alignment` appears, plus the core's own
/// list. Each must be reserved on both sides. A name the schema reserves and
/// the core does not is the harmful direction: the store accepts the feature,
/// and the export it produces then fails validation on the features map.
#[test]
fn the_reserved_feature_names_match_the_schema() {
    let features = Validator::new("TimeSeriesFeatures").expect("the SDK bundle has the definition");
    let mut names: Vec<String> = RESERVED_FEATURE_NAMES
        .iter()
        .map(|n| n.to_string())
        .collect();
    for fixture_name in TIME_SERIES_FIXTURES {
        let row = serde_json::to_value(parse_row(fixture_name)).expect("the row serializes");
        names.extend(row.as_object().expect("a row is an object").keys().cloned());
    }
    names.sort();
    names.dedup();

    let schema_allows: Vec<&String> = names
        .iter()
        .filter(|n| {
            features
                .violations(&serde_json::json!({ n.as_str(): 1 }))
                .is_empty()
        })
        .collect();
    let core_allows: Vec<&String> = names
        .iter()
        .filter(|n| !RESERVED_FEATURE_NAMES.contains(&n.as_str()))
        .collect();

    assert!(
        schema_allows.is_empty() && core_allows.is_empty(),
        "the reserved feature names drifted from the schema.\n\
         a row field the core does not reserve: {core_allows:?} -- the store would accept \
         these features and then export a document the schema rejects; add them to \
         RESERVED_FEATURE_NAMES.\n\
         reserved by the core, not by the schema: {schema_allows:?} -- harmless for reads, \
         but the schema should carry them too; upstream them."
    );
}
