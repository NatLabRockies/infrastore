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
  infrastore --store demo.h5 add --descriptor load.json --csv other.csv
  infrastore --store demo.h5 add --descriptor batch.json --compression deflate:6

A descriptor may hold one object or an array of them (one transaction).
`infrastore template <TYPE>` prints a starting point.";

pub const LIST: &str = "\
Examples:
  infrastore --store demo.h5 list
  infrastore --store demo.h5 list --name-glob 'load_*' --limit 20
  infrastore --store demo.h5 list --type SingleTimeSeries --resolution PT1H --wide
  infrastore --store demo.h5 -f json list --feature model_year=2030";

pub const GET: &str = "\
Examples:
  infrastore --store demo.h5 get --owner-id 42 --name load
  infrastore --store demo.h5 get --owner-id 42 --name load --full
  infrastore --store demo.h5 -f csv get --name load \\
      --time-range 2024-01-01T00:00:00Z..2024-01-01T06:00:00Z";

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

Writes both halves of the artifact: backup.h5 and backup.h5.sqlite.";

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

Needs no --store: it only prints a descriptor to edit.";

pub const COMPLETIONS: &str = "\
Examples:
  infrastore completions zsh > ~/.zfunc/_infrastore
  infrastore completions bash > /etc/bash_completion.d/infrastore
  infrastore completions fish > ~/.config/fish/completions/infrastore.fish";
