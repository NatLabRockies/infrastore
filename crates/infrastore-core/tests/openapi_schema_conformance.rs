//! Validates the checked-in OpenAPI row fixtures
//! (`conformance/openapi_row_fixtures/`) against the vendored SiennaSchemas
//! wire-format specs (`conformance/sienna_schemas/`).
//!
//! Each time-series fixture is checked against its own per-type schema AND
//! against `TimeSeries/TimeSeriesAssociation.json`'s `oneOf` wrapper; the
//! supplemental-attribute-association fixture is checked against
//! `Core/Associations/SupplementalAttributeAssociation.json`. A failure here
//! means either a fixture drifted from the wire contract or the vendored
//! schemas are stale — refresh with `scripts/sync_sienna_schemas.sh` and see
//! `conformance/sienna_schemas/SOURCE.md`.
//!
//! The schemas are draft-07 and their `$ref`s are relative filesystem paths
//! (`common.json#/definitions/...`, `../Core/common.json#/definitions/...`),
//! not `$id`-anchored URLs. Each schema is compiled with a synthetic
//! `vendored:///<path relative to schemas_dir()>` base URI — not `file://`,
//! which on Windows would have to carry `canonicalize()`'s verbatim-path
//! prefix (`\\?\D:\...`) and is not a valid URI. RFC 3986 relative-reference
//! resolution is scheme-agnostic, so the library's standard resolution still
//! turns those `$ref`s into `vendored:` URIs pointing at the sibling vendored
//! files; a custom `Retrieve` impl below then splits the resolved URI's path
//! into segments and rejoins them onto `schemas_dir()` with native path
//! separators, then reads and parses whichever file that names. This needs
//! no `resolve-file`/`resolve-http` feature and no network access — see the
//! `jsonschema` dependency comment in `infrastore-core/Cargo.toml`.

use std::path::{Path, PathBuf};

use jsonschema::{Draft, Retrieve, Uri, Validator};
use serde_json::Value;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root exists")
}

fn schemas_dir() -> PathBuf {
    repo_root().join("conformance/sienna_schemas")
}

fn fixtures_dir() -> PathBuf {
    repo_root().join("conformance/openapi_row_fixtures")
}

/// Resolves every relative `$ref` by reading straight off the vendored
/// schema tree on disk; see the module doc for how the base URIs line up.
struct VendoredRetriever;

impl Retrieve for VendoredRetriever {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        if uri.scheme().as_str() != "vendored" {
            return Err(format!("expected a vendored: URI, got {uri}").into());
        }
        let mut path = schemas_dir();
        for segment in uri.path().as_str().split('/') {
            if segment.is_empty() {
                continue;
            }
            if segment == ".." {
                return Err(
                    format!("refusing to escape the vendored schema tree for {uri}").into(),
                );
            }
            path.push(segment);
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("reading vendored schema referenced as {uri}: {e}"))?;
        Ok(serde_json::from_str(&text)?)
    }
}

/// Builds a synthetic `vendored:///a/b/c.json` base URI from `path`'s
/// location relative to `schemas_dir()`, joined with forward slashes
/// regardless of platform. No `canonicalize()`: on Windows that returns a
/// verbatim path (`\\?\D:\...`) that is not valid inside a URI.
fn base_uri_for(path: &Path) -> String {
    let rel = path
        .strip_prefix(schemas_dir())
        .unwrap_or_else(|e| panic!("{} is not under schemas_dir(): {e}", path.display()));
    let joined = rel
        .components()
        .map(|c| {
            c.as_os_str()
                .to_str()
                .unwrap_or_else(|| panic!("non-UTF-8 path component in {}", path.display()))
        })
        .collect::<Vec<_>>()
        .join("/");
    format!("vendored:///{joined}")
}

fn read_json(path: &Path) -> Value {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("parsing {} as JSON: {e}", path.display()))
}

/// Compiles the schema at `path` against draft-07, resolving its `$ref`s
/// against the vendored tree.
fn compile(path: &Path) -> Validator {
    let schema = read_json(path);
    jsonschema::options()
        .with_draft(Draft::Draft7)
        .with_retriever(VendoredRetriever)
        .with_base_uri(base_uri_for(path))
        .build(&schema)
        .unwrap_or_else(|e| panic!("compiling schema {}: {e}", path.display()))
}

fn fixture(name: &str) -> Value {
    read_json(&fixtures_dir().join(name))
}

/// Validates `instance` (from fixture file `fixture_name`) against
/// `validator` (compiled from `schema_name`), panicking with the offending
/// field and error on the first mismatch.
fn assert_conforms(validator: &Validator, instance: &Value, fixture_name: &str, schema_name: &str) {
    if let Err(error) = validator.validate(instance) {
        panic!(
            "{fixture_name} does not conform to {schema_name}: {error} (at instance path \
             '{}', schema path '{}'). If the fixture is right, the vendored schema is stale — \
             refresh with scripts/sync_sienna_schemas.sh.",
            error.instance_path(),
            error.schema_path(),
        );
    }
}

/// One (fixture file, `time_series_type` schema file) pair for every
/// time-series row type. Kept as one list so both the per-type and the
/// `TimeSeriesAssociation` `oneOf` checks below iterate the same set.
fn time_series_cases() -> Vec<(&'static str, &'static str)> {
    vec![
        ("single_time_series.json", "SingleTimeSeries.json"),
        (
            "non_sequential_time_series.json",
            "NonSequentialTimeSeries.json",
        ),
        ("deterministic.json", "Deterministic.json"),
        (
            "deterministic_single_time_series.json",
            "DeterministicSingleTimeSeries.json",
        ),
        ("probabilistic.json", "Probabilistic.json"),
        ("scenarios.json", "Scenarios.json"),
    ]
}

#[test]
fn each_time_series_fixture_conforms_to_its_own_schema() {
    for (fixture_name, schema_name) in time_series_cases() {
        let validator = compile(&schemas_dir().join("TimeSeries").join(schema_name));
        let instance = fixture(fixture_name);
        assert_conforms(&validator, &instance, fixture_name, schema_name);
    }
}

#[test]
fn each_time_series_fixture_conforms_to_the_association_one_of() {
    let validator = compile(
        &schemas_dir()
            .join("TimeSeries")
            .join("TimeSeriesAssociation.json"),
    );
    for (fixture_name, _) in time_series_cases() {
        let instance = fixture(fixture_name);
        assert_conforms(
            &validator,
            &instance,
            fixture_name,
            "TimeSeriesAssociation.json (oneOf)",
        );
    }
}

#[test]
fn supplemental_attribute_association_fixture_conforms() {
    let validator = compile(
        &schemas_dir()
            .join("Core")
            .join("Associations")
            .join("SupplementalAttributeAssociation.json"),
    );
    let instance = fixture("supplemental_attribute_association.json");
    assert_conforms(
        &validator,
        &instance,
        "supplemental_attribute_association.json",
        "SupplementalAttributeAssociation.json",
    );
}
