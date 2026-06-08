//! End-to-end round-trip tests: drive the `tss` binary to add series from a
//! sidecar + CSV, then read them back via `get -f csv` and compare values.

use std::fs;
use std::path::Path;
use std::process::Command;

/// Run `tss --store <store> <args...>`, asserting success, returning stdout.
fn run(store: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_tss"))
        .arg("--store")
        .arg(store)
        .args(args)
        .output()
        .expect("failed to spawn tss");
    assert!(
        output.status.success(),
        "tss {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

/// Collect the non-empty data lines (skipping the header) from CSV output.
fn data_lines(csv: &str) -> Vec<String> {
    csv.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .skip(1)
        .map(str::to_string)
        .collect()
}

/// Flatten every CSV value cell (skipping the header) into one list.
fn flat_values(csv: &str) -> Vec<String> {
    data_lines(csv)
        .iter()
        .flat_map(|line| line.split(',').map(str::to_string).collect::<Vec<_>>())
        .collect()
}

fn write(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

#[test]
fn single_round_trip_all_dtypes() {
    let cases: &[(&str, &str, &[&str])] = &[
        ("f64", "1.5\n2.5\n3.5\n", &["1.5", "2.5", "3.5"]),
        ("f32", "1.5\n2.5\n3.5\n", &["1.5", "2.5", "3.5"]),
        ("i64", "1\n2\n3\n", &["1", "2", "3"]),
        ("i32", "1\n2\n3\n", &["1", "2", "3"]),
        ("u64", "1\n2\n3\n", &["1", "2", "3"]),
        ("bool", "true\nfalse\ntrue\n", &["true", "false", "true"]),
    ];
    let dir = tempfile::tempdir().unwrap();
    for (dtype, csv_body, expected) in cases {
        let store = dir.path().join(format!("{dtype}.nc"));
        write(dir.path(), "data.csv", csv_body);
        let toml = format!(
            r#"owner_uuid = "42"
owner_type = "Generator"
name = "load"
type = "single"
dtype = "{dtype}"
csv = "data.csv"
has_header = false
initial_timestamp = "2024-01-01T00:00:00Z"
resolution = "1h"
"#
        );
        let sidecar = write(dir.path(), "s.toml", &toml);
        run(&store, &["add", "--sidecar", sidecar.to_str().unwrap()]);

        let out = run(
            &store,
            &["-f", "csv", "get", "--owner-uuid", "42", "--name", "load"],
        );
        assert_eq!(data_lines(&out), *expected, "dtype {dtype} round-trip");
    }
}

#[test]
fn non_sequential_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("ns.nc");
    write(
        dir.path(),
        "ns.csv",
        "2024-01-01T00:00:00Z,10\n2024-01-01T05:00:00Z,20\n2024-01-02T00:00:00Z,30\n",
    );
    let sidecar = write(
        dir.path(),
        "ns.toml",
        r#"owner_uuid = "9"
owner_type = "Generator"
name = "events"
type = "non_sequential"
dtype = "f64"
csv = "ns.csv"
has_header = false
"#,
    );
    run(&store, &["add", "--sidecar", sidecar.to_str().unwrap()]);

    let out = run(
        &store,
        &["-f", "csv", "get", "--owner-uuid", "9", "--name", "events"],
    );
    // CSV: "timestamp,value" header, then ts,value rows. Check the value column.
    let values: Vec<String> = data_lines(&out)
        .iter()
        .map(|l| l.split(',').nth(1).unwrap().to_string())
        .collect();
    assert_eq!(values, vec!["10", "20", "30"]);
}

#[test]
fn forecast_round_trips() {
    let dir = tempfile::tempdir().unwrap();

    // Deterministic: H=2 (2h/1h), count=3 -> 6 flat values.
    let det_store = dir.path().join("det.nc");
    write(dir.path(), "det.csv", "1\n2\n3\n4\n5\n6\n");
    let det = write(
        dir.path(),
        "det.toml",
        r#"owner_uuid = "1"
owner_type = "Generator"
name = "det"
type = "deterministic"
dtype = "i64"
csv = "det.csv"
has_header = false
initial_timestamp = "2024-01-01T00:00:00Z"
resolution = "1h"
horizon = "2h"
interval = "1h"
count = 3
"#,
    );
    run(&det_store, &["add", "--sidecar", det.to_str().unwrap()]);
    let out = run(
        &det_store,
        &["-f", "csv", "get", "--owner-uuid", "1", "--name", "det"],
    );
    assert_eq!(flat_values(&out), ["1", "2", "3", "4", "5", "6"]);

    // Probabilistic: P=3, H=2, count=2 -> 12 flat values.
    let prob_store = dir.path().join("prob.nc");
    let prob_vals: String = (1..=12).map(|i| format!("{i}\n")).collect();
    write(dir.path(), "prob.csv", &prob_vals);
    let prob = write(
        dir.path(),
        "prob.toml",
        r#"owner_uuid = "2"
owner_type = "Generator"
name = "prob"
type = "probabilistic"
dtype = "i64"
csv = "prob.csv"
has_header = false
initial_timestamp = "2024-01-01T00:00:00Z"
resolution = "1h"
horizon = "2h"
interval = "1h"
count = 2
percentiles = [10.0, 50.0, 90.0]
"#,
    );
    run(&prob_store, &["add", "--sidecar", prob.to_str().unwrap()]);
    let out = run(
        &prob_store,
        &["-f", "csv", "get", "--owner-uuid", "2", "--name", "prob"],
    );
    assert_eq!(flat_values(&out).len(), 12);

    // Scenarios: scenario_count inferred (8 values / (H=2 * count=2) = 2).
    let scen_store = dir.path().join("scen.nc");
    let scen_vals: String = (1..=8).map(|i| format!("{i}\n")).collect();
    write(dir.path(), "scen.csv", &scen_vals);
    let scen = write(
        dir.path(),
        "scen.toml",
        r#"owner_uuid = "3"
owner_type = "Generator"
name = "scen"
type = "scenarios"
dtype = "i64"
csv = "scen.csv"
has_header = false
initial_timestamp = "2024-01-01T00:00:00Z"
resolution = "1h"
horizon = "2h"
interval = "1h"
count = 2
"#,
    );
    run(&scen_store, &["add", "--sidecar", scen.to_str().unwrap()]);
    let out = run(
        &scen_store,
        &["-f", "csv", "get", "--owner-uuid", "3", "--name", "scen"],
    );
    assert_eq!(flat_values(&out).len(), 8);
}

#[test]
fn multidim_single_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("md.nc");
    write(dir.path(), "md.csv", "1,2\n3,4\n5,6\n");
    let sidecar = write(
        dir.path(),
        "md.toml",
        r#"owner_uuid = "5"
owner_type = "Generator"
name = "curve"
type = "single"
dtype = "f64"
csv = "md.csv"
has_header = false
element_shape = [2]
initial_timestamp = "2024-01-01T00:00:00Z"
resolution = "1h"
"#,
    );
    run(&store, &["add", "--sidecar", sidecar.to_str().unwrap()]);
    let out = run(
        &store,
        &["-f", "csv", "get", "--owner-uuid", "5", "--name", "curve"],
    );
    assert_eq!(flat_values(&out), ["1", "2", "3", "4", "5", "6"]);
}

#[test]
fn list_info_and_json_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("ok.nc");
    write(dir.path(), "d.csv", "1\n2\n3\n4\n");
    let sidecar = write(
        dir.path(),
        "d.toml",
        r#"owner_uuid = "42"
owner_type = "Generator"
name = "load"
type = "single"
dtype = "f64"
units = "MW"
csv = "d.csv"
has_header = false
initial_timestamp = "2024-01-01T00:00:00Z"
resolution = "1h"
"#,
    );
    run(&store, &["add", "--sidecar", sidecar.to_str().unwrap()]);

    // list in all three formats
    run(&store, &["list"]);
    run(&store, &["-f", "csv", "list"]);
    let json = run(&store, &["-f", "json", "list"]);
    assert!(json.contains("\"items\""), "list json wraps items");

    // info json carries stats
    let info = run(
        &store,
        &["-f", "json", "info", "--owner-uuid", "42", "--name", "load"],
    );
    assert!(info.contains("\"mean\""), "info json includes stats");
}
