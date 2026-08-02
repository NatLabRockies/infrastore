//! End-to-end round-trip tests: drive the `infrastore` binary to add series from a
//! descriptor + CSV, then read them back via `get -f csv` and compare values.

use std::fs;
use std::path::Path;
use std::process::Command;

/// Run `infrastore --store <store> <args...>`, asserting success, returning stdout.
fn run(store: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_infrastore"))
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

/// Collect the non-empty data lines (skipping the header) from CSV output.
fn data_lines(csv: &str) -> Vec<String> {
    csv.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .skip(1)
        .map(str::to_string)
        .collect()
}

/// Data rows with the leading `timestamp` column dropped.
///
/// Every sequential CSV the CLI writes carries a timestamp column, a
/// SingleTimeSeries included: its grid lives in metadata that a piped file does
/// not carry, so emitting values alone silently dropped the time axis.
fn value_lines(csv: &str) -> Vec<String> {
    data_lines(csv)
        .iter()
        .map(|line| match line.split_once(',') {
            Some((_ts, rest)) => rest.to_string(),
            None => line.clone(),
        })
        .collect()
}

/// Flatten every CSV value cell (skipping the header and the timestamp column)
/// into one list.
fn flat_values(csv: &str) -> Vec<String> {
    value_lines(csv)
        .iter()
        .flat_map(|line| line.split(',').map(str::to_string).collect::<Vec<_>>())
        .collect()
}

fn write(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

/// Write a data CSV, prepending the header row `add` requires.
///
/// Test bodies are written as data only. The generated header is deliberately
/// *not* named `timestamp` or `issue_time`, so layout detection keeps reading
/// the body as the flat write layout.
fn write_csv(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
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
        let store = dir.path().join(format!("{dtype}.h5"));
        write_csv(dir.path(), "data.csv", csv_body);
        let json = format!(
            r#"{{
  "owner_id": 42,
  "owner_type": "Generator",
  "name": "load",
  "type": "single",
  "element_type": "{dtype}",
  "csv": "data.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H"
}}"#
        );
        let descriptor = write(dir.path(), "s.json", &json);
        run(
            &store,
            &["add", "--descriptor", descriptor.to_str().unwrap()],
        );

        let out = run(
            &store,
            &[
                "-f",
                "csv",
                "get",
                "--owner-id",
                "42",
                "--owner-category",
                "component",
                "--name",
                "load",
            ],
        );
        assert_eq!(value_lines(&out), *expected, "dtype {dtype} round-trip");
    }
}

#[test]
fn non_sequential_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("ns.h5");
    write_csv(
        dir.path(),
        "ns.csv",
        "2024-01-01T00:00:00Z,10\n2024-01-01T05:00:00Z,20\n2024-01-02T00:00:00Z,30\n",
    );
    let descriptor = write(
        dir.path(),
        "ns.json",
        r#"{
  "owner_id": 9,
  "owner_type": "Generator",
  "name": "events",
  "type": "non_sequential",
  "element_type": "f64",
  "csv": "ns.csv"
}"#,
    );
    run(
        &store,
        &["add", "--descriptor", descriptor.to_str().unwrap()],
    );

    let out = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "9", "--name", "events"],
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
    let det_store = dir.path().join("det.h5");
    write_csv(dir.path(), "det.csv", "1\n2\n3\n4\n5\n6\n");
    let det = write(
        dir.path(),
        "det.json",
        r#"{
  "owner_id": 1,
  "owner_type": "Generator",
  "name": "det",
  "type": "deterministic",
  "element_type": "i64",
  "csv": "det.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H",
  "horizon": "PT2H",
  "interval": "PT1H",
  "count": 3
}"#,
    );
    run(&det_store, &["add", "--descriptor", det.to_str().unwrap()]);
    let out = run(
        &det_store,
        &["-f", "csv", "get", "--owner-id", "1", "--name", "det"],
    );
    // Timestamped forecast CSV: one row per (window, step), values in
    // (window, step) order over the stored [H, count] layout.
    assert_eq!(out.lines().next().unwrap(), "issue_time,target_time,value");
    let lines = data_lines(&out);
    assert_eq!(lines.len(), 6);
    let values: Vec<&str> = lines.iter().map(|l| l.split(',').nth(2).unwrap()).collect();
    assert_eq!(values, ["1", "4", "2", "5", "3", "6"]);
    assert!(lines[0].starts_with("2024-01-01T00:00:00+00:00,2024-01-01T00:00:00+00:00"));
    assert!(lines[1].starts_with("2024-01-01T00:00:00+00:00,2024-01-01T01:00:00+00:00"));

    // Probabilistic: P=3, H=2, count=2 -> 12 flat values.
    let prob_store = dir.path().join("prob.h5");
    let prob_vals: String = (1..=12).map(|i| format!("{i}\n")).collect();
    write_csv(dir.path(), "prob.csv", &prob_vals);
    let prob = write(
        dir.path(),
        "prob.json",
        r#"{
  "owner_id": 2,
  "owner_type": "Generator",
  "name": "prob",
  "type": "probabilistic",
  "element_type": "i64",
  "csv": "prob.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H",
  "horizon": "PT2H",
  "interval": "PT1H",
  "count": 2,
  "percentiles": [10.0, 50.0, 90.0]
}"#,
    );
    run(
        &prob_store,
        &["add", "--descriptor", prob.to_str().unwrap()],
    );
    let out = run(
        &prob_store,
        &["-f", "csv", "get", "--owner-id", "2", "--name", "prob"],
    );
    // P=3 value columns, count*H = 4 rows.
    assert_eq!(
        out.lines().next().unwrap(),
        "issue_time,target_time,value[p10],value[p50],value[p90]"
    );
    let lines = data_lines(&out);
    assert_eq!(lines.len(), 4);
    assert_eq!(lines.iter().flat_map(|l| l.split(',').skip(2)).count(), 12);

    // Scenarios: scenario_count inferred (8 values / (H=2 * count=2) = 2).
    let scen_store = dir.path().join("scen.h5");
    let scen_vals: String = (1..=8).map(|i| format!("{i}\n")).collect();
    write_csv(dir.path(), "scen.csv", &scen_vals);
    let scen = write(
        dir.path(),
        "scen.json",
        r#"{
  "owner_id": 3,
  "owner_type": "Generator",
  "name": "scen",
  "type": "scenarios",
  "element_type": "i64",
  "csv": "scen.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H",
  "horizon": "PT2H",
  "interval": "PT1H",
  "count": 2
}"#,
    );
    run(
        &scen_store,
        &["add", "--descriptor", scen.to_str().unwrap()],
    );
    let out = run(
        &scen_store,
        &["-f", "csv", "get", "--owner-id", "3", "--name", "scen"],
    );
    // S=2 value columns, count*H = 4 rows.
    assert_eq!(
        out.lines().next().unwrap(),
        "issue_time,target_time,value[s0],value[s1]"
    );
    let lines = data_lines(&out);
    assert_eq!(lines.len(), 4);
    assert_eq!(lines.iter().flat_map(|l| l.split(',').skip(2)).count(), 8);
}

#[test]
fn multidim_single_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("md.h5");
    write_csv(dir.path(), "md.csv", "1,2\n3,4\n5,6\n");
    let descriptor = write(
        dir.path(),
        "md.json",
        r#"{
  "owner_id": 5,
  "owner_type": "Generator",
  "name": "curve",
  "type": "single",
  "element_type": "f64",
  "csv": "md.csv",
  "element_shape": [2],
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H"
}"#,
    );
    run(
        &store,
        &["add", "--descriptor", descriptor.to_str().unwrap()],
    );
    let out = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "5", "--name", "curve"],
    );
    assert_eq!(flat_values(&out), ["1", "2", "3", "4", "5", "6"]);
}

#[test]
fn list_info_and_json_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("ok.h5");
    write_csv(dir.path(), "d.csv", "1\n2\n3\n4\n");
    let descriptor = write(
        dir.path(),
        "d.json",
        r#"{
  "owner_id": 42,
  "owner_type": "Generator",
  "name": "load",
  "type": "single",
  "element_type": "f64",
  "units": "MW",
  "csv": "d.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H"
}"#,
    );
    run(
        &store,
        &["add", "--descriptor", descriptor.to_str().unwrap()],
    );

    // list in all three formats
    let table = run(&store, &["list"]);
    assert!(
        table.contains("Category"),
        "list table includes the owner category column"
    );
    let csv = run(&store, &["-f", "csv", "list"]);
    assert!(
        csv.contains("owner_category") || csv.contains("Category"),
        "list csv includes owner category header"
    );
    let json = run(&store, &["-f", "json", "list"]);
    assert!(json.contains("\"items\""), "list json wraps items");
    assert!(
        json.contains("\"owner_category\""),
        "list json includes owner_category"
    );

    // filtering by owner category resolves the same series
    let filtered = run(
        &store,
        &["-f", "json", "list", "--owner-category", "component"],
    );
    assert!(
        filtered.contains("\"name\": \"load\"") || filtered.contains("\"name\":\"load\""),
        "owner-category filter matches component-owned series"
    );

    // info json carries stats and owner_category
    let info = run(
        &store,
        &[
            "-f",
            "json",
            "info",
            "--owner-id",
            "42",
            "--owner-category",
            "component",
            "--name",
            "load",
        ],
    );
    assert!(info.contains("\"mean\""), "info json includes stats");
    assert!(
        info.contains("\"owner_category\""),
        "info json includes owner_category"
    );
}

#[test]
fn batch_json_array_adds_multiple() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("batch.h5");
    write_csv(dir.path(), "a.csv", "1\n2\n3\n");
    write_csv(dir.path(), "b.csv", "4\n5\n6\n");
    let descriptor = write(
        dir.path(),
        "batch.json",
        r#"[
  {
    "owner_id": 10,
    "owner_type": "Generator",
    "name": "series_a",
    "type": "single",
    "element_type": "f64",
    "csv": "a.csv",
    "initial_timestamp": "2024-01-01T00:00:00Z",
    "resolution": "PT1H"
  },
  {
    "owner_id": 10,
    "owner_type": "Generator",
    "name": "series_b",
    "type": "single",
    "element_type": "f64",
    "csv": "b.csv",
    "initial_timestamp": "2024-01-01T00:00:00Z",
    "resolution": "PT1H"
  }
]"#,
    );
    run(
        &store,
        &["add", "--descriptor", descriptor.to_str().unwrap()],
    );

    let out_a = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "10", "--name", "series_a"],
    );
    assert_eq!(value_lines(&out_a), ["1", "2", "3"]);

    let out_b = run(
        &store,
        &["-f", "csv", "get", "--owner-id", "10", "--name", "series_b"],
    );
    assert_eq!(value_lines(&out_b), ["4", "5", "6"]);
}

/// Seed a store with two SingleTimeSeries (owners 1 and 2, name "load").
fn seed_two(dir: &Path, store: &Path) {
    write_csv(dir, "d.csv", "1.0\n2.0\n3.0\n4.0\n");
    for owner in [1, 2] {
        let json = format!(
            r#"{{
  "owner_id": {owner},
  "owner_type": "Generator",
  "name": "load",
  "type": "single",
  "element_type": "f64",
  "csv": "d.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H"
}}"#
        );
        let descriptor = write(dir, "s.json", &json);
        run(
            store,
            &["add", "--descriptor", descriptor.to_str().unwrap()],
        );
    }
}

#[test]
fn admin_commands_json() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("admin.h5");
    seed_two(dir.path(), &store);

    let stats = run(&store, &["-f", "json", "stats"]);
    assert!(
        stats.contains("\"associations.static\": 2"),
        "stats: {stats}"
    );
    assert!(stats.contains("arrays.distinct_total"), "stats: {stats}");

    let res = run(&store, &["-f", "json", "resolutions"]);
    assert!(res.contains("PT1H"), "resolutions: {res}");

    let verify = run(&store, &["-f", "json", "verify"]);
    assert!(verify.contains("\"errors\""), "verify: {verify}");

    let cc = run(&store, &["-f", "json", "check-consistency"]);
    assert!(cc.contains("PT1H"), "check-consistency: {cc}");

    let summary = run(&store, &["-f", "json", "summary"]);
    assert!(summary.contains("\"static\""), "summary: {summary}");

    // Table output smoke-check (must not crash).
    run(&store, &["stats"]);
    run(&store, &["summary"]);
}

#[test]
fn rename_and_remove_all() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("rn.h5");
    seed_two(dir.path(), &store);

    // Rename owner 1's series.
    run(
        &store,
        &[
            "rename",
            "--owner-id",
            "1",
            "--owner-category",
            "component",
            "--name",
            "load",
            "--new-name",
            "load2",
        ],
    );
    let list = run(&store, &["-f", "json", "list", "--owner-id", "1"]);
    assert!(list.contains("load2"), "renamed list: {list}");
    assert!(!list.contains("\"load\""), "old name gone: {list}");

    // Remove every "load" series (owner 2 still has it) with --all.
    run(&store, &["remove", "--all", "--force", "--name", "load"]);
    let after = run(&store, &["-f", "json", "list", "--name", "load"]);
    assert!(after.contains("\"items\": []"), "removed: {after}");
}

/// Run `infrastore`, expecting failure; returns stderr.
/// Run `infrastore`, asserting a nonzero exit **and** the `Error: ` stderr prefix that
/// `main` writes. The prefix is part of the CLI's contract with a shell caller:
/// it is how a user tells a diagnostic apart from log output.
fn run_err(store: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_infrastore"))
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

/// Seed one store with distinctly-named series for glob/export tests.
fn seed_named(dir: &Path, store: &Path, names: &[&str]) {
    write_csv(dir, "n.csv", "1.0\n2.0\n3.0\n");
    for (i, name) in names.iter().enumerate() {
        let json = format!(
            r#"{{
  "owner_id": {},
  "owner_type": "Generator",
  "name": "{name}",
  "type": "single",
  "element_type": "f64",
  "csv": "n.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H"
}}"#,
            i + 1
        );
        let descriptor = write(dir, "n.json", &json);
        run(
            store,
            &["add", "--descriptor", descriptor.to_str().unwrap()],
        );
    }
}

#[test]
fn name_glob_selector() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("glob.h5");
    seed_named(dir.path(), &store, &["wind_speed", "wind_dir", "solar"]);

    let list = run(&store, &["-f", "json", "list", "--name-glob", "wind_*"]);
    assert!(list.contains("wind_speed") && list.contains("wind_dir"));
    assert!(!list.contains("solar"));

    run(
        &store,
        &["remove", "--all", "--force", "--name-glob", "wind_*"],
    );
    let after = run(&store, &["-f", "json", "list"]);
    assert!(after.contains("solar") && !after.contains("wind_"));
}

#[test]
fn dry_run_mutates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("dry.h5");
    seed_two(dir.path(), &store);

    let out = run(&store, &["remove", "--all", "--dry-run", "--name", "load"]);
    assert!(out.contains("Would remove 2 time series"), "dry-run: {out}");
    let out = run(&store, &["clear", "--dry-run"]);
    assert!(out.contains("Would clear 2"), "clear dry-run: {out}");
    let out = run(
        &store,
        &[
            "replace-owner",
            "--old",
            "1",
            "--new",
            "9",
            "--owner-category",
            "component",
            "--dry-run",
        ],
    );
    assert!(out.contains("Would reassign 1"), "replace dry-run: {out}");
    let out = run(
        &store,
        &[
            "rename",
            "--owner-id",
            "1",
            "--name",
            "load",
            "--new-name",
            "x",
            "--dry-run",
        ],
    );
    assert!(out.contains("Would rename"), "rename dry-run: {out}");

    // Nothing changed.
    let list = run(&store, &["-f", "json", "list"]);
    assert_eq!(list.matches("\"load\"").count(), 2, "unchanged: {list}");
}

#[test]
fn export_to_dir_and_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("exp.h5");
    seed_named(dir.path(), &store, &["a_series", "b_series"]);

    // stdout export requires a unique match.
    let out = run(&store, &["-f", "csv", "export", "--name", "a_series"]);
    assert_eq!(out.lines().next().unwrap(), "timestamp,value");
    assert_eq!(data_lines(&out).len(), 3);
    let err = run_err(&store, &["-f", "csv", "export"]);
    assert!(err.contains("--dir"), "multi-match error: {err}");

    // Directory export writes one file per series.
    let out_dir = dir.path().join("exported");
    run(
        &store,
        &["-f", "json", "export", "--dir", out_dir.to_str().unwrap()],
    );
    let mut files: Vec<String> = fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            "1_Generator_a_series_SingleTimeSeries.json",
            "2_Generator_b_series_SingleTimeSeries.json"
        ]
    );
    let body = fs::read_to_string(out_dir.join(&files[0])).unwrap();
    assert!(body.contains("\"values\""), "export json: {body}");

    // Table format is refused.
    let err = run_err(&store, &["export", "--name", "a_series"]);
    assert!(err.contains("csv or -f json"), "table refused: {err}");
}

#[test]
fn ext_round_trips_through_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("lt.h5");
    write_csv(dir.path(), "lt.csv", "1.0\n2.0\n");
    let descriptor = write(
        dir.path(),
        "lt.json",
        r#"{
  "owner_id": 5,
  "owner_type": "Generator",
  "name": "load",
  "type": "single",
  "element_type": "f64",
  "ext": "Profile",
  "csv": "lt.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H"
}"#,
    );
    run(
        &store,
        &["add", "--descriptor", descriptor.to_str().unwrap()],
    );
    let info = run(&store, &["-f", "json", "info", "--owner-id", "5"]);
    assert!(info.contains("\"ext\": \"Profile\""), "info: {info}");
    let list = run(&store, &["-f", "json", "list"]);
    assert!(list.contains("\"ext\": \"Profile\""), "list: {list}");
}

#[test]
fn compression_flag_only_on_creation() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("comp.h5");
    write_csv(dir.path(), "c.csv", "1.0\n2.0\n");
    let descriptor = write(
        dir.path(),
        "c.json",
        r#"{
  "owner_id": 1,
  "owner_type": "Generator",
  "name": "load",
  "type": "single",
  "element_type": "f64",
  "csv": "c.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H"
}"#,
    );
    run(
        &store,
        &[
            "add",
            "--descriptor",
            descriptor.to_str().unwrap(),
            "--compression",
            "deflate:9",
        ],
    );
    // A second add against the existing store must not accept the flag.
    let err = run_err(
        &store,
        &[
            "add",
            "--descriptor",
            descriptor.to_str().unwrap(),
            "--compression",
            "none",
        ],
    );
    assert!(err.contains("already exists"), "existing-store: {err}");
}

#[test]
fn completions_generate() {
    let output = Command::new(env!("CARGO_BIN_EXE_infrastore"))
        .args(["completions", "zsh"])
        .output()
        .expect("failed to spawn infrastore");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("_infrastore"), "zsh completion body: {text}");
}
