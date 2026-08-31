//! The worked examples appended to each command's `--help`.
//!
//! Every command has one, and a test enforces that: a synopsis tells a reader
//! what the flags *are*, but a runnable line tells them what the command is
//! *for*, and a command whose help ends at the flag list is the one people
//! guess at. They live here rather than inline in the `Commands` enum so that
//! enum stays readable as a list of commands.
//!
//! House style, so the blocks read as one set:
//!
//! - open with `Examples:`, then two-space-indented invocations;
//! - use the same running store (`demo.h5`), owner (`42`), and series (`load`)
//!   throughout, so the examples compose into a plausible session;
//! - spell types, categories, and durations the way the CLI prints them
//!   (`SingleTimeSeries`, `Component`, `PT1H`);
//! - keep every line inside 100 columns, wrapping with a trailing `\` when a
//!   realistic invocation does not fit.

/// Shown under the grouped command listing on `infrastore --help`.
pub const ROOT: &str = "\
Examples:
  infrastore template SingleTimeSeries > load.json        # start a descriptor
  infrastore --store demo.h5 add --descriptor load.json   # create the store, load the CSV
  infrastore --store demo.h5 list                         # what landed
  infrastore --store demo.h5 get --owner-id 42 --name load

Every command carries its own examples: run `infrastore <command> --help`.
The store path also comes from INFRASTORE_STORE, so --store can be omitted.";

pub const ADD: &str = "\
Examples:
  infrastore --store demo.h5 add --descriptor load.json
  infrastore --store demo.h5 add --descriptor load.json --csv other.csv --dry-run
  infrastore --store demo.h5 add --descriptor batch.json --replace --batch-size 500
  generate.py | infrastore --store demo.h5 add --descriptor -
  infrastore --store demo.h5 add --csv load.csv --owner-id 42 --owner-type Generator \\
      --name load --type SingleTimeSeries --element-type f64 \\
      --resolution PT1H --initial-timestamp 2024-01-01T00:00:00Z
  infrastore --store demo.h5 --assume-timezone UTC add --descriptor load.json

A descriptor may hold one object or an array of them (one transaction).
`infrastore template <TYPE>` prints a starting point; the inline flags above are
the same fields for a one-off. Set \"layout\": \"wide\" plus an owner_map to load
one `timestamp,gen_001,gen_002,...` file as one series per column.

A timestamp with no offset (`2024-01-01 00:00:00`) names no instant, so it needs
--assume-timezone UTC, or the fixed offset the data was written in (-07:00).";

pub const INIT: &str = "\
Examples:
  infrastore --store demo.h5 init
  infrastore --store demo.h5 init --compression deflate:6
  infrastore --store demo.h5 init --catalog in-memory

--catalog in-memory holds the catalog in RAM for the duration of the command
instead of journaling every commit: much faster for a bulk load, and everything
is lost if the process dies before it finishes. Either way the command writes
the catalog out before it exits, so the store is complete when it returns.";

pub const MERGE: &str = "\
Examples:
  infrastore --store demo.h5 merge --from other.h5
  infrastore --store demo.h5 merge --from other.h5 --name-glob 'load_*' --dry-run
  infrastore --store demo.h5 merge --from other.h5 --owner-id 42 --replace

Arrays move as bytes, so nothing is lost to a CSV round trip.";

pub const LIST: &str = "\
Examples:
  infrastore --store demo.h5 list
  infrastore --store demo.h5 list --name-glob 'load_*' --limit 20
  infrastore --store demo.h5 list --type SingleTimeSeries --resolution PT1H --wide
  infrastore --store demo.h5 -f json list --feature model_year=2030";

pub const GET: &str = "\
Examples:
  infrastore --store demo.h5 get --owner-id 42 --name load
  infrastore --store demo.h5 get --owner-id 42 --name load --plot
  infrastore --store demo.h5 get --name load --tail --limit 24
  infrastore --store demo.h5 get --name load --stride 24 --full
  infrastore --store demo.h5 get --name load_forecast --type Deterministic --window 0
  infrastore --store demo.h5 -f csv get --name load \\
      --time-range 2024-01-01T00:00:00Z..2024-01-01T06:00:00Z";

pub const GRID: &str = "\
Examples:
  infrastore --store demo.h5 grid --name max_active_power --resolution PT1H
  infrastore --store demo.h5 -f csv grid --name-glob 'load_*' --resolution PT1H \\
      --time-range 2024-01-01T00:00:00Z..2024-02-01T00:00:00Z
  infrastore --store demo.h5 grid --type NonSequentialTimeSeries --label full

Every column shares one timeline, so SingleTimeSeries needs --resolution.
The CSV it writes is the wide form `add` reads back.";

pub const PLOT: &str = "\
Examples:
  infrastore --store demo.h5 plot --name load --out load.svg
  infrastore --store demo.h5 plot --name load --kind duration --out ldc.html
  infrastore --store demo.h5 plot --name load --kind heatmap --out heat.svg
  infrastore --store demo.h5 plot --name load_prob --type Probabilistic \\
      --kind fan --window 0 --out fan.svg
  infrastore --store demo.h5 plot --name load --type Deterministic \\
      --kind overlay --out forecast.svg

The output is one self-contained file, light and dark, with no external assets.";

pub const INFO: &str = "\
Examples:
  infrastore --store demo.h5 info --owner-id 42 --name load
  infrastore --store demo.h5 info --name load --no-stats
  infrastore --store demo.h5 -f json info --name load --type DeterministicSingleTimeSeries";

pub const EXPORT: &str = "\
Examples:
  infrastore --store demo.h5 -f csv export --owner-id 42 --name load
  infrastore --store demo.h5 -f csv export --name-glob 'load_*' --dir out/
  infrastore --store demo.h5 -f json export --dir out/";

pub const TRANSFORM: &str = "\
Examples:
  infrastore --store demo.h5 transform --horizon PT24H --interval PT1H
  infrastore --store demo.h5 transform --horizon PT3H --interval PT1H --resolution PT1H

The horizon must fit inside every matched series, so scope with --resolution
or --owner-category when one short series would otherwise fail the run.";

pub const REMOVE: &str = "\
Examples:
  infrastore --store demo.h5 remove --owner-id 42 --name load --type SingleTimeSeries
  infrastore --store demo.h5 remove --all --name-glob 'scratch_*' --dry-run
  infrastore --store demo.h5 remove --all --owner-id 42 --force";

pub const RENAME: &str = "\
Examples:
  infrastore --store demo.h5 rename --owner-id 42 --name load --new-name demand
  infrastore --store demo.h5 rename --name load --type SingleTimeSeries \\
      --new-name demand --dry-run";

pub const COPY: &str = "\
Examples:
  infrastore --store demo.h5 copy --owner-id 42 --name load \\
      --dst-owner-id 43 --dst-owner-type Generator
  infrastore --store demo.h5 copy --name load --dst-owner-id 43 \\
      --dst-owner-type Generator --new-name load_backup --dry-run";

pub const REPLACE_OWNER: &str = "\
Examples:
  infrastore --store demo.h5 replace-owner --old 42 --new 43 --owner-category Component
  infrastore --store demo.h5 replace-owner --old 42 --new 43 \\
      --owner-category SupplementalAttribute --dry-run";

pub const CLEAR: &str = "\
Examples:
  infrastore --store demo.h5 clear --dry-run
  infrastore --store demo.h5 clear --owner-id 42 --owner-category Component
  infrastore --store demo.h5 clear --force

--owner-id and --owner-category go together; with neither, clear empties the
whole store.";

pub const PERSIST: &str = "\
Examples:
  infrastore --store demo.h5 persist --dest backup.h5
  infrastore --store demo.h5 persist --dest backup.h5 --dry-run
  infrastore --store demo.h5 persist --dest backup.h5 --force

Writes both halves of the artifact: backup.h5 and backup.h5.sqlite. An existing
destination needs --force (or -y): a save that fails partway can leave neither
the old nor the new pair on disk.";

pub const NAMES: &str = "\
Examples:
  infrastore --store demo.h5 names
  infrastore --store demo.h5 names --owner-id 42
  infrastore --store demo.h5 -f csv names --type SingleTimeSeries";

pub const OWNER_TYPES: &str = "\
Examples:
  infrastore --store demo.h5 owner-types
  infrastore --store demo.h5 owner-types --name load";

pub const OWNERS: &str = "\
Examples:
  infrastore --store demo.h5 owners
  infrastore --store demo.h5 owners --type SingleTimeSeries --resolution PT1H
  infrastore --store demo.h5 owners --owner-category SupplementalAttribute

Takes only --owner-category, --type, and --resolution; use `list` for the rest.";

pub const EXISTS: &str = "\
Examples:
  infrastore --store demo.h5 exists --owner-id 42 --name load
  infrastore --store demo.h5 exists --name-glob 'load_*'

Exits 0 when something matches and 1 when nothing does, so it drops into
`if infrastore --store demo.h5 exists --name load; then ...`.";

pub const DIFF: &str = "\
Examples:
  infrastore --store demo.h5 diff --against baseline.h5
  infrastore --store demo.h5 diff --against baseline.h5 --name-glob 'load_*'
  infrastore --store demo.h5 -f json diff --against baseline.h5 --all

Compares catalog identities and content hashes; no arrays are read. Exits 1
when the two stores differ, so it drops straight into a CI gate.";

pub const ATTACH: &str = "\
Examples:
  infrastore --store demo.h5 attach --component-id 42 --component-type Generator \\
      --attribute-id 7 --attribute-type GeographicInfo
  infrastore --store demo.h5 attach --from attachments.csv --dry-run

--from reads a `component_id,component_type,attribute_id,attribute_type` CSV in
one all-or-nothing transaction.";

pub const DETACH: &str = "\
Examples:
  infrastore --store demo.h5 detach --component-id 42
  infrastore --store demo.h5 detach --attribute-type GeographicInfo --dry-run
  infrastore --store demo.h5 detach --all --force";

pub const LINK: &str = "\
Examples:
  infrastore --store demo.h5 link --parent-id 42 --parent-type Generator \\
      --child-id 7 --child-type Bus
  infrastore --store demo.h5 link --from topology.csv

--from reads a `parent_id,parent_type,child_id,child_type` CSV.";

pub const UNLINK: &str = "\
Examples:
  infrastore --store demo.h5 unlink --parent-id 42
  infrastore --store demo.h5 unlink --child-type Bus --dry-run
  infrastore --store demo.h5 unlink --all --force";

pub const REASSIGN: &str = "\
Examples:
  infrastore --store demo.h5 reassign --old 42 --new 43
  infrastore --store demo.h5 reassign --old 42 --new 43 --attributes --dry-run

With neither --attributes nor --links, both catalogs move. Time series follow
`infrastore replace-owner`.";

pub const COMPACT: &str = "\
Examples:
  infrastore --store demo.h5 compact
  infrastore --store demo.h5 -f json compact --force";

pub const STATS: &str = "\
Examples:
  infrastore --store demo.h5 stats
  infrastore --store demo.h5 -f json stats";

pub const STORE_INFO: &str = "\
Examples:
  infrastore --store demo.h5 store-info
  infrastore --store demo.h5 -f json store-info";

pub const UPGRADE: &str = "\
Examples:
  infrastore --store demo.h5 upgrade
  infrastore --store demo.h5 -f json upgrade

Needed only for a store written by an older infrastore. Every read command opens
the store read-only and so cannot upgrade it: such a store reports \"the store's
catalog is at revision N ... open the store once for writing\", and this command
is that open. It changes nothing else, and is a no-op on a store that is already
current -- safe to run unconditionally, including against any artifact the
read-only gRPC server is about to serve.";

pub const ARRAYS: &str = "\
Examples:
  infrastore --store demo.h5 arrays
  infrastore --store demo.h5 arrays --data-hash 2018057b
  infrastore --store demo.h5 -f json arrays --owner-id 42

--data-hash takes any prefix of a content hash, in either case.";

pub const SUMMARY: &str = "\
Examples:
  infrastore --store demo.h5 summary
  infrastore --store demo.h5 summary --static-only
  infrastore --store demo.h5 -f csv summary --forecast-only";

pub const RESOLUTIONS: &str = "\
Examples:
  infrastore --store demo.h5 resolutions
  infrastore --store demo.h5 -f json resolutions";

pub const PARAMS: &str = "\
Examples:
  infrastore --store demo.h5 params
  infrastore --store demo.h5 params --resolution PT1H --interval PT1H";

pub const ATTRIBUTES: &str = "\
Examples:
  infrastore --store demo.h5 attributes
  infrastore --store demo.h5 attributes --component-id 42
  infrastore --store demo.h5 attributes --summary
  infrastore --store demo.h5 -f json attributes --component-type Generator";

pub const LINKS: &str = "\
Examples:
  infrastore --store demo.h5 links
  infrastore --store demo.h5 links --parent-id 42
  infrastore --store demo.h5 -f json links --parent-type Bus --child-type Generator";

pub const VERIFY: &str = "\
Examples:
  infrastore --store demo.h5 verify
  infrastore --store demo.h5 -f json verify

Exits 1 when the report lists any error, so it drops straight into a script.";

pub const CHECK_CONSISTENCY: &str = "\
Examples:
  infrastore --store demo.h5 check-consistency
  infrastore --store demo.h5 check-consistency --resolution PT1H";

pub const TEMPLATE: &str = "\
Examples:
  infrastore template SingleTimeSeries
  infrastore template Deterministic > forecast.json
  infrastore template NonSequentialTimeSeries
  infrastore template PersistentTimeSeries

Needs no --store: it only prints a descriptor to edit.";

pub const COMPLETIONS: &str = "\
Examples:
  infrastore completions zsh > ~/.zfunc/_infrastore
  infrastore completions bash > /etc/bash_completion.d/infrastore
  infrastore completions fish > ~/.config/fish/completions/infrastore.fish";
