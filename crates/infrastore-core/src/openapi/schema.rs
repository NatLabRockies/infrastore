//! The SiennaSchemas wire contract, enforced on the import path.
//!
//! [`super`] maps catalog rows to and from the schema's spelling; this module
//! holds the schemas themselves and checks that an incoming row actually *is*
//! what they describe, before any of it is mapped. Export needs no such check —
//! it writes rows the store produced — but import takes a document from
//! somewhere else, and a hand-written `Deserialize` is a much weaker contract
//! than the one SiennaSchemas publishes: it can only say a field is missing or
//! mistyped, never that a `Deterministic` arrived without its `interval`, that
//! `owner_category` is a word outside the enum, or that a row matches two of the
//! six types at once.
//!
//! # Vendored, not fetched
//!
//! The schemas are the checked-in copies at `sienna_schemas/` (see its
//! `SOURCE.md` for the upstream commit and `scripts/sync_sienna_schemas.sh` for
//! the refresh), embedded with [`include_str!`] so validation needs no
//! filesystem and no network — this repo's policy is that neither the build nor
//! CI fetches anything. They live under the crate rather than in `conformance/`
//! precisely because they are compiled in: a path outside the package would not
//! survive `cargo package`.
//!
//! # `$ref` resolution
//!
//! They are draft-07 and their `$ref`s are relative filesystem paths
//! (`common.json#/definitions/...`, `../Core/common.json#/definitions/...`),
//! not `$id`-anchored URLs. Each is compiled against a synthetic
//! `vendored:///<path>` base URI — not `file://`, which on Windows would have
//! to carry `canonicalize()`'s verbatim-path prefix and is not a valid URI.
//! RFC 3986 relative-reference resolution is scheme-agnostic, so the standard
//! resolution turns those `$ref`s into `vendored:` URIs naming the sibling
//! schemas, and [`Vendored`] serves them from the embedded map. This is the same
//! arrangement `tests/openapi_schema_conformance.rs` uses against the same
//! files.

use std::collections::HashMap;
use std::sync::OnceLock;

use jsonschema::{Draft, Retrieve, Uri, Validator};
use serde_json::Value;

use crate::error::{Result, TimeSeriesError};

/// Every vendored schema, keyed by its path relative to `sienna_schemas/` —
/// which is exactly the path a `$ref` resolves to.
const SCHEMAS: &[(&str, &str)] = &[
    (
        "TimeSeries/TimeSeriesAssociation.json",
        include_str!("../../sienna_schemas/TimeSeries/TimeSeriesAssociation.json"),
    ),
    (
        "TimeSeries/SingleTimeSeries.json",
        include_str!("../../sienna_schemas/TimeSeries/SingleTimeSeries.json"),
    ),
    (
        "TimeSeries/NonSequentialTimeSeries.json",
        include_str!("../../sienna_schemas/TimeSeries/NonSequentialTimeSeries.json"),
    ),
    (
        "TimeSeries/Deterministic.json",
        include_str!("../../sienna_schemas/TimeSeries/Deterministic.json"),
    ),
    (
        "TimeSeries/DeterministicSingleTimeSeries.json",
        include_str!("../../sienna_schemas/TimeSeries/DeterministicSingleTimeSeries.json"),
    ),
    (
        "TimeSeries/Probabilistic.json",
        include_str!("../../sienna_schemas/TimeSeries/Probabilistic.json"),
    ),
    (
        "TimeSeries/Scenarios.json",
        include_str!("../../sienna_schemas/TimeSeries/Scenarios.json"),
    ),
    (
        "TimeSeries/common.json",
        include_str!("../../sienna_schemas/TimeSeries/common.json"),
    ),
    (
        "Core/common.json",
        include_str!("../../sienna_schemas/Core/common.json"),
    ),
    (
        "Core/Associations/SupplementalAttributeAssociation.json",
        include_str!(
            "../../sienna_schemas/Core/Associations/SupplementalAttributeAssociation.json"
        ),
    ),
];

/// The per-type schema each `time_series_type` names, in the order
/// `TimeSeries/TimeSeriesAssociation.json`'s own `discriminator.mapping` lists
/// them. That wrapper is a `oneOf` over exactly these six, dispatched on this
/// field; a row is checked against the one its discriminator selects rather
/// than against the wrapper, because a failed `oneOf` reports only that nothing
/// matched — never *which* field of the type the row plainly meant to be is
/// wrong.
///
/// Each member pins `time_series_type` with `const`, so the six are mutually
/// exclusive by construction and dispatching loses none of the wrapper's force.
const TIME_SERIES_SCHEMAS: &[(&str, &str)] = &[
    ("SingleTimeSeries", "TimeSeries/SingleTimeSeries.json"),
    (
        "NonSequentialTimeSeries",
        "TimeSeries/NonSequentialTimeSeries.json",
    ),
    ("Deterministic", "TimeSeries/Deterministic.json"),
    (
        "DeterministicSingleTimeSeries",
        "TimeSeries/DeterministicSingleTimeSeries.json",
    ),
    ("Probabilistic", "TimeSeries/Probabilistic.json"),
    ("Scenarios", "TimeSeries/Scenarios.json"),
];

/// The schema a supplemental-attribute association row is checked against.
const SUPPLEMENTAL_ATTRIBUTE_SCHEMA: &str =
    "Core/Associations/SupplementalAttributeAssociation.json";

fn parsed() -> &'static HashMap<&'static str, Value> {
    static PARSED: OnceLock<HashMap<&'static str, Value>> = OnceLock::new();
    PARSED.get_or_init(|| {
        SCHEMAS
            .iter()
            .map(|(path, text)| {
                let value = serde_json::from_str(text)
                    .unwrap_or_else(|e| panic!("vendored schema {path} is not valid JSON: {e}"));
                (*path, value)
            })
            .collect()
    })
}

/// Serves a `$ref` from the embedded map. Every URI reaching it was produced by
/// resolving a relative reference against a `vendored:///<path>` base, so its
/// path is a key of [`SCHEMAS`] once the leading slash is dropped.
struct Vendored;

impl Retrieve for Vendored {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        if uri.scheme().as_str() != "vendored" {
            return Err(format!("expected a vendored: URI, got {uri}").into());
        }
        let path = uri.path().as_str().trim_start_matches('/');
        parsed()
            .get(path)
            .cloned()
            .ok_or_else(|| format!("no vendored schema at {path}").into())
    }
}

fn compile(path: &'static str) -> Validator {
    let schema = parsed()
        .get(path)
        .unwrap_or_else(|| panic!("{path} is not one of the vendored schemas"));
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_retriever(Vendored)
        .with_base_uri(format!("vendored:///{path}"))
        .build(schema)
        .unwrap_or_else(|e| panic!("vendored schema {path} does not compile: {e}"))
}

fn time_series_validators() -> &'static HashMap<&'static str, Validator> {
    static VALIDATORS: OnceLock<HashMap<&'static str, Validator>> = OnceLock::new();
    VALIDATORS.get_or_init(|| {
        TIME_SERIES_SCHEMAS
            .iter()
            .map(|(ts_type, path)| (*ts_type, compile(path)))
            .collect()
    })
}

fn supplemental_attribute_validator() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| compile(SUPPLEMENTAL_ATTRIBUTE_SCHEMA))
}

/// Report every way `row` departs from `schema`, or `Ok(())`.
///
/// All of them, not the first: a row that drifted usually drifted in more than
/// one place, and a caller fixing a document one error per run is the worst way
/// to spend a round trip. `index` and `table` locate the row in the document,
/// since the schema's own error paths are relative to the row.
fn validate(validator: &Validator, row: &Value, index: usize, table: &str) -> Result<()> {
    let problems: Vec<String> = validator
        .iter_errors(row)
        .map(|e| format!("{}: {e}", e.instance_path()))
        .collect();
    if problems.is_empty() {
        return Ok(());
    }
    Err(TimeSeriesError::InvalidParameter(format!(
        "{table} row {index} does not match the SiennaSchemas wire contract: {}",
        problems.join("; ")
    )))
}

/// Check one time-series association row against the schema its own
/// `time_series_type` selects.
///
/// A row whose discriminator is absent or names something outside the six is
/// rejected here, naming the six — the one check the per-type schemas cannot
/// make, since each only knows its own `const`.
pub(super) fn check_time_series_row(row: &Value, index: usize) -> Result<()> {
    let declared = row.get("time_series_type").and_then(Value::as_str);
    let Some(validator) = declared.and_then(|t| time_series_validators().get(t)) else {
        let known: Vec<&str> = TIME_SERIES_SCHEMAS.iter().map(|(t, _)| *t).collect();
        return Err(TimeSeriesError::InvalidParameter(format!(
            "time_series_associations row {index} declares time_series_type {}, which is not \
             one of the six the wire contract defines ({})",
            declared.map_or_else(|| "nothing".to_string(), |t| format!("{t:?}")),
            known.join(", "),
        )));
    };
    validate(validator, row, index, "time_series_associations")
}

/// Check one supplemental-attribute association row against
/// `Core/Associations/SupplementalAttributeAssociation.json`.
pub(super) fn check_supplemental_attribute_row(row: &Value, index: usize) -> Result<()> {
    validate(
        supplemental_attribute_validator(),
        row,
        index,
        "supplemental_attribute_associations",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_time_series_row() -> Value {
        serde_json::json!({
            "association_id": 1,
            "owner_id": 7,
            "owner_type": "ThermalStandard",
            "owner_category": "Component",
            "time_series_type": "SingleTimeSeries",
            "name": "max_active_power",
            "features": {},
            "uri": "abc",
            "element_type": "f64",
            "element_shape": [],
            "initial_timestamp": "2030-01-01T00:00:00Z",
            "resolution": "PT1H",
            "length": 24
        })
    }

    #[test]
    fn a_conforming_row_passes() {
        check_time_series_row(&single_time_series_row(), 0).expect("row conforms");
    }

    #[test]
    fn a_missing_required_field_is_named() {
        let mut row = single_time_series_row();
        row.as_object_mut().unwrap().remove("owner_type");
        let err = check_time_series_row(&row, 3).expect_err("owner_type is required");
        let message = err.to_string();
        assert!(message.contains("row 3"), "{message}");
        assert!(message.contains("owner_type"), "{message}");
    }

    #[test]
    fn an_unknown_time_series_type_matches_no_member() {
        let mut row = single_time_series_row();
        row["time_series_type"] = Value::from("Sporadic");
        check_time_series_row(&row, 0).expect_err("Sporadic is not one of the six types");
    }

    #[test]
    fn an_owner_category_outside_the_enum_is_rejected() {
        let mut row = single_time_series_row();
        row["owner_category"] = Value::from("Turbine");
        check_time_series_row(&row, 0).expect_err("owner_category is a closed enum");
    }

    #[test]
    fn a_forecast_without_its_timing_fields_is_rejected() {
        let mut row = single_time_series_row();
        row["time_series_type"] = Value::from("Deterministic");
        check_time_series_row(&row, 0)
            .expect_err("a Deterministic row needs horizon, interval, and count");
    }

    #[test]
    fn a_supplemental_attribute_row_round_trips_the_contract() {
        let row = serde_json::json!({
            "component_id": 7,
            "component_type": "ThermalStandard",
            "attribute_id": 12,
            "attribute_type": "GeometricDistributionForcedOutage"
        });
        check_supplemental_attribute_row(&row, 0).expect("row conforms");

        let mut missing = row.clone();
        missing.as_object_mut().unwrap().remove("attribute_id");
        check_supplemental_attribute_row(&missing, 0).expect_err("attribute_id is required");
    }
}
