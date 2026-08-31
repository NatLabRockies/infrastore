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

/// Serializes subprocess spawns against the tests that open a store *in this
/// process*.
///
/// HDF5 opens its files without `O_CLOEXEC`, so a child forked while a store is
/// open here inherits the descriptor — and with it the advisory lock, which
/// lives on the open file description and survives `exec`. The unrelated child
/// then holds that store locked until it exits, and an `infrastore` invocation
/// in between cannot open it at all. `cli_errors.rs` carries the same gate and
/// the longer account of the CI failure that produced it.
///
/// Every spawn takes the read guard; a test holding a store takes the write
/// guard for exactly as long as its handle lives.
static SPAWN_GATE: std::sync::RwLock<()> = std::sync::RwLock::new(());

fn raw(store: &Path, args: &[&str]) -> std::process::Output {
    // Recovering a poisoned lock rather than propagating it: one test panicking
    // under the guard should not bury every other test in poison panics.
    let _gate = SPAWN_GATE.read().unwrap_or_else(|e| e.into_inner());
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
    // A write reports the ids it created — the durable handles for the rows it
    // just wrote — not only how many there were.
    let attached = json_stdout(&store, &["attach", "--from", batch.to_str().unwrap()]);
    assert_eq!(attached["attached"], 2, "{attached}");
    assert_eq!(
        attached["ids"].as_array().unwrap().len(),
        2,
        "attach must report one id per row: {attached}"
    );
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
    let added = json_stdout(&store, &["add", "--descriptor", d]);
    assert_eq!(added["added"], 1, "{added}");
    // `--id` on `get`/`info` resolves what `add` reports, so `add` has to
    // report it: the docs promise the id comes from here.
    let series = added["series"].as_array().unwrap();
    assert_eq!(series.len(), 1, "{added}");
    let id = series[0]["id"]
        .as_i64()
        .expect("add reports the catalog id");
    assert_eq!(series[0]["name"], "load", "{added}");
    assert_eq!(
        json_stdout(&store, &["info", "--id", &id.to_string()])["name"],
        "load",
    );
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

// ---------------------------------------------------------------------------
// plot: the forecast kinds, and the shapes a heatmap refuses
//
// `plot_writes_one_self_contained_document_for_every_kind` covers the three
// static kinds and the *error* a `fan` gives without a forecast. The drawing
// paths behind `fan` and `overlay` -- the percentile bands, the scenario
// traces, and the actuals a Deterministic is drawn against -- were reached by
// nothing, which is most of this module.
// ---------------------------------------------------------------------------

/// A Probabilistic with an *odd* number of percentiles: the outer pair nests
/// into a band and the median is left over as its own emphasized line, which
/// is the branch a symmetric pair alone never reaches. Two legend entries also
/// earn a legend, where a lone band renders none.
fn seed_probabilistic(dir: &Path, store: &Path, name: &str) {
    let mut body = String::from("value\n");
    for v in 0..12 {
        body.push_str(&format!("{v}\n"));
    }
    write(dir, "fan.csv", &body);
    let json = format!(
        r#"{{"owner_id": 7, "owner_type": "Generator", "name": "{name}", "units": "MW",
             "type": "Probabilistic", "element_type": "f64", "csv": "fan.csv",
             "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H",
             "horizon": "PT2H", "interval": "PT1H", "count": 2,
             "percentiles": [10.0, 50.0, 90.0]}}"#
    );
    let d = write(dir, "fan.json", &json);
    run(store, &["add", "--descriptor", d.to_str().unwrap()]);
}

/// Every plot document is self-contained: no script, no external reference,
/// and both themes. Asserted for each kind rather than once, because each is
/// assembled by a different function.
fn assert_self_contained(kind: &str, svg: &str) {
    assert!(svg.starts_with("<svg "), "{kind}: {svg}");
    assert!(
        svg.contains("prefers-color-scheme"),
        "{kind} needs both themes"
    );
    assert!(!svg.contains("<script"), "{kind} must carry no script");
    assert!(!svg.contains("xlink:href"), "{kind} must reference nothing");
}

#[test]
fn a_fan_draws_a_probabilistics_percentile_bands() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("fan.h5");
    seed_probabilistic(dir.path(), &store, "load_prob");

    let svg = run(
        &store,
        &["plot", "--name", "load_prob", "--kind", "fan", "--out", "-"],
    );
    assert_self_contained("fan", &svg);
    // The outer pair nests into one band, labelled by the bounds it spans --
    // a band is a filled area, not two separate strokes.
    assert!(
        svg.contains(r#"class="band"#),
        "expected a filled band: {svg}"
    );
    assert!(
        svg.contains("p10\u{2013}p90"),
        "the band is labelled by its bounds: {svg}"
    );
    // The median has no partner to pair with, so it stays a line of its own.
    assert!(svg.contains("p50"), "the median is drawn and named: {svg}");
    // The series carries units, and the chart says so rather than leaving the
    // axis bare.
    assert!(svg.contains("MW"), "the unit belongs on the axis: {svg}");
    // The subtitle names which window was drawn: a forecast holds several and
    // the chart shows one.
    assert!(svg.contains("window 0"), "{svg}");
}

#[test]
fn a_fan_draws_one_trace_per_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("scen.h5");
    // 3 scenarios x 2 horizon steps x 2 windows.
    let mut body = String::from("value\n");
    for v in 0..12 {
        body.push_str(&format!("{v}\n"));
    }
    write(dir.path(), "s.csv", &body);
    let d = write(
        dir.path(),
        "s.json",
        r#"{"owner_id": 7, "owner_type": "Generator", "name": "load_scen",
            "type": "Scenarios", "element_type": "f64", "csv": "s.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H",
            "horizon": "PT2H", "interval": "PT1H", "count": 2,
            "scenario_count": 3}"#,
    );
    run(&store, &["add", "--descriptor", d.to_str().unwrap()]);

    let svg = run(
        &store,
        &["plot", "--name", "load_scen", "--kind", "fan", "--out", "-"],
    );
    assert_self_contained("fan/scenarios", &svg);
    // Scenarios are unordered alternatives, not confidence bounds, so each is
    // its own labelled trace -- there is no pair to nest into a band.
    for label in ["s0", "s1", "s2"] {
        assert!(svg.contains(label), "{label} missing from: {svg}");
    }
}

#[test]
fn an_overlay_draws_a_deterministic_against_the_actuals_it_came_from() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("ov.h5");
    seed_one(dir.path(), &store);
    // `transform` derives the Deterministic from the stored SingleTimeSeries,
    // which is exactly the pairing an overlay is for.
    run(
        &store,
        &["transform", "--horizon", "PT2H", "--interval", "PT1H"],
    );

    let svg = run(
        &store,
        &[
            "plot",
            "--name",
            "load",
            "--kind",
            "overlay",
            "--type",
            "DeterministicSingleTimeSeries",
            "--out",
            "-",
        ],
    );
    assert_self_contained("overlay", &svg);
    // The point of the kind: the source series is drawn under the windows, and
    // named so the two are tellable apart.
    assert!(
        svg.contains("actual"),
        "the source series must be labelled: {svg}"
    );
}

#[test]
fn each_forecast_kind_refuses_the_other_ones_shape_and_names_the_remedy() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("wrong.h5");
    seed_one(dir.path(), &store);
    seed_probabilistic(dir.path(), &store, "load_prob");
    run(
        &store,
        &["transform", "--horizon", "PT2H", "--interval", "PT1H"],
    );

    // A fan over a Deterministic has no spread to draw, and the message points
    // at the kind that does rather than just refusing.
    let err = run_err(
        &store,
        &[
            "plot",
            "--name",
            "load",
            "--kind",
            "fan",
            "--type",
            "DeterministicSingleTimeSeries",
            "--out",
            "-",
        ],
    );
    assert!(err.contains("overlay"), "name the remedy: {err}");

    // An overlay needs a Deterministic; a Probabilistic is refused by name.
    let err = run_err(
        &store,
        &[
            "plot",
            "--name",
            "load_prob",
            "--kind",
            "overlay",
            "--out",
            "-",
        ],
    );
    assert!(err.contains("Probabilistic"), "name what was found: {err}");

    // And an overlay over a plain static series is refused the same way.
    let err = run_err(
        &store,
        &[
            "plot",
            "--name",
            "load",
            "--kind",
            "overlay",
            "--type",
            "SingleTimeSeries",
            "--out",
            "-",
        ],
    );
    assert!(
        err.contains("SingleTimeSeries"),
        "name what was found: {err}"
    );
}

#[test]
fn a_heatmap_refuses_a_resolution_it_cannot_lay_out_against_a_day() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("hm.h5");

    // PT7H divides neither a day nor anything else useful: a time-of-day axis
    // would not close.
    write(dir.path(), "v.csv", "value\n1\n2\n3\n4\n");
    let d = write(
        dir.path(),
        "s.json",
        r#"{"owner_id": 1, "owner_type": "G", "name": "odd",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT7H"}"#,
    );
    run(&store, &["add", "--descriptor", d.to_str().unwrap()]);
    let err = run_err(
        &store,
        &["plot", "--name", "odd", "--kind", "heatmap", "--out", "-"],
    );
    assert!(err.contains("divides a day"), "{err}");

    // A calendar resolution has no time-of-day at all -- a month is not a
    // number of hours -- so it is refused with its own reason.
    write(dir.path(), "m.csv", "value\n1\n2\n3\n");
    let d = write(
        dir.path(),
        "m.json",
        r#"{"owner_id": 2, "owner_type": "G", "name": "monthly",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "m.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "P1M"}"#,
    );
    run(&store, &["add", "--descriptor", d.to_str().unwrap()]);
    let err = run_err(
        &store,
        &[
            "plot", "--name", "monthly", "--kind", "heatmap", "--out", "-",
        ],
    );
    assert!(err.contains("calendar resolution"), "{err}");
}

#[test]
fn a_heatmap_draws_one_series_and_says_so_when_the_selector_matched_more() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("many.h5");
    let mut body = String::from("value\n");
    for v in 0..24 {
        body.push_str(&format!("{v}\n"));
    }
    write(dir.path(), "v.csv", &body);
    for owner in [1, 2] {
        let json = format!(
            r#"{{"owner_id": {owner}, "owner_type": "G", "name": "load",
                 "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
                 "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}}"#
        );
        let d = write(dir.path(), &format!("s{owner}.json"), &json);
        run(&store, &["add", "--descriptor", d.to_str().unwrap()]);
    }

    // Two owners share the name, so the selector matches both. The message has
    // to say how to narrow it, not just that it failed.
    let err = run_err(
        &store,
        &["plot", "--name", "load", "--kind", "heatmap", "--out", "-"],
    );
    assert!(err.contains("--owner-id"), "name the remedy: {err}");

    // Narrowed, it draws.
    let svg = run(
        &store,
        &[
            "plot",
            "--name",
            "load",
            "--owner-id",
            "1",
            "--kind",
            "heatmap",
            "--out",
            "-",
        ],
    );
    assert_self_contained("heatmap", &svg);
}

// ---------------------------------------------------------------------------
// The maintenance commands' non-JSON output and their --dry-run previews
//
// `compact`, `remove`, `remove-all` and `clear` were each driven only through
// `-f json`, or only in the form that does the work. The preview a `--dry-run`
// prints and the table a human sees are separate code paths from the JSON, and
// a `--dry-run` that quietly modified the store would be the worst kind of bug
// for a flag whose whole purpose is that it does not.
// ---------------------------------------------------------------------------

#[test]
fn compact_reports_what_it_reclaimed_in_every_format() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("c.h5");
    seed_one(dir.path(), &store);
    // Something to actually reclaim, so the numbers are not all trivially zero.
    write(dir.path(), "w.csv", "value\n9\n8\n7\n");
    let d = write(
        dir.path(),
        "two.json",
        r#"{"owner_id": 43, "owner_type": "Generator", "name": "spill",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "w.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    run(&store, &["add", "--descriptor", d.to_str().unwrap()]);
    run(
        &store,
        &["remove", "--owner-id", "43", "--name", "spill", "--force"],
    );

    // The default table names every metric, so a human can read the result
    // without knowing the JSON key names.
    let table = run(&store, &["compact", "--force"]);
    for metric in [
        "slots_reclaimed",
        "datasets_dropped",
        "feature_sets_reclaimed",
        "timestamp_sets_reclaimed",
        "bytes_reclaimed",
    ] {
        assert!(table.contains(metric), "{metric} missing from: {table}");
    }

    // CSV carries the same rows under a Metric/Value header, for a script that
    // would rather not parse JSON.
    let csv = run(&store, &["-f", "csv", "compact", "--force"]);
    let header = csv.lines().next().unwrap_or_default();
    assert!(
        header.contains("Metric") && header.contains("Value"),
        "{csv}"
    );
    assert!(csv.contains("bytes_reclaimed"), "{csv}");

    // The store still reads after being rewritten -- compaction is not a
    // destructive operation on live data.
    assert!(run(&store, &["list"]).contains("load"));
}

#[test]
fn a_dry_run_previews_each_destructive_command_without_touching_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("dry.h5");
    seed_one(dir.path(), &store);
    write(dir.path(), "b.csv", "value\n4\n5\n6\n");
    let d = write(
        dir.path(),
        "b.json",
        r#"{"owner_id": 42, "owner_type": "Generator", "name": "wind",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "b.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    run(&store, &["add", "--descriptor", d.to_str().unwrap()]);

    // `remove` names the one series it would delete.
    let out = run(
        &store,
        &["remove", "--owner-id", "42", "--name", "load", "--dry-run"],
    );
    assert!(out.contains("Would remove"), "{out}");
    assert!(out.contains("load"), "{out}");

    // `remove --all` lists them, so the selector can be checked before it runs.
    let out = run(
        &store,
        &["remove", "--all", "--owner-id", "42", "--dry-run"],
    );
    assert!(out.contains("Would remove 2 time series"), "{out}");
    assert!(
        out.contains("name=load") && out.contains("name=wind"),
        "{out}"
    );

    // `clear` counts the whole store.
    let out = run(&store, &["clear", "--dry-run"]);
    assert!(out.contains("Would clear 2"), "{out}");

    // None of the three touched anything.
    let listed = run(&store, &["list"]);
    assert!(
        listed.contains("load") && listed.contains("wind"),
        "a --dry-run must not remove anything: {listed}"
    );
}

#[test]
fn remove_all_reports_a_zero_rather_than_an_empty_document() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("none.h5");
    seed_one(dir.path(), &store);

    // A selector that matches nothing is not an error -- a script looping over
    // owners should not have to special-case the empty one.
    let out = run(&store, &["remove", "--all", "--owner-id", "999", "--force"]);
    assert!(out.contains("No time series matched"), "{out}");

    let json = run(
        &store,
        &[
            "-f",
            "json",
            "remove",
            "--all",
            "--owner-id",
            "999",
            "--force",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["removed"], 0, "{json}");
}

#[test]
fn clear_requires_its_owner_flags_together_or_not_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("half.h5");
    seed_one(dir.path(), &store);

    // Half a selector is ambiguous: clearing "owner 42, any category" is a
    // different operation from clearing the store, so it is refused rather
    // than guessed.
    let err = run_err(&store, &["clear", "--owner-id", "42", "--force"]);
    assert!(err.contains("--owner-category"), "{err}");

    // Both together scope the clear to that owner.
    let out = run(
        &store,
        &[
            "-f",
            "json",
            "clear",
            "--owner-id",
            "42",
            "--owner-category",
            "Component",
            "--force",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["cleared"], 1, "{out}");
}

// ---------------------------------------------------------------------------
// Every dtype, through the CSV encode and both decode paths
//
// `csv_io` matches on `Dtype` in four places -- text encode, text decode, JSON
// decode, and the lossy f64 decode `stats` uses. The suites only ever drove
// f64, so the narrow integer arms were the widest untested surface in the
// module. A dtype that encodes but decodes wrong is a silent data bug, so each
// one goes in as text and has to come back out as the same text.
// ---------------------------------------------------------------------------

#[test]
fn every_element_type_survives_csv_in_and_csv_out() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("dtypes.h5");

    // Values chosen to be exactly representable in each type and to exercise
    // the sign bit where the type has one.
    let cases: &[(&str, &str, &[&str])] = &[
        ("f64", "1.5\n-2.25\n3\n", &["1.5", "-2.25", "3"]),
        ("f32", "1.5\n-2.25\n3\n", &["1.5", "-2.25", "3"]),
        ("i64", "1\n-2\n3\n", &["1", "-2", "3"]),
        ("i32", "1\n-2\n3\n", &["1", "-2", "3"]),
        ("i16", "1\n-2\n32767\n", &["1", "-2", "32767"]),
        ("i8", "1\n-2\n127\n", &["1", "-2", "127"]),
        ("u64", "1\n2\n3\n", &["1", "2", "3"]),
        ("u32", "1\n2\n3\n", &["1", "2", "3"]),
        ("u16", "1\n2\n65535\n", &["1", "2", "65535"]),
        ("u8", "1\n2\n255\n", &["1", "2", "255"]),
        ("bool", "true\nfalse\ntrue\n", &["true", "false", "true"]),
    ];

    for (dtype, body, expected) in cases {
        let csv_name = format!("{dtype}.csv");
        write(dir.path(), &csv_name, &format!("value\n{body}"));
        let json = format!(
            r#"{{"owner_id": 1, "owner_type": "Generator", "name": "{dtype}_series",
                 "type": "SingleTimeSeries", "element_type": "{dtype}", "csv": "{csv_name}",
                 "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}}"#
        );
        let d = write(dir.path(), &format!("{dtype}.json"), &json);
        run(&store, &["add", "--descriptor", d.to_str().unwrap()]);

        // Text decode.
        let out = run(
            &store,
            &["-f", "csv", "get", "--name", &format!("{dtype}_series")],
        );
        let got: Vec<String> = data_lines(&out)
            .iter()
            .map(|l| l.rsplit(',').next().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            got, *expected,
            "{dtype} did not survive the text round trip"
        );

        // JSON decode is a separate match: a number must come back a JSON
        // number and a bool a JSON bool, not a string of either.
        let out = run(
            &store,
            &["-f", "json", "get", "--name", &format!("{dtype}_series")],
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let values = v["values"]
            .as_array()
            .unwrap_or_else(|| panic!("no values array in {out}"));
        assert_eq!(values.len(), 3, "{dtype}: {out}");
        if *dtype == "bool" {
            assert!(
                values[0].is_boolean(),
                "{dtype} must decode to JSON bools: {out}"
            );
        } else {
            assert!(
                values[0].is_number(),
                "{dtype} must decode to JSON numbers: {out}"
            );
        }

        // And the lossy f64 decode behind the per-series summary, which is a
        // fourth match on the same dtype.
        let info = run(&store, &["info", "--name", &format!("{dtype}_series")]);
        assert!(info.contains(dtype), "{dtype} missing from: {info}");
    }
}

// ---------------------------------------------------------------------------
// The two confirmation policies, without a terminal
//
// Every other test passes `--force`, so neither `confirm::ask` nor
// `confirm::ask_strict` was ever reached. They differ in exactly one way, and
// it is the interesting one: with no terminal to answer, `ask` proceeds and
// `ask_strict` refuses. Getting that backwards would either break every script
// that already works or silently overwrite an artifact nobody confirmed.
// ---------------------------------------------------------------------------

#[test]
fn a_recoverable_prompt_proceeds_when_there_is_nobody_to_answer_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("noprompt.h5");
    seed_one(dir.path(), &store);

    // No --force, no terminal: `remove` is an operation the invocation named
    // out loud, and there is nobody to confirm it to, so it runs.
    run(&store, &["remove", "--owner-id", "42", "--name", "load"]);
    let listed = run(&store, &["list"]);
    assert!(
        !listed.contains("load"),
        "a non-interactive remove should proceed: {listed}"
    );
}

#[test]
fn an_unrecoverable_prompt_refuses_instead_and_names_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("src.h5");
    seed_one(dir.path(), &store);
    let dest = dir.path().join("dest.h5");

    // The first save has nothing to replace, so nothing is asked.
    run(&store, &["persist", "--dest", dest.to_str().unwrap()]);

    // The second would overwrite it. A failed `persist_to` can leave neither
    // the old nor the new pair on disk, so with no terminal this stops rather
    // than proceeding -- and says which flag would allow it.
    let err = run_err(&store, &["persist", "--dest", dest.to_str().unwrap()]);
    assert!(err.contains("--force"), "name the flag: {err}");
    assert!(err.contains("already exist"), "say what is at risk: {err}");

    // The destination is untouched by the refusal.
    assert!(dest.exists(), "the refusal must not delete the target");

    // Said out loud, it goes through -- by either spelling.
    run(
        &store,
        &["persist", "--dest", dest.to_str().unwrap(), "--force"],
    );
    run(
        &store,
        &["--yes", "persist", "--dest", dest.to_str().unwrap()],
    );
}

// ---------------------------------------------------------------------------
// The association writes: previews, empty filters, and what a bad batch says
//
// `detach`/`unlink`/`reassign` already have `--dry-run` cover; `attach` and
// `link` did not, and neither did the zero-match replies or the row-level
// complaints `--from` makes about a malformed batch. A batch that reports the
// wrong row number is worse than one that fails, so each message is asserted
// to name the row it choked on.
// ---------------------------------------------------------------------------

#[test]
fn attach_and_link_preview_what_they_would_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("prev.h5");
    run(&store, &["init"]);

    let out = run(
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
            "--dry-run",
        ],
    );
    assert!(out.contains("Would attach 1"), "{out}");

    let out = run(
        &store,
        &[
            "link",
            "--parent-id",
            "42",
            "--parent-type",
            "Generator",
            "--child-id",
            "9",
            "--child-type",
            "Bus",
            "--dry-run",
        ],
    );
    assert!(out.contains("Would add 1"), "{out}");

    // A preview writes nothing.
    assert!(data_lines(&run(&store, &["-f", "csv", "attributes"])).is_empty());
    assert!(data_lines(&run(&store, &["-f", "csv", "links"])).is_empty());

    // The JSON preview carries the rows themselves, so a script can check them
    // before committing.
    let out = run(
        &store,
        &[
            "-f",
            "json",
            "attach",
            "--component-id",
            "42",
            "--component-type",
            "Generator",
            "--attribute-id",
            "7",
            "--attribute-type",
            "GeographicInfo",
            "--dry-run",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["would_attach"], 1, "{out}");
    assert_eq!(v["attachments"][0]["component_id"], 42, "{out}");
}

#[test]
fn detach_and_unlink_report_a_zero_when_nothing_matched() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("empty.h5");
    run(&store, &["init"]);

    // Nothing is attached, so nothing matches. That is a zero, not an error:
    // a cleanup script should be able to run twice.
    let out = run(&store, &["detach", "--component-id", "42", "--force"]);
    assert!(out.contains("No attachments matched"), "{out}");

    let out = run(&store, &["unlink", "--all", "--force"]);
    assert!(out.to_lowercase().contains("no "), "{out}");
}

#[test]
fn a_malformed_association_batch_names_the_row_it_choked_on() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("batch.h5");
    run(&store, &["init"]);

    // A header with no rows under it is a mistake, not an empty batch: the
    // caller meant to attach something.
    let empty = write(
        dir.path(),
        "empty.csv",
        "component_id,component_type,attribute_id,attribute_type\n",
    );
    let err = run_err(&store, &["attach", "--from", empty.to_str().unwrap()]);
    assert!(err.contains("no rows"), "{err}");

    // A non-integer id names the row and the column, not just the file.
    let bad_id = write(
        dir.path(),
        "bad_id.csv",
        "component_id,component_type,attribute_id,attribute_type\n\
         43,Generator,8,GeographicInfo\nnope,Bus,9,GeographicInfo\n",
    );
    let err = run_err(&store, &["attach", "--from", bad_id.to_str().unwrap()]);
    assert!(err.contains("row 2"), "name the row: {err}");
    assert!(err.contains("not an integer"), "{err}");

    // So does an empty type, which would otherwise attach a nameless thing.
    let blank = write(
        dir.path(),
        "blank.csv",
        "component_id,component_type,attribute_id,attribute_type\n43,,8,GeographicInfo\n",
    );
    let err = run_err(&store, &["attach", "--from", blank.to_str().unwrap()]);
    assert!(err.contains("row 1"), "name the row: {err}");
    assert!(err.contains("component_type"), "name the column: {err}");

    // `link` reads the same shape under its own column names, and rejects the
    // attach header rather than silently reading the pairs in the wrong order.
    let links = write(
        dir.path(),
        "links.csv",
        "parent_id,parent_type,child_id,child_type\n1,Generator,2,Bus\n3,Generator,4,Bus\n",
    );
    run(&store, &["link", "--from", links.to_str().unwrap()]);
    assert_eq!(data_lines(&run(&store, &["-f", "csv", "links"])).len(), 2);

    let err = run_err(&store, &["link", "--from", empty.to_str().unwrap()]);
    assert!(err.contains("parent_id,parent_type"), "{err}");
}

// ---------------------------------------------------------------------------
// The JSON half of every preview, and the read commands' non-default formats
//
// A `--dry-run` renders twice -- once as a table for a human, once as JSON for
// a script -- and the suites only ever read one of them per command. The same
// is true of `grid` and the discovery commands, whose CSV and JSON forms are
// separate `match` arms from the table they all default to.
// ---------------------------------------------------------------------------

#[test]
fn every_preview_says_the_same_thing_in_json_as_it_does_in_a_table() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("prev.h5");
    seed_one(dir.path(), &store);

    let json = |args: &[&str]| -> serde_json::Value {
        let out = run(&store, args);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}\n{out}"))
    };

    let v = json(&[
        "-f",
        "json",
        "remove",
        "--owner-id",
        "42",
        "--name",
        "load",
        "--dry-run",
    ]);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["would_remove"], 1, "{v}");

    let v = json(&[
        "-f",
        "json",
        "remove",
        "--all",
        "--owner-id",
        "42",
        "--dry-run",
    ]);
    assert_eq!(v["would_remove"], 1, "{v}");
    // The matches carry the identifying triple, so a caller can see *which*
    // series a filter caught rather than only how many.
    assert_eq!(v["matches"][0]["name"], "load", "{v}");
    assert_eq!(v["matches"][0]["owner_id"], 42, "{v}");

    let v = json(&["-f", "json", "clear", "--dry-run"]);
    assert_eq!(v["would_clear"], 1, "{v}");

    let v = json(&[
        "-f",
        "json",
        "copy",
        "--owner-id",
        "42",
        "--name",
        "load",
        "--dst-owner-id",
        "99",
        "--dst-owner-type",
        "Generator",
        "--new-name",
        "copied",
        "--dry-run",
    ]);
    assert_eq!(v["would_copy"], 1, "{v}");
    assert_eq!(v["dst_owner_id"], 99, "{v}");
    assert_eq!(v["dst_name"], "copied", "{v}");

    // A `persist` preview names both halves of the artifact: they are one
    // logical thing and a caller checking only the .h5 would miss the catalog.
    let dest = dir.path().join("out.h5");
    let v = json(&[
        "-f",
        "json",
        "persist",
        "--dest",
        dest.to_str().unwrap(),
        "--dry-run",
    ]);
    let would: Vec<String> = v["would_write"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    assert_eq!(would.len(), 2, "{v}");
    assert!(would.iter().any(|p| p.ends_with(".h5")), "{v}");
    assert!(would.iter().any(|p| p.ends_with(".sqlite")), "{v}");
    assert!(v["overwriting"].as_array().unwrap().is_empty(), "{v}");
    assert!(!dest.exists(), "a preview must not write");

    // Once it exists, the preview says what it would replace.
    run(&store, &["persist", "--dest", dest.to_str().unwrap()]);
    let v = json(&[
        "-f",
        "json",
        "persist",
        "--dest",
        dest.to_str().unwrap(),
        "--dry-run",
    ]);
    assert_eq!(v["overwriting"].as_array().unwrap().len(), 2, "{v}");
}

#[test]
fn merge_previews_and_declines_the_two_degenerate_cases() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("dest.h5");
    let source = dir.path().join("source.h5");
    seed_one(dir.path(), &dest);
    write(dir.path(), "s.csv", "value\n7\n8\n9\n");
    let d = write(
        dir.path(),
        "s.json",
        r#"{"owner_id": 77, "owner_type": "Generator", "name": "hydro",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "s.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    run(&source, &["add", "--descriptor", d.to_str().unwrap()]);

    // Merging a store into itself would read and write the same file at once.
    let err = run_err(&dest, &["merge", "--from", dest.to_str().unwrap()]);
    assert!(err.contains("destination store itself"), "{err}");

    // A selector matching nothing in the source is a zero, not a failure.
    let out = run(
        &dest,
        &[
            "-f",
            "json",
            "merge",
            "--from",
            source.to_str().unwrap(),
            "--owner-id",
            "999",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["merged"], 0, "{out}");

    // The preview lists what would come across, and moves nothing.
    let out = run(
        &dest,
        &[
            "-f",
            "json",
            "merge",
            "--from",
            source.to_str().unwrap(),
            "--dry-run",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["would_merge"], 1, "{out}");
    assert_eq!(v["matches"][0]["name"], "hydro", "{out}");
    assert!(
        !run(&dest, &["list"]).contains("hydro"),
        "a preview must not merge"
    );

    // The table preview names them for a human.
    let out = run(
        &dest,
        &["merge", "--from", source.to_str().unwrap(), "--dry-run"],
    );
    assert!(out.contains("Would merge 1"), "{out}");
    assert!(out.contains("hydro"), "{out}");
}

#[test]
fn grid_and_the_discovery_commands_render_in_every_format() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("g.h5");
    seed_one(dir.path(), &store);

    // `grid` is a matrix, so each format lays it out differently: CSV gets a
    // header row, JSON gets the columns beside the rows.
    let csv = run(&store, &["-f", "csv", "grid", "--resolution", "PT1H"]);
    let header = csv.lines().next().unwrap_or_default();
    assert!(header.starts_with("timestamp"), "{csv}");
    assert_eq!(data_lines(&csv).len(), 3, "{csv}");

    let out = run(&store, &["-f", "json", "grid", "--resolution", "PT1H"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["columns"].as_array().unwrap().len(), 1, "{out}");
    assert_eq!(v["rows"].as_array().unwrap().len(), 3, "{out}");

    // `--label full` spells name@owner instead of the bare owner id, which is
    // what a single-name grid falls back to.
    let csv = run(
        &store,
        &[
            "-f",
            "csv",
            "grid",
            "--resolution",
            "PT1H",
            "--label",
            "full",
        ],
    );
    assert!(csv.lines().next().unwrap().contains("load@42"), "{csv}");
    let csv = run(
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
    assert!(csv.lines().next().unwrap().contains("42"), "{csv}");

    // A selector that matches nothing has no grid to lay out. The reader
    // refuses before the command gets as far as laying out columns, so the
    // message comes from the core and names the type it looked for.
    let err = run_err(
        &store,
        &["grid", "--resolution", "PT1H", "--owner-id", "999"],
    );
    assert!(
        err.contains("no SingleTimeSeries match the filter"),
        "{err}"
    );

    // The discovery commands are single columns, and each format renders that
    // column its own way.
    for cmd in ["names", "owner-types"] {
        let csv = run(&store, &["-f", "csv", cmd]);
        assert!(!data_lines(&csv).is_empty(), "{cmd}: {csv}");
        let out = run(&store, &["-f", "json", cmd]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let items = v["items"]
            .as_array()
            .unwrap_or_else(|| panic!("{cmd}: no items array in {out}"));
        assert!(!items.is_empty(), "{cmd}: {out}");
    }

    // An empty result prints a note rather than a bare blank table.
    let out = run(&store, &["names", "--owner-id", "999"]);
    assert!(out.contains("no results"), "{out}");

    // `exists` answers in each format, and exits nonzero when it does not.
    assert!(run(&store, &["exists", "--name", "load"]).contains("true"));
    let out = run(&store, &["-f", "csv", "exists", "--name", "load"]);
    assert!(out.contains("exists") && out.contains("true"), "{out}");
    let out = run(&store, &["-f", "json", "exists", "--name", "load"]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["exists"],
        true
    );
    // The nonzero exit is the point: `infrastore exists ... && ...` in a shell.
    run_err(&store, &["exists", "--name", "nope"]);
}

/// A `grid` range is a query bound like any other, and has to be spelled the
/// way the timeline it slices is.
///
/// `grid` filters the reader's own axis in the CLI rather than handing the
/// range to the core, so it does not get the core's check for free the way
/// every other ranged read does. It used to skip it entirely: `get` refused a
/// mismatched bound while `grid` quietly answered one.
#[test]
fn a_grid_range_must_be_spelled_the_way_the_timeline_is() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("spell.h5");
    write(dir.path(), "v.csv", "value\n1\n2\n3\n4\n");

    // One zoneless series, one that records instants.
    let zl = write(
        dir.path(),
        "zl.json",
        r#"{"owner_id": 1, "owner_type": "G", "name": "wall",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
            "initial_timestamp": "2024-01-01T00:00:00", "resolution": "PT1H"}"#,
    );
    run(
        &store,
        &["--zoneless", "add", "--descriptor", zl.to_str().unwrap()],
    );
    let aware = write(
        dir.path(),
        "aw.json",
        r#"{"owner_id": 2, "owner_type": "G", "name": "instant",
            "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
            "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}"#,
    );
    run(&store, &["add", "--descriptor", aware.to_str().unwrap()]);

    let zoned_bound = "2024-01-01T00:00:00Z..2024-01-01T02:00:00Z";
    let wall_bound = "2024-01-01T00:00:00..2024-01-01T02:00:00";

    // An instant against a wall-clock timeline has no defined mapping.
    let err = run_err(
        &store,
        &[
            "grid",
            "--resolution",
            "PT1H",
            "--spelling",
            "zoneless",
            "--time-range",
            zoned_bound,
        ],
    );
    assert!(err.contains("is zoneless"), "{err}");

    // And a wall clock against a timeline of instants names none.
    let err = run_err(
        &store,
        &[
            "--zoneless",
            "grid",
            "--resolution",
            "PT1H",
            "--spelling",
            "zoned",
            "--time-range",
            wall_bound,
        ],
    );
    assert!(err.contains("carry no zone"), "{err}");

    // Matched both ways, the slice is taken as before.
    let out = run(
        &store,
        &[
            "--zoneless",
            "-f",
            "csv",
            "grid",
            "--resolution",
            "PT1H",
            "--spelling",
            "zoneless",
            "--time-range",
            wall_bound,
        ],
    );
    assert_eq!(data_lines(&out).len(), 2, "{out}");

    let out = run(
        &store,
        &[
            "-f",
            "csv",
            "grid",
            "--resolution",
            "PT1H",
            "--spelling",
            "zoned",
            "--time-range",
            zoned_bound,
        ],
    );
    assert_eq!(data_lines(&out).len(), 2, "{out}");
}

/// A CSV whose rows disagree about their offset stores every instant exactly,
/// but records one spelling — so a later row reads back at a different wall
/// clock than it went in as. That is silent, and silently moving a wall clock
/// is what this whole feature exists to stop, so the ingest says so and names
/// the remedy.
#[test]
fn a_csv_whose_rows_disagree_about_their_offset_says_so_and_names_the_fix() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("mix.h5");
    // Two rows either side of the US spring-forward, each written at the offset
    // in force locally that day.
    write(
        dir.path(),
        "m.csv",
        "timestamp,value\n2024-03-09T12:00:00-07:00,1\n2024-03-11T12:00:00-06:00,2\n",
    );
    let mixed = write(
        dir.path(),
        "m.json",
        r#"{"owner_id": 1, "owner_type": "G", "name": "mix",
            "type": "NonSequentialTimeSeries", "element_type": "f64", "csv": "m.csv"}"#,
    );

    let out = raw(&store, &["add", "--descriptor", mixed.to_str().unwrap()]);
    assert!(out.status.success(), "a mixed file is still ingested");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("warning"), "{stderr}");
    assert!(stderr.contains("row 2"), "name the row: {stderr}");
    assert!(
        stderr.contains("time_reference"),
        "name the remedy: {stderr}"
    );

    // The warning is true: the second row's wall clock did move.
    let back = run(&store, &["-f", "csv", "get", "--name", "mix"]);
    assert!(back.contains("2024-03-11T11:00:00-07:00"), "{back}");

    // And the remedy works -- a named zone renders each instant in that zone,
    // reproducing both wall clocks exactly.
    let zoned = write(
        dir.path(),
        "z.json",
        r#"{"owner_id": 2, "owner_type": "G", "name": "mixzone",
            "type": "NonSequentialTimeSeries", "element_type": "f64", "csv": "m.csv",
            "time_reference": "America/Denver"}"#,
    );
    let out = raw(&store, &["add", "--descriptor", zoned.to_str().unwrap()]);
    assert!(out.status.success());
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("warning"),
        "an explicit time_reference is the caller having decided; do not second-guess it"
    );
    let back = run(&store, &["-f", "csv", "get", "--name", "mixzone"]);
    assert!(back.contains("2024-03-09T12:00:00-07:00"), "{back}");
    assert!(back.contains("2024-03-11T12:00:00-06:00"), "{back}");

    // A file that agrees with itself says nothing.
    write(
        dir.path(),
        "s.csv",
        "timestamp,value\n2024-03-09T12:00:00-07:00,1\n2024-03-11T12:00:00-07:00,2\n",
    );
    let same = write(
        dir.path(),
        "s.json",
        r#"{"owner_id": 3, "owner_type": "G", "name": "same",
            "type": "NonSequentialTimeSeries", "element_type": "f64", "csv": "s.csv"}"#,
    );
    let out = raw(&store, &["add", "--descriptor", same.to_str().unwrap()]);
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("warning"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A calendar period steps on the UTC calendar whatever the series' spelling
/// says, and that is warned about — but only where it can actually bite.
///
/// The gate used to be `is_zoned()`, which is true for `utc` as well, so every
/// UTC series with a monthly period was warned about DST drift against the very
/// calendar it steps on. A warning that cannot come true, on the most common
/// spelling there is, is how a real one gets ignored.
#[test]
fn the_calendar_period_warning_fires_only_where_the_calendars_can_disagree() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("cal.h5");
    write(dir.path(), "m.csv", "value\n1\n2\n3\n");

    let add_monthly = |owner: i64, name: &str, reference: Option<&str>| {
        let spelling = match reference {
            Some(r) => format!(r#", "time_reference": "{r}""#),
            None => String::new(),
        };
        let json = format!(
            r#"{{"owner_id": {owner}, "owner_type": "G", "name": "{name}",
                 "type": "SingleTimeSeries", "element_type": "f64", "csv": "m.csv",
                 "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "P1M"{spelling}}}"#
        );
        let d = write(dir.path(), &format!("{name}.json"), &json);
        let out = raw(
            &store,
            &[
                "--log-level",
                "warn",
                "add",
                "--descriptor",
                d.to_str().unwrap(),
            ],
        );
        assert!(out.status.success(), "{name} should still be stored");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };

    // UTC is the calendar the stepping already uses, so there is nothing to
    // drift against. Same for a wall clock, which is held as if UTC.
    assert!(
        !add_monthly(1, "utc_monthly", Some("utc")).contains("calendar period"),
        "a UTC series cannot drift from the UTC calendar"
    );
    assert!(
        !add_monthly(2, "wall_monthly", Some("zoneless")).contains("calendar period"),
        "a wall clock is held as if UTC, so it steps on its own calendar"
    );

    // A named zone genuinely can disagree -- both at a month boundary and at a
    // DST transition -- so it is warned about, and the remedy is named.
    let warned = add_monthly(3, "zone_monthly", Some("America/Denver"));
    assert!(warned.contains("calendar period"), "{warned}");
    assert!(warned.contains("NonSequentialTimeSeries"), "{warned}");

    // So can a fixed offset, at a month boundary.
    assert!(
        add_monthly(4, "offset_monthly", Some("-07:00")).contains("calendar period"),
        "a fixed offset can still disagree at a month boundary"
    );
}

/// A zone name the tz database does not recognize is stored, and said out loud.
///
/// The core validates a zone name's *shape* and never resolves it, so a typo
/// reaches storage intact. The CLI is the layer with a database, and this was
/// the one spelling it let through in silence — the same typo passed to
/// `--assume-timezone` is a hard error. It warns rather than refuses, because
/// the store deliberately accepts a name this build has not heard of yet.
#[test]
fn a_descriptor_zone_the_database_does_not_know_is_warned_about_and_stored() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("zone.h5");
    write(dir.path(), "v.csv", "value\n1\n2\n3\n");

    let add_with = |owner: i64, name: &str, zone: &str| {
        let json = format!(
            r#"{{"owner_id": {owner}, "owner_type": "G", "name": "{name}",
                 "type": "SingleTimeSeries", "element_type": "f64", "csv": "v.csv",
                 "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H",
                 "time_reference": "{zone}"}}"#
        );
        let d = write(dir.path(), &format!("{name}.json"), &json);
        let out = raw(&store, &["add", "--descriptor", d.to_str().unwrap()]);
        assert!(out.status.success(), "{name} must still be stored");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };

    let warned = add_with(1, "typo", "America/Dever");
    assert!(warned.contains("warning"), "{warned}");
    assert!(warned.contains("America/Dever"), "name the zone: {warned}");
    assert!(
        warned.contains("store-info"),
        "name where to see it: {warned}"
    );

    // Stored as given: the store records names its database predates.
    let listed = run(&store, &["-f", "json", "info", "--name", "typo"]);
    assert!(listed.contains("America/Dever"), "{listed}");

    // A real zone says nothing, and neither do the non-zone spellings.
    assert!(!add_with(2, "real", "America/Denver").contains("warning"));
    assert!(!add_with(3, "offset", "-07:00").contains("warning"));
    assert!(!add_with(4, "wall", "zoneless").contains("warning"));

    // `store-info` still reports it, which is the pre-existing surface.
    let info = run(&store, &["store-info"]);
    assert!(info.contains("unrecognized"), "{info}");
}

/// Two stores holding the same series compare clean even though their catalog
/// ids differ.
///
/// An id names a row in one catalog, not a series, so two stores built in
/// different orders assign them differently. `diff` pairs on identity and
/// content hash and never reads the id — this pins that, since "diff ignores
/// the id" is now a documented guarantee rather than an accident of the code.
#[test]
fn diff_ignores_catalog_ids() {
    let dir = tempfile::tempdir().unwrap();
    let left = dir.path().join("left.h5");
    let right = dir.path().join("right.h5");

    write(dir.path(), "a.csv", "value\n1\n2\n3\n");
    write(dir.path(), "b.csv", "value\n4\n5\n6\n");
    let descriptor = |name: &str, csv: &str| {
        format!(
            r#"{{"owner_id": 42, "owner_type": "Generator", "name": "{name}",
                 "type": "SingleTimeSeries", "element_type": "f64", "csv": "{csv}",
                 "initial_timestamp": "2024-01-01T00:00:00Z", "resolution": "PT1H"}}"#
        )
    };
    let a = write(dir.path(), "a.json", &descriptor("alpha", "a.csv"));
    let b = write(dir.path(), "b.json", &descriptor("beta", "b.csv"));

    // Same two series, opposite insertion orders, so the ids are swapped.
    run(&left, &["add", "--descriptor", a.to_str().unwrap()]);
    run(&left, &["add", "--descriptor", b.to_str().unwrap()]);
    run(&right, &["add", "--descriptor", b.to_str().unwrap()]);
    run(&right, &["add", "--descriptor", a.to_str().unwrap()]);

    let id_of = |store: &Path, name: &str| -> i64 {
        let listed = run(store, &["-f", "json", "list", "--name", name]);
        let rows: serde_json::Value = serde_json::from_str(&listed).unwrap();
        rows["items"][0]["id"].as_i64().unwrap()
    };
    assert_ne!(
        id_of(&left, "alpha"),
        id_of(&right, "alpha"),
        "the two stores must disagree about the id for this test to mean anything",
    );

    let same = run(&left, &["diff", "--against", right.to_str().unwrap()]);
    assert!(same.contains("0 added, 0 removed, 0 changed"), "{same}");
}

// --- composite element types in the JSON views ------------------------------
//
// The CLI cannot yet *write* a composite element type — `add` reads numbers —
// so these seed the store through the core and drive the read commands, which
// is the surface under test.

/// Four piecewise-linear curves, ragged on purpose: the widest has two points,
/// so every row is padded to five slots and the decoded view is the only place
/// the original curves are visible.
fn curves() -> infrastore_core::DecodedValues {
    use infrastore_core::XyPoint;
    infrastore_core::DecodedValues::PiecewiseLinear(vec![
        vec![XyPoint { x: 0.0, y: 1.0 }, XyPoint { x: 1.0, y: 3.0 }],
        vec![XyPoint { x: 0.0, y: 2.0 }],
        vec![],
        vec![XyPoint { x: 2.0, y: 9.5 }],
    ])
}

/// A static series and a `Deterministic` over the same four curves.
fn seed_curves(store: &Path) {
    use chrono::{Duration, TimeZone, Utc};
    use infrastore_core::{
        Deterministic, Features, OwnerCategory, Period, SingleTimeSeries, TimeSeriesData,
    };

    let t0 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let hour = Period::Fixed(Duration::hours(1));
    // Held for the whole lifetime of the handle, not just the open: see
    // SPAWN_GATE. A sibling test's fork inside this window would inherit the
    // store's lock and keep it after this function returns.
    let _gate = SPAWN_GATE.write().unwrap_or_else(|e| e.into_inner());
    let mut s = infrastore_core::create_store(Some(store), false).unwrap();
    let mut add = |data| {
        s.add_time_series(
            42,
            "Generator",
            OwnerCategory::Component,
            data,
            Features::new(),
        )
        .unwrap();
    };
    add(TimeSeriesData::SingleTimeSeries(
        SingleTimeSeries::from_values(t0, hour, &curves(), "cost").unwrap(),
    ));
    // [horizon = 2, count = 2] over the same four curves.
    add(TimeSeriesData::Deterministic(
        Deterministic::from_values(
            t0,
            hour,
            Period::Fixed(Duration::hours(2)),
            hour,
            2,
            &curves(),
            "cost_fc",
        )
        .unwrap(),
    ));
    // Explicit, and before the gate is released: every assertion below is a
    // child process reading this file, and libhdf5 holds an exclusive lock on
    // one it has open for writing.
    drop(s);
}

#[test]
fn composite_json_carries_the_decoded_curves_beside_the_raw_values() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("curves.h5");
    seed_curves(&store);

    for name in ["cost", "cost_fc"] {
        let out: serde_json::Value =
            serde_json::from_str(&run(&store, &["-f", "json", "get", "--name", name])).unwrap();
        // Both keys, on both paths: `element_values` is what a caller wants,
        // but `values` is the field that was there first and scripts read it.
        assert_eq!(
            out["values"].as_array().map(Vec::len),
            Some(20),
            "{name} lost its raw values: {out}"
        );
        assert_eq!(out["element_values"]["kind"], "piecewise_linear", "{out}");
        let steps = out["element_values"]["timesteps"].as_array().unwrap();
        assert_eq!(steps.len(), 4, "{name}: {out}");
        assert_eq!(
            steps[0][1],
            serde_json::json!({"x": 1.0, "y": 3.0}),
            "{out}"
        );
        assert!(steps[2].as_array().unwrap().is_empty(), "{out}");
    }
}

#[test]
fn a_strided_composite_read_decodes_the_rows_it_kept() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("curves.h5");
    seed_curves(&store);

    let out: serde_json::Value = serde_json::from_str(&run(
        &store,
        &["-f", "json", "get", "--name", "cost", "--stride", "2"],
    ))
    .unwrap();
    let steps = out["element_values"]["timesteps"].as_array().unwrap();
    // The decoded curves name the same rows as `timestamps` and `values`, not
    // the whole stored array.
    assert_eq!(steps.len(), 2, "{out}");
    assert_eq!(out["timestamps"].as_array().unwrap().len(), 2, "{out}");
    assert_eq!(
        steps[0][0],
        serde_json::json!({"x": 0.0, "y": 1.0}),
        "{out}"
    );
    assert!(steps[1].as_array().unwrap().is_empty(), "{out}");
}

/// `store-info` reports the catalog revision beside the artifact's format
/// version — the two move independently, and this is where a user looks after a
/// read-only open reports that a store needs upgrading.
#[test]
fn store_info_reports_the_catalog_schema_revision() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("info.h5");
    run(&store, &["init"]);
    let info = run(&store, &["-f", "json", "store-info"]);
    assert!(info.contains("catalog_schema_revision"), "{info}");
    assert!(info.contains("data_format_version"), "{info}");
}

/// `upgrade` actually migrates a stale store — the behavior that distinguishes
/// it from every read command.
///
/// The no-op test above cannot show this: a current store opens read-only just
/// fine and reports revision 2 either way, so it would pass unchanged if this
/// command quietly used the read-only opener and migrated nothing. This one
/// backdates the catalog's revision stamp and walks the whole loop a user hits
/// — a read refuses and names the remedy, `upgrade` applies it, the read works.
#[test]
fn upgrade_migrates_a_stale_catalog_and_unblocks_the_read_commands() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("stale.h5");
    run(&store, &["init"]);

    let sqlite = dir.path().join("stale.h5.sqlite");
    let revision = || -> i64 {
        rusqlite::Connection::open(&sqlite)
            .unwrap()
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap()
    };
    {
        let conn = rusqlite::Connection::open(&sqlite).unwrap();
        conn.execute("UPDATE schema_version SET version = 1", [])
            .unwrap();
    }
    assert_eq!(revision(), 1);

    // Every read command opens read-only, so it cannot migrate — it refuses and
    // says what to do instead of failing somewhere deeper.
    let err = run_err(&store, &["list"]);
    assert!(err.contains("revision"), "{err}");
    assert!(err.contains("open the store once for writing"), "{err}");

    // `upgrade` is that writable open.
    let out = run(&store, &["-f", "json", "upgrade"]);
    assert!(out.contains("\"catalog_schema_revision\": 2"), "{out}");
    assert_eq!(revision(), 2);

    // And the read that just refused now works.
    run(&store, &["list"]);
}

/// Both version fields report the *store*, not the build.
///
/// `data_format_version` used to be the compile-time constant, which was
/// indistinguishable from the truth while the version check was strict
/// equality — an open either matched or failed. It is not indistinguishable any
/// more: an upgradable stamp is left in place until the catalog migrates, and a
/// read-only open never re-stamps at all, so the constant and the file can
/// legitimately disagree. Reporting the constant would have this command answer
/// a question about the build while appearing to answer one about the store.
#[test]
fn store_info_reads_the_format_version_off_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("stamp.h5");
    run(&store, &["init"]);

    let info = run(&store, &["-f", "json", "store-info"]);
    let parsed: serde_json::Value = serde_json::from_str(&info).unwrap();
    // A store this build just created carries this build's stamp, so the value
    // matches -- but it now arrives from the file rather than from `env!`.
    assert_eq!(
        parsed["data_format_version"].as_str().unwrap(),
        infrastore_core::DATA_FORMAT_VERSION
    );

    // The same field on `upgrade`, which is the command a stale store is sent
    // to and therefore the one most likely to be read during a version problem.
    let up = run(&store, &["-f", "json", "upgrade"]);
    let parsed: serde_json::Value = serde_json::from_str(&up).unwrap();
    assert_eq!(
        parsed["data_format_version"].as_str().unwrap(),
        infrastore_core::DATA_FORMAT_VERSION
    );
}

/// `upgrade` is the writable open that runs the migration ladder, and it is the
/// only CLI route to one that does nothing else — every read command, including
/// `store-info`, opens the store read-only and so cannot upgrade it.
///
/// On a current store it is a no-op that still reports the revision, which is
/// what makes it safe to run unconditionally in a deploy script.
#[test]
fn upgrade_is_the_writable_open_and_a_no_op_on_a_current_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("up.h5");
    run(&store, &["init"]);
    for _ in 0..2 {
        let out = run(&store, &["-f", "json", "upgrade"]);
        assert!(out.contains("\"catalog_schema_revision\": 2"), "{out}");
        assert!(out.contains("data_format_version"), "{out}");
    }
    // It must not invent a store where none exists — that is `init`'s job, and
    // silently creating one would turn a typo'd path into an empty store.
    let missing = dir.path().join("nope.h5");
    let err = run_err(&missing, &["upgrade"]);
    assert!(err.contains("not found"), "{err}");
}
