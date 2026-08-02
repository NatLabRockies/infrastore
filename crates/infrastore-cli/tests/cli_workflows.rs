//! End-to-end tests for the workflow surface added on top of the original
//! descriptor→CSV round trip: wide ingest and the columnar `grid` read, the
//! discovery commands, `diff`, association writes, plotting, and the safety
//! guards on `add` and `persist`.
//!
//! These drive the real binary rather than calling the modules, because most of
//! what they assert is about the *command line* — which flags are accepted, what
//! the exit status is, whether one command's output feeds the next.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

/// Run `infrastore --store <store> <args...>`, asserting success.
fn run(store: &Path, args: &[&str]) -> String {
    let output = raw(store, args);
    assert!(
        output.status.success(),
        "infrastore {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

/// Run `infrastore`, asserting a nonzero exit; returns stderr.
fn run_err(store: &Path, args: &[&str]) -> String {
    let output = raw(store, args);
    assert!(
        !output.status.success(),
        "infrastore {args:?} unexpectedly succeeded:\nstdout: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    String::from_utf8(output.stderr).expect("utf8 stderr")
}

fn raw(store: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_infrastore"))
        .arg("--store")
        .arg(store)
        .args(args)
        // Not a terminal either way under `cargo test`, but stated explicitly:
        // every confirmation prompt these tests reach must resolve without one.
        .stdin(Stdio::null())
        .output()
        .expect("failed to spawn infrastore")
}

fn write(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

/// Non-empty lines after the header.
fn data_lines(csv: &str) -> Vec<String> {
    csv.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .skip(1)
        .map(str::to_string)
        .collect()
}

/// A three-column wide CSV: two days of hourly values for three generators.
fn wide_csv(dir: &Path, name: &str) -> std::path::PathBuf {
    let mut body = String::from("timestamp,gen_001,gen_002,gen_003\n");
    for h in 0..4 {
        body.push_str(&format!(
            "2024-01-01T{h:02}:00:00Z,{},{},{}\n",
            h,
            10 + h,
            20 + h
        ));
    }
    write(dir, name, &body)
}

fn wide_descriptor(extra: &str) -> String {
    format!(
        r#"{{
  "csv": "wide.csv",
  "layout": "wide",
  "type": "SingleTimeSeries",
  "name": "max_active_power",
  "owner_type": "ThermalStandard",
  "element_type": "f64",
  "units": "MW",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H"{extra}
}}"#
    )
}

// --- A1: wide-CSV ingest ---------------------------------------------------

#[test]
fn a_wide_csv_loads_one_series_per_column_via_an_owner_map_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("wide.h5");
    wide_csv(dir.path(), "wide.csv");
    write(
        dir.path(),
        "components.csv",
        "column,owner_id,owner_type\ngen_001,1,ThermalStandard\ngen_002,2,\ngen_003,3,HydroDispatch\n",
    );
    let desc = write(
        dir.path(),
        "wide.json",
        &wide_descriptor(",\n  \"owner_map\": \"components.csv\""),
    );

    run(&store, &["add", "--descriptor", desc.to_str().unwrap()]);

    let listed = run(&store, &["-f", "csv", "list"]);
    assert_eq!(
        data_lines(&listed).len(),
        3,
        "one series per column: {listed}"
    );
    // The per-column owner_type wins where the map supplies one, and the
    // descriptor's is the default where it does not.
    assert!(listed.contains("HydroDispatch"), "{listed}");
    assert_eq!(
        listed.matches("ThermalStandard").count(),
        2,
        "gen_002 should fall back to the descriptor's owner_type: {listed}"
    );

    let values = run(
        &store,
        &[
            "-f",
            "csv",
            "get",
            "--owner-id",
            "2",
            "--name",
            "max_active_power",
        ],
    );
    let cells: Vec<String> = data_lines(&values)
        .iter()
        .map(|l| l.split(',').nth(1).unwrap().to_string())
        .collect();
    assert_eq!(
        cells,
        ["10", "11", "12", "13"],
        "column 2's values: {values}"
    );
}

#[test]
fn a_wide_csv_accepts_an_inline_owner_map_and_integer_headers() {
    let dir = tempfile::tempdir().unwrap();

    let inline_store = dir.path().join("inline.h5");
    wide_csv(dir.path(), "wide.csv");
    let inline = write(
        dir.path(),
        "inline.json",
        &wide_descriptor(",\n  \"owner_map\": {\"gen_001\": 11, \"gen_002\": 12, \"gen_003\": 13}"),
    );
    run(
        &inline_store,
        &["add", "--descriptor", inline.to_str().unwrap()],
    );
    let owners = run(&inline_store, &["-f", "csv", "owners"]);
    assert_eq!(data_lines(&owners), ["11", "12", "13"], "{owners}");

    // Headers that already are ids need no map at all.
    let header_store = dir.path().join("header.h5");
    write(
        dir.path(),
        "ids.csv",
        "timestamp,7,8\n2024-01-01T00:00:00Z,1,2\n2024-01-01T01:00:00Z,3,4\n",
    );
    let by_header = write(
        dir.path(),
        "byheader.json",
        &wide_descriptor(",\n  \"owner_id_from\": \"header\"").replace("wide.csv", "ids.csv"),
    );
    run(
        &header_store,
        &["add", "--descriptor", by_header.to_str().unwrap()],
    );
    let owners = run(&header_store, &["-f", "csv", "owners"]);
    assert_eq!(data_lines(&owners), ["7", "8"], "{owners}");
}

/// The failure that a 500-column load must not discover halfway through: the
/// error has to name the unmapped columns, or the caller is diffing two files
/// by hand.
#[test]
fn an_unmapped_wide_column_is_named_in_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("wide.h5");
    wide_csv(dir.path(), "wide.csv");
    let desc = write(
        dir.path(),
        "wide.json",
        &wide_descriptor(",\n  \"owner_map\": {\"gen_001\": 1}"),
    );
    let err = run_err(&store, &["add", "--descriptor", desc.to_str().unwrap()]);
    assert!(err.contains("gen_002"), "{err}");
    assert!(err.contains("gen_003"), "{err}");
    assert!(!store.exists(), "a rejected load must not create the store");
}

#[test]
fn a_wide_descriptor_rejects_the_fields_that_do_not_apply() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("wide.h5");
    wide_csv(dir.path(), "wide.csv");

    // An owner_id would contradict the per-column mapping.
    let with_owner = write(
        dir.path(),
        "a.json",
        &wide_descriptor(",\n  \"owner_id\": 5,\n  \"owner_map\": {\"gen_001\": 1}"),
    );
    let err = run_err(
        &store,
        &["add", "--descriptor", with_owner.to_str().unwrap()],
    );
    assert!(err.contains("owner_id"), "{err}");

    // A forecast's value block is already three axes deep before any split.
    let forecast = write(
        dir.path(),
        "b.json",
        &wide_descriptor(",\n  \"owner_map\": {\"gen_001\": 1}").replace(
            "\"type\": \"SingleTimeSeries\"",
            "\"type\": \"Deterministic\"",
        ),
    );
    let err = run_err(&store, &["add", "--descriptor", forecast.to_str().unwrap()]);
    assert!(err.to_lowercase().contains("wide"), "{err}");

    // No mapping at all is the mistake most worth a pointed message.
    let unmapped = write(dir.path(), "c.json", &wide_descriptor(""));
    let err = run_err(&store, &["add", "--descriptor", unmapped.to_str().unwrap()]);
    assert!(err.contains("owner_map"), "{err}");
    assert!(err.contains("owner_id_from"), "{err}");
}

// --- B6: grid, and the round trip that closes the pair ---------------------

#[test]
fn grid_renders_one_column_per_series_and_add_reads_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("wide.h5");
    wide_csv(dir.path(), "wide.csv");
    let desc = write(
        dir.path(),
        "wide.json",
        &wide_descriptor(",\n  \"owner_map\": {\"gen_001\": 1, \"gen_002\": 2, \"gen_003\": 3}"),
    );
    run(&store, &["add", "--descriptor", desc.to_str().unwrap()]);

    let grid = run(&store, &["-f", "csv", "grid", "--resolution", "PT1H"]);
    let header = grid.lines().next().unwrap();
    // One shared name, so the columns are bare owner ids — the form a wide
    // `add --owner-id-from header` reads straight back.
    assert_eq!(header, "timestamp,1,2,3", "{grid}");
    assert_eq!(data_lines(&grid).len(), 4, "{grid}");
    assert!(grid.contains("2024-01-01T00:00:00+00:00,0,10,20"), "{grid}");

    // Feed it back into a second store and confirm the values survive.
    let round_trip = dir.path().join("round.h5");
    write(dir.path(), "grid.csv", &grid);
    let back = write(
        dir.path(),
        "back.json",
        &wide_descriptor(",\n  \"owner_id_from\": \"header\"").replace("wide.csv", "grid.csv"),
    );
    run(
        &round_trip,
        &["add", "--descriptor", back.to_str().unwrap()],
    );
    let regrid = run(&round_trip, &["-f", "csv", "grid", "--resolution", "PT1H"]);
    assert_eq!(regrid, grid, "grid -> add -> grid must be a fixed point");
}

#[test]
fn grid_labels_columns_by_name_when_the_selection_spans_several() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("mixed.h5");
    write(dir.path(), "v.csv", "value\n1\n2\n");
    for (owner, name) in [(1, "load"), (2, "wind")] {
        let json = format!(
            r#"{{"owner_id": {owner}, "owner_type": "G", "name": "{name}",
                 "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
                 "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}}"#
        );
        let d = write(dir.path(), &format!("{name}.json"), &json);
        run(&store, &["add", "--descriptor", d.to_str().unwrap()]);
    }
    let grid = run(&store, &["-f", "csv", "grid", "--resolution", "PT1H"]);
    let header = grid.lines().next().unwrap();
    assert!(header.contains("load@1"), "{header}");
    assert!(header.contains("wind@2"), "{header}");

    // --label owner forces the bare form even then.
    let bare = run(
        &store,
        &[
            "-f",
            "csv",
            "grid",
            "--resolution",
            "PT1H",
            "--label",
            "owner",
        ],
    );
    assert_eq!(bare.lines().next().unwrap(), "timestamp,1,2", "{bare}");
}

// --- A2/A3/A4/A8: the other ways to run `add` ------------------------------

fn seed_one(dir: &Path, store: &Path) {
    write(dir, "v.csv", "value\n1\n2\n3\n");
    let json = r#"{"owner_id": 42, "owner_type": "Generator", "name": "load",
                   "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
                   "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#;
    let d = write(dir, "one.json", json);
    run(store, &["add", "--descriptor", d.to_str().unwrap()]);
}

#[test]
fn add_dry_run_reports_the_plan_without_creating_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("dry.h5");
    write(dir.path(), "v.csv", "value\n1\n2\n3\n");
    let json = r#"{"owner_id": 42, "owner_type": "Generator", "name": "load",
                   "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
                   "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#;
    let d = write(dir.path(), "one.json", json);

    let out = run(
        &store,
        &["add", "--descriptor", d.to_str().unwrap(), "--dry-run"],
    );
    assert!(out.contains("load"), "{out}");
    assert!(out.contains("Would add 1"), "{out}");
    assert!(!store.exists(), "--dry-run must not create the store");

    // And it is the shape errors it is for: a declared element_shape the data
    // does not divide by is caught before anything is written.
    let bad = write(
        dir.path(),
        "bad.json",
        &json.replace(
            "\"resolution\": \"PT1H\"",
            "\"resolution\": \"PT1H\", \"element_shape\": [2]",
        ),
    );
    let err = run_err(
        &store,
        &["add", "--descriptor", bad.to_str().unwrap(), "--dry-run"],
    );
    assert!(err.contains("divisible"), "{err}");
}

#[test]
fn add_reads_a_descriptor_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("stdin.h5");
    write(dir.path(), "v.csv", "value\n1\n2\n3\n");
    let json = format!(
        r#"{{"owner_id": 42, "owner_type": "Generator", "name": "load",
             "type": "SingleTimeSeries", "element_type": "f64", "csv": "{}",
             "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}}"#,
        dir.path().join("v.csv").display()
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_infrastore"))
        .arg("--store")
        .arg(&store)
        .args(["add", "--descriptor", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(json.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(data_lines(&run(&store, &["-f", "csv", "list"])).len(), 1);
}

#[test]
fn add_accepts_an_inline_descriptor_and_rejects_mixing_the_two_forms() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("inline.h5");
    let csv = write(dir.path(), "v.csv", "value\n1\n2\n3\n");

    run(
        &store,
        &[
            "add",
            "--csv",
            csv.to_str().unwrap(),
            "--owner-id",
            "42",
            "--owner-type",
            "Generator",
            "--name",
            "load",
            "--type",
            "SingleTimeSeries",
            "--element-type",
            "f64",
            "--units",
            "MW",
            "--resolution",
            "PT1H",
            "--initial-timestamp",
            "2024-01-01T00:00:00Z",
            "--feature",
            "model_year=2030",
        ],
    );
    let listed = run(&store, &["-f", "csv", "list"]);
    assert!(listed.contains("model_year=2030"), "{listed}");
    assert!(listed.contains("MW"), "{listed}");

    // Neither form at all, and both at once, are both errors that say so.
    let err = run_err(&store, &["add"]);
    assert!(err.contains("--descriptor"), "{err}");
    let d = write(dir.path(), "one.json", "{}");
    let err = run_err(
        &store,
        &["add", "--descriptor", d.to_str().unwrap(), "--name", "load"],
    );
    assert!(err.contains("inline"), "{err}");
}

#[test]
fn add_replace_makes_a_reload_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("replace.h5");
    seed_one(dir.path(), &store);
    let d = dir.path().join("one.json");

    // Without --replace the identity collides.
    let err = run_err(&store, &["add", "--descriptor", d.to_str().unwrap()]);
    assert!(!err.is_empty());

    // With it, the reload succeeds and there is still exactly one series.
    write(dir.path(), "v.csv", "value\n9\n8\n7\n");
    run(
        &store,
        &["add", "--descriptor", d.to_str().unwrap(), "--replace"],
    );
    let listed = run(&store, &["-f", "csv", "list"]);
    assert_eq!(data_lines(&listed).len(), 1, "{listed}");
    let values = run(&store, &["-f", "csv", "get", "--name", "load"]);
    assert!(values.contains(",9"), "the new values must win: {values}");
}

#[test]
fn add_batch_size_and_quiet_load_the_same_series() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("batch.h5");
    wide_csv(dir.path(), "wide.csv");
    let desc = write(
        dir.path(),
        "wide.json",
        &wide_descriptor(",\n  \"owner_map\": {\"gen_001\": 1, \"gen_002\": 2, \"gen_003\": 3}"),
    );
    let out = run(
        &store,
        &[
            "add",
            "--descriptor",
            desc.to_str().unwrap(),
            "--batch-size",
            "2",
            "--quiet",
        ],
    );
    assert!(out.is_empty(), "--quiet should print nothing: {out}");
    assert_eq!(data_lines(&run(&store, &["-f", "csv", "list"])).len(), 3);
}

// --- A6/A7: init and the catalog mode --------------------------------------

#[test]
fn init_creates_a_store_with_a_compression_policy() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("init.h5");
    run(&store, &["init", "--compression", "deflate:6"]);
    assert!(store.exists());
    let info = run(&store, &["-f", "csv", "store-info"]);
    assert!(info.contains("deflate:6"), "{info}");

    // A second init is an error rather than a silent no-op.
    let err = run_err(&store, &["init"]);
    assert!(err.contains("already exists"), "{err}");
}

#[test]
fn an_in_memory_catalog_reaches_disk_only_at_persist() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("mem.h5");
    write(dir.path(), "v.csv", "value\n1\n2\n3\n");
    let json = r#"{"owner_id": 42, "owner_type": "Generator", "name": "load",
                   "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
                   "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#;
    let d = write(dir.path(), "one.json", json);
    run(
        &store,
        &[
            "add",
            "--descriptor",
            d.to_str().unwrap(),
            "--catalog",
            "in-memory",
        ],
    );
    // The arrays streamed to the .h5 file, but no catalog was written beside it.
    assert!(store.exists());
    assert!(
        !dir.path().join("mem.h5.sqlite").exists(),
        "an in-memory catalog must not create the sqlite half until persist"
    );

    // Persisting from a fresh in-memory open writes the pair.
    let dest = dir.path().join("saved.h5");
    let out = run(&store, &["persist", "--dest", dest.to_str().unwrap()]);
    assert!(out.contains("Persisted"), "{out}");
    assert!(dest.exists() && dir.path().join("saved.h5.sqlite").exists());
}

// --- D: discovery ----------------------------------------------------------

#[test]
fn the_discovery_commands_answer_what_is_in_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("d.h5");
    write(dir.path(), "v.csv", "value\n1\n2\n3\n");
    for (owner, owner_type, name) in [(1, "Generator", "load"), (2, "Bus", "voltage")] {
        let json = format!(
            r#"{{"owner_id": {owner}, "owner_type": "{owner_type}", "name": "{name}",
                 "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
                 "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}}"#
        );
        let d = write(dir.path(), &format!("{name}.json"), &json);
        run(&store, &["add", "--descriptor", d.to_str().unwrap()]);
    }

    assert_eq!(
        data_lines(&run(&store, &["-f", "csv", "names"])),
        ["load", "voltage"]
    );
    assert_eq!(
        data_lines(&run(&store, &["-f", "csv", "owner-types"])),
        ["Bus", "Generator"]
    );
    assert_eq!(
        data_lines(&run(&store, &["-f", "csv", "owners"])),
        ["1", "2"]
    );
    // The selector scopes them, which is what makes them compose with `list`.
    assert_eq!(
        data_lines(&run(&store, &["-f", "csv", "names", "--owner-id", "1"])),
        ["load"]
    );
    // And a flag `owners` cannot honor is refused rather than ignored.
    let err = run_err(&store, &["owners", "--name", "load"]);
    assert!(err.contains("--name"), "{err}");
}

#[test]
fn exists_reports_through_its_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("e.h5");
    seed_one(dir.path(), &store);

    let found = run(&store, &["exists", "--owner-id", "42", "--name", "load"]);
    assert_eq!(found.trim(), "true");

    let output = raw(&store, &["exists", "--name", "nothing-here"]);
    assert!(!output.status.success(), "a miss must exit nonzero");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "false");
}

// --- B7: line-delimited JSON ----------------------------------------------

#[test]
fn jsonl_emits_one_object_per_line_with_no_enclosing_array() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("j.h5");
    seed_one(dir.path(), &store);
    write(dir.path(), "v2.csv", "value\n4\n5\n6\n");
    let second = write(
        dir.path(),
        "two.json",
        r#"{"owner_id": 43, "owner_type": "Generator", "name": "load",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "v2.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    run(&store, &["add", "--descriptor", second.to_str().unwrap()]);

    let out = run(&store, &["-f", "jsonl", "list"]);
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 2, "one line per series: {out}");
    for line in lines {
        let parsed: serde_json::Value = serde_json::from_str(line).expect("each line parses");
        assert_eq!(parsed["name"], "load");
    }
    assert!(!out.contains("\"items\""), "jsonl has no wrapper: {out}");
}

// --- B1/B2: the forecast views --------------------------------------------

fn seed_forecast(dir: &Path, store: &Path) {
    // 2 percentiles x 2 horizon steps x 3 windows.
    let mut body = String::from("value\n");
    for v in 0..12 {
        body.push_str(&format!("{v}\n"));
    }
    write(dir, "p.csv", &body);
    let json = r#"{"owner_id": 42, "owner_type": "Generator", "name": "load_prob",
                   "type": "Probabilistic", "element_type": "f64", "csv": "p.csv",
                   "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H",
                   "horizon": "PT2H", "interval": "PT1H", "count": 3,
                   "percentiles": [10.0, 90.0]}"#;
    let d = write(dir, "p.json", json);
    run(store, &["add", "--descriptor", d.to_str().unwrap()]);
}

#[test]
fn a_forecast_table_shows_issue_and_target_times_not_a_flat_index_dump() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("f.h5");
    seed_forecast(dir.path(), &store);

    let table = run(&store, &["get", "--name", "load_prob"]);
    assert!(table.contains("issue_time"), "{table}");
    assert!(table.contains("target_time"), "{table}");
    assert!(table.contains("value[p10]"), "{table}");
    assert!(!table.contains("row-major flat"), "{table}");
}

#[test]
fn a_forecast_window_can_be_selected_by_index_or_issue_time() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("f.h5");
    seed_forecast(dir.path(), &store);

    let all = run(&store, &["-f", "csv", "get", "--name", "load_prob"]);
    assert_eq!(data_lines(&all).len(), 6, "3 windows x 2 steps: {all}");

    let one = run(
        &store,
        &["-f", "csv", "get", "--name", "load_prob", "--window", "1"],
    );
    let rows = data_lines(&one);
    assert_eq!(rows.len(), 2, "{one}");
    assert!(rows[0].starts_with("2024-01-01T01:00:00"), "{one}");

    let by_time = run(
        &store,
        &[
            "-f",
            "csv",
            "get",
            "--name",
            "load_prob",
            "--issue-time",
            "2024-01-01T01:00:00Z",
        ],
    );
    assert_eq!(by_time, one, "--issue-time must resolve to the same window");

    // An issue time that is not a window boundary is a mistake, not a rounding.
    let err = run_err(
        &store,
        &[
            "get",
            "--name",
            "load_prob",
            "--issue-time",
            "2024-01-01T01:30:00Z",
        ],
    );
    assert!(err.contains("no window is issued"), "{err}");

    // And the flags do not silently do nothing on a static series.
    write(dir.path(), "v.csv", "value\n1\n2\n3\n");
    let d = write(
        dir.path(),
        "s.json",
        r#"{"owner_id": 7, "owner_type": "G", "name": "load",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    run(&store, &["add", "--descriptor", d.to_str().unwrap()]);
    let err = run_err(&store, &["get", "--name", "load", "--window", "0"]);
    assert!(err.contains("--window"), "{err}");
}

// --- B4/B5: row windows and richer stats -----------------------------------

#[test]
fn tail_and_stride_narrow_the_rows_a_table_shows() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("rows.h5");
    let mut body = String::from("value\n");
    for v in 0..24 {
        body.push_str(&format!("{v}\n"));
    }
    write(dir.path(), "v.csv", &body);
    let d = write(
        dir.path(),
        "s.json",
        r#"{"owner_id": 1, "owner_type": "G", "name": "load",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    run(&store, &["add", "--descriptor", d.to_str().unwrap()]);

    let tail = run(&store, &["get", "--name", "load", "--tail", "--limit", "3"]);
    assert!(
        tail.contains(" 23 "),
        "the last value must be shown: {tail}"
    );
    assert!(!tail.contains(" 0 "), "the first must not be: {tail}");

    // --stride shapes the data, so unlike --limit it applies to a pipe too.
    let strided = run(
        &store,
        &["-f", "csv", "get", "--name", "load", "--stride", "6"],
    );
    assert_eq!(data_lines(&strided).len(), 4, "{strided}");

    // A plain CSV read is still never truncated.
    let piped = run(
        &store,
        &["-f", "csv", "get", "--name", "load", "--limit", "3"],
    );
    assert_eq!(data_lines(&piped).len(), 24, "{piped}");
}

#[test]
fn info_reports_the_distribution_not_just_min_max_mean() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("stats.h5");
    let mut body = String::from("value\n");
    for v in 0..100 {
        body.push_str(&format!("{v}\n"));
    }
    write(dir.path(), "v.csv", &body);
    let d = write(
        dir.path(),
        "s.json",
        r#"{"owner_id": 1, "owner_type": "G", "name": "load",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    run(&store, &["add", "--descriptor", d.to_str().unwrap()]);

    let json: serde_json::Value =
        serde_json::from_str(&run(&store, &["-f", "json", "info", "--name", "load"])).unwrap();
    assert_eq!(json["p50"], 49.5);
    assert_eq!(json["first"], 0.0);
    assert_eq!(json["last"], 99.0);
    assert_eq!(json["non_finite"], 0);
    assert!(json["stddev"].as_f64().unwrap() > 28.0);
}

// --- C1/C2: plotting -------------------------------------------------------

#[test]
fn get_plot_draws_a_sparkline_with_the_range_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("p.h5");
    seed_one(dir.path(), &store);
    let out = run(&store, &["get", "--name", "load", "--plot"]);
    assert!(out.contains('▁') || out.contains('█'), "{out}");
    assert!(out.contains("min 1"), "{out}");
    assert!(out.contains("max 3"), "{out}");
}

#[test]
fn plot_writes_one_self_contained_document_for_every_kind() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("plot.h5");
    // A full day, so the heatmap has a grid to lay out.
    let mut body = String::from("value\n");
    for v in 0..48 {
        body.push_str(&format!("{}\n", v % 24));
    }
    write(dir.path(), "v.csv", &body);
    let d = write(
        dir.path(),
        "s.json",
        r#"{"owner_id": 1, "owner_type": "G", "name": "load", "units": "MW",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    run(&store, &["add", "--descriptor", d.to_str().unwrap()]);

    for kind in ["line", "duration", "heatmap"] {
        let out = dir.path().join(format!("{kind}.svg"));
        run(
            &store,
            &[
                "plot",
                "--name",
                "load",
                "--kind",
                kind,
                "--out",
                out.to_str().unwrap(),
            ],
        );
        let svg = fs::read_to_string(&out).unwrap();
        assert!(svg.starts_with("<svg "), "{kind}: {svg}");
        assert!(
            svg.contains("prefers-color-scheme"),
            "{kind} needs both themes"
        );
        assert!(!svg.contains("<script"), "{kind} must carry no script");
        assert!(!svg.contains("xlink:href"), "{kind} must reference nothing");
    }

    // An .html destination wraps the same document in a page.
    let html = dir.path().join("chart.html");
    run(
        &store,
        &["plot", "--name", "load", "--out", html.to_str().unwrap()],
    );
    let page = fs::read_to_string(&html).unwrap();
    assert!(page.starts_with("<!doctype html>"), "{page}");
    assert!(page.contains("<svg "), "{page}");

    // A fan needs a forecast, and says so when it does not get one.
    let err = run_err(
        &store,
        &["plot", "--name", "load", "--kind", "fan", "--out", "-"],
    );
    assert!(err.contains("fan"), "{err}");
}

#[test]
fn plot_refuses_more_series_than_it_has_distinguishable_colors() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("many.h5");
    write(dir.path(), "v.csv", "value\n1\n2\n3\n");
    for owner in 0..10 {
        let json = format!(
            r#"{{"owner_id": {owner}, "owner_type": "G", "name": "load",
                 "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
                 "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}}"#
        );
        let d = write(dir.path(), &format!("{owner}.json"), &json);
        run(&store, &["add", "--descriptor", d.to_str().unwrap()]);
    }
    let err = run_err(&store, &["plot", "--name", "load", "--out", "-"]);
    assert!(
        err.contains("grid"),
        "the error should point at grid: {err}"
    );
}

// --- F1/F2: diff and merge -------------------------------------------------

#[test]
fn diff_reports_added_removed_and_changed_and_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let left = dir.path().join("left.h5");
    let right = dir.path().join("right.h5");
    seed_one(dir.path(), &left);

    // An identical copy differs in nothing and exits 0.
    run(&left, &["persist", "--dest", right.to_str().unwrap()]);
    let same = run(&left, &["diff", "--against", right.to_str().unwrap()]);
    assert!(same.contains("0 added, 0 removed, 0 changed"), "{same}");

    // Change one series' values and add another.
    write(dir.path(), "v.csv", "value\n9\n9\n9\n");
    let d = dir.path().join("one.json");
    run(
        &left,
        &["add", "--descriptor", d.to_str().unwrap(), "--replace"],
    );
    write(dir.path(), "v2.csv", "value\n1\n2\n3\n");
    let extra = write(
        dir.path(),
        "extra.json",
        r#"{"owner_id": 99, "owner_type": "G", "name": "extra",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "v2.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    run(&left, &["add", "--descriptor", extra.to_str().unwrap()]);

    let output = raw(&left, &["diff", "--against", right.to_str().unwrap()]);
    assert!(!output.status.success(), "a difference must exit nonzero");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("changed"), "{text}");
    assert!(
        text.contains("removed"),
        "the extra series is only on the left: {text}"
    );
}

#[test]
fn merge_copies_series_between_stores() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("src.h5");
    let dest = dir.path().join("dst.h5");
    seed_one(dir.path(), &source);
    run(&dest, &["init"]);

    let dry = run(
        &dest,
        &["merge", "--from", source.to_str().unwrap(), "--dry-run"],
    );
    assert!(dry.contains("Would merge 1"), "{dry}");
    assert_eq!(data_lines(&run(&dest, &["-f", "csv", "list"])).len(), 0);

    run(&dest, &["merge", "--from", source.to_str().unwrap()]);
    let listed = run(&dest, &["-f", "csv", "list"]);
    assert_eq!(data_lines(&listed).len(), 1, "{listed}");
    // Values move as bytes, so the content hash is preserved exactly.
    let left_hash = run(&source, &["-f", "csv", "list"]);
    assert_eq!(
        left_hash.lines().nth(1).unwrap(),
        listed.lines().nth(1).unwrap()
    );
}

// --- E: association writes -------------------------------------------------

#[test]
fn attachments_and_links_can_be_written_from_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("assoc.h5");
    run(&store, &["init"]);

    run(
        &store,
        &[
            "attach",
            "--component-id",
            "42",
            "--component-type",
            "Generator",
            "--attribute-id",
            "7",
            "--attribute-type",
            "GeographicInfo",
        ],
    );
    let listed = run(&store, &["-f", "csv", "attributes"]);
    assert_eq!(data_lines(&listed).len(), 1, "{listed}");

    // The bulk form, whose header is checked because the two (id, type) pairs
    // are otherwise indistinguishable.
    let batch = write(
        dir.path(),
        "batch.csv",
        "component_id,component_type,attribute_id,attribute_type\n\
         43,Generator,8,GeographicInfo\n44,Bus,9,GeographicInfo\n",
    );
    run(&store, &["attach", "--from", batch.to_str().unwrap()]);
    assert_eq!(
        data_lines(&run(&store, &["-f", "csv", "attributes"])).len(),
        3
    );
    let swapped = write(
        dir.path(),
        "swapped.csv",
        "attribute_id,attribute_type,component_id,component_type\n8,X,43,Y\n",
    );
    let err = run_err(&store, &["attach", "--from", swapped.to_str().unwrap()]);
    assert!(err.contains("component_id,component_type"), "{err}");

    run(
        &store,
        &[
            "link",
            "--parent-id",
            "42",
            "--parent-type",
            "Generator",
            "--child-id",
            "1",
            "--child-type",
            "Bus",
        ],
    );
    assert_eq!(data_lines(&run(&store, &["-f", "csv", "links"])).len(), 1);

    // Reassigning moves both catalogs by default.
    let moved = run(&store, &["reassign", "--old", "42", "--new", "142"]);
    assert!(moved.contains("attachment"), "{moved}");
    assert!(moved.contains("link"), "{moved}");
    assert!(
        run(&store, &["-f", "csv", "attributes"]).contains("142"),
        "the attachment should have moved"
    );

    // A bare detach would empty the catalog, so it insists you mean it.
    let err = run_err(&store, &["detach"]);
    assert!(err.contains("--all"), "{err}");
    run(&store, &["detach", "--component-id", "142", "--force"]);
    assert_eq!(
        data_lines(&run(&store, &["-f", "csv", "attributes"])).len(),
        2
    );
    run(&store, &["unlink", "--all", "--force"]);
    assert_eq!(data_lines(&run(&store, &["-f", "csv", "links"])).len(), 0);
}

// --- G1/G2: the safety guards ---------------------------------------------

#[test]
fn persist_refuses_to_overwrite_without_being_told_to() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("p.h5");
    let dest = dir.path().join("backup.h5");
    seed_one(dir.path(), &store);

    let dry = run(
        &store,
        &["persist", "--dest", dest.to_str().unwrap(), "--dry-run"],
    );
    assert!(dry.contains("backup.h5"), "{dry}");
    assert!(!dest.exists(), "--dry-run must write nothing");

    run(&store, &["persist", "--dest", dest.to_str().unwrap()]);
    assert!(dest.exists());

    // The second save would replace an artifact that a failure could destroy.
    let err = run_err(&store, &["persist", "--dest", dest.to_str().unwrap()]);
    assert!(err.contains("--force"), "{err}");

    // Both --force and the global --yes get past it.
    run(
        &store,
        &["persist", "--dest", dest.to_str().unwrap(), "--force"],
    );
    run(
        &store,
        &["--yes", "persist", "--dest", dest.to_str().unwrap()],
    );
}
