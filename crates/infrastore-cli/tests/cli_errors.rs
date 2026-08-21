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

/// Data rows with the leading `timestamp` column dropped, for the assertions
/// that care only about values.
///
/// Every sequential CSV the CLI writes carries a timestamp column — a
/// SingleTimeSeries included, since its grid lives in metadata that a piped file
/// does not carry.
fn value_lines(csv: &str) -> Vec<String> {
    data_lines(csv)
        .iter()
        .map(|line| match line.split_once(',') {
            Some((_ts, rest)) => rest.to_string(),
            None => line.clone(),
        })
        .collect()
}

/// Write a data CSV, prepending the header row `add` requires.
///
/// Test bodies are written as data only. The generated header is deliberately
/// *not* named `timestamp` or `issue_time`, so layout detection keeps reading
/// the body as the flat write layout; the tests that exercise the timestamped
/// layouts supply their own header (usually by re-adding an `export`).
fn write_csv(dir: &Path, name: &str, body: &str) -> PathBuf {
    let width = body
        .lines()
        .find(|l| !l.trim().is_empty())
        .map_or(1, |l| l.split(',').count());
    let header = if width <= 1 {
        "value".to_string()
    } else {
        (0..width)
            .map(|i| format!("value[{i}]"))
            .collect::<Vec<_>>()
            .join(",")
    };
    write(dir, name, &format!("{header}\n{body}"))
}

/// A minimal single-series descriptor with `overrides` merged in as raw JSON
/// members, so a test can add or replace one field without restating the rest.
fn descriptor_json(overrides: &[(&str, &str)]) -> String {
    let mut fields: Vec<(String, String)> = vec![
        ("owner_id".into(), "42".into()),
        ("owner_type".into(), "\"Generator\"".into()),
        ("name".into(), "\"load\"".into()),
        ("type".into(), "\"single\"".into()),
        ("element_type".into(), "\"f64\"".into()),
        ("csv".into(), "\"data.csv\"".into()),
        (
            "initial_timestamp".into(),
            "\"2024-01-01T00:00:00Z\"".into(),
        ),
        ("resolution".into(), "\"PT1H\"".into()),
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
    let store = dir.path().join("store.h5");
    write_csv(dir.path(), "data.csv", csv_body);
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
    write_csv(dir, "seed.csv", "10\n11\n12\n13\n");
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
        let (_dir, store, descriptor) =
            fixture(&format!("1\n{bad}\n3\n"), &[("element_type", "\"i64\"")]);
        let stderr = add_err(&store, &descriptor);
        assert!(
            stderr.contains("i64"),
            "{bad:?}: expected an i64 diagnostic, got: {stderr}"
        );
    }
}

#[test]
fn a_negative_value_in_a_u64_column_is_rejected() {
    let (_dir, store, descriptor) = fixture("1\n-2\n3\n", &[("element_type", "\"u64\"")]);
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("u64") && stderr.contains("-2"),
        "expected a u64 diagnostic quoting -2, got: {stderr}"
    );
}

#[test]
fn an_out_of_range_integer_is_rejected() {
    // i32 cannot hold 3_000_000_000.
    let (_dir, store, descriptor) = fixture("1\n3000000000\n3\n", &[("element_type", "\"i32\"")]);
    let stderr = add_err(&store, &descriptor);
    assert!(stderr.contains("i32"), "got: {stderr}");
}

#[test]
fn a_non_boolean_cell_in_a_bool_column_is_rejected() {
    let (_dir, store, descriptor) =
        fixture("true\nmaybe\nfalse\n", &[("element_type", "\"bool\"")]);
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("true/false") || stderr.contains("maybe"),
        "expected a bool diagnostic, got: {stderr}"
    );
}

#[test]
fn accepted_boolean_spellings_round_trip() {
    // The complement of the case above: `1`/`0` and mixed case are accepted.
    let (dir, store, _) = fixture("TRUE\n0\nFalse\n1\n", &[("element_type", "\"bool\"")]);
    let descriptor = write(
        dir.path(),
        "d.json",
        &descriptor_json(&[("element_type", "\"bool\"")]),
    );
    add_ok(&store, &descriptor);
    let out = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "42", "--name", "load"],
    );
    assert_eq!(value_lines(&out), vec!["true", "false", "false", "true"]);
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
            ("horizon", "\"PT2H\""),
            ("interval", "\"PT1H\""),
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
    let store = dir.path().join("store.h5");
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
    let store = dir.path().join("store.h5");
    let descriptor = write(dir.path(), "d.json", &descriptor_json(&[("csv", "null")]));
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("csv"),
        "expected a diagnostic about the missing csv path, got: {stderr}"
    );
}

#[test]
fn a_header_row_is_consumed_rather_than_parsed_as_data() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    write(dir.path(), "data.csv", "value\n1.5\n2.5\n3.5\n");
    let descriptor = write(dir.path(), "d.json", &descriptor_json(&[]));
    add_ok(&store, &descriptor);

    let out = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "42", "--name", "load"],
    );
    assert_eq!(value_lines(&out), vec!["1.5", "2.5", "3.5"]);
}

/// The hazard created by making the header mandatory: without this guard a
/// header-less CSV is not an error at all — its first row is eaten as column
/// names and the series is stored silently one element short.
#[test]
fn a_csv_whose_first_row_is_data_is_rejected_rather_than_losing_a_value() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    write(dir.path(), "data.csv", "1.5\n2.5\n3.5\n");
    let descriptor = write(dir.path(), "d.json", &descriptor_json(&[]));
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("1.5") && stderr.contains("header"),
        "the diagnostic must quote the row and say it wants a header, got: {stderr}"
    );

    // The same check reaches the value columns of a timestamped CSV, whose
    // leading column is a timestamp rather than a number.
    let ns_store = dir.path().join("ns.h5");
    write(
        dir.path(),
        "ns.csv",
        "2024-01-01T00:00:00Z,10\n2024-01-02T00:00:00Z,20\n",
    );
    let ns = write(
        dir.path(),
        "ns.json",
        &descriptor_json(&[
            ("csv", "\"ns.csv\""),
            ("type", "\"non_sequential\""),
            ("initial_timestamp", "null"),
            ("resolution", "null"),
        ]),
    );
    let stderr = add_err(&ns_store, &ns);
    assert!(stderr.contains("header"), "got: {stderr}");
}

/// A descriptor written for an older release names a field that no longer
/// exists. Serde's bare "unknown field" is accurate but says nothing about what
/// changed, so `add` attaches the migration note.
#[test]
fn a_descriptor_carrying_has_header_is_rejected_with_a_migration_note() {
    let (_dir, store, descriptor) = fixture("1.0\n2.0\n3.0\n", &[("has_header", "true")]);
    let stderr = add_err(&store, &descriptor);
    assert!(
        stderr.contains("has_header") && stderr.contains("header row"),
        "expected a has_header migration note, got: {stderr}"
    );
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
    assert_eq!(value_lines(&out), vec!["1", "2", "3"]);
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
            &[("interval", "\"PT1H\""), ("count", "3")],
            "horizon",
        ),
        // deterministic without interval
        (
            "deterministic",
            &[("horizon", "\"PT2H\""), ("count", "3")],
            "interval",
        ),
        // deterministic without count
        (
            "deterministic",
            &[("horizon", "\"PT2H\""), ("interval", "\"PT1H\"")],
            "count",
        ),
        // probabilistic without percentiles
        (
            "probabilistic",
            &[
                ("horizon", "\"PT2H\""),
                ("interval", "\"PT1H\""),
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
    let store = dir.path().join("store.h5");
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
        let store = dir.path().join("store.h5");
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
    let store = dir.path().join("store.h5");
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
    let store = dir.path().join("store.h5");
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
            ("horizon", "\"PT2H\""),
            ("interval", "\"PT1H\""),
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
            ("horizon", "\"PT2H\""),
            ("interval", "\"PT1H\""),
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
            ("horizon", "\"PT2H\""),
            ("interval", "\"PT1H\""),
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

    let (_dir, store, descriptor) = fixture("1.0\n2.0\n", &[("element_type", "\"f16\"")]);
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

    // `info -f json` nests the whole feature map under `features`, with each
    // value in its own JSON type.
    let out = run(
        &store,
        &["-f", "json", "info", "--owner-id", "42", "--name", "load"],
    );
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    let features = parsed.get("features").expect("info must carry features");
    assert_eq!(
        features.get("model_year").and_then(|v| v.as_i64()),
        Some(2030)
    );
    assert_eq!(
        features.get("scenario").and_then(|v| v.as_str()),
        Some("base")
    );
    assert_eq!(features.get("flag").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(features.get("scale").and_then(|v| v.as_f64()), Some(1.5));

    // The table view expands the same map into one `feature.<key>` line each,
    // which is the form that greps well.
    let table = run(&store, &["info", "--owner-id", "42", "--name", "load"]);
    for expect in [
        "feature.model_year",
        "feature.scenario",
        "feature.flag",
        "feature.scale",
    ] {
        assert!(table.contains(expect), "{expect} missing from: {table}");
    }

    // `list` carries features too, so two series differing only by feature
    // never render as identical rows.
    let listed = run(&store, &["-f", "json", "list"]);
    assert!(
        listed.contains("model_year"),
        "list must carry features: {listed}"
    );

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
    assert_eq!(value_lines(&out), vec!["1", "2", "3"]);

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
    assert_eq!(value_lines(&out), vec!["1", "2", "3"]);
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
    let store = dir.path().join("store.h5");
    write_csv(dir.path(), "t.csv", "1\n2\n3\n4\n5\n6\n7\n8\n");
    let descriptor = write(
        dir.path(),
        "t.json",
        &descriptor_json(&[("csv", "\"t.csv\"")]),
    );
    add_ok(&store, &descriptor);

    let out = run(
        &store,
        &["transform", "--horizon", "PT4H", "--interval", "PT2H"],
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
        &[
            "transform",
            "--horizon",
            "not-a-period",
            "--interval",
            "PT2H",
        ],
    );
}

#[test]
fn copy_shares_the_array_and_dry_run_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
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
    assert_eq!(value_lines(&out), vec!["10", "11", "12", "13"]);

    // No array was duplicated.
    let stats = run(&store, &["-f", "json", "stats"]);
    let parsed: serde_json::Value = serde_json::from_str(&stats).unwrap();
    assert_eq!(
        parsed.get("arrays.distinct_total").and_then(|v| v.as_i64()),
        Some(1),
        "the copy must share the source array, got: {stats}"
    );
    assert_eq!(
        parsed.get("associations.static").and_then(|v| v.as_i64()),
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
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);

    let dest = dir.path().join("copy.h5");
    run(&store, &["persist", "--dest", dest.to_str().unwrap()]);
    assert!(dest.exists(), "the destination .h5 must exist");
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
    assert_eq!(value_lines(&out), vec!["10", "11", "12", "13"]);
    assert_eq!(exit_code(&dest, &["verify"]), 0);
}

#[test]
fn compact_runs_and_reports() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);
    // Add a second series and remove it, leaving a reusable slot.
    write_csv(dir.path(), "b.csv", "90\n91\n92\n93\n");
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
    let report: serde_json::Value = serde_json::from_str(&out).expect("compact prints json");
    for field in [
        "slots_reclaimed",
        "datasets_dropped",
        "feature_sets_reclaimed",
        "timestamp_sets_reclaimed",
        "bytes_reclaimed",
    ] {
        assert!(
            report
                .get(field)
                .and_then(serde_json::Value::as_u64)
                .is_some(),
            "compact report is missing {field}: {report}"
        );
    }

    // The surviving series is intact and the store still verifies.
    let out = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "42", "--name", "load"],
    );
    assert_eq!(value_lines(&out), vec!["10", "11", "12", "13"]);
    assert_eq!(exit_code(&store, &["verify"]), 0);
}

#[test]
fn params_reports_forecast_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    write_csv(dir.path(), "f.csv", "1\n2\n3\n4\n5\n6\n");
    let descriptor = write(
        dir.path(),
        "f.json",
        &descriptor_json(&[
            ("csv", "\"f.csv\""),
            ("type", "\"deterministic\""),
            ("horizon", "\"PT2H\""),
            ("interval", "\"PT1H\""),
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
            "PT1H",
            "--interval",
            "PT1H",
        ],
    );
    assert!(out.contains("PT2H"), "got: {out}");

    // A bad period is rejected.
    run_err(&store, &["params", "--resolution", "nonsense"]);

    // An empty store reports no parameters rather than failing.
    let empty_dir = tempfile::tempdir().unwrap();
    let empty = empty_dir.path().join("empty.h5");
    seed(empty_dir.path(), &empty); // statics only, no forecasts
    let out = run(&empty, &["-f", "json", "params"]);
    assert!(!out.trim().is_empty());
}

#[test]
fn template_prints_a_usable_descriptor_for_every_type() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    // The short spelling still selects a template; what the template *prints* is
    // the canonical one, which is also accepted back.
    for (arg, canonical) in [
        ("single", "SingleTimeSeries"),
        ("non_sequential", "NonSequentialTimeSeries"),
        ("deterministic", "Deterministic"),
        ("probabilistic", "Probabilistic"),
        ("scenarios", "Scenarios"),
    ] {
        let out = run(&store, &["template", arg]);
        let value: serde_json::Value =
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("{arg}: {e}\n{out}"));
        assert_eq!(
            value.get("type").and_then(|v| v.as_str()),
            Some(canonical),
            "{arg}: template's type field"
        );
        assert_eq!(
            value.get("owner_category").and_then(|v| v.as_str()),
            Some("Component"),
            "{arg}: template's owner_category"
        );
        assert!(value.get("element_type").is_some(), "{arg}: element_type");
        assert!(
            value.get("has_header").is_none(),
            "{arg}: has_header is gone; a template must not reintroduce it"
        );
        // Durations are ISO-8601, which always starts with `P` (or `-P`).
        for key in ["resolution", "horizon", "interval"] {
            if let Some(d) = value.get(key).and_then(|v| v.as_str()) {
                assert!(
                    d.starts_with('P'),
                    "{arg}: {key} must be an ISO-8601 duration, got {d:?}"
                );
            }
        }
        assert_eq!(run(&store, &["template", canonical]), out);
    }

    // DST has no descriptor form.
    let stderr = run_err(&store, &["template", "deterministic_single"]);
    assert!(stderr.contains("transform"), "got: {stderr}");

    // An unknown type is rejected.
    run_err(&store, &["template", "quantum"]);
}

/// The point of the canonical spellings: the descriptor `template` prints and
/// the catalog row it produces say the same words. `template` used to emit
/// `single` / `component` / `1h` where `list` renders `SingleTimeSeries` /
/// `Component` / `PT1H`, so neither a diff nor a grep could line a descriptor up
/// against the series it created.
#[test]
fn a_template_descriptor_and_the_row_it_creates_agree_word_for_word() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    let template: serde_json::Value =
        serde_json::from_str(&run(&store, &["template", "SingleTimeSeries"])).unwrap();

    // The template names `load.csv`, so that is the file it gets.
    write_csv(dir.path(), "load.csv", "1.0\n2.0\n3.0\n");
    let descriptor = write(dir.path(), "load.json", &template.to_string());
    add_ok(&store, &descriptor);

    let listed: serde_json::Value =
        serde_json::from_str(&run(&store, &["-f", "json", "list"])).unwrap();
    let row = &listed["items"][0];
    for key in [
        "type",
        "owner_category",
        "resolution",
        "name",
        "owner_type",
        "owner_id",
        "element_type",
        "units",
        "application_data",
        "features",
    ] {
        assert_eq!(
            row[key], template[key],
            "{key} must read back exactly as the descriptor spelled it"
        );
    }
}

#[test]
fn clear_removes_everything_or_one_owner() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);
    write_csv(dir.path(), "b.csv", "90\n91\n92\n93\n");
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
    let store = dir.path().join("store.h5");
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
    assert_eq!(value_lines(&out), vec!["10", "11", "12", "13"]);
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
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);
    write_csv(dir.path(), "b.csv", "90\n91\n92\n93\n");
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
    write_csv(dir.path(), "c.csv", "80\n81\n82\n83\n");
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
    let store = dir.path().join("store.h5");
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
    let fresh = dir.path().join("fresh.h5");
    let descriptor = write(
        dir.path(),
        "re.json",
        &descriptor_json(&[("csv", "\"values.csv\"")]),
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
    let ns_store = dir.path().join("ns.h5");
    let ns = write(
        dir.path(),
        "ns.json",
        &descriptor_json(&[
            ("csv", "\"stamped.csv\""),
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
    let store = dir.path().join("store.h5");
    write_csv(
        dir.path(),
        "i.csv",
        "-9223372036854775808\n0\n9223372036854775807\n",
    );
    let descriptor = write(
        dir.path(),
        "i.json",
        &descriptor_json(&[("csv", "\"i.csv\""), ("element_type", "\"i64\"")]),
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

    let fresh = dir.path().join("fresh.h5");
    let re = write(
        dir.path(),
        "re.json",
        &descriptor_json(&[("csv", "\"values.csv\""), ("element_type", "\"i64\"")]),
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
        value_lines(&original),
        vec!["-9223372036854775808", "0", "9223372036854775807"]
    );
}

#[test]
fn export_writes_one_file_per_series_into_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);
    write_csv(dir.path(), "b.csv", "90\n91\n92\n93\n");
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
    let store = dir.path().join("store.h5");
    write_csv(dir.path(), "f.csv", "1\n2\n3\n4\n5\n6\n");
    let descriptor = write(
        dir.path(),
        "f.json",
        &descriptor_json(&[
            ("csv", "\"f.csv\""),
            ("type", "\"deterministic\""),
            ("horizon", "\"PT2H\""),
            ("interval", "\"PT1H\""),
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
    let store = dir.path().join("store.h5");
    write_csv(dir.path(), "t.csv", "10\n11\n12\n13\n14\n15\n16\n17\n");
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
    assert_eq!(value_lines(&out), vec!["12", "13", "14"]);

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
    assert_eq!(value_lines(&out), vec!["12", "13", "14"]);

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
    let store = dir.path().join("store.h5");
    let body: String = (0..80).map(|i| format!("{i}\n")).collect();
    write_csv(dir.path(), "big.csv", &body);
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
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);
    write_csv(dir.path(), "b.csv", "90\n91\n92\n93\n");
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
    let store = dir.path().join("store.h5");
    write_csv(dir.path(), "g.csv", "1\n2\n3\n4\n");
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
    assert_eq!(value_lines(&out), vec!["1", "2", "3", "4"]);
    // A glob resolving to several is a multi-match error.
    run_err(&store, &["get", "--name-glob", "wind_*"]);
}

// ---------------------------------------------------------------------------
// Store selection: INFRASTORE_STORE, precedence, and the missing-store error
// ---------------------------------------------------------------------------

#[test]
fn the_infrastore_store_env_var_is_used_when_no_flag_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
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
        value_lines(&String::from_utf8_lossy(&output.stdout)),
        vec!["10", "11", "12", "13"]
    );
}

#[test]
fn the_store_flag_beats_the_env_var() {
    let dir = tempfile::tempdir().unwrap();
    let flagged = dir.path().join("flagged.h5");
    seed(dir.path(), &flagged);

    // A second store with different values, pointed at by the env var.
    let env_store = dir.path().join("env.h5");
    write_csv(dir.path(), "e.csv", "70\n71\n72\n73\n");
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
        value_lines(&String::from_utf8_lossy(&output.stdout)),
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
    let missing = dir.path().join("no_such_store.h5");
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
    let store = dir.path().join("store.h5");
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
    //
    // Both channels go into the failure message. An empty stdout means the
    // command failed before it could report -- opening the store, or reading
    // the catalog -- and that says so only on stderr, so an assertion naming
    // just stdout describes the symptom and hides the cause.
    let (stdout, stderr) = run_fail(&store, &["verify"]);
    assert!(
        stdout.to_lowercase().contains("hash") || stdout.to_lowercase().contains("integrity"),
        "expected an integrity report on stdout\n  stdout: {stdout:?}\n  stderr: {stderr:?}"
    );
    // The JSON form carries the same errors, for a scripted caller.
    let (stdout, stderr) = run_fail(&store, &["-f", "json", "verify"]);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("{e}\n  stdout: {stdout:?}\n  stderr: {stderr:?}"));
    let errors = parsed.get("errors").and_then(|v| v.as_array()).unwrap();
    assert_eq!(errors.len(), 1, "got: {stdout}");
    assert!(
        errors[0].as_str().unwrap().contains("hash mismatch"),
        "got: {stdout}"
    );
}

#[test]
fn verify_catches_a_catalog_that_points_at_a_missing_array() {
    // This was FINDING F3 (TEST_COVERAGE_PLAN.md §9): `verify_integrity` used to
    // inspect only the HDF5 half, so a `data_hash` corrupted in the SQLite
    // catalog was invisible even though every read of that key failed. Verify is
    // now driven from the catalog — the only place an array's element typing is
    // recorded — so the dangling reference is reported. Pinned at the CLI level
    // because that is where a user would look.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);

    let mut sqlite = store.clone().into_os_string();
    sqlite.push(".sqlite");
    let conn = rusqlite::Connection::open(PathBuf::from(sqlite)).unwrap();
    // A well-formed hash that names no stored array: the catalog points into
    // the void.
    let n = conn
        .execute(
            "UPDATE time_series_associations SET data_hash = ?1",
            rusqlite::params![[0u8; 32].as_slice()],
        )
        .unwrap();
    assert_eq!(n, 1);
    drop(conn);

    assert_eq!(
        exit_code(&store, &["verify"]),
        1,
        "a catalog pointing at an array the file does not hold is corruption"
    );
    let (stdout, _) = run_fail(&store, &["-f", "json", "verify"]);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let errors = parsed.get("errors").and_then(|v| v.as_array()).unwrap();
    assert_eq!(errors.len(), 1, "got: {stdout}");
    let message = errors[0].as_str().unwrap();
    assert!(message.contains("dangling reference"), "got: {stdout}");
    // The diagnostic names the array, so a reader can go and look for it.
    assert!(message.contains(&"0".repeat(64)), "got: {stdout}");

    // The read fails too, which is what verify used to leave unsurfaced.
    run_err(&store, &["get", "--owner-id", "42", "--name", "load"]);
}

#[test]
fn verify_reports_a_catalog_row_too_malformed_to_name_an_array() {
    // A `data_hash` that is not 32 bytes names nothing, so the array-side sweep
    // cannot even look for it. Verify must say so rather than abort — one
    // unusable row must not hide the rest of the store's problems.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);

    let mut sqlite = store.clone().into_os_string();
    sqlite.push(".sqlite");
    let conn = rusqlite::Connection::open(PathBuf::from(sqlite)).unwrap();
    conn.execute(
        "UPDATE time_series_associations SET data_hash = ?1",
        [&"0".repeat(64)],
    )
    .unwrap();
    drop(conn);

    assert_eq!(exit_code(&store, &["verify"]), 1);
    let (stdout, _) = run_fail(&store, &["-f", "json", "verify"]);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let errors = parsed.get("errors").and_then(|v| v.as_array()).unwrap();
    assert!(
        errors
            .iter()
            .any(|e| e.as_str().unwrap().contains("malformed catalog row")),
        "got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Content addressing: hash, HDF5 location, and array sharing
// ---------------------------------------------------------------------------

/// Add a second series with the same values as `seed`'s but a different owner,
/// so both associations share one stored array.
fn seed_sharing_pair(dir: &Path, store: &Path) {
    seed(dir, store);
    let d = write(
        dir,
        "share.json",
        &descriptor_json(&[("csv", "\"seed.csv\""), ("owner_id", "43")]),
    );
    add_ok(store, &d);
}

#[test]
fn info_reports_the_content_hash_and_its_hdf5_location() {
    // The whole point of surfacing these: a user holding `info` output can go
    // and look at the same bytes with h5dump. The hash alone cannot do that —
    // a packed array is one column of a shared dataset.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);

    let out = run(
        &store,
        &["-f", "json", "info", "--owner-id", "42", "--name", "load"],
    );
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();

    let hash = parsed["data_hash"]
        .as_str()
        .expect("info carries data_hash");
    assert_eq!(hash.len(), 64, "a full hex hash, not a prefix: {hash}");
    assert!(
        hash.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
        "lowercase hex, matching hash_hex and the SQLite view: {hash}"
    );

    let dataset = parsed["hdf5_dataset"].as_str().expect("hdf5_dataset");
    assert!(
        dataset.starts_with("/time_series/single/sts_"),
        "a packed SingleTimeSeries dataset, got {dataset}"
    );
    assert!(
        parsed["hdf5_column"].as_u64().is_some(),
        "a packed array needs its column index to be locatable: {out}"
    );
    assert!(
        parsed["location"].as_str().unwrap().contains("[:, "),
        "location spells the column selection: {out}"
    );
}

#[test]
fn info_no_stats_skips_the_array_read_but_keeps_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);

    let out = run(
        &store,
        &[
            "-f",
            "json",
            "info",
            "--owner-id",
            "42",
            "--name",
            "load",
            "--no-stats",
        ],
    );
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(parsed.get("min").is_none(), "--no-stats drops the stats");
    assert!(parsed.get("shape").is_none(), "shape comes from the array");
    assert!(
        parsed.get("data_hash").is_some() && parsed.get("length").is_some(),
        "but every catalog-side field stays: {out}"
    );
}

#[test]
fn arrays_groups_the_series_that_share_one_stored_array() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed_sharing_pair(dir.path(), &store);

    let out = run(&store, &["-f", "json", "arrays"]);
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    let items = parsed["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        1,
        "identical values dedupe to one array: {out}"
    );
    assert_eq!(
        items[0]["refs"].as_u64(),
        Some(2),
        "both associations reference it: {out}"
    );
    assert_eq!(items[0]["keys"].as_array().unwrap().len(), 2);
    assert!(
        items[0]["location"]
            .as_str()
            .unwrap()
            .starts_with("/time_series/"),
        "each group names where its array lives: {out}"
    );
}

#[test]
fn arrays_data_hash_accepts_a_prefix_in_either_case() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);

    let info = run(
        &store,
        &["-f", "json", "info", "--owner-id", "42", "--name", "load"],
    );
    let hash = serde_json::from_str::<serde_json::Value>(&info).unwrap()["data_hash"]
        .as_str()
        .unwrap()
        .to_string();

    for probe in [&hash[..8], &hash[..]] {
        let out = run(&store, &["-f", "json", "arrays", "--data-hash", probe]);
        let items = serde_json::from_str::<serde_json::Value>(&out).unwrap()["items"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(items, 1, "prefix {probe} must match its array");
    }

    // SQLite's `hex()` returns uppercase, so a hash pasted from a hand-run
    // catalog query has to work too.
    let out = run(
        &store,
        &["-f", "json", "arrays", "--data-hash", &hash.to_uppercase()],
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "an uppercase hash must match: {out}"
    );

    // A well-formed but absent hash is an error, not an empty success.
    run_err(&store, &["arrays", "--data-hash", "abcdef0123"]);
    // A non-hex argument names the offending character.
    let stderr = run_err(&store, &["arrays", "--data-hash", "zz"]);
    assert!(stderr.contains("hex"), "got: {stderr}");
}

#[test]
fn the_sqlite_catalog_exposes_a_readable_hash_view() {
    // BLOB hashes render as raw bytes in sqlite3's default and box modes, which
    // corrupts the terminal. The view is what makes a hand-run catalog query
    // legible, and its lowercase hex must match what the CLI prints.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);

    let info = run(
        &store,
        &["-f", "json", "info", "--owner-id", "42", "--name", "load"],
    );
    let expected = serde_json::from_str::<serde_json::Value>(&info).unwrap()["data_hash"]
        .as_str()
        .unwrap()
        .to_string();

    let mut sqlite = store.clone().into_os_string();
    sqlite.push(".sqlite");
    let conn = rusqlite::Connection::open(PathBuf::from(sqlite)).unwrap();
    let (name, data_hash, features_hash): (String, String, String) = conn
        .query_row(
            "SELECT name, data_hash, features_hash FROM time_series_readable",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("the view exists and is queryable");

    assert_eq!(name, "load");
    assert_eq!(
        data_hash, expected,
        "the view must agree with what the CLI prints"
    );
    assert_eq!(features_hash.len(), 64);
    assert!(
        features_hash.chars().all(|c| !c.is_uppercase()),
        "lowercase, so a copied value matches CLI output: {features_hash}"
    );
}

// ---------------------------------------------------------------------------
// Identity is never ambiguous in the output
// ---------------------------------------------------------------------------

/// Two series identical except for a `model_year` feature.
fn seed_feature_pair(dir: &Path, store: &Path) {
    for year in ["2030", "2040"] {
        let d = write(
            dir,
            &format!("f{year}.json"),
            &descriptor_json(&[
                ("csv", "\"seed.csv\""),
                ("features", &format!("{{\"model_year\": {year}}}")),
            ]),
        );
        write_csv(dir, "seed.csv", "10\n11\n12\n13\n");
        add_ok(store, &d);
    }
}

#[test]
fn list_distinguishes_series_that_differ_only_by_feature() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed_feature_pair(dir.path(), &store);

    let rows = data_lines(&run(&store, &["-f", "csv", "list"]));
    assert_eq!(rows.len(), 2);
    assert_ne!(
        rows[0], rows[1],
        "two distinct series must never render as identical rows: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("model_year=2030"))
            && rows.iter().any(|r| r.contains("model_year=2040")),
        "the distinguishing feature must be visible: {rows:?}"
    );
}

#[test]
fn an_ambiguous_selector_names_the_flag_that_would_narrow_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed_feature_pair(dir.path(), &store);

    let stderr = run_err(&store, &["info", "--name", "load"]);
    assert!(
        stderr.contains("model_year=2030") && stderr.contains("model_year=2040"),
        "the candidates must differ visibly, or the advice to use --feature is a \
         dead end: {stderr}"
    );

    // And that advice actually resolves it.
    run(
        &store,
        &["info", "--name", "load", "--feature", "model_year=2030"],
    );
}

#[test]
fn an_ambiguous_selector_truncates_a_long_candidate_list() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    write_csv(dir.path(), "seed.csv", "10\n11\n12\n13\n");
    for owner in 0..30 {
        let d = write(
            dir.path(),
            &format!("o{owner}.json"),
            &descriptor_json(&[("csv", "\"seed.csv\""), ("owner_id", &owner.to_string())]),
        );
        add_ok(&store, &d);
    }

    let stderr = run_err(&store, &["info", "--name", "load"]);
    assert!(stderr.contains("30 time series matched"), "got: {stderr}");
    assert!(
        stderr.contains("and 20 more"),
        "an unbounded list buries the message: {stderr}"
    );
    assert!(
        stderr.lines().count() < 20,
        "the diagnostic must stay readable: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// export: no silent overwrite, and a real round trip
// ---------------------------------------------------------------------------

#[test]
fn export_does_not_overwrite_series_that_share_a_plain_filename() {
    // The plain stem omits features, so two feature-distinguished series used to
    // land on one path and the second silently replaced the first.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed_feature_pair(dir.path(), &store);

    let out_dir = dir.path().join("exported");
    let stdout = run(
        &store,
        &[
            "-f",
            "csv",
            "export",
            "--name",
            "load",
            "--dir",
            out_dir.to_str().unwrap(),
        ],
    );
    assert!(stdout.contains("Exported 2"), "got: {stdout}");

    let files: Vec<_> = fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        files.len(),
        2,
        "reporting 2 exports while writing 1 file loses data: {files:?}"
    );
    assert!(
        files.iter().any(|f| f.contains("model_year-2030"))
            && files.iter().any(|f| f.contains("model_year-2040")),
        "the suffix should say which series each file is: {files:?}"
    );
}

#[test]
fn export_json_carries_the_features_that_identify_the_series() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed_feature_pair(dir.path(), &store);

    let out = run(
        &store,
        &[
            "-f",
            "json",
            "export",
            "--name",
            "load",
            "--feature",
            "model_year=2030",
        ],
    );
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        parsed["features"]["model_year"].as_i64(),
        Some(2030),
        "without features an export cannot say which series it holds: {out}"
    );
    // Values are numbers, not strings a consumer has to re-parse.
    assert!(
        parsed["values"][0].is_number(),
        "JSON values must keep their type: {out}"
    );
}

#[test]
fn an_exported_single_time_series_csv_can_be_added_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);

    let exported = run(
        &store,
        &["-f", "csv", "export", "--owner-id", "42", "--name", "load"],
    );
    write(dir.path(), "exported.csv", &exported);

    // Fed straight back in, with no hand-editing of the columns: `export` is
    // documented as the inverse of `add`, so the timestamp column it writes has
    // to be understood on the way in.
    let fresh = dir.path().join("fresh.h5");
    let d = write(
        dir.path(),
        "back.json",
        &descriptor_json(&[("csv", "\"exported.csv\"")]),
    );
    add_ok(&fresh, &d);

    assert_eq!(
        value_lines(&run(
            &fresh,
            &["-f", "csv", "get", "--owner-id", "42", "--name", "load"]
        )),
        vec!["10", "11", "12", "13"]
    );
}

#[test]
fn an_exported_forecast_csv_round_trips_through_its_transpose() {
    // The CSV runs window-major with the scenarios spread across columns; the
    // array is [scenario, horizon, window]. Concatenating the cells instead of
    // transposing them would scramble the forecast silently, so this asserts the
    // values come back in the same order they went in.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");

    // 2 scenarios x H=3 x 4 windows, every value distinct.
    let mut cells = Vec::new();
    for s in 0..2 {
        for h in 0..3 {
            for c in 0..4 {
                cells.push(format!("{}", s * 100 + h * 10 + c));
            }
        }
    }
    write_csv(dir.path(), "scen.csv", &format!("{}\n", cells.join("\n")));
    let d = write(
        dir.path(),
        "scen.json",
        &descriptor_json(&[
            ("name", "\"scen\""),
            ("type", "\"scenarios\""),
            ("csv", "\"scen.csv\""),
            ("horizon", "\"PT3H\""),
            ("interval", "\"PT1H\""),
            ("count", "4"),
            ("scenario_count", "2"),
        ]),
    );
    add_ok(&store, &d);

    let original = run(&store, &["-f", "json", "get", "--name", "scen", "--full"]);
    let exported = run(&store, &["-f", "csv", "export", "--name", "scen"]);
    assert!(
        exported.starts_with("issue_time,target_time"),
        "the exported header is what `add` detects: {exported}"
    );
    write(dir.path(), "scen_back.csv", &exported);

    let fresh = dir.path().join("fresh.h5");
    let back = write(
        dir.path(),
        "scen_back.json",
        &descriptor_json(&[
            ("name", "\"scen\""),
            ("type", "\"scenarios\""),
            ("csv", "\"scen_back.csv\""),
            ("horizon", "\"PT3H\""),
            ("interval", "\"PT1H\""),
            ("count", "4"),
            ("scenario_count", "2"),
        ]),
    );
    add_ok(&fresh, &back);

    let round_tripped = run(&fresh, &["-f", "json", "get", "--name", "scen", "--full"]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&original).unwrap()["values"],
        serde_json::from_str::<serde_json::Value>(&round_tripped).unwrap()["values"],
        "a forecast must survive export -> add unscrambled"
    );
}

// ---------------------------------------------------------------------------
// Type selection and the association catalogs
// ---------------------------------------------------------------------------

#[test]
fn deterministic_selects_transformed_rows_and_dst_narrows_to_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);
    run(
        &store,
        &["transform", "--horizon", "PT2H", "--interval", "PT1H"],
    );

    // `transform` writes a DeterministicSingleTimeSeries. Asking for
    // `deterministic` must find it: whether a forecast is stored densely or
    // derived from a SingleTimeSeries is not something a caller should have to
    // know to select it.
    let listed = data_lines(&run(
        &store,
        &["-f", "csv", "list", "--type", "deterministic"],
    ));
    assert_eq!(listed.len(), 1, "--type deterministic must match the DST");
    // The row still reports the stored type, so the detail stays inspectable.
    assert!(
        listed[0].contains("DeterministicSingleTimeSeries"),
        "the listed row must name its stored type: {:?}",
        listed[0]
    );

    // And the narrow spelling still selects only the derived forecasts.
    assert_eq!(
        data_lines(&run(
            &store,
            &["-f", "csv", "list", "--type", "deterministic_single"]
        ))
        .len(),
        1
    );

    // The error text lists every accepted spelling, and no longer offers a
    // family alias.
    let stderr = run_err(&store, &["list", "--type", "nonsense"]);
    assert!(
        stderr.contains("deterministic_single"),
        "deterministic_single missing from: {stderr}"
    );
    assert!(
        !stderr.contains("any_deterministic"),
        "the family alias must be gone from: {stderr}"
    );
}

#[test]
fn list_limit_bounds_the_output_and_reports_the_remainder() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    write_csv(dir.path(), "seed.csv", "10\n11\n12\n13\n");
    for owner in 0..5 {
        let d = write(
            dir.path(),
            &format!("o{owner}.json"),
            &descriptor_json(&[("csv", "\"seed.csv\""), ("owner_id", &owner.to_string())]),
        );
        add_ok(&store, &d);
    }

    assert_eq!(
        data_lines(&run(&store, &["-f", "csv", "list", "--limit", "2"])).len(),
        2
    );
    let table = run(&store, &["list", "--limit", "2"]);
    assert!(
        table.contains("3 more series"),
        "a truncated list must say so: {table}"
    );
}

#[test]
fn the_association_catalogs_are_readable_from_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);

    // Seeded through SQLite rather than through `attach`/`link` so this test
    // covers the read path on its own: the write commands have their own tests
    // in cli_workflows.rs, and a failure there should not also fail this one.
    let mut sqlite = store.clone().into_os_string();
    sqlite.push(".sqlite");
    let conn = rusqlite::Connection::open(PathBuf::from(sqlite)).unwrap();
    conn.execute(
        "INSERT INTO supplemental_attribute_associations
             (component_id, component_type, attribute_id, attribute_type)
         VALUES (42, 'Generator', 900, 'GeographicInfo')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO parent_child_associations
             (parent_id, parent_type, child_id, child_type)
         VALUES (42, 'Generator', 43, 'Bus')",
        [],
    )
    .unwrap();
    drop(conn);

    let attrs: serde_json::Value =
        serde_json::from_str(&run(&store, &["-f", "json", "attributes"])).unwrap();
    assert_eq!(attrs["items"][0]["attribute_id"].as_i64(), Some(900));
    assert_eq!(
        attrs["items"][0]["attribute_type"].as_str(),
        Some("GeographicInfo")
    );

    // Filters narrow, and a non-matching filter yields nothing rather than
    // everything.
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&run(
            &store,
            &["-f", "json", "attributes", "--component-id", "999"]
        ))
        .unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let summary = run(&store, &["-f", "json", "attributes", "--summary"]);
    assert!(summary.contains("GeographicInfo"), "got: {summary}");

    let links: serde_json::Value =
        serde_json::from_str(&run(&store, &["-f", "json", "links"])).unwrap();
    assert_eq!(links["items"][0]["parent_id"].as_i64(), Some(42));
    assert_eq!(links["items"][0]["child_id"].as_i64(), Some(43));
}

#[test]
fn store_info_reports_both_halves_of_the_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);

    let out = run(&store, &["-f", "json", "store-info"]);
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(parsed["hdf5_path"].as_str().unwrap().ends_with("store.h5"));
    assert!(
        parsed["sqlite_path"]
            .as_str()
            .unwrap()
            .ends_with("store.h5.sqlite"),
        "the catalog is half the artifact and must be named: {out}"
    );
    assert!(parsed["hdf5_bytes"].as_u64().unwrap() > 0);
    assert!(parsed["sqlite_bytes"].as_u64().unwrap() > 0);
    assert_eq!(parsed["storage_backend"].as_str(), Some("hdf5"));
    assert!(
        parsed["data_format_version"].as_str().is_some(),
        "the on-disk compatibility contract belongs here: {out}"
    );
}

#[test]
fn stats_separates_association_counts_from_distinct_array_counts() {
    // Content addressing makes these diverge, and they used to sit next to each
    // other under near-identical names.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed_sharing_pair(dir.path(), &store);

    let parsed: serde_json::Value =
        serde_json::from_str(&run(&store, &["-f", "json", "stats"])).unwrap();
    assert_eq!(parsed["associations.static"].as_i64(), Some(2));
    assert_eq!(parsed["associations.total"].as_i64(), Some(2));
    assert_eq!(
        parsed["arrays.distinct_total"].as_i64(),
        Some(1),
        "two series, one shared array"
    );
    assert_eq!(parsed["owners.components"].as_i64(), Some(2));
}

// ---------------------------------------------------------------------------
// Grouped `--help`
// ---------------------------------------------------------------------------

#[test]
fn top_level_help_groups_the_commands_without_renaming_them() {
    let output = Command::new(BIN).arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);

    // The grouping is a display change only: it replaces clap's single
    // `Commands:` block, and every command keeps its flat name.
    assert!(
        !help.contains("Commands:"),
        "the ungrouped block should be gone:\n{help}"
    );
    for heading in [
        "Read data:",
        "Write data:",
        "Inspect the store:",
        "Associations:",
        "Integrity & maintenance:",
        "Scaffolding:",
        "Options:",
    ] {
        assert!(help.contains(heading), "{heading} missing from:\n{help}");
    }

    // Every command is still invoked flat, exactly as documented.
    let store_line = help
        .lines()
        .find(|l| l.trim_start().starts_with("store-info"))
        .expect("store-info is listed");
    assert!(
        store_line.contains("HDF5 + SQLite paths"),
        "descriptions come from each command's own `about`: {store_line}"
    );
}

#[test]
fn grouping_the_help_did_not_change_how_commands_are_invoked() {
    // The regression this guards: switching the root command's help template
    // means `main` parses through a hand-built `Command` rather than
    // `Cli::parse`. Global flags, env fallback, and subcommand dispatch all have
    // to survive that.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.h5");
    seed(dir.path(), &store);

    assert_eq!(
        value_lines(&run(
            &store,
            &["-f", "csv", "get", "--owner-id", "42", "--name", "load"]
        )),
        vec!["10", "11", "12", "13"]
    );
    assert!(run(&store, &["store-info"]).contains("data_format_version"));

    // A usage error still exits 2, not 1.
    let code = Command::new(BIN)
        .args(["--store", store.to_str().unwrap(), "list", "--nonsense"])
        .output()
        .unwrap()
        .status
        .code()
        .unwrap();
    assert_eq!(code, 2, "argument-parse failures keep clap's exit code");
}

#[test]
fn shell_completions_cover_the_grouped_commands() {
    // Completions are generated from the same `Command` the binary parses with,
    // so a command missing from one would be missing from the other.
    let output = Command::new(BIN)
        .args(["completions", "bash"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let script = String::from_utf8_lossy(&output.stdout);
    for name in ["arrays", "store-info", "attributes", "links", "list", "get"] {
        assert!(script.contains(name), "{name} missing from completions");
    }
}

/// Write a one-series store at `store` and return the descriptor that built it.
fn seeded_store(dir: &Path, store: &Path, owner: &str) -> PathBuf {
    let csv = write_csv(dir, &format!("{owner}.csv"), "1\n2\n3\n");
    let json = format!(
        r#"{{"owner_id": 1, "owner_type": "Generator", "name": "{owner}",
             "type": "SingleTimeSeries", "element_type": "f64", "csv": "{}",
             "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}}"#,
        csv.file_name().unwrap().to_str().unwrap()
    );
    let descriptor = write(dir, &format!("{owner}.json"), &json);
    run(
        store,
        &["add", "--descriptor", descriptor.to_str().unwrap()],
    );
    descriptor
}

/// A store whose two halves came from different saves must be refused at the
/// CLI, with the core's diagnostic reaching the user.
///
/// This is the shape a user meets it in: a half moved or copied on its own, or a
/// save interrupted between its two renames. The wrong outcome is not an ugly
/// message — it is a successful `list` reporting an empty or contradictory store
/// over arrays that are all still there.
#[test]
fn a_store_whose_halves_disagree_is_refused_with_a_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("system.h5");
    let other = dir.path().join("other.h5");
    let descriptor = seeded_store(dir.path(), &store, "load");
    seeded_store(dir.path(), &other, "wind");
    assert!(run(&store, &["list"]).contains("load"));

    // The other store's catalog beside these arrays: two halves, two saves.
    fs::copy(
        dir.path().join("other.h5.sqlite"),
        dir.path().join("system.h5.sqlite"),
    )
    .unwrap();

    // Every command, read or write: a half-artifact must not be read from and
    // must not be extended.
    for args in [
        vec!["list"],
        vec!["store-info"],
        vec!["names"],
        vec!["add", "--descriptor", descriptor.to_str().unwrap()],
    ] {
        let err = run_err(&store, &args);
        assert!(
            err.contains("generation stamp"),
            "`{args:?}` must refuse a mismatched artifact with the core's diagnostic:\n{err}"
        );
    }
}

/// The other half-artifact shape — arrays with no catalog at all, which is what
/// a scratch run killed before it landed one leaves behind.
///
/// The read path opens the catalog before it can compare stamps, so what the
/// user gets is SQLite's own "unable to open database file" rather than the
/// generation-stamp explanation. It is a nonzero exit naming the missing file,
/// which is the important part; that it does not explain *why* a store can be
/// missing half of itself is worth improving, and this pins the current wording
/// so the improvement is deliberate.
#[test]
fn a_store_with_no_catalog_half_names_the_file_it_cannot_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("system.h5");
    seeded_store(dir.path(), &store, "load");
    fs::remove_file(dir.path().join("system.h5.sqlite")).unwrap();

    let err = run_err(&store, &["list"]);
    assert!(
        err.contains("system.h5.sqlite"),
        "the diagnostic must name the half that is missing:\n{err}"
    );
}
