//! The SiennaSchemas wire contract, enforced on the import path.
//!
//! [`super`] maps catalog rows to and from the schema's spelling; this module
//! checks that an incoming row actually *is* what the schema describes, before
//! any of it is mapped. Export needs no such check — it writes rows the store
//! produced — but import takes a document from somewhere else, and the
//! generated types can only say a field is missing, mistyped, or unknown. The
//! schema also knows the closed enums, the bounds, and the reserved feature
//! names.
//!
//! The check is `power_openapi_models::validate::Validator`, which carries the
//! strict bundle of [`power_openapi_models::SCHEMA_VERSION`] compiled in, so
//! validation needs no filesystem and no network.

use std::sync::OnceLock;

use power_openapi_models::validate::Validator;
use serde_json::Value;

use crate::error::{Result, TimeSeriesError};

/// The six `time_series_type` values the `TimeSeriesAssociation` `oneOf`
/// defines.
const TIME_SERIES_TYPES: &[&str] = &[
    "SingleTimeSeries",
    "NonSequentialTimeSeries",
    "Deterministic",
    "DeterministicSingleTimeSeries",
    "Probabilistic",
    "Scenarios",
];

fn compile(definition: &str) -> Validator {
    Validator::new(definition).unwrap_or_else(|e| {
        panic!("the compiled-in schema bundle has no usable {definition} definition: {e}")
    })
}

fn time_series_validator() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| compile("TimeSeriesAssociation"))
}

fn supplemental_attribute_validator() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| compile("SupplementalAttributeAssociation"))
}

/// Report every way `row` departs from the definition, or `Ok(())`.
///
/// All of them, not the first: a row that drifted usually drifted in more than
/// one place. `index` and `table` locate the row in the document, since the
/// schema's own error paths are relative to the row.
fn validate(validator: &Validator, row: &Value, index: usize, table: &str) -> Result<()> {
    let problems = validator.violations(row);
    if problems.is_empty() {
        return Ok(());
    }
    Err(TimeSeriesError::InvalidParameter(format!(
        "{table} row {index} does not match the SiennaSchemas {} wire contract: {}",
        power_openapi_models::SCHEMA_VERSION,
        problems.join("; ")
    )))
}

/// Check one time-series association row against `TimeSeriesAssociation`.
///
/// A row whose discriminator is absent or names something outside the six is
/// rejected first, naming the six: the `oneOf` would report only the closest
/// branch's `const`, which does not say what the row declared.
pub(super) fn check_time_series_row(row: &Value, index: usize) -> Result<()> {
    let declared = row.get("time_series_type").and_then(Value::as_str);
    if !declared.is_some_and(|t| TIME_SERIES_TYPES.contains(&t)) {
        return Err(TimeSeriesError::InvalidParameter(format!(
            "time_series_associations row {index} declares time_series_type {}, which is not \
             one of the six the wire contract defines ({})",
            declared.map_or_else(|| "nothing".to_string(), |t| format!("{t:?}")),
            TIME_SERIES_TYPES.join(", "),
        )));
    }
    validate(
        time_series_validator(),
        row,
        index,
        "time_series_associations",
    )
}

/// Check one supplemental-attribute association row against
/// `SupplementalAttributeAssociation`.
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
    fn an_unknown_field_is_named() {
        let mut row = single_time_series_row();
        row["owner_typ"] = Value::from("Typo");
        let err = check_time_series_row(&row, 1).expect_err("the row schema is closed");
        let message = err.to_string();
        assert!(message.contains("row 1"), "{message}");
        assert!(message.contains("owner_typ"), "{message}");
    }

    #[test]
    fn an_unknown_time_series_type_matches_no_member() {
        let mut row = single_time_series_row();
        row["time_series_type"] = Value::from("Sporadic");
        let err = check_time_series_row(&row, 0).expect_err("Sporadic is not one of the six");
        assert!(err.to_string().contains("Sporadic"), "{err}");
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
