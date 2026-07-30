//! `infrastore` — a command-line tool for loading and inspecting a infrastore store
//! directly on disk (HDF5 + SQLite).

mod color;
mod commands;
mod csv_io;
mod descriptor;
mod fields;
mod output;
mod parse;
mod select;
mod store_access;

use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Styles};
use clap::{FromArgMatches, Parser, Subcommand};

use output::Format;
use select::SelectorArgs;

/// Help styling: green bold headers/usage, cyan literals.
const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().bold())
    .usage(AnsiColor::Green.on_default().bold())
    .literal(AnsiColor::Cyan.on_default().bold())
    .placeholder(AnsiColor::Cyan.on_default());

/// How `--help` groups the subcommands, in display order.
///
/// Twenty-five commands in one flat list is a wall of text that tells a reader
/// nothing about which ones belong together. clap renders every subcommand under
/// a single heading and has no per-subcommand grouping, so the top-level help
/// template below omits its `{subcommands}` block and substitutes the listing
/// [`grouped_command_help`] builds from this table.
///
/// Only the *display* is grouped. Every command keeps its flat name, so
/// `infrastore list` is still `infrastore list` — no script, documented example,
/// or shell completion changes meaning.
///
/// The names here are the membership contract; the one-line descriptions come
/// from each command's own `about`, so they cannot drift. A test asserts this
/// table lists exactly the commands clap knows about, which is what stops a
/// newly added command from silently vanishing out of the help.
const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    ("Read data", &["list", "get", "info", "export"]),
    (
        "Write data",
        &[
            "add",
            "transform",
            "remove",
            "rename",
            "copy",
            "replace-owner",
            "clear",
        ],
    ),
    (
        "Inspect the store",
        &[
            "stats",
            "store-info",
            "arrays",
            "summary",
            "resolutions",
            "params",
        ],
    ),
    ("Associations", &["attributes", "links"]),
    (
        "Integrity & maintenance",
        &["verify", "check-consistency", "compact", "persist"],
    ),
    ("Scaffolding", &["template", "completions", "help"]),
];

/// clap adds `help` during its own build, after [`Cli::command`] hands us the
/// command, so its description is not available to look up like the rest.
const HELP_SUBCOMMAND_ABOUT: &str = "Print this message or the help of the given subcommand(s)";

/// Render [`COMMAND_GROUPS`] as the block that replaces clap's `Commands:`
/// section.
///
/// Descriptions are pulled from `cmd` rather than restated here. The
/// description column is aligned across *all* groups, not per group, so the
/// listing reads as one table with headings rather than several ragged ones.
fn grouped_command_help(cmd: &clap::Command) -> String {
    let about_of = |name: &str| -> String {
        if name == "help" {
            return HELP_SUBCOMMAND_ABOUT.to_string();
        }
        cmd.get_subcommands()
            .find(|s| s.get_name() == name)
            .and_then(|s| s.get_about())
            // Only the first line: several commands carry a long `about` whose
            // later paragraphs belong in `<cmd> --help`, not the index.
            .map(|a| a.to_string().lines().next().unwrap_or_default().to_string())
            .unwrap_or_default()
    };

    let width = COMMAND_GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter())
        .map(|n| n.len())
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for (i, (title, names)) in COMMAND_GROUPS.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&color::header(&format!("{title}:")));
        out.push('\n');
        for name in *names {
            let about = about_of(name);
            // Pad on the untinted name, then colorize: ANSI escapes have no
            // display width but would otherwise be counted by `{:width$}`.
            let padded = format!("{name:width$}");
            out.push_str(&format!("  {}  {about}\n", color::literal(&padded)));
        }
    }
    out
}

/// The root command with its subcommand listing replaced by the grouped one.
///
/// Set on the root only. Subcommands keep clap's default template, so
/// `infrastore list --help` is unaffected.
fn build_command() -> clap::Command {
    use clap::CommandFactory;
    let cmd = Cli::command();
    let groups = grouped_command_help(&cmd);
    debug_assert!(
        !groups.contains('{') && !groups.contains('}'),
        "a command description containing braces would be parsed as a help-template \
         placeholder; see the `command_descriptions_are_template_safe` test"
    );
    // `{options}` emits the option list without its heading — clap writes that
    // only from `{all-args}`, which would drag the ungrouped subcommand block
    // back in — so the heading is supplied here, styled to match the group ones.
    let options_heading = color::header("Options:");
    let template = format!(
        "{{before-help}}{{about-with-newline}}\n\
         {{usage-heading}} {{usage}}\n\n\
         {groups}\n\
         {options_heading}\n\
         {{options}}{{after-help}}"
    );
    cmd.help_template(template)
}

#[derive(Parser, Debug)]
#[command(
    name = "infrastore",
    version,
    about = "Load and inspect an infrastore store on disk",
    styles = HELP_STYLES
)]
struct Cli {
    /// Path to the HDF5 store file (.h5). The SQLite catalog is derived
    /// automatically. Falls back to the INFRASTORE_STORE environment variable.
    #[arg(long, global = true, env = "INFRASTORE_STORE")]
    store: Option<PathBuf>,

    /// Output format.
    #[arg(short = 'f', long, default_value_t = Format::Table, global = true)]
    format: Format,

    /// Log filter (also read from RUST_LOG); defaults to `warn`.
    #[arg(long, env = "RUST_LOG", global = true)]
    log_level: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Add time series from a descriptor JSON + CSV data.
    Add {
        /// Descriptor JSON describing the series (single object or array of objects).
        #[arg(long)]
        descriptor: PathBuf,
        /// Override the CSV path from the descriptor (single-series descriptors only).
        #[arg(long)]
        csv: Option<PathBuf>,
        /// Compression for a store created by this command: none, deflate, or
        /// deflate:LEVEL (0-9). Errors if the store already exists.
        #[arg(long)]
        compression: Option<String>,
        /// Disable byte-shuffle for deflate compression (only with --compression).
        #[arg(long)]
        no_shuffle: bool,
    },
    /// List stored time series matching the given filters.
    List {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Show at most N series (table/CSV output).
        #[arg(long)]
        limit: Option<usize>,
        /// Add the remaining metadata columns (timestamps, horizon, count,
        /// element shape, ext).
        #[arg(long)]
        wide: bool,
    },
    /// Read and display a single time series.
    Get {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Restrict to a half-open time range START..END (RFC3339 or epoch-ms).
        #[arg(long)]
        time_range: Option<String>,
        /// Max rows to show in table output (default 50).
        #[arg(long)]
        limit: Option<usize>,
        /// Show all rows in table output.
        #[arg(long)]
        full: bool,
    },
    /// Metadata, content hash, HDF5 location, and stats for one series.
    Info {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Skip the min/max/mean stats, which are the only part that reads the
        /// array itself.
        #[arg(long)]
        no_stats: bool,
    },
    /// Delete time series.
    ///
    /// A selector resolving to one series removes that one; with `--all` a
    /// selector may match several.
    Remove {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Remove every series matching the selector (may be more than one).
        #[arg(long)]
        all: bool,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        force: bool,
        /// Show what would be removed without changing the store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Derive DeterministicSingleTimeSeries from stored SingleTimeSeries.
    Transform {
        /// Forecast horizon, e.g. 24h.
        #[arg(long)]
        horizon: String,
        /// Forecast interval, e.g. 1h.
        #[arg(long)]
        interval: String,
        /// Restrict to one owner category (component|supplemental_attribute).
        #[arg(long)]
        owner_category: Option<String>,
        /// Restrict to one resolution, e.g. 1h.
        #[arg(long)]
        resolution: Option<String>,
    },
    /// Rename the single series a selector resolves to.
    Rename {
        #[command(flatten)]
        selector: SelectorArgs,
        /// The new name.
        #[arg(long)]
        new_name: String,
        /// Show what would be renamed without changing the store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Copy the single series a selector resolves to onto another owner.
    Copy {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Destination owner id.
        #[arg(long)]
        dst_owner_id: i64,
        /// Destination owner type.
        #[arg(long)]
        dst_owner_type: String,
        /// Optional new name for the copy (defaults to the source name).
        #[arg(long)]
        new_name: Option<String>,
        /// Show what would be copied without changing the store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Reassign every series from one owner to another.
    ReplaceOwner {
        #[arg(long)]
        old: i64,
        #[arg(long)]
        new: i64,
        #[arg(long)]
        owner_category: String,
        /// Show how many series would move without changing the store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove all series, or all for one owner.
    Clear {
        #[arg(long)]
        owner_id: Option<i64>,
        #[arg(long)]
        owner_category: Option<String>,
        #[arg(long)]
        force: bool,
        /// Show how many series would be cleared without changing the store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Write the store to a new HDF5 + SQLite artifact.
    Persist {
        /// Destination `.h5` path.
        #[arg(long)]
        dest: PathBuf,
    },
    /// Reclaim reusable space; print the compaction report.
    Compact {
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        force: bool,
    },
    /// Write series values to CSV or JSON files.
    ///
    /// The read-direction inverse of `add`: one file per matched series into
    /// --dir, or stdout when the selector matches exactly one series. The CSV it
    /// writes is re-readable by `add`, which detects the layout from the header.
    Export {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Directory to write one file per matched series.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Generate shell completions to stdout.
    ///
    /// For example `infrastore completions zsh`.
    Completions {
        /// Shell to generate for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Association, owner, and distinct-array counts.
    ///
    /// Association counts are catalog rows; array counts are distinct content
    /// hashes. Content addressing makes the two diverge, so they are namespaced
    /// (`associations.*`, `owners.*`, `arrays.*`) rather than listed flat.
    Stats,
    /// HDF5 + SQLite paths, on-disk format version, and compression.
    StoreInfo,
    /// Distinct stored arrays: content hash, HDF5 location, and sharers.
    Arrays {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Restrict to arrays whose content hash starts with this hex prefix.
        #[arg(long, value_name = "HEX")]
        data_hash: Option<String>,
    },
    /// Component <-> supplemental-attribute associations.
    Attributes {
        #[arg(long)]
        component_id: Option<i64>,
        #[arg(long)]
        attribute_id: Option<i64>,
        #[arg(long)]
        component_type: Option<String>,
        #[arg(long)]
        attribute_type: Option<String>,
        /// Show counts grouped by (component type, attribute type) instead of
        /// individual rows.
        #[arg(long)]
        summary: bool,
    },
    /// Directed parent -> child component associations.
    Links {
        #[arg(long)]
        parent_id: Option<i64>,
        #[arg(long)]
        child_id: Option<i64>,
        #[arg(long)]
        parent_type: Option<String>,
        #[arg(long)]
        child_type: Option<String>,
    },
    /// Grouped static and/or forecast summaries.
    Summary {
        #[arg(long)]
        static_only: bool,
        #[arg(long)]
        forecast_only: bool,
    },
    /// Verify stored array integrity (nonzero exit on errors).
    ///
    /// Recomputes each stored array's content hash and reports the ones that
    /// disagree with the hash recorded alongside them. This checks the HDF5
    /// half of the store only: the SQLite catalog is not inspected, so a clean
    /// report does not mean the store as a whole is sound. A catalog that is
    /// corrupted, truncated, or paired with the wrong .h5 file still verifies
    /// clean here, while every read of the affected series fails.
    ///
    /// For catalog-side checks use `infrastore check-consistency` (per-resolution grid
    /// agreement) and `infrastore compact` (which reports the unreachable arrays and
    /// feature sets a delete left behind — an expected state, not corruption).
    Verify,
    /// Verify per-resolution static grid consistency.
    CheckConsistency {
        #[arg(long)]
        resolution: Option<String>,
    },
    /// List distinct resolutions and forecast intervals.
    Resolutions,
    /// Show the store's forecast parameters.
    Params {
        #[arg(long)]
        resolution: Option<String>,
        #[arg(long)]
        interval: Option<String>,
    },
    /// Print an example descriptor JSON for a time-series type.
    Template {
        /// single|non_sequential|deterministic|probabilistic|scenarios
        #[arg(value_name = "TYPE")]
        ts_type: String,
    },
}

fn main() {
    // Parsed through `build_command` rather than `Cli::parse` so the grouped
    // help template is the one a user actually sees.
    let matches = build_command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    init_tracing(cli.log_level.as_deref());
    if let Err(e) = run(&cli) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    match &cli.command {
        Commands::Add {
            descriptor,
            csv,
            compression,
            no_shuffle,
        } => {
            let compression = compression
                .as_deref()
                .map(|spec| parse::parse_compression(spec, !no_shuffle))
                .transpose()?;
            commands::add::run(
                &require_store(cli)?,
                descriptor,
                csv.as_deref(),
                compression,
            )
        }
        Commands::List {
            selector,
            limit,
            wide,
        } => commands::show::list(&require_store(cli)?, selector, *limit, *wide, cli.format),
        Commands::Get {
            selector,
            time_range,
            limit,
            full,
        } => commands::show::get(
            &require_store(cli)?,
            selector,
            time_range.as_deref(),
            *limit,
            *full,
            cli.format,
        ),
        Commands::Info { selector, no_stats } => {
            commands::show::info(&require_store(cli)?, selector, *no_stats, cli.format)
        }
        Commands::Remove {
            selector,
            all,
            force,
            dry_run,
        } => {
            let store = require_store(cli)?;
            if *all {
                commands::manage::remove_all(&store, selector, *force, *dry_run)
            } else {
                commands::manage::remove(&store, selector, *force, *dry_run)
            }
        }
        Commands::Transform {
            horizon,
            interval,
            owner_category,
            resolution,
        } => commands::manage::transform(
            &require_store(cli)?,
            horizon,
            interval,
            owner_category.as_deref(),
            resolution.as_deref(),
        ),
        Commands::Rename {
            selector,
            new_name,
            dry_run,
        } => commands::manage::rename(&require_store(cli)?, selector, new_name, *dry_run),
        Commands::Copy {
            selector,
            dst_owner_id,
            dst_owner_type,
            new_name,
            dry_run,
        } => commands::manage::copy(
            &require_store(cli)?,
            selector,
            *dst_owner_id,
            dst_owner_type,
            new_name.as_deref(),
            *dry_run,
        ),
        Commands::ReplaceOwner {
            old,
            new,
            owner_category,
            dry_run,
        } => commands::manage::replace_owner(
            &require_store(cli)?,
            *old,
            *new,
            owner_category,
            *dry_run,
        ),
        Commands::Clear {
            owner_id,
            owner_category,
            force,
            dry_run,
        } => commands::manage::clear(
            &require_store(cli)?,
            *owner_id,
            owner_category.as_deref(),
            *force,
            *dry_run,
        ),
        Commands::Persist { dest } => commands::manage::persist(&require_store(cli)?, dest),
        Commands::Compact { force } => {
            commands::manage::compact(&require_store(cli)?, *force, cli.format)
        }
        Commands::Export { selector, dir } => {
            commands::export::run(&require_store(cli)?, selector, dir.as_deref(), cli.format)
        }
        Commands::Completions { shell } => {
            // The same command the binary parses with, so completions can never
            // describe a different set of subcommands than `--help` does.
            clap_complete::generate(
                *shell,
                &mut build_command(),
                "infrastore",
                &mut std::io::stdout(),
            );
            Ok(())
        }
        Commands::Stats => commands::admin::stats(&require_store(cli)?, cli.format),
        Commands::StoreInfo => commands::admin::store_info(&require_store(cli)?, cli.format),
        Commands::Arrays {
            selector,
            data_hash,
        } => commands::admin::arrays(
            &require_store(cli)?,
            selector,
            data_hash.as_deref(),
            cli.format,
        ),
        Commands::Attributes {
            component_id,
            attribute_id,
            component_type,
            attribute_type,
            summary,
        } => commands::assoc::attributes(
            &require_store(cli)?,
            *component_id,
            *attribute_id,
            component_type.as_deref(),
            attribute_type.as_deref(),
            *summary,
            cli.format,
        ),
        Commands::Links {
            parent_id,
            child_id,
            parent_type,
            child_type,
        } => commands::assoc::links(
            &require_store(cli)?,
            *parent_id,
            *child_id,
            parent_type.as_deref(),
            child_type.as_deref(),
            cli.format,
        ),
        Commands::Summary {
            static_only,
            forecast_only,
        } => commands::admin::summary(
            &require_store(cli)?,
            *static_only,
            *forecast_only,
            cli.format,
        ),
        Commands::Verify => commands::admin::verify(&require_store(cli)?, cli.format),
        Commands::CheckConsistency { resolution } => commands::admin::check_consistency(
            &require_store(cli)?,
            resolution.as_deref(),
            cli.format,
        ),
        Commands::Resolutions => commands::admin::resolutions(&require_store(cli)?, cli.format),
        Commands::Params {
            resolution,
            interval,
        } => commands::admin::params(
            &require_store(cli)?,
            resolution.as_deref(),
            interval.as_deref(),
            cli.format,
        ),
        Commands::Template { ts_type } => commands::manage::template(ts_type),
    }
}

fn require_store(cli: &Cli) -> Result<PathBuf, String> {
    cli.store
        .clone()
        .ok_or_else(|| "missing --store <path.h5>".to_string())
}

fn init_tracing(level: Option<&str>) {
    use tracing_subscriber::EnvFilter;
    let filter = match level {
        Some(l) => EnvFilter::new(l),
        None => EnvFilter::new("warn"),
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::collections::BTreeSet;

    /// Every command name listed across [`COMMAND_GROUPS`].
    fn grouped_names() -> Vec<&'static str> {
        COMMAND_GROUPS
            .iter()
            .flat_map(|(_, names)| names.iter().copied())
            .collect()
    }

    /// The command names clap actually defines. `help` is added during clap's
    /// own build rather than by the derive, so it is folded in here.
    fn clap_names() -> BTreeSet<String> {
        let mut names: BTreeSet<String> = Cli::command()
            .get_subcommands()
            .map(|s| s.get_name().to_string())
            .collect();
        names.insert("help".to_string());
        names
    }

    /// The guard that keeps the grouped help honest: a command added to
    /// `Commands` without a home in `COMMAND_GROUPS` would simply not appear in
    /// `--help`, which is worse than the flat list this replaced.
    #[test]
    fn every_command_appears_in_exactly_one_group() {
        let grouped = grouped_names();
        let listed: BTreeSet<String> = grouped.iter().map(|s| (*s).to_string()).collect();

        assert_eq!(
            listed.len(),
            grouped.len(),
            "a command is listed in more than one group: {grouped:?}"
        );

        let actual = clap_names();
        let missing: Vec<_> = actual.difference(&listed).collect();
        assert!(
            missing.is_empty(),
            "these commands exist but are in no group, so `--help` hides them: {missing:?}"
        );
        let unknown: Vec<_> = listed.difference(&actual).collect();
        assert!(
            unknown.is_empty(),
            "these names are grouped but are not commands (renamed? removed?): {unknown:?}"
        );
    }

    /// The generated block is spliced into a clap help *template*, where `{` and
    /// `}` delimit placeholders. A brace in any `about` would be silently
    /// swallowed or mangled.
    #[test]
    fn command_descriptions_are_template_safe() {
        let rendered = grouped_command_help(&Cli::command());
        assert!(
            !rendered.contains('{') && !rendered.contains('}'),
            "a command description contains a brace, which the help template would \
             read as a placeholder:\n{rendered}"
        );
    }

    /// The index is a scanning aid, so its lines have to stay scannable. This is
    /// what stops a long `about` first line — the sort that belongs in
    /// `long_about` — from stretching the listing off the side of a terminal.
    #[test]
    fn the_grouped_listing_stays_within_a_readable_width() {
        const MAX: usize = 100;
        // Rendered without color so the assertion measures display width, not
        // ANSI escapes; `color::enabled()` is already false under `cargo test`
        // because stdout is not a terminal, but the intent is worth stating.
        let rendered = grouped_command_help(&Cli::command());
        let long: Vec<&str> = rendered
            .lines()
            .filter(|l| l.chars().count() > MAX)
            .collect();
        assert!(
            long.is_empty(),
            "these help lines exceed {MAX} columns; shorten the command's first \
             doc-comment line and move the detail to a following paragraph, which \
             clap renders as long help:\n{long:#?}"
        );
    }

    /// Group headings are the whole point of the exercise; a template that lost
    /// them would still render a valid-looking command list.
    #[test]
    fn the_help_template_renders_every_group_heading() {
        let mut buf = Vec::new();
        build_command().write_help(&mut buf).expect("help renders");
        let help = String::from_utf8(buf).expect("utf8 help");

        for (title, names) in COMMAND_GROUPS {
            assert!(
                help.contains(&format!("{title}:")),
                "group heading {title:?} missing from --help:\n{help}"
            );
            for name in *names {
                assert!(
                    help.contains(*name),
                    "command {name:?} missing from --help:\n{help}"
                );
            }
        }
        assert!(
            help.contains("Options:"),
            "the options heading is written by the template, not by clap:\n{help}"
        );
    }
}
