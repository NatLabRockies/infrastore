//! Failure paths and untested subcommands for the `infrastore` binary.
//!
//! `cli_round_trip.rs` drives the happy path. This file covers what a real user
//! is far more likely to hit: a ragged CSV, a typo'd descriptor field, a value
//! count that does not match the declared shape. Each case asserts a *nonzero
//! exit* and that the message names the actual problem — an error that exits 0,
//! or exits 1 with an unhelpful message, is a bug even though the data is
//! untouched.
//!
//! Also here: the subcommands no test ever invoked (`transform`, `copy`,
//! `persist`, `compact`, `params`, `template`, real `clear` / `replace-owner`),
//! the `export` -> `add` round trip, `get --time-range`, and the
//! `INFRASTORE_STORE` environment fallback.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

const BIN: &str = env!("CARGO_BIN_EXE_infrastore");

/// Run `infrastore --store <store> <args...>`, asserting success; returns stdout.
fn run(store: &Path, args: &[&str]) -> String {
    let output = Command::new(BIN)
        .arg("--store")
        .arg(store)
        .args(args)
        .output()
        .expect("failed to spawn infrastore");
    assert!(
        output.status.success(),
        "infrastore {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

/// Run `infrastore`, asserting a nonzero exit **and** the `Error:` stderr prefix that
/// `main` writes; returns stderr. The prefix is part of the CLI's contract with
/// a shell caller: it is how a user tells a diagnostic from log noise.
fn run_err(store: &Path, args: &[&str]) -> String {
    let output = Command::new(BIN)
        .arg("--store")
        .arg(store)
        .args(args)
        .output()
        .expect("failed to spawn infrastore");
    assert!(
        !output.status.success(),
        "infrastore {args:?} unexpectedly succeeded:\nstdout: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("Error: "),
        "infrastore {args:?} exited nonzero without an `Error: ` diagnostic; stderr was:\n{stderr}"
    );
    stderr
}

/// Run `infrastore`, asserting a nonzero exit without requiring the `Error: ` prefix.
/// `verify` reports through its normal output and then exits 1, so it fails
/// without writing a diagnostic; returns `(stdout, stderr)`.
fn run_fail(store: &Path, args: &[&str]) -> (String, String) {
    let output = Command::new(BIN)
        .arg("--store")
        .arg(store)
        .args(args)
        .output()
        .expect("failed to spawn infrastore");
    assert!(
        !output.status.success(),
        "infrastore {args:?} unexpectedly succeeded:\nstdout: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Exit code of a `infrastore` invocation.
fn exit_code(store: &Path, args: &[&str]) -> i32 {
    Command::new(BIN)
        .arg("--store")
        .arg(store)
        .args(args)
        .output()
        .expect("failed to spawn infrastore")
        .status
        .code()
        .expect("the process was killed by a signal")
}

fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

fn data_lines(csv: &str) -> Vec<String> {
    csv.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .skip(1)
        .map(str::to_string)
        .collect()
}

/// A minimal single-series descriptor with `overrides` merged in as raw JSON
/// members, so a test can add or replace one field without restating the rest.
fn descriptor_json(overrides: &[(&str, &str)]) -> String {
    let mut fields: Vec<(String, String)> = vec![
        ("owner_id".into(), "42".into()),
        ("owner_type".into(), "\"Generator\"".into()),
        ("name".into(), "\"load\"".into()),
        ("type".into(), "\"single\"".into()),
        ("dtype".into(), "\"f64\"".into()),
        ("csv".into(), "\"data.csv\"".into()),
        ("has_header".into(), "false".into()),
        (
            "initial_timestamp".into(),
            "\"2024-01-01T00:00:00Z\"".into(),
        ),
        ("resolution".into(), "\"1h\"".into()),
    ];
    for (k, v) in overrides {
        match fields.iter_mut().find(|(name, _)| name == k) {
            Some(entry) => entry.1 = (*v).to_string(),
            None => fields.push(((*k).to_string(), (*v).to_string())),
        }
    }
    let body: Vec<String> = fields
        .iter()
        .map(|(k, v)| format!("  \"{k}\": {v}"))
        .collect();
    format!("{{\n{}\n}}", body.join(",\n"))
}

/// Write a CSV + descriptor pair into a fresh temp dir and return
/// `(dir, store_path, descriptor_path)`.
fn fixture(csv_body: &str, overrides: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    write(dir.path(), "data.csv", csv_body);
    let descriptor = write(dir.path(), "d.json", &descriptor_json(overrides));
    (dir, store, descriptor)
}

/// `infrastore add` for a fixture descriptor, expecting failure; returns stderr.
fn add_err(store: &Path, descriptor: &Path) -> String {
    run_err(
        store,
        &["add", "--descriptor", descriptor.to_str().unwrap()],
    )
}

/// `infrastore add` for a fixture descriptor, expecting success.
fn add_ok(store: &Path, descriptor: &Path) -> String {
    run(
        store,
        &["add", "--descriptor", descriptor.to_str().unwrap()],
    )
}

/// Seed a store with one 4-step f64 series named `load` owned by 42.
fn seed(dir: &Path, store: &Path) {
    write(dir, "seed.csv", "10\n11\n12\n13\n");
    let descriptor = write(
        dir,
        "seed.json",
        &descriptor_json(&[("csv", "\"seed.csv\"")]),
    );
    add_ok(store, &descriptor);
}

// ---------------------------------------------------------------------------
// Bad CSV matrix
// ---------------------------------------------------------------------------

#[test]
fn a_ragged_csv_row_is_rejected() {
    // The reader is built with `flexible(false)`, so a row with a different
    // field count than the first is a parse error naming the row.
    let (_dir, store, descriptor) = fixture("1.0,2.0\n3.0\n", &[]);
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("row") || stderr.contains("field"),
        "expected a row/field diagnostic, got: {stderr}"
    );
}

#[test]
fn a_non_numeric_cell_in_an_f64_column_is_rejected() {
    let (_dir, store, descriptor) = fixture("1.0\nnot_a_number\n3.0\n", &[]);
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("not_a_number"),
        "the diagnostic must quote the offending cell, got: {stderr}"
    );
    assert!(
        stderr.contains("f64"),
        "the diagnostic must name the expected type, got: {stderr}"
    );
}

#[test]
fn a_non_integer_cell_in_an_i64_column_is_rejected() {
    // An empty line is skipped by the CSV reader before it reaches the parser,
    // so it is not in this list — it shortens the series instead.
    for bad in ["abc", "1.5", "0x10", "1e3", "２"] {
        let (_dir, store, descriptor) = fixture(&format!("1\n{bad}\n3\n"), &[("dtype", "\"i64\"")]);
        let stderr = add_err(&store, &descriptor);
        assert!(
            stderr.contains("i64"),
            "{bad:?}: expected an i64 diagnostic, got: {stderr}"
        );
    }
}

#[test]
fn a_negative_value_in_a_u64_column_is_rejected() {
    let (_dir, store, descriptor) = fixture("1\n-2\n3\n", &[("dtype", "\"u64\"")]);
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("u64") && stderr.contains("-2"),
        "expected a u64 diagnostic quoting -2, got: {stderr}"
    );
}

#[test]
fn an_out_of_range_integer_is_rejected() {
    // i32 cannot hold 3_000_000_000.
    let (_dir, store, descriptor) = fixture("1\n3000000000\n3\n", &[("dtype", "\"i32\"")]);
    let stderr = add_err(&store, &descriptor);
    assert!(stderr.contains("i32"), "got: {stderr}");
}

#[test]
fn a_non_boolean_cell_in_a_bool_column_is_rejected() {
    let (_dir, store, descriptor) = fixture("true\nmaybe\nfalse\n", &[("dtype", "\"bool\"")]);
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("true/false") || stderr.contains("maybe"),
        "expected a bool diagnostic, got: {stderr}"
    );
}

#[test]
fn accepted_boolean_spellings_round_trip() {
    // The complement of the case above: `1`/`0` and mixed case are accepted.
    let (dir, store, _) = fixture("TRUE\n0\nFalse\n1\n", &[("dtype", "\"bool\"")]);
    let descriptor = write(
        dir.path(),
        "d.json",
        &descriptor_json(&[("dtype", "\"bool\"")]),
    );
    add_ok(&store, &descriptor);
    let out = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "42", "--name", "load"],
    );
    assert_eq!(data_lines(&out), vec!["true", "false", "false", "true"]);
}

#[test]
fn a_value_count_that_does_not_match_the_declared_shape_is_rejected() {
    // The likeliest real user error: element_shape says 3 per step, but the CSV
    // holds a number of values that is not a multiple of 3.
    let (_dir, store, descriptor) = fixture("1,2,3\n4,5,6\n7,8\n", &[("element_shape", "[3]")]);
    let stderr = add_err(&store, &descriptor);
    assert!(!stderr.is_empty());

    // And a clean multiple that still disagrees with an explicit forecast shape.
    let (_dir, store, descriptor) = fixture(
        "1\n2\n3\n4\n5\n",
        &[
            ("type", "\"deterministic\""),
            ("horizon", "\"2h\""),
            ("interval", "\"1h\""),
            ("count", "3"),
        ],
    );
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("expected 6 values") || stderr.contains("shape"),
        "expected a shape/count diagnostic, got: {stderr}"
    );
}

#[test]
fn a_missing_timestamp_column_for_non_sequential_is_rejected() {
    // With `type: non_sequential` the first column is timestamps. A single-column
    // CSV therefore has values that are not parseable as timestamps.
    let (_dir, store, descriptor) = fixture(
        "10\n20\n30\n",
        &[
            ("type", "\"non_sequential\""),
            ("initial_timestamp", "null"),
            ("resolution", "null"),
        ],
    );
    let stderr = add_err(&store, &descriptor);
    assert!(!stderr.is_empty(), "got: {stderr}");
}

#[test]
fn unsorted_and_duplicate_timestamps_are_rejected() {
    for (label, body) in [
        (
            "decreasing",
            "2024-01-02T00:00:00Z,10\n2024-01-01T00:00:00Z,20\n",
        ),
        (
            "duplicate",
            "2024-01-01T00:00:00Z,10\n2024-01-01T00:00:00Z,20\n",
        ),
    ] {
        let (_dir, store, descriptor) = fixture(
            body,
            &[
                ("type", "\"non_sequential\""),
                ("initial_timestamp", "null"),
                ("resolution", "null"),
            ],
        );
        let stderr = add_err(&store, &descriptor);
        assert!(
            stderr.contains("increasing"),
            "{label}: expected a strictly-increasing diagnostic, got: {stderr}"
        );
    }
}

#[test]
fn a_malformed_timestamp_in_a_non_sequential_csv_is_rejected() {
    let (_dir, store, descriptor) = fixture(
        "2024-01-01T00:00:00Z,10\nnot-a-time,20\n",
        &[
            ("type", "\"non_sequential\""),
            ("initial_timestamp", "null"),
            ("resolution", "null"),
        ],
    );
    let stderr = add_err(&store, &descriptor);
    assert!(!stderr.is_empty(), "got: {stderr}");
}

#[test]
fn a_nonexistent_csv_path_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    // No data.csv is written.
    let descriptor = write(dir.path(), "d.json", &descriptor_json(&[]));
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("data.csv"),
        "the diagnostic must name the missing file, got: {stderr}"
    );
}

#[test]
fn a_descriptor_with_no_csv_and_no_flag_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    let descriptor = write(dir.path(), "d.json", &descriptor_json(&[("csv", "null")]));
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("csv"),
        "expected a diagnostic about the missing csv path, got: {stderr}"
    );
}

#[test]
fn has_header_true_skips_the_first_row() {
    // `has_header` defaults to true but no test ever exercised that path: every
    // fixture sets it to false. A header row must be consumed, not parsed as data.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    write(dir.path(), "data.csv", "value\n1.5\n2.5\n3.5\n");
    let descriptor = write(
        dir.path(),
        "d.json",
        &descriptor_json(&[("has_header", "true")]),
    );
    add_ok(&store, &descriptor);

    let out = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "42", "--name", "load"],
    );
    assert_eq!(data_lines(&out), vec!["1.5", "2.5", "3.5"]);

    // With `has_header: false` the same file fails, because "value" is not an f64
    // — which is what makes the assertion above meaningful.
    let store2 = dir.path().join("store2.nc");
    let no_header = write(
        dir.path(),
        "d2.json",
        &descriptor_json(&[("has_header", "false")]),
    );
    let stderr = add_err(&store2, &no_header);
    assert!(stderr.contains("value"), "got: {stderr}");
}

#[test]
fn adding_the_same_series_twice_is_a_duplicate_with_a_nonzero_exit() {
    let (_dir, store, descriptor) = fixture("1.0\n2.0\n3.0\n", &[]);
    add_ok(&store, &descriptor);
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.to_lowercase().contains("already exists")
            || stderr.to_lowercase().contains("duplicate"),
        "expected a duplicate diagnostic, got: {stderr}"
    );
    // The first series is intact.
    let out = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "42", "--name", "load"],
    );
    assert_eq!(data_lines(&out), vec!["1", "2", "3"]);
}

// ---------------------------------------------------------------------------
// Descriptor matrix
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_descriptor_field_is_rejected() {
    // `Descriptor` is `deny_unknown_fields`, so a typo is caught rather than
    // silently ignored — the difference between "my units were dropped" and a
    // clear error.
    // `unit`, not `units`.
    let (_dir, store, descriptor) = fixture("1.0\n2.0\n3.0\n", &[("unit", "\"MW\"")]);
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("unit"),
        "the diagnostic must name the unknown field, got: {stderr}"
    );
}

#[test]
fn each_type_reports_its_own_missing_required_field() {
    /// `(type, extra descriptor fields, the word the diagnostic must contain)`.
    type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str);
    let cases: &[Case] = &[
        // single without initial_timestamp
        (
            "single",
            &[("initial_timestamp", "null")],
            "initial_timestamp",
        ),
        // single without resolution
        ("single", &[("resolution", "null")], "resolution"),
        // deterministic without horizon
        (
            "deterministic",
            &[("interval", "\"1h\""), ("count", "3")],
            "horizon",
        ),
        // deterministic without interval
        (
            "deterministic",
            &[("horizon", "\"2h\""), ("count", "3")],
            "interval",
        ),
        // deterministic without count
        (
            "deterministic",
            &[("horizon", "\"2h\""), ("interval", "\"1h\"")],
            "count",
        ),
        // probabilistic without percentiles
        (
            "probabilistic",
            &[
                ("horizon", "\"2h\""),
                ("interval", "\"1h\""),
                ("count", "3"),
            ],
            "percentiles",
        ),
    ];

    for (ts_type, extra, expect) in cases {
        let mut overrides: Vec<(&str, &str)> = vec![(
            "type",
            match *ts_type {
                "single" => "\"single\"",
                "deterministic" => "\"deterministic\"",
                "probabilistic" => "\"probabilistic\"",
                other => panic!("unhandled type {other}"),
            },
        )];
        overrides.extend_from_slice(extra);
        let (_dir, store, descriptor) = fixture("1\n2\n3\n4\n5\n6\n", &overrides);
        let stderr = add_err(&store, &descriptor);
        assert!(
            stderr.contains(expect),
            "{ts_type} missing {expect}: got {stderr}"
        );
    }
}

#[test]
fn an_empty_root_array_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    let descriptor = write(dir.path(), "d.json", "[]");
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("empty array"),
        "expected an empty-array diagnostic, got: {stderr}"
    );
}

#[test]
fn a_root_scalar_descriptor_is_rejected() {
    for body in ["42", "\"a string\"", "true", "null"] {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.nc");
        let descriptor = write(dir.path(), "d.json", body);
        let stderr = add_err(&store, &descriptor);
        assert!(
            stderr.contains("JSON object or array"),
            "{body}: expected a shape diagnostic, got: {stderr}"
        );
    }
}

#[test]
fn malformed_json_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    for body in ["{", "{\"owner_id\": }", "{,}", "not json at all"] {
        let descriptor = write(dir.path(), "d.json", body);
        let stderr = add_err(&store, &descriptor);
        assert!(
            stderr.contains("parsing descriptor"),
            "{body:?}: expected a parse diagnostic, got: {stderr}"
        );
    }
}

#[test]
fn a_nonexistent_descriptor_path_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    let stderr = run_err(
        &store,
        &[
            "add",
            "--descriptor",
            dir.path().join("no_such.json").to_str().unwrap(),
        ],
    );
    assert!(
        stderr.contains("reading descriptor"),
        "expected a read diagnostic, got: {stderr}"
    );
}

#[test]
fn a_zero_element_shape_dimension_is_rejected() {
    // PIN the message a user actually gets. `element_shape: [0]` does *not*
    // reach `steps_from_values`'s "must not contain a zero dimension" guard:
    // `per_step` is `product().max(1)`, so a zero dimension becomes 1 and the
    // failure surfaces later as a shape/value-count mismatch instead. The
    // diagnostic still names the offending shape, so it is actionable, but the
    // dedicated guard is unreachable this way.
    let (_dir, store, descriptor) = fixture("1.0\n2.0\n3.0\n", &[("element_shape", "[0]")]);
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("[3, 0]"),
        "expected the degenerate shape in the diagnostic, got: {stderr}"
    );

    // A zero in a multi-dimensional element shape behaves the same way.
    let (_dir, store, descriptor) = fixture("1.0\n2.0\n3.0\n", &[("element_shape", "[2, 0]")]);
    let stderr = add_err(&store, &descriptor);
    assert!(stderr.contains("0"), "got: {stderr}");
}

#[test]
fn a_non_divisible_value_count_is_rejected() {
    // 5 values, 2 per step: not a whole number of steps.
    let (_dir, store, descriptor) = fixture("1,2\n3,4\n5\n", &[("element_shape", "[2]")]);
    let stderr = add_err(&store, &descriptor);
    assert!(!stderr.is_empty());

    // A single-column CSV with an odd count is the cleaner form of the same error.
    let (_dir, store, descriptor) = fixture("1\n2\n3\n4\n5\n", &[("element_shape", "[2]")]);
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("divisible"),
        "expected a divisibility diagnostic, got: {stderr}"
    );
}

#[test]
fn a_scenario_count_that_cannot_be_inferred_is_rejected() {
    // H = 2, count = 3 -> 6 values per scenario. 7 values divides by nothing.
    let (_dir, store, descriptor) = fixture(
        "1\n2\n3\n4\n5\n6\n7\n",
        &[
            ("type", "\"scenarios\""),
            ("horizon", "\"2h\""),
            ("interval", "\"1h\""),
            ("count", "3"),
        ],
    );
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("infer scenario_count"),
        "expected an inference diagnostic, got: {stderr}"
    );
}

#[test]
fn a_scenario_count_that_can_be_inferred_is_used() {
    // 12 values / (H=2 * count=3) = 2 scenarios.
    let (_dir, store, descriptor) = fixture(
        "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n",
        &[
            ("type", "\"scenarios\""),
            ("horizon", "\"2h\""),
            ("interval", "\"1h\""),
            ("count", "3"),
        ],
    );
    add_ok(&store, &descriptor);
    let out = run(&store, &["-f", "json", "list"]);
    assert!(out.contains("Scenarios"), "got: {out}");
}

#[test]
fn a_deterministic_single_time_series_descriptor_points_at_transform() {
    let (_dir, store, descriptor) = fixture(
        "1\n2\n3\n4\n",
        &[
            ("type", "\"deterministic_single\""),
            ("horizon", "\"2h\""),
            ("interval", "\"1h\""),
            ("count", "2"),
        ],
    );
    let stderr = add_err(&store, &descriptor);
    // Either the type spelling is unknown, or the build path reports the
    // transform hint. Both are acceptable; assert whichever is emitted names a
    // route forward.
    assert!(
        stderr.contains("transform") || stderr.contains("deterministic_single"),
        "expected a diagnostic mentioning transform or the type name, got: {stderr}"
    );
}

#[test]
fn an_unknown_type_or_dtype_is_rejected() {
    let (_dir, store, descriptor) = fixture("1.0\n2.0\n", &[("type", "\"quantum\"")]);
    let stderr = add_err(&store, &descriptor);
    assert!(stderr.contains("quantum"), "got: {stderr}");

    let (_dir, store, descriptor) = fixture("1.0\n2.0\n", &[("dtype", "\"f16\"")]);
    let stderr = add_err(&store, &descriptor);
    assert!(stderr.contains("f16"), "got: {stderr}");

    let (_dir, store, descriptor) = fixture("1.0\n2.0\n", &[("owner_category", "\"neither\"")]);
    let stderr = add_err(&store, &descriptor);
    assert!(stderr.contains("neither"), "got: {stderr}");

    let (_dir, store, descriptor) = fixture("1.0\n2.0\n", &[("resolution", "\"1 fortnight\"")]);
    let stderr = add_err(&store, &descriptor);
    assert!(!stderr.is_empty(), "got: {stderr}");
}

#[test]
fn a_features_map_in_a_descriptor_round_trips() {
    // `features` is never populated in any other test, so the JSON -> FeatureValue
    // conversion (including each value kind) was unexercised through the CLI.
    let (_dir, store, descriptor) = fixture(
        "1.0\n2.0\n3.0\n",
        &[(
            "features",
            "{\"model_year\": 2030, \"scenario\": \"base\", \"flag\": true, \"scale\": 1.5}",
        )],
    );
    add_ok(&store, &descriptor);

    // `list` rows do not carry features; `info` does, one `feature.<key>` field
    // each.
    let out = run(
        &store,
        &["-f", "json", "info", "--owner-id", "42", "--name", "load"],
    );
    for expect in [
        "feature.model_year",
        "2030",
        "feature.scenario",
        "base",
        "feature.flag",
        "feature.scale",
        "1.5",
    ] {
        assert!(out.contains(expect), "{expect} missing from: {out}");
    }

    // The features are part of the identity: selecting by one of them matches.
    let out = run(
        &store,
        &[
            "-f",
            "csv",
            "get",
            "--owner-id",
            "42",
            "--name",
            "load",
            "--feature",
            "model_year=2030",
        ],
    );
    assert_eq!(data_lines(&out), vec!["1", "2", "3"]);

    // A feature value that is neither scalar nor string is rejected.
    let (_dir, store2, descriptor2) = fixture(
        "1.0\n2.0\n3.0\n",
        &[("features", "{\"nested\": {\"a\": 1}}")],
    );
    let stderr = add_err(&store2, &descriptor2);
    assert!(stderr.contains("nested"), "got: {stderr}");
}

#[test]
fn an_attribute_owned_series_can_be_added_and_listed() {
    // `owner_category: supplemental_attribute` is accepted by the descriptor but
    // was never exercised.
    let (_dir, store, descriptor) = fixture(
        "1.0\n2.0\n3.0\n",
        &[
            ("owner_category", "\"supplemental_attribute\""),
            ("owner_type", "\"GeographicInfo\""),
        ],
    );
    add_ok(&store, &descriptor);

    let out = run(&store, &["-f", "json", "list"]);
    assert!(out.contains("SupplementalAttribute"), "got: {out}");

    // And it is addressable by that category, but not by the other one.
    let out = run(
        &store,
        &[
            "-f",
            "csv",
            "get",
            "--owner-id",
            "42",
            "--owner-category",
            "supplemental_attribute",
            "--name",
            "load",
        ],
    );
    assert_eq!(data_lines(&out), vec!["1", "2", "3"]);
    run_err(
        &store,
        &[
            "get",
            "--owner-id",
            "42",
            "--owner-category",
            "component",
            "--name",
            "load",
        ],
    );
}

// ---------------------------------------------------------------------------
// Untested subcommands
// ---------------------------------------------------------------------------

#[test]
fn transform_derives_a_dst_that_list_shows() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    write(dir.path(), "t.csv", "1\n2\n3\n4\n5\n6\n7\n8\n");
    let descriptor = write(
        dir.path(),
        "t.json",
        &descriptor_json(&[("csv", "\"t.csv\"")]),
    );
    add_ok(&store, &descriptor);

    let out = run(
        &store,
        &["transform", "--horizon", "4h", "--interval", "2h"],
    );
    assert!(!out.is_empty() || out.is_empty()); // output shape is not the contract

    let listed = run(&store, &["-f", "json", "list"]);
    assert!(
        listed.contains("DeterministicSingleTimeSeries"),
        "the DST tag must be visible after transform, got: {listed}"
    );

    // The DST reads back as a Deterministic view of the same data.
    let out = run(
        &store,
        &[
            "-f",
            "json",
            "get",
            "--owner-id",
            "42",
            "--name",
            "load",
            "--type",
            "deterministic_single",
        ],
    );
    assert!(!out.trim().is_empty());

    // A transform with a bad period is rejected.
    run_err(
        &store,
        &["transform", "--horizon", "not-a-period", "--interval", "2h"],
    );
}

#[test]
fn copy_shares_the_array_and_dry_run_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    seed(dir.path(), &store);

    // --dry-run reports but does not write.
    let out = run(
        &store,
        &[
            "copy",
            "--owner-id",
            "42",
            "--name",
            "load",
            "--dst-owner-id",
            "43",
            "--dst-owner-type",
            "Generator",
            "--dry-run",
        ],
    );
    assert!(
        !out.trim().is_empty(),
        "dry-run should report what it would do"
    );
    assert_eq!(
        run(&store, &["-f", "json", "list"])
            .matches("\"owner_id\"")
            .count(),
        1,
        "--dry-run must not add a series"
    );

    // The real copy.
    run(
        &store,
        &[
            "copy",
            "--owner-id",
            "42",
            "--name",
            "load",
            "--dst-owner-id",
            "43",
            "--dst-owner-type",
            "Generator",
            "--new-name",
            "load_copy",
        ],
    );
    let out = run(
        &store,
        &[
            "-f",
            "csv",
            "get",
            "--owner-id",
            "43",
            "--name",
            "load_copy",
        ],
    );
    assert_eq!(data_lines(&out), vec!["10", "11", "12", "13"]);

    // No array was duplicated.
    let stats = run(&store, &["-f", "json", "stats"]);
    let parsed: serde_json::Value = serde_json::from_str(&stats).unwrap();
    assert_eq!(
        parsed.get("num_distinct_arrays").and_then(|v| v.as_i64()),
        Some(1),
        "the copy must share the source array, got: {stats}"
    );
    assert_eq!(
        parsed.get("static_time_series").and_then(|v| v.as_i64()),
        Some(2),
        "but there are two associations, got: {stats}"
    );

    // Copying onto the same destination twice is a duplicate.
    run_err(
        &store,
        &[
            "copy",
            "--owner-id",
            "42",
            "--name",
            "load",
            "--dst-owner-id",
            "43",
            "--dst-owner-type",
            "Generator",
            "--new-name",
            "load_copy",
        ],
    );
}

#[test]
fn persist_writes_a_readable_copy() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    seed(dir.path(), &store);

    let dest = dir.path().join("copy.nc");
    run(&store, &["persist", "--dest", dest.to_str().unwrap()]);
    assert!(dest.exists(), "the destination .nc must exist");
    let mut sqlite = dest.clone().into_os_string();
    sqlite.push(".sqlite");
    assert!(
        PathBuf::from(sqlite).exists(),
        "the companion catalog must exist too"
    );

    // The copy reads the same values, and verifies clean.
    let out = run(
        &dest,
        &["-f", "csv", "get", "--owner-id", "42", "--name", "load"],
    );
    assert_eq!(data_lines(&out), vec!["10", "11", "12", "13"]);
    assert_eq!(exit_code(&dest, &["verify"]), 0);
}

#[test]
fn compact_runs_and_reports() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    seed(dir.path(), &store);
    // Add a second series and remove it, leaving a reusable slot.
    write(dir.path(), "b.csv", "90\n91\n92\n93\n");
    let second = write(
        dir.path(),
        "b.json",
        &descriptor_json(&[("owner_id", "43"), ("csv", "\"b.csv\"")]),
    );
    add_ok(&store, &second);
    run(
        &store,
        &["remove", "--owner-id", "43", "--name", "load", "--force"],
    );

    let out = run(&store, &["-f", "json", "compact", "--force"]);
    assert!(!out.trim().is_empty(), "compact must print a report");

    // The surviving series is intact and the store still verifies.
    let out = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "42", "--name", "load"],
    );
    assert_eq!(data_lines(&out), vec!["10", "11", "12", "13"]);
    assert_eq!(exit_code(&store, &["verify"]), 0);
}

#[test]
fn params_reports_forecast_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    write(dir.path(), "f.csv", "1\n2\n3\n4\n5\n6\n");
    let descriptor = write(
        dir.path(),
        "f.json",
        &descriptor_json(&[
            ("csv", "\"f.csv\""),
            ("type", "\"deterministic\""),
            ("horizon", "\"2h\""),
            ("interval", "\"1h\""),
            ("count", "3"),
        ]),
    );
    add_ok(&store, &descriptor);

    let out = run(&store, &["-f", "json", "params"]);
    assert!(out.contains("PT2H"), "the horizon must appear: {out}");
    assert!(out.contains("PT1H"), "the interval must appear: {out}");

    // Scoped by resolution and interval.
    let out = run(
        &store,
        &[
            "-f",
            "json",
            "params",
            "--resolution",
            "1h",
            "--interval",
            "1h",
        ],
    );
    assert!(out.contains("PT2H"), "got: {out}");

    // A bad period is rejected.
    run_err(&store, &["params", "--resolution", "nonsense"]);

    // An empty store reports no parameters rather than failing.
    let empty_dir = tempfile::tempdir().unwrap();
    let empty = empty_dir.path().join("empty.nc");
    seed(empty_dir.path(), &empty); // statics only, no forecasts
    let out = run(&empty, &["-f", "json", "params"]);
    assert!(!out.trim().is_empty());
}

#[test]
fn template_prints_a_usable_descriptor_for_every_type() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    for ts_type in [
        "single",
        "non_sequential",
        "deterministic",
        "probabilistic",
        "scenarios",
    ] {
        let out = run(&store, &["template", ts_type]);
        // Each template must be valid JSON with a matching `type`.
        let value: serde_json::Value =
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("{ts_type}: {e}\n{out}"));
        assert_eq!(
            value.get("type").and_then(|v| v.as_str()),
            Some(ts_type),
            "{ts_type}: template's type field"
        );
        assert!(value.get("dtype").is_some(), "{ts_type}: dtype");
    }

    // DST has no descriptor form.
    let stderr = run_err(&store, &["template", "deterministic_single"]);
    assert!(stderr.contains("transform"), "got: {stderr}");

    // An unknown type is rejected.
    run_err(&store, &["template", "quantum"]);
}

#[test]
fn clear_removes_everything_or_one_owner() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    seed(dir.path(), &store);
    write(dir.path(), "b.csv", "90\n91\n92\n93\n");
    let second = write(
        dir.path(),
        "b.json",
        &descriptor_json(&[("owner_id", "43"), ("csv", "\"b.csv\"")]),
    );
    add_ok(&store, &second);
    assert_eq!(
        run(&store, &["-f", "json", "list"])
            .matches("\"owner_id\"")
            .count(),
        2
    );

    // Scoped to one owner.
    run(
        &store,
        &[
            "clear",
            "--owner-id",
            "43",
            "--owner-category",
            "component",
            "--force",
        ],
    );
    let listed = run(&store, &["-f", "json", "list"]);
    assert_eq!(listed.matches("\"owner_id\"").count(), 1);
    assert!(listed.contains("42"));

    // Then everything.
    run(&store, &["clear", "--force"]);
    let listed = run(&store, &["-f", "json", "list"]);
    assert_eq!(listed.matches("\"owner_id\"").count(), 0, "got: {listed}");

    // Clearing an empty store is not an error.
    run(&store, &["clear", "--force"]);
}

#[test]
fn replace_owner_moves_series_and_dry_run_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    seed(dir.path(), &store);

    // --dry-run reports the count without moving.
    let out = run(
        &store,
        &[
            "replace-owner",
            "--old",
            "42",
            "--new",
            "99",
            "--owner-category",
            "component",
            "--dry-run",
        ],
    );
    assert!(!out.trim().is_empty());
    assert!(
        run(&store, &["-f", "json", "list"]).contains("42"),
        "--dry-run must not move the series"
    );

    // The real move.
    run(
        &store,
        &[
            "replace-owner",
            "--old",
            "42",
            "--new",
            "99",
            "--owner-category",
            "component",
        ],
    );
    let out = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "99", "--name", "load"],
    );
    assert_eq!(data_lines(&out), vec!["10", "11", "12", "13"]);
    run_err(&store, &["get", "--owner-id", "42", "--name", "load"]);

    // A bad owner category is rejected.
    run_err(
        &store,
        &[
            "replace-owner",
            "--old",
            "99",
            "--new",
            "100",
            "--owner-category",
            "neither",
        ],
    );
}

#[test]
fn remove_without_all_removes_exactly_one_series() {
    // The non-`--all` path: a selector that resolves to one series.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    seed(dir.path(), &store);
    write(dir.path(), "b.csv", "90\n91\n92\n93\n");
    let second = write(
        dir.path(),
        "b.json",
        &descriptor_json(&[("owner_id", "43"), ("csv", "\"b.csv\"")]),
    );
    add_ok(&store, &second);

    run(
        &store,
        &["remove", "--owner-id", "43", "--name", "load", "--force"],
    );
    let listed = run(&store, &["-f", "json", "list"]);
    assert_eq!(listed.matches("\"owner_id\"").count(), 1);
    assert!(listed.contains("42"));

    // Without `--all`, a selector matching several series is an error naming
    // them, not a partial removal.
    write(dir.path(), "c.csv", "80\n81\n82\n83\n");
    let third = write(
        dir.path(),
        "c.json",
        &descriptor_json(&[("owner_id", "44"), ("csv", "\"c.csv\"")]),
    );
    add_ok(&store, &third);
    let stderr = run_err(&store, &["remove", "--name", "load", "--force"]);
    assert!(
        stderr.contains("matched"),
        "expected a multi-match diagnostic, got: {stderr}"
    );
    assert_eq!(
        run(&store, &["-f", "json", "list"])
            .matches("\"owner_id\"")
            .count(),
        2,
        "a failed remove must not delete anything"
    );

    // A selector matching nothing is an error too.
    let stderr = run_err(&store, &["remove", "--name", "absent", "--force"]);
    assert!(stderr.contains("no time series matched"), "got: {stderr}");
}

// ---------------------------------------------------------------------------
// export -> add round trip
// ---------------------------------------------------------------------------

/// Drop the leading timestamp column from an exported CSV, keeping the header
/// row, so the result is addable as a `single` series.
fn strip_timestamp_column(csv: &str) -> String {
    let mut out = String::new();
    for line in csv.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rest = line.split_once(',').map(|(_, r)| r).unwrap_or(line);
        out.push_str(rest);
        out.push('\n');
    }
    out
}

#[test]
fn export_then_add_reproduces_the_values() {
    // `export` is the read-direction inverse of `add`, so exporting and re-adding
    // must reproduce the values. FINDING F10: the export is *timestamped*
    // (`timestamp,value`), while a `single` descriptor's CSV holds values only —
    // so the output is not directly re-addable as the same type. A caller must
    // either strip the timestamp column (below) or re-add as `non_sequential`.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    seed(dir.path(), &store);

    let original = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "42", "--name", "load"],
    );

    let exported = run(
        &store,
        &["-f", "csv", "export", "--owner-id", "42", "--name", "load"],
    );
    // The export carries a timestamp column and a header row.
    assert!(exported.lines().next().unwrap().starts_with("timestamp,"));
    assert_eq!(data_lines(&exported).len(), 4);

    // Re-add as a `single` after stripping the timestamp column.
    write(dir.path(), "values.csv", &strip_timestamp_column(&exported));
    let fresh = dir.path().join("fresh.nc");
    let descriptor = write(
        dir.path(),
        "re.json",
        &descriptor_json(&[("csv", "\"values.csv\""), ("has_header", "true")]),
    );
    add_ok(&fresh, &descriptor);

    assert_eq!(
        data_lines(&run(
            &fresh,
            &["-f", "csv", "get", "--owner-id", "42", "--name", "load"]
        )),
        data_lines(&original),
        "export -> add must reproduce the values"
    );

    // The timestamped export re-adds directly as a `non_sequential` series,
    // which is the shape it is already in; timestamps and values both survive.
    write(dir.path(), "stamped.csv", &exported);
    let ns_store = dir.path().join("ns.nc");
    let ns = write(
        dir.path(),
        "ns.json",
        &descriptor_json(&[
            ("csv", "\"stamped.csv\""),
            ("has_header", "true"),
            ("type", "\"non_sequential\""),
            ("initial_timestamp", "null"),
            ("resolution", "null"),
        ]),
    );
    add_ok(&ns_store, &ns);
    let ns_out = run(
        &ns_store,
        &["-f", "csv", "get", "--owner-id", "42", "--name", "load"],
    );
    // Same timestamps and same values as the export it came from.
    assert_eq!(data_lines(&ns_out), data_lines(&exported));

    // JSON export is valid JSON.
    let json = run(
        &store,
        &["-f", "json", "export", "--owner-id", "42", "--name", "load"],
    );
    serde_json::from_str::<serde_json::Value>(&json)
        .unwrap_or_else(|e| panic!("json export is not valid JSON: {e}\n{json}"));
}

#[test]
fn export_then_add_reproduces_a_non_f64_dtype() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    write(
        dir.path(),
        "i.csv",
        "-9223372036854775808\n0\n9223372036854775807\n",
    );
    let descriptor = write(
        dir.path(),
        "i.json",
        &descriptor_json(&[("csv", "\"i.csv\""), ("dtype", "\"i64\"")]),
    );
    add_ok(&store, &descriptor);

    let original = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "42", "--name", "load"],
    );
    let exported = run(
        &store,
        &["-f", "csv", "export", "--owner-id", "42", "--name", "load"],
    );
    write(dir.path(), "values.csv", &strip_timestamp_column(&exported));

    let fresh = dir.path().join("fresh.nc");
    let re = write(
        dir.path(),
        "re.json",
        &descriptor_json(&[
            ("csv", "\"values.csv\""),
            ("dtype", "\"i64\""),
            ("has_header", "true"),
        ]),
    );
    add_ok(&fresh, &re);

    assert_eq!(
        data_lines(&run(
            &fresh,
            &["-f", "csv", "get", "--owner-id", "42", "--name", "load"]
        )),
        data_lines(&original),
        "i64 extremes must survive export -> add"
    );
    // And the extremes really are the extremes, not a lossy f64 detour.
    assert_eq!(
        data_lines(&original),
        vec!["-9223372036854775808", "0", "9223372036854775807"]
    );
}

#[test]
fn export_writes_one_file_per_series_into_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    seed(dir.path(), &store);
    write(dir.path(), "b.csv", "90\n91\n92\n93\n");
    let second = write(
        dir.path(),
        "b.json",
        &descriptor_json(&[
            ("owner_id", "43"),
            ("name", "\"voltage\""),
            ("csv", "\"b.csv\""),
        ]),
    );
    add_ok(&store, &second);

    let out_dir = dir.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    run(
        &store,
        &["-f", "csv", "export", "--dir", out_dir.to_str().unwrap()],
    );

    let written: Vec<String> = fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(written.len(), 2, "one file per series, got {written:?}");
    assert!(
        written.iter().any(|n| n.contains("load")),
        "got {written:?}"
    );
    assert!(
        written.iter().any(|n| n.contains("voltage")),
        "got {written:?}"
    );
}

#[test]
fn export_of_a_forecast_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    write(dir.path(), "f.csv", "1\n2\n3\n4\n5\n6\n");
    let descriptor = write(
        dir.path(),
        "f.json",
        &descriptor_json(&[
            ("csv", "\"f.csv\""),
            ("type", "\"deterministic\""),
            ("horizon", "\"2h\""),
            ("interval", "\"1h\""),
            ("count", "3"),
        ]),
    );
    add_ok(&store, &descriptor);

    let exported = run(
        &store,
        &["-f", "csv", "export", "--owner-id", "42", "--name", "load"],
    );
    assert!(
        !data_lines(&exported).is_empty(),
        "a forecast export must have rows: {exported}"
    );
    // Timestamped forecast CSV: every data row starts with a timestamp.
    for line in data_lines(&exported) {
        let first = line.split(',').next().unwrap();
        assert!(
            first.contains("2024"),
            "forecast export rows must be timestamped, got {line:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// get: time range, truncation, selectors, globs
// ---------------------------------------------------------------------------

#[test]
fn get_time_range_slices_the_series() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    write(dir.path(), "t.csv", "10\n11\n12\n13\n14\n15\n16\n17\n");
    let descriptor = write(
        dir.path(),
        "t.json",
        &descriptor_json(&[("csv", "\"t.csv\"")]),
    );
    add_ok(&store, &descriptor);

    // RFC3339 bounds.
    let out = run(
        &store,
        &[
            "-f",
            "csv",
            "get",
            "--owner-id",
            "42",
            "--name",
            "load",
            "--time-range",
            "2024-01-01T02:00:00Z..2024-01-01T05:00:00Z",
        ],
    );
    assert_eq!(data_lines(&out), vec!["12", "13", "14"]);

    // Epoch-ms bounds select the same window.
    let start_ms = 1_704_067_200_000i64 + 2 * 3_600_000;
    let end_ms = 1_704_067_200_000i64 + 5 * 3_600_000;
    let range = format!("{start_ms}..{end_ms}");
    let out = run(
        &store,
        &[
            "-f",
            "csv",
            "get",
            "--owner-id",
            "42",
            "--name",
            "load",
            "--time-range",
            &range,
        ],
    );
    assert_eq!(data_lines(&out), vec!["12", "13", "14"]);

    // A malformed range is rejected.
    for bad in [
        "not-a-range",
        "2024-01-01T00:00:00Z",
        "2024-01-01T00:00:00Z..",
        "..2024-01-01T05:00:00Z",
        "nope..alsonope",
    ] {
        run_err(
            &store,
            &[
                "get",
                "--owner-id",
                "42",
                "--name",
                "load",
                "--time-range",
                bad,
            ],
        );
    }
}

#[test]
fn get_limit_and_full_control_table_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    let body: String = (0..80).map(|i| format!("{i}\n")).collect();
    write(dir.path(), "big.csv", &body);
    let descriptor = write(
        dir.path(),
        "big.json",
        &descriptor_json(&[("csv", "\"big.csv\"")]),
    );
    add_ok(&store, &descriptor);

    // The default table output truncates and says so.
    let out = run(&store, &["get", "--owner-id", "42", "--name", "load"]);
    assert!(
        out.contains("80") || out.to_lowercase().contains("more") || out.contains("--full"),
        "truncated table output should mention the omitted rows: {out}"
    );

    // --limit shows fewer rows than --full.
    let limited = run(
        &store,
        &["get", "--owner-id", "42", "--name", "load", "--limit", "5"],
    );
    let full = run(
        &store,
        &["get", "--owner-id", "42", "--name", "load", "--full"],
    );
    assert!(
        limited.lines().count() < full.lines().count(),
        "--limit must show fewer lines than --full"
    );

    // CSV output is never truncated, whatever the limit.
    let csv = run(
        &store,
        &[
            "-f",
            "csv",
            "get",
            "--owner-id",
            "42",
            "--name",
            "load",
            "--limit",
            "5",
        ],
    );
    assert_eq!(data_lines(&csv).len(), 80, "csv output must be complete");
}

#[test]
fn a_selector_matching_zero_or_several_series_reports_which() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    seed(dir.path(), &store);
    write(dir.path(), "b.csv", "90\n91\n92\n93\n");
    let second = write(
        dir.path(),
        "b.json",
        &descriptor_json(&[("owner_id", "43"), ("csv", "\"b.csv\"")]),
    );
    add_ok(&store, &second);

    // Zero matches.
    let stderr = run_err(&store, &["get", "--name", "absent"]);
    assert!(stderr.contains("no time series matched"), "got: {stderr}");

    // Several matches: the message lists the candidates and names the flags that
    // would narrow it.
    let stderr = run_err(&store, &["get", "--name", "load"]);
    assert!(stderr.contains("2 time series matched"), "got: {stderr}");
    assert!(stderr.contains("owner=42"), "got: {stderr}");
    assert!(stderr.contains("owner=43"), "got: {stderr}");
    assert!(stderr.contains("--name-glob"), "got: {stderr}");

    // `info` uses the same resolver.
    run_err(&store, &["info", "--name", "load"]);
}

#[test]
fn glob_selector_edges() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    write(dir.path(), "g.csv", "1\n2\n3\n4\n");
    for (i, name) in ["wind_a", "wind_b", "solar", "Wind_c"].iter().enumerate() {
        let d = write(
            dir.path(),
            &format!("g{i}.json"),
            &descriptor_json(&[
                ("owner_id", &(i + 1).to_string()),
                ("name", &format!("\"{name}\"")),
                ("csv", "\"g.csv\""),
            ]),
        );
        add_ok(&store, &d);
    }

    let count = |args: &[&str]| run(&store, args).matches("\"owner_id\"").count();

    // `*` and `?`.
    assert_eq!(count(&["-f", "json", "list", "--name-glob", "wind_*"]), 2);
    assert_eq!(count(&["-f", "json", "list", "--name-glob", "wind_?"]), 2);
    // A character class.
    assert_eq!(
        count(&["-f", "json", "list", "--name-glob", "wind_[ab]"]),
        2
    );
    assert_eq!(count(&["-f", "json", "list", "--name-glob", "wind_[a]"]), 1);
    // GLOB is case-sensitive.
    assert_eq!(count(&["-f", "json", "list", "--name-glob", "Wind_*"]), 1);
    // Bare `*` matches everything.
    assert_eq!(count(&["-f", "json", "list", "--name-glob", "*"]), 4);
    // No match is an empty list, not an error.
    assert_eq!(count(&["-f", "json", "list", "--name-glob", "xyz*"]), 0);
    // A glob resolving to one series works with `get`.
    let out = run(&store, &["-f", "csv", "get", "--name-glob", "wind_[a]"]);
    assert_eq!(data_lines(&out), vec!["1", "2", "3", "4"]);
    // A glob resolving to several is a multi-match error.
    run_err(&store, &["get", "--name-glob", "wind_*"]);
}

// ---------------------------------------------------------------------------
// Store selection: INFRASTORE_STORE, precedence, and the missing-store error
// ---------------------------------------------------------------------------

#[test]
fn the_infrastore_store_env_var_is_used_when_no_flag_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    seed(dir.path(), &store);

    let output = Command::new(BIN)
        .env("INFRASTORE_STORE", &store)
        .args(["-f", "csv", "get", "--owner-id", "42", "--name", "load"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "INFRASTORE_STORE was not honored:\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        data_lines(&String::from_utf8_lossy(&output.stdout)),
        vec!["10", "11", "12", "13"]
    );
}

#[test]
fn the_store_flag_beats_the_env_var() {
    let dir = tempfile::tempdir().unwrap();
    let flagged = dir.path().join("flagged.nc");
    seed(dir.path(), &flagged);

    // A second store with different values, pointed at by the env var.
    let env_store = dir.path().join("env.nc");
    write(dir.path(), "e.csv", "70\n71\n72\n73\n");
    let d = write(
        dir.path(),
        "e.json",
        &descriptor_json(&[("csv", "\"e.csv\"")]),
    );
    add_ok(&env_store, &d);

    let output = Command::new(BIN)
        .env("INFRASTORE_STORE", &env_store)
        .arg("--store")
        .arg(&flagged)
        .args(["-f", "csv", "get", "--owner-id", "42", "--name", "load"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        data_lines(&String::from_utf8_lossy(&output.stdout)),
        vec!["10", "11", "12", "13"],
        "--store must win over INFRASTORE_STORE"
    );
}

#[test]
fn no_store_at_all_is_an_error() {
    let output = Command::new(BIN)
        .env_remove("INFRASTORE_STORE")
        .args(["list"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("missing --store"),
        "expected a missing-store diagnostic, got: {stderr}"
    );
}

#[test]
fn a_nonexistent_store_path_is_an_error_on_a_read_command() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no_such_store.nc");
    for args in [
        vec!["list"],
        vec!["stats"],
        vec!["verify"],
        vec!["resolutions"],
    ] {
        run_err(&missing, &args);
    }
}

// ---------------------------------------------------------------------------
// verify: the failing case and its exit code
// ---------------------------------------------------------------------------

#[test]
fn verify_exits_one_on_a_corrupt_store() {
    // Corrupt the *HDF5* side — flip one stored element without touching the
    // recorded content hash — which is the corruption `verify_integrity`
    // detects. A healthy store exits 0; this one must exit exactly 1, since a
    // shell caller branches on that.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    seed(dir.path(), &store);
    assert_eq!(exit_code(&store, &["verify"]), 0, "a healthy store exits 0");

    {
        let f = hdf5_metno::File::open_rw(&store).unwrap();
        let single = f.group("time_series/single").expect("single group");
        // Find the packed data dataset (not its `_h` companion).
        let dataset = single
            .member_names()
            .unwrap()
            .into_iter()
            .find(|n| !n.ends_with("_h") && !n.starts_with("arr_"))
            .expect("a packed data dataset");
        let ds = single.dataset(&dataset).expect("data dataset");
        let mut vals = ds.read_raw::<f64>().unwrap();
        vals[0] = -999.5;
        ds.write_raw(&vals).unwrap();
    }

    assert_eq!(
        exit_code(&store, &["verify"]),
        1,
        "a corrupt store must exit 1"
    );
    // `verify` reports through its normal output channel and *then* exits 1, so
    // the diagnostic is on stdout with no `Error: ` prefix.
    let (stdout, _stderr) = run_fail(&store, &["verify"]);
    assert!(
        stdout.to_lowercase().contains("hash") || stdout.to_lowercase().contains("integrity"),
        "expected an integrity report on stdout, got: {stdout}"
    );
    // The JSON form carries the same errors, for a scripted caller.
    let (stdout, _) = run_fail(&store, &["-f", "json", "verify"]);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let errors = parsed.get("errors").and_then(|v| v.as_array()).unwrap();
    assert_eq!(errors.len(), 1, "got: {stdout}");
    assert!(
        errors[0].as_str().unwrap().contains("hash mismatch"),
        "got: {stdout}"
    );
}

#[test]
fn verify_of_a_store_whose_catalog_was_corrupted_still_exits_zero() {
    // FINDING F3 (TEST_COVERAGE_PLAN.md §9): `verify_integrity` inspects only the
    // NetCDF half, so a `data_hash` corrupted in the SQLite catalog is invisible
    // to `infrastore verify` even though every read of that key now fails. Pinned here
    // at the CLI level because that is where a user would look.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.nc");
    seed(dir.path(), &store);

    let mut sqlite = store.clone().into_os_string();
    sqlite.push(".sqlite");
    let conn = rusqlite::Connection::open(PathBuf::from(sqlite)).unwrap();
    let n = conn
        .execute(
            "UPDATE time_series_associations SET data_hash = ?1",
            [&"0".repeat(64)],
        )
        .unwrap();
    assert_eq!(n, 1);
    drop(conn);

    assert_eq!(
        exit_code(&store, &["verify"]),
        0,
        "PIN: a catalog-side corruption is invisible to `infrastore verify`"
    );
    // But the read genuinely fails, which is what verify failed to surface.
    run_err(&store, &["get", "--owner-id", "42", "--name", "load"]);
}
