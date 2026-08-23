//! The benchmark harness, run at a size where correctness is the only thing
//! being measured.
//!
//! Nothing else drives this binary: it is a development tool, so a change to
//! the core API that breaks it is not caught by any other suite, and the
//! breakage surfaces only when someone reaches for a benchmark. These run each
//! scenario over a handful of series -- enough to exercise the bulk add, the
//! per-timestep read loop, and the reporting, without measuring anything.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_infrastore-bench");

/// Run a scenario at a token size, asserting it exits clean; returns stdout.
fn bench(args: &[&str]) -> String {
    let out = Command::new(BIN)
        .args(args)
        .output()
        .expect("failed to spawn infrastore-bench");
    assert!(
        out.status.success(),
        "infrastore-bench {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_add_scenario_runs_against_a_real_store() {
    let dir = tempfile::tempdir().unwrap();
    let out = bench(&[
        "add",
        "--count",
        "4",
        "--length",
        "8",
        "--path",
        dir.path().to_str().unwrap(),
    ]);
    // Both shapes are added, and both are reported -- a run that silently
    // skipped one would still exit 0.
    assert!(out.contains("SingleTimeSeries"), "{out}");
    assert!(out.contains("Deterministic"), "{out}");
}

#[test]
fn the_read_scenario_reports_per_step_timings() {
    let dir = tempfile::tempdir().unwrap();
    let out = bench(&[
        "read",
        "--count",
        "4",
        "--length",
        "8",
        "--steps",
        "3",
        "--path",
        dir.path().to_str().unwrap(),
    ]);
    // The point of the scenario is the distribution, not a single mean.
    assert!(
        out.to_lowercase().contains("p50") || out.to_lowercase().contains("median"),
        "expected per-step statistics: {out}"
    );
}

#[test]
fn the_in_memory_backend_runs_the_whole_thing_without_touching_a_disk() {
    // `all` chains add into read, which is the path a real benchmark run takes
    // and the only one where the two share a store.
    let out = bench(&[
        "all",
        "--count",
        "3",
        "--length",
        "4",
        "--steps",
        "2",
        "--in-memory",
    ]);
    assert!(out.contains("SingleTimeSeries"), "{out}");
}
