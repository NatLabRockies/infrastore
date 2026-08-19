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

/// `grid` slices and truncates like `get` does: `--time-range` selects the rows
/// read, `--limit`/`--full` bound the table only. A CSV pipe of a grid is fed
/// straight back into `add`, so a silent truncation there would drop data on the
/// floor.
#[test]
fn grid_time_range_selects_rows_and_the_table_bound_never_reaches_the_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("g.h5");
    let mut body = String::from("timestamp,gen_001,gen_002\n");
    for h in 0..8 {
        body.push_str(&format!("2024-01-01T{h:02}:00:00Z,{h},{}\n", 10 + h));
    }
    write(dir.path(), "wide.csv", &body);
    let desc = write(
        dir.path(),
        "wide.json",
        &wide_descriptor(",\n  \"owner_map\": {\"gen_001\": 1, \"gen_002\": 2}"),
    );
    run(&store, &["add", "--descriptor", desc.to_str().unwrap()]);

    let sliced = run(
        &store,
        &[
            "-f",
            "csv",
            "grid",
            "--resolution",
            "PT1H",
            "--time-range",
            "2024-01-01T02:00:00Z..2024-01-01T05:00:00Z",
        ],
    );
    let rows = data_lines(&sliced);
    assert_eq!(rows.len(), 3, "half-open, so 02, 03, 04: {sliced}");
    assert!(rows[0].starts_with("2024-01-01T02:00:00"), "{sliced}");
    assert!(
        rows[0].ends_with(",2,12"),
        "both columns come along: {sliced}"
    );

    // The table bound applies to the table.
    let table = run(&store, &["grid", "--resolution", "PT1H", "--limit", "2"]);
    assert!(table.contains("more rows"), "{table}");

    // But not to a pipe, which is read by a program that cannot tell.
    let piped = run(
        &store,
        &["-f", "csv", "grid", "--resolution", "PT1H", "--limit", "2"],
    );
    assert_eq!(data_lines(&piped).len(), 8, "{piped}");
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
    // A descriptor on stdin has no directory of its own to resolve a relative
    // `csv` against, so this one carries an absolute path — encoded by
    // `serde_json` rather than interpolated, because a Windows path is full of
    // backslashes and every one of them is an escape character to a JSON parser.
    let csv_path = serde_json::to_string(&dir.path().join("v.csv").to_string_lossy()).unwrap();
    let json = format!(
        r#"{{"owner_id": 42, "owner_type": "Generator", "name": "load",
             "type": "SingleTimeSeries", "element_type": "f64", "csv": {csv_path},
             "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}}"#
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_infrastore"))
        .arg("--store")
        .arg(&store)
        .args(["add", "--descriptor", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
        "add --descriptor - failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
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
fn an_in_memory_catalog_is_written_out_before_the_command_exits() {
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
    // The catalog was held in RAM for the load and written once at the end, so
    // the command still leaves a complete, readable artifact. It has to: the CLI
    // runs one command per process, and a catalog still in RAM at exit is lost
    // along with every array it names. (This used to defer the write to a later
    // `persist` invocation — a different process, whose catalog was empty, so
    // the load silently produced an unreadable store.)
    assert!(store.exists());
    assert!(
        dir.path().join("mem.h5.sqlite").exists(),
        "the catalog must reach disk before the command exits"
    );
    let listed = run(&store, &["list"]);
    assert!(
        listed.contains("load"),
        "the loaded series is addressable:\n{listed}"
    );

    // And it saves elsewhere like any other store.
    let dest = dir.path().join("saved.h5");
    let out = run(&store, &["persist", "--dest", dest.to_str().unwrap()]);
    assert!(out.contains("Persisted"), "{out}");
    assert!(dest.exists() && dir.path().join("saved.h5.sqlite").exists());
    let saved = run(&dest, &["list"]);
    assert!(
        saved.contains("load"),
        "the saved copy carries the data:\n{saved}"
    );
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

/// `export -f jsonl` used to be handled as a synonym for `-f json`, writing
/// pretty-printed documents into files named `.json` — which is precisely what a
/// line-oriented consumer of an export cannot read.
#[test]
fn export_jsonl_writes_one_series_per_line() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("e.h5");
    seed_one(dir.path(), &store);
    write(dir.path(), "v2.csv", "value\n4\n5\n6\n");
    let second = write(
        dir.path(),
        "two.json",
        r#"{"owner_id": 43, "owner_type": "Generator", "name": "spill",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "v2.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    run(&store, &["add", "--descriptor", second.to_str().unwrap()]);

    // A single match goes to stdout, on one line.
    let out = run(&store, &["-f", "jsonl", "export", "--name", "load"]);
    assert_eq!(out.lines().count(), 1, "{out}");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(parsed["name"], "load");
    assert_eq!(parsed["values"], serde_json::json!([1.0, 2.0, 3.0]));

    // And a directory export names the files for the format it wrote.
    let out_dir = dir.path().join("jsonl-out");
    run(
        &store,
        &["-f", "jsonl", "export", "--dir", out_dir.to_str().unwrap()],
    );
    let mut written: Vec<String> = fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    written.sort();
    assert_eq!(written.len(), 2, "{written:?}");
    for name in &written {
        assert!(name.ends_with(".jsonl"), "{written:?}");
        let body = fs::read_to_string(out_dir.join(name)).unwrap();
        assert_eq!(body.lines().count(), 1, "{name}: {body}");
        serde_json::from_str::<serde_json::Value>(body.trim()).expect("each file is one JSON line");
    }

    // `-f json` keeps its pretty documents and its `.json` names.
    let json_dir = dir.path().join("json-out");
    run(
        &store,
        &["-f", "json", "export", "--dir", json_dir.to_str().unwrap()],
    );
    let first = fs::read_dir(&json_dir).unwrap().next().unwrap().unwrap();
    assert!(
        first.file_name().to_string_lossy().ends_with(".json"),
        "{:?}",
        first.file_name()
    );
    assert!(
        fs::read_to_string(first.path()).unwrap().lines().count() > 1,
        "-f json is still pretty-printed"
    );
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

/// `--stride` selects data, so it has to reach the formats a program reads.
///
/// It used to be applied only on the way to the table: `-f json` emitted the
/// whole stored array and the whole timestamp vector regardless, which is worse
/// than ignoring the flag — the document said `shape: [24]` while the caller had
/// asked for four rows, so a consumer could not even tell it had been ignored.
#[test]
fn stride_reaches_json_and_jsonl_not_only_the_table() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("stride.h5");
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

    let strided: serde_json::Value = serde_json::from_str(&run(
        &store,
        &["-f", "json", "get", "--name", "load", "--stride", "6"],
    ))
    .unwrap();
    assert_eq!(
        strided["values"].as_array().unwrap(),
        &vec![
            serde_json::json!(0.0),
            serde_json::json!(6.0),
            serde_json::json!(12.0),
            serde_json::json!(18.0)
        ],
        "every sixth value, not all 24: {strided}"
    );
    assert_eq!(strided["timestamps"].as_array().unwrap().len(), 4);
    assert_eq!(
        strided["timestamps"][1], "2024-01-01T06:00:00+00:00",
        "the timestamps must follow the values they label: {strided}"
    );
    // The metadata has to describe the document, not the array it came from.
    assert_eq!(strided["shape"], serde_json::json!([4]));
    assert_eq!(strided["stride"], 6);

    // jsonl is the same document on one line.
    let line = run(
        &store,
        &["-f", "jsonl", "get", "--name", "load", "--stride", "6"],
    );
    assert_eq!(line.lines().count(), 1, "{line}");
    let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(parsed, strided);

    // An unstrided read is untouched: full length, and no `stride` key claiming
    // a thinning that did not happen.
    let full: serde_json::Value =
        serde_json::from_str(&run(&store, &["-f", "json", "get", "--name", "load"])).unwrap();
    assert_eq!(full["values"].as_array().unwrap().len(), 24);
    assert_eq!(full["shape"], serde_json::json!([24]));
    assert!(full.get("stride").is_none(), "{full}");

    // And the three formats agree on which rows survived.
    let csv = run(
        &store,
        &["-f", "csv", "get", "--name", "load", "--stride", "6"],
    );
    let kept: Vec<String> = data_lines(&csv)
        .iter()
        .map(|l| l.split(',').next_back().unwrap().to_string())
        .collect();
    assert_eq!(kept, ["0", "6", "12", "18"], "{csv}");
}

/// A stride keeps whole timesteps: the row it drops is every value of that
/// timestep, not every sixth number in the flattened array.
#[test]
fn stride_slices_a_multidimensional_series_by_whole_timesteps() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("md.h5");
    let mut body = String::from("a,b\n");
    for step in 0..6 {
        body.push_str(&format!("{},{}\n", step * 10, step * 10 + 1));
    }
    write(dir.path(), "md.csv", &body);
    let d = write(
        dir.path(),
        "md.json",
        r#"{"owner_id": 1, "owner_type": "G", "name": "curve",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "md.csv",
            "element_shape": [2],
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    run(&store, &["add", "--descriptor", d.to_str().unwrap()]);

    let out: serde_json::Value = serde_json::from_str(&run(
        &store,
        &["-f", "json", "get", "--name", "curve", "--stride", "2"],
    ))
    .unwrap();
    assert_eq!(
        out["values"],
        serde_json::json!([0.0, 1.0, 20.0, 21.0, 40.0, 41.0]),
        "both columns of steps 0, 2 and 4: {out}"
    );
    // Only the time axis shrinks; the per-step width is unchanged.
    assert_eq!(out["shape"], serde_json::json!([3, 2]));
    assert_eq!(out["element_shape"], serde_json::json!([2]));

    let csv = run(
        &store,
        &["-f", "csv", "get", "--name", "curve", "--stride", "2"],
    );
    assert_eq!(data_lines(&csv).len(), 3, "{csv}");
    assert!(csv.contains("value[0],value[1]"), "{csv}");
}

/// A strided forecast is not the stored array either, so its JSON switches to
/// the readable `columns`/`rows` view the `--window` slice already used rather
/// than emitting a flat array the rows contradict.
#[test]
fn stride_reshapes_the_forecast_json_into_the_rows_it_kept() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("f.h5");
    seed_forecast(dir.path(), &store);

    let out: serde_json::Value = serde_json::from_str(&run(
        &store,
        &["-f", "json", "get", "--name", "load_prob", "--stride", "2"],
    ))
    .unwrap();
    assert_eq!(out["stride"], 2);
    // 3 windows x 2 horizon steps, every second row.
    assert_eq!(out["rows"].as_array().unwrap().len(), 3, "{out}");
    assert!(
        out.get("values").is_none(),
        "a flat array would describe rows this document does not carry: {out}"
    );
    assert_eq!(
        out["columns"],
        serde_json::json!(["issue_time", "target_time", "value[p10]", "value[p90]"])
    );

    // Unstrided, the stored array is still what a caller gets.
    let plain: serde_json::Value =
        serde_json::from_str(&run(&store, &["-f", "json", "get", "--name", "load_prob"])).unwrap();
    assert_eq!(plain["values"].as_array().unwrap().len(), 12);
    assert!(plain.get("rows").is_none(), "{plain}");
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

/// Two series whose feature maps *render* alike are still two series.
///
/// `diff` used to pair rows on the rendered identity, where features are `k=v`
/// pairs joined by `,`. That rendering is not injective -- `{"a": "1,b=2"}` and
/// `{"a": "1", "b": "2"}` both come out as `a=1,b=2` -- so the two collapsed
/// into one map entry, one of them dropped out of the comparison, and a store
/// that genuinely differed was reported as `0 changed` with exit 0. Anything
/// gating CI on that status passed silently.
#[test]
fn diff_pairs_series_by_identity_not_by_how_the_identity_renders() {
    let dir = tempfile::tempdir().unwrap();
    let left = dir.path().join("collide_left.h5");
    let right = dir.path().join("collide_right.h5");

    // Two series identical but for features that render to the same text.
    let descriptor = |values: &str| {
        format!(
            r#"[{{"owner_id": 42, "owner_type": "G", "name": "load",
                  "type": "SingleTimeSeries", "element_type": "f64", "csv": "{values}",
                  "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H",
                  "features": {{"a": "1,b=2"}}}},
                 {{"owner_id": 42, "owner_type": "G", "name": "load",
                  "type": "SingleTimeSeries", "element_type": "f64", "csv": "same.csv",
                  "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H",
                  "features": {{"a": "1", "b": "2"}}}}]"#
        )
    };
    write(dir.path(), "same.csv", "value\n1\n2\n");
    write(dir.path(), "differs.csv", "value\n7\n7\n");
    let l = write(dir.path(), "cl.json", &descriptor("same.csv"));
    let r = write(dir.path(), "cr.json", &descriptor("differs.csv"));
    run(&left, &["add", "--descriptor", l.to_str().unwrap()]);
    run(&right, &["add", "--descriptor", r.to_str().unwrap()]);

    // Both stores really do hold two series each.
    assert_eq!(data_lines(&run(&left, &["-f", "csv", "list"])).len(), 2);
    assert_eq!(data_lines(&run(&right, &["-f", "csv", "list"])).len(), 2);

    // One of the two differs, so the diff reports it and exits nonzero.
    let output = raw(&left, &["diff", "--against", right.to_str().unwrap()]);
    assert!(
        !output.status.success(),
        "a store that differs must exit nonzero: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // `--all` so the identical row shows up too. Parsed from `raw`, since a
    // differing diff deliberately exits nonzero.
    let json = raw(
        &left,
        &[
            "-f",
            "json",
            "diff",
            "--against",
            right.to_str().unwrap(),
            "--all",
        ],
    );
    let report: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&json.stdout)).unwrap();
    assert_eq!(report["changed"], 1, "{report}");
    assert_eq!(report["same"], 1, "{report}");
    assert_eq!(report["added"], 0, "{report}");
    assert_eq!(report["removed"], 0, "{report}");
    // Neither series was swallowed by the other.
    assert_eq!(report["items"].as_array().unwrap().len(), 2, "{report}");
}

/// `diff` reports the differences by default and exits nonzero on them; `--all`
/// adds the identical rows, and two stores that agree still exit zero.
#[test]
fn diff_all_lists_the_identical_series_and_agreement_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let left = dir.path().join("left.h5");
    let right = dir.path().join("right.h5");
    seed_one(dir.path(), &left);
    seed_one(dir.path(), &right);

    let output = raw(&left, &["diff", "--against", right.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "identical stores must exit zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let quiet: serde_json::Value = serde_json::from_str(&run(
        &left,
        &["-f", "json", "diff", "--against", right.to_str().unwrap()],
    ))
    .unwrap();
    assert_eq!(quiet["same"], 1);
    assert_eq!(
        quiet["items"].as_array().unwrap().len(),
        0,
        "the identical series is counted, not listed: {quiet}"
    );

    let all: serde_json::Value = serde_json::from_str(&run(
        &left,
        &[
            "-f",
            "json",
            "diff",
            "--against",
            right.to_str().unwrap(),
            "--all",
        ],
    ))
    .unwrap();
    let items = all["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{all}");
    assert_eq!(items[0]["status"], "same");
    assert_eq!(
        items[0]["left_data_hash"], items[0]["right_data_hash"],
        "same values, same content hash: {all}"
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

/// Every association write carries `--dry-run`, and each has to leave the
/// catalog exactly as it found it — including the removals, where getting this
/// wrong is unrecoverable.
#[test]
fn the_association_writes_all_have_a_dry_run_that_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("dry.h5");
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

    let attrs = || data_lines(&run(&store, &["-f", "csv", "attributes"])).len();
    let links = || data_lines(&run(&store, &["-f", "csv", "links"])).len();

    let added = run(
        &store,
        &[
            "attach",
            "--component-id",
            "50",
            "--component-type",
            "Bus",
            "--attribute-id",
            "8",
            "--attribute-type",
            "GeographicInfo",
            "--dry-run",
        ],
    );
    assert!(added.contains("50"), "{added}");
    assert_eq!(attrs(), 1, "--dry-run must not attach");

    run(
        &store,
        &[
            "link",
            "--parent-id",
            "50",
            "--parent-type",
            "Bus",
            "--child-id",
            "2",
            "--child-type",
            "Load",
            "--dry-run",
        ],
    );
    assert_eq!(links(), 1, "--dry-run must not link");

    // The removals report a count, and `--dry-run` stands in for the
    // confirmation `--force` would otherwise have to answer.
    let detached = run(&store, &["detach", "--all", "--dry-run"]);
    assert!(detached.contains('1'), "{detached}");
    assert_eq!(attrs(), 1, "--dry-run must not detach");
    run(&store, &["unlink", "--all", "--dry-run"]);
    assert_eq!(links(), 1, "--dry-run must not unlink");

    run(
        &store,
        &["reassign", "--old", "42", "--new", "142", "--dry-run"],
    );
    assert!(
        run(&store, &["-f", "csv", "attributes"]).contains("42"),
        "--dry-run must not reassign"
    );
}

/// `reassign` moves both catalogs by default; `--attributes` and `--links`
/// narrow it to one. A consumer renumbering only its attribute graph would
/// otherwise have to move the links too and put them back.
#[test]
fn reassign_can_move_one_association_catalog_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("r.h5");
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

    run(
        &store,
        &["reassign", "--old", "42", "--new", "142", "--attributes"],
    );
    assert!(
        run(&store, &["-f", "csv", "attributes"]).contains("142"),
        "the attachment moves"
    );
    let links = run(&store, &["-f", "csv", "links"]);
    assert!(
        links.contains("42") && !links.contains("142"),
        "the link must be left alone: {links}"
    );

    run(
        &store,
        &["reassign", "--old", "42", "--new", "142", "--links"],
    );
    assert!(run(&store, &["-f", "csv", "links"]).contains("142"));
}

/// `attributes --summary` groups the catalog by (component type, attribute
/// type) instead of listing rows, which is the only view that stays readable on
/// a real system's attachment count.
#[test]
fn attributes_summary_counts_by_type_pair() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("s.h5");
    run(&store, &["init"]);
    let batch = write(
        dir.path(),
        "batch.csv",
        "component_id,component_type,attribute_id,attribute_type\n\
         1,Generator,10,GeographicInfo\n\
         2,Generator,11,GeographicInfo\n\
         3,Bus,12,GeographicInfo\n",
    );
    run(&store, &["attach", "--from", batch.to_str().unwrap()]);

    let summary: serde_json::Value =
        serde_json::from_str(&run(&store, &["-f", "json", "attributes", "--summary"])).unwrap();
    let items = summary["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "one row per type pair: {summary}");
    let generators = items
        .iter()
        .find(|r| r["component_type"] == "Generator")
        .unwrap_or_else(|| panic!("no Generator row: {summary}"));
    assert_eq!(generators["count"], 2);
    assert_eq!(generators["attribute_type"], "GeographicInfo");
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

// --- A9: stdout under -f json stays machine-readable -----------------------

/// Run a command under `-f json` and parse its stdout, asserting nothing but
/// JSON reached the pipe.
fn json_stdout(store: &Path, args: &[&str]) -> serde_json::Value {
    let mut full = vec!["-f", "json"];
    full.extend_from_slice(args);
    let out = run(store, &full);
    serde_json::from_str(out.trim()).unwrap_or_else(|e| {
        panic!("`infrastore -f json {args:?}` put non-JSON on stdout ({e}):\n{out}")
    })
}

/// Every command that changes the store reports through `-f json` too.
///
/// The read commands always did; the write commands used to print prose to
/// stdout no matter what `--format` said, which meant `infrastore -f json
/// remove ... | jq` died on "Removed 1 time series." A scripted mutation has to
/// be as pipeable as a scripted query, so this walks the whole write surface.
#[test]
fn every_mutating_command_emits_json_on_stdout_under_f_json() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("m.h5");

    // init / add
    let fresh = dir.path().join("fresh.h5");
    assert_eq!(json_stdout(&fresh, &["init"])["catalog"], "attached");

    write(dir.path(), "v.csv", "value\n1\n2\n3\n");
    let desc = write(
        dir.path(),
        "one.json",
        r#"{"owner_id": 42, "owner_type": "Generator", "name": "load",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    let d = desc.to_str().unwrap();
    assert_eq!(json_stdout(&store, &["add", "--descriptor", d])["added"], 1);
    // `add --dry-run` reports the *plan*, so it keeps the `{"items": [...]}`
    // shape every listing uses rather than the `{"dry_run": true}` status the
    // other write commands report — its output is rows, not a status line.
    let plan = json_stdout(&store, &["add", "--descriptor", d, "--dry-run"]);
    assert_eq!(plan["items"].as_array().unwrap().len(), 1, "{plan}");

    // rename / copy / replace-owner, each with its dry run
    let sel = ["--owner-id", "42", "--name", "load"];
    let mut args = vec!["rename", "--new-name", "load2"];
    args.extend_from_slice(&sel);
    args.push("--dry-run");
    assert_eq!(json_stdout(&store, &args)["would_rename"], 1);
    args.pop();
    assert_eq!(json_stdout(&store, &args)["renamed"], 1);

    let mut args = vec![
        "copy",
        "--dst-owner-id",
        "43",
        "--dst-owner-type",
        "Generator",
    ];
    args.extend_from_slice(&["--owner-id", "42", "--name", "load2"]);
    assert_eq!(json_stdout(&store, &args)["copied"], 1);

    assert_eq!(
        json_stdout(
            &store,
            &[
                "replace-owner",
                "--old",
                "43",
                "--new",
                "44",
                "--owner-category",
                "Component"
            ],
        )["reassigned"],
        1
    );

    // transform / compact / persist / export / plot
    assert_eq!(
        json_stdout(
            &store,
            &["transform", "--horizon", "PT2H", "--interval", "PT1H"],
        )["transformed"],
        2
    );
    assert!(json_stdout(&store, &["compact", "--force"]).is_object());

    let backup = dir.path().join("backup.h5");
    assert_eq!(
        json_stdout(&store, &["persist", "--dest", backup.to_str().unwrap()])["persisted"],
        true
    );

    let outdir = dir.path().join("exported");
    let exported = json_stdout(
        &store,
        &[
            "export",
            "--dir",
            outdir.to_str().unwrap(),
            "--owner-id",
            "42",
        ],
    );
    // Owner 42 holds `load2` plus the forecast `transform` derived from it.
    assert_eq!(exported["exported"], 2);
    assert_eq!(exported["files"].as_array().unwrap().len(), 2);

    let svg = dir.path().join("c.svg");
    let mut args = vec!["plot", "--out", svg.to_str().unwrap()];
    args.extend_from_slice(&["--owner-id", "42", "--name", "load2"]);
    assert!(json_stdout(&store, &args)["wrote"].is_string());

    // The association catalogs.
    let attach = [
        "attach",
        "--component-id",
        "1",
        "--component-type",
        "Bus",
        "--attribute-id",
        "9",
        "--attribute-type",
        "GeoLocation",
    ];
    assert_eq!(json_stdout(&store, &attach)["attached"], 1);
    let link = [
        "link",
        "--parent-id",
        "1",
        "--parent-type",
        "Bus",
        "--child-id",
        "2",
        "--child-type",
        "Generator",
    ];
    assert_eq!(json_stdout(&store, &link)["linked"], 1);
    // Scoped to one catalog, the other key is absent rather than a misleading 0.
    let moved = json_stdout(&store, &["reassign", "--old", "1", "--new", "7", "--links"]);
    assert_eq!(moved["links"], 1);
    assert!(moved.get("attachments").is_none(), "{moved}");
    assert_eq!(
        json_stdout(&store, &["unlink", "--parent-id", "7", "--force"])["unlinked"],
        1
    );
    assert_eq!(
        json_stdout(&store, &["detach", "--component-id", "1", "--force"])["detached"],
        1
    );
    // A filter that matches nothing still reports a count, not an empty pipe.
    assert_eq!(
        json_stdout(&store, &["detach", "--component-id", "999", "--force"])["detached"],
        0
    );

    // merge / remove / clear
    let other = dir.path().join("other.h5");
    run(&other, &["add", "--descriptor", d]);
    assert_eq!(
        json_stdout(
            &other,
            &[
                "merge",
                "--from",
                store.to_str().unwrap(),
                "--owner-id",
                "42"
            ]
        )["merged"],
        2
    );
    // `--type` because `transform` left a forecast sharing the name.
    assert_eq!(
        json_stdout(
            &store,
            &[
                "remove",
                "--owner-id",
                "42",
                "--name",
                "load2",
                "--type",
                "SingleTimeSeries",
                "--force"
            ]
        )["removed"],
        1
    );
    assert_eq!(
        json_stdout(&store, &["remove", "--all", "--owner-id", "999", "--force"])["removed"],
        0
    );
    assert!(json_stdout(&store, &["clear", "--force"])["cleared"].is_number());
}

/// The interactive prompt and its abort notice belong on stderr.
///
/// A prompt written to stdout lands in the middle of the document `-f json |
/// jq` is reading. These tests never see a terminal, so `ask` auto-confirms;
/// what is asserted here is the strict form, which refuses instead — and must
/// refuse without putting its explanation on stdout.
#[test]
fn a_refused_confirmation_says_so_on_stderr_leaving_stdout_clean() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("q.h5");
    let dest = dir.path().join("dest.h5");
    seed_one(dir.path(), &store);
    run(&store, &["persist", "--dest", dest.to_str().unwrap()]);

    let out = raw(
        &store,
        &["-f", "json", "persist", "--dest", dest.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(
        out.stdout.is_empty(),
        "a refusal must leave stdout empty for the reader: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--force"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A failure reports in the same format the command was asked for.
///
/// Errors always went to stderr, so they never broke a `jq` reading stdout —
/// but a caller parsing one stream had to switch to line-scraping the moment
/// something went wrong. Under `-f json` the message is a document too.
#[test]
fn an_error_is_json_on_stderr_under_f_json_and_prose_otherwise() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.h5");

    let out = raw(&missing, &["-f", "json", "list"]);
    assert!(!out.status.success());
    assert!(
        out.stdout.is_empty(),
        "a failure leaves stdout empty: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&out.stderr).expect("stderr parses as one JSON document");
    assert_eq!(doc["status"], "error");
    assert!(
        doc["message"].as_str().unwrap().contains("nope.h5"),
        "{doc}"
    );

    // `jsonl` gets the same document compact, on one line.
    let jsonl = raw(&missing, &["-f", "jsonl", "list"]);
    let line = String::from_utf8(jsonl.stderr).unwrap();
    assert_eq!(line.trim().lines().count(), 1, "{line}");
    let doc: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(doc["status"], "error");

    // The non-JSON formats keep the `Error:` prefix the exit-status table
    // documents.
    for format in ["table", "csv"] {
        let out = raw(&missing, &["-f", format, "list"]);
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(err.starts_with("Error: "), "-f {format}: {err}");
    }
}

/// A reader that stops early is not an error.
///
/// The commands whose stdout *is* the artifact — `export` with no `--dir`,
/// `plot --out -`, `template` — used bare `print!`, which panics on `EPIPE`
/// because Rust ignores `SIGPIPE`. `infrastore export | head` died with "failed
/// printing to stdout" and exit 101 as soon as the document outgrew the pipe
/// buffer, so the series has to be big enough to reach it.
#[test]
fn a_closed_pipe_ends_a_stdout_stream_cleanly_instead_of_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("pipe.h5");

    let mut csv = String::from("value\n");
    for i in 0..50_000 {
        csv.push_str(&format!("{}\n", i as f64 * 1.5));
    }
    write(dir.path(), "big.csv", &csv);
    let desc = write(
        dir.path(),
        "big.json",
        r#"{"owner_id": 1, "owner_type": "Generator", "name": "big",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "big.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    run(&store, &["add", "--descriptor", desc.to_str().unwrap()]);

    for args in [
        vec!["-f", "csv", "export", "--owner-id", "1"],
        vec![
            "plot",
            "--out",
            "-",
            "--owner-id",
            "1",
            "--name",
            "big",
            "--limit",
            "50000",
        ],
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_infrastore"))
            .arg("--store")
            .arg(&store)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn infrastore");
        // Close the read end while the writer is still going.
        drop(child.stdout.take());
        let out = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            !err.contains("panicked"),
            "{args:?} panicked on a closed pipe: {err}"
        );
    }

    // The same stream, read to the end, is still whole — the pipe handling must
    // not have truncated anything.
    let whole = run(&store, &["-f", "csv", "export", "--owner-id", "1"]);
    assert_eq!(data_lines(&whole).len(), 50_000, "every row still written");

    // `template` writes to stdout too, and has no store to read. Its descriptor
    // fits in the pipe buffer, so this only checks it still writes cleanly
    // through the shared helper.
    let out = Command::new(env!("CARGO_BIN_EXE_infrastore"))
        .args(["template", "SingleTimeSeries"])
        .output()
        .unwrap();
    assert!(out.status.success(), "template: {out:?}");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("a JSON descriptor");
    assert_eq!(doc["type"], "SingleTimeSeries");
}

#[test]
fn add_replace_works_on_a_fresh_store_and_alongside_new_series() {
    // `--replace` means "replace it if it is there". Routing the whole load's
    // identities through the all-or-nothing bulk remove made it fail with
    // `NotFound` whenever *any* of them was absent — which is every first load
    // into a new store, and every descriptor that adds a series next to the ones
    // it replaces.
    let dir = tempfile::tempdir().unwrap();
    let two = r#"[{"owner_id": 42, "owner_type": "Generator", "name": "load",
                   "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
                   "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"},
                  {"owner_id": 42, "owner_type": "Generator", "name": "newone",
                   "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
                   "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}]"#;

    // Nothing exists yet: --replace still loads.
    let fresh = dir.path().join("fresh.h5");
    write(dir.path(), "v.csv", "value\n1\n2\n3\n");
    let d = write(dir.path(), "two.json", two);
    run(
        &fresh,
        &["add", "--descriptor", d.to_str().unwrap(), "--replace"],
    );
    assert_eq!(data_lines(&run(&fresh, &["-f", "csv", "list"])).len(), 2);

    // One of the two already exists: the present one is replaced, the absent one
    // is added, and neither fails the batch.
    let mixed = dir.path().join("mixed.h5");
    seed_one(dir.path(), &mixed);
    write(dir.path(), "v.csv", "value\n9\n8\n7\n");
    run(
        &mixed,
        &["add", "--descriptor", d.to_str().unwrap(), "--replace"],
    );
    let listed = run(&mixed, &["-f", "csv", "list"]);
    assert_eq!(data_lines(&listed).len(), 2, "{listed}");
    let values = run(&mixed, &["-f", "csv", "get", "--name", "load"]);
    assert!(values.contains(",9"), "the new values must win: {values}");

    // Still idempotent: a second identical run replaces rather than duplicating.
    run(
        &mixed,
        &["add", "--descriptor", d.to_str().unwrap(), "--replace"],
    );
    assert_eq!(data_lines(&run(&mixed, &["-f", "csv", "list"])).len(), 2);
}

#[test]
fn a_failed_in_memory_load_still_leaves_an_openable_store() {
    // Creating the store stamps the HDF5 half at once, while an in-memory
    // catalog writes no `.sqlite` until `persist_catalog`. Returning early on a
    // mid-load error therefore used to leave a stamped array file with no
    // catalog — the `MismatchedArtifact` state, which is terminal: the corrected
    // re-run cannot open it either, and the only recovery is to delete the file.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("halfway.h5");
    write(dir.path(), "v.csv", "value\n1\n2\n3\n");

    // A descriptor that declares the same series twice: the load opens the
    // store, then fails on the duplicate.
    let dup = r#"[{"owner_id": 42, "owner_type": "Generator", "name": "load",
                   "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
                   "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"},
                  {"owner_id": 42, "owner_type": "Generator", "name": "load",
                   "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
                   "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}]"#;
    let d = write(dir.path(), "dup.json", dup);
    let err = run_err(
        &store,
        &[
            "add",
            "--descriptor",
            d.to_str().unwrap(),
            "--catalog",
            "in-memory",
        ],
    );
    assert!(!err.is_empty(), "the duplicate must still fail the load");

    // Both halves are on disk, and the store opens.
    assert!(store.exists(), "the HDF5 half should be there");
    assert!(
        crate::catalog_beside(&store).exists(),
        "the catalog half must be written even on the failure path"
    );
    run(&store, &["-f", "csv", "list"]);

    // And a corrected load at the same path succeeds.
    let one = r#"{"owner_id": 42, "owner_type": "Generator", "name": "load",
                  "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
                  "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#;
    let good = write(dir.path(), "one_only.json", one);
    run(&store, &["add", "--descriptor", good.to_str().unwrap()]);
    assert_eq!(data_lines(&run(&store, &["-f", "csv", "list"])).len(), 1);
}

fn catalog_beside(store: &Path) -> std::path::PathBuf {
    let mut name = store.as_os_str().to_owned();
    name.push(".sqlite");
    std::path::PathBuf::from(name)
}

/// A timestamped CSV's timestamp column is a claim about the data, not decoration.
///
/// The regular types take their timeline from `initial_timestamp` + `resolution`
/// and store no timestamps of their own, so the column an exported file carries
/// was stripped and dropped — parsed for the irregular types, discarded for
/// these. That made the round trip `export` advertises silently lossy: a slice
/// fed back under a descriptor naming a different grid had every value
/// relocated onto it, and a column of outright garbage was accepted without a
/// word.
#[test]
fn a_timestamp_column_must_agree_with_the_grid_the_descriptor_declares() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("grid.h5");
    write(dir.path(), "v.csv", "value\n10\n11\n12\n13\n14\n15\n");
    let hourly = |name: &str, csv: &str, initial: &str, resolution: &str, owner: i64| {
        format!(
            r#"{{"owner_id": {owner}, "owner_type": "Generator", "name": "{name}",
                 "type": "SingleTimeSeries", "element_type": "f64", "csv": "{csv}",
                 "initial_timestamp": "{initial}", "resolution": "{resolution}"}}"#
        )
    };
    let d = write(
        dir.path(),
        "seed.json",
        &hourly("load", "v.csv", "2024-01-01T00:00:00Z", "PT1H", 42),
    );
    run(&store, &["add", "--descriptor", d.to_str().unwrap()]);

    // Export a slice: it starts at 02:00, not at the series' own start.
    let slice = run(
        &store,
        &[
            "-f",
            "csv",
            "export",
            "--owner-id",
            "42",
            "--name",
            "load",
            "--time-range",
            "2024-01-01T02:00:00Z..2024-01-01T05:00:00Z",
        ],
    );
    assert!(slice.contains("2024-01-01T02:00:00"), "{slice}");
    write(dir.path(), "slice.csv", &slice);

    // Re-adding it under a descriptor naming a different grid is refused, and
    // the message says how to resolve it either way.
    let wrong = write(
        dir.path(),
        "wrong.json",
        &hourly("relocated", "slice.csv", "2030-06-15T00:00:00Z", "PT15M", 7),
    );
    let dest = dir.path().join("dest.h5");
    let err = run_err(&dest, &["add", "--descriptor", wrong.to_str().unwrap()]);
    assert!(err.contains("2030-06-15"), "{err}");
    assert!(err.contains("drop the timestamp column"), "{err}");
    assert!(!dest.exists() || data_lines(&run(&dest, &["-f", "csv", "list"])).is_empty());

    // The same file under the grid it was written from loads, values intact —
    // the round trip `export` advertises.
    let right = write(
        dir.path(),
        "right.json",
        &hourly("ok", "slice.csv", "2024-01-01T02:00:00Z", "PT1H", 9),
    );
    let round = dir.path().join("round.h5");
    run(&round, &["add", "--descriptor", right.to_str().unwrap()]);
    let got = run(&round, &["-f", "csv", "get", "--owner-id", "9"]);
    assert!(got.contains("2024-01-01T02:00:00"), "{got}");
    assert!(got.contains(",12"), "{got}");

    // Garbage in the column is reported rather than ignored.
    write(
        dir.path(),
        "garbage.csv",
        "timestamp,value\nnot-a-timestamp,1.0\nBANANA,2.0\n",
    );
    let bad = write(
        dir.path(),
        "garbage.json",
        &hourly("garbage", "garbage.csv", "2024-01-01T00:00:00Z", "PT1H", 8),
    );
    let err = run_err(
        &dir.path().join("garbage.h5"),
        &["add", "--descriptor", bad.to_str().unwrap()],
    );
    assert!(err.contains("invalid timestamp"), "{err}");

    // A value-only CSV is unaffected: no column, nothing to disagree with.
    let plain = dir.path().join("plain.h5");
    let d2 = write(
        dir.path(),
        "plain.json",
        &hourly("load", "v.csv", "2024-01-01T00:00:00Z", "PT1H", 42),
    );
    run(&plain, &["add", "--descriptor", d2.to_str().unwrap()]);
    assert_eq!(data_lines(&run(&plain, &["-f", "csv", "list"])).len(), 1);
}

/// `summary -f csv` is a CSV, not a report with tables in it.
///
/// Static and forecast series are two shapes, and the human view shows them as
/// two tables under two headings. The CSV path printed those headings into the
/// stream and then emitted both tables, so the output carried rows of 1, 6, 6,
/// 1, 8 and 8 fields — a strict reader dies on row two. Machine output is now
/// one uniform table with a `Kind` column and `-` where a column does not apply.
#[test]
fn summary_csv_is_one_uniform_table() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("summary.h5");
    seed_one(dir.path(), &store);
    // A forecast too, so both shapes are present.
    run(
        &store,
        &["transform", "--horizon", "PT2H", "--interval", "PT1H"],
    );

    let out = run(&store, &["-f", "csv", "summary"]);
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().expect("a header row");
    assert!(header.starts_with("Kind,"), "{out}");
    let width = header.split(',').count();

    let rows: Vec<&str> = lines.collect();
    assert!(rows.len() >= 2, "both kinds should appear: {out}");
    for row in &rows {
        assert_eq!(
            row.split(',').count(),
            width,
            "every row must have the header's width: {out}"
        );
    }
    // No prose headings leaked into the stream.
    assert!(!out.contains("Static series"), "{out}");
    assert!(!out.contains("Forecast series"), "{out}");
    // Both kinds are labelled, and each carries the columns that apply to it.
    assert!(rows.iter().any(|r| r.starts_with("static,")), "{out}");
    assert!(rows.iter().any(|r| r.starts_with("forecast,")), "{out}");

    // The human view still shows its two headed tables.
    let human = run(&store, &["summary"]);
    assert!(human.contains("Static series"), "{human}");
    assert!(human.contains("Forecast series"), "{human}");
}

/// A canvas an SVG cannot express is refused, not written.
///
/// `--width`/`--height` are bare floats that went straight into the root
/// element, so `--width=-100` wrote `width="-100"` (an error per the SVG spec)
/// and `--width=nan` wrote `width="NaN"` (not a `<length>` at all, and it leaked
/// into the body geometry as `x="NaN"`). Both reported success and exit 0.
#[test]
fn plot_refuses_a_canvas_an_svg_cannot_express() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("canvas.h5");
    seed_one(dir.path(), &store);
    let out = dir.path().join("chart.svg");

    for (flag, value) in [
        ("--width", "-100"),
        ("--width", "nan"),
        ("--width", "0"),
        ("--width", "inf"),
        ("--width", "10"),
        ("--height", "-5"),
        ("--height", "nan"),
    ] {
        let err = run_err(
            &store,
            &[
                "plot",
                "--kind",
                "line",
                "--name",
                "load",
                &format!("{flag}={value}"),
                "--out",
                out.to_str().unwrap(),
            ],
        );
        assert!(
            err.contains(flag.trim_start_matches("--")),
            "{flag}={value}: {err}"
        );
        assert!(!out.exists(), "{flag}={value} must not write a chart");
    }

    // An ordinary canvas is unaffected, and lands in the document.
    run(
        &store,
        &[
            "plot",
            "--kind",
            "line",
            "--name",
            "load",
            "--width=800",
            "--height=400",
            "--out",
            out.to_str().unwrap(),
        ],
    );
    let svg = fs::read_to_string(&out).unwrap();
    assert!(svg.contains(r#"viewBox="0 0 800 400""#), "{svg}");
    assert!(!svg.contains("NaN"), "{svg}");
}
