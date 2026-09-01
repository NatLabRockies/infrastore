//! `infrastore` — a command-line tool for loading and inspecting a infrastore store
//! directly on disk (HDF5 + SQLite).

mod chart;
mod color;
mod commands;
mod confirm;
mod csv_io;
mod descriptor;
mod fields;
mod help;
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
    ("Read data", &["list", "get", "grid", "info", "export"]),
    (
        "Write data",
        &[
            "init",
            "add",
            "merge",
            "transform",
            "remove",
            "rename",
            "copy",
            "replace-owner",
            "clear",
        ],
    ),
    ("Discover", &["names", "owner-types", "owners", "exists"]),
    ("Visualize", &["plot"]),
    (
        "Inspect the store",
        &[
            "stats",
            "store-info",
            "upgrade",
            "arrays",
            "summary",
            "resolutions",
            "params",
        ],
    ),
    (
        "Associations",
        &[
            "attributes",
            "links",
            "attach",
            "detach",
            "link",
            "unlink",
            "reassign",
        ],
    ),
    (
        "Integrity & maintenance",
        &["verify", "check-consistency", "compact", "persist", "diff"],
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
    after_help = help::ROOT,
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

    /// Answer every confirmation prompt with yes.
    ///
    /// The scriptable counterpart to the per-command --force flags, so a
    /// script need not know which commands prompt.
    #[arg(short = 'y', long, global = true)]
    yes: bool,

    /// Read timestamps that carry no time zone as being in this one: UTC, a
    /// fixed offset like -07:00, or an IANA zone name like America/Denver.
    ///
    /// Without it (or --zoneless) a zoneless timestamp is an error, because it
    /// names no instant — which leaves the most ordinary CSV in this domain
    /// (`2024-01-01 00:00:00,...`) unloadable without rewriting the file. A
    /// timestamp that carries its own offset is never overridden.
    ///
    /// Prefer a named zone over a fixed offset for data that crosses a daylight
    /// saving transition: a year of Denver data read as -07:00 renders every
    /// timestamp after March an hour wrong, while America/Denver renders all of
    /// them correctly. A row whose wall clock daylight saving skips or repeats
    /// is refused by name rather than resolved to a guess.
    // `allow_hyphen_values` so a western offset can be written the obvious way,
    // `--assume-timezone -07:00`, rather than only as `--assume-timezone=-07:00`.
    #[arg(long, value_name = "ZONE", global = true, allow_hyphen_values = true)]
    assume_timezone: Option<String>,

    /// Store timestamps that carry no time zone as the wall clocks they are,
    /// naming no instant.
    ///
    /// The alternative to --assume-timezone, for data that has no time zone and
    /// wants none — modeled profiles on 24-hour days, say. Nothing is converted:
    /// the fields are stored as written and read back unlabelled, and the series
    /// records `time_reference = zoneless`.
    ///
    /// The store then refuses to answer an instant-bearing query bound against
    /// such a series, and refuses to put it in one reader or one ranged bulk
    /// read alongside series that do record instants — there is no single
    /// meaning a bound or a shared timestamp axis could carry for both.
    #[arg(long, global = true, conflicts_with = "assume_timezone")]
    zoneless: bool,

    #[command(subcommand)]
    command: Commands,
}

// The variants differ a lot in size (an inline `add` carries twenty-odd fields;
// `stats` carries none). This is a parsed command line, constructed once per
// process and matched immediately, so boxing the large variants would trade
// clarity for an allocation nobody measures.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
enum Commands {
    /// Add time series from a descriptor JSON + CSV data, or from flags.
    #[command(after_help = help::ADD)]
    Add {
        /// Descriptor JSON describing the series (single object or array of
        /// objects). `-` reads it from stdin.
        #[arg(long)]
        descriptor: Option<PathBuf>,
        /// CSV data path. With --descriptor it overrides the descriptor's own
        /// (single-series descriptors only); without one it starts an inline add.
        #[arg(long)]
        csv: Option<PathBuf>,
        #[command(flatten)]
        inline: commands::add::InlineArgs,
        /// Resolve every descriptor and print what would be written, without
        /// opening the store.
        #[arg(long)]
        dry_run: bool,
        /// Remove any series that already has one of these identities first.
        #[arg(long)]
        replace: bool,
        /// Commit every N series instead of the whole load in one transaction.
        #[arg(long, value_name = "N")]
        batch_size: Option<usize>,
        /// Print nothing but errors.
        #[arg(long, short = 'q')]
        quiet: bool,
        /// Compression for a store created by this command: none, deflate, or
        /// deflate:LEVEL (0-9). Errors if the store already exists.
        #[arg(long)]
        compression: Option<String>,
        /// Disable byte-shuffle for deflate compression (only with --compression).
        #[arg(long)]
        no_shuffle: bool,
        /// Where the SQLite catalog lives while the store is open.
        #[arg(long, value_name = "MODE", default_value_t = store_access::CatalogChoice::Attached)]
        catalog: store_access::CatalogChoice,
    },
    /// Create an empty store with an explicit compression and catalog policy.
    #[command(after_help = help::INIT)]
    Init {
        /// Compression: none, deflate, or deflate:LEVEL (0-9).
        #[arg(long)]
        compression: Option<String>,
        /// Disable byte-shuffle for deflate compression (only with --compression).
        #[arg(long)]
        no_shuffle: bool,
        /// Where the SQLite catalog lives while the store is open.
        #[arg(long, value_name = "MODE", default_value_t = store_access::CatalogChoice::Attached)]
        catalog: store_access::CatalogChoice,
    },
    /// Copy matching series from another store into this one.
    #[command(after_help = help::MERGE)]
    Merge {
        /// Source store (`.h5`) to read from.
        #[arg(long)]
        from: PathBuf,
        #[command(flatten)]
        selector: SelectorArgs,
        /// Replace a destination series that already has the same identity.
        #[arg(long)]
        replace: bool,
        /// Show what would be merged without changing either store.
        #[arg(long)]
        dry_run: bool,
    },
    /// List stored time series matching the given filters.
    #[command(after_help = help::LIST)]
    List {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Show at most N series (table/CSV output).
        #[arg(long)]
        limit: Option<usize>,
        /// Add the remaining metadata columns (timestamps, horizon, count,
        /// element shape, application_data).
        #[arg(long)]
        wide: bool,
    },
    /// Read and display a single time series.
    #[command(after_help = help::GET)]
    Get {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Restrict to a half-open time range START..END (RFC3339 or epoch-ms).
        #[arg(long)]
        time_range: Option<String>,
        /// Evaluate a PersistentTimeSeries at this instant; repeat for more.
        #[arg(long, value_name = "TIMESTAMP")]
        at: Vec<String>,
        /// Max rows to show in table output (default 50).
        #[arg(long)]
        limit: Option<usize>,
        /// Show all rows in table output.
        #[arg(long)]
        full: bool,
        /// Take the table's rows from the end of the series, not the start.
        #[arg(long)]
        tail: bool,
        /// Keep only every Nth row, in every format (applied before --limit).
        #[arg(long, value_name = "N")]
        stride: Option<usize>,
        /// Draw a terminal sparkline instead of the values.
        #[arg(long)]
        plot: bool,
        /// Sparkline width in characters (defaults to the terminal width).
        #[arg(long, value_name = "COLS")]
        plot_width: Option<usize>,
        /// Show only forecast window N.
        #[arg(long, value_name = "N")]
        window: Option<usize>,
        /// Show only the forecast window issued at this timestamp.
        #[arg(long, value_name = "TIMESTAMP")]
        issue_time: Option<String>,
    },
    /// Render N series as N columns against one shared time axis.
    #[command(after_help = help::GRID)]
    Grid {
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
        /// How to name the columns.
        #[arg(long, value_name = "MODE", default_value = "auto")]
        label: commands::grid::ColumnLabel,
    },
    /// Draw a chart to a self-contained SVG or HTML file.
    #[command(after_help = help::PLOT)]
    Plot {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Which view to draw.
        #[arg(long, default_value = "line")]
        kind: commands::plot::Kind,
        /// Destination file (.svg or .html); `-` writes to stdout.
        #[arg(long, default_value = "chart.svg")]
        out: PathBuf,
        /// Restrict to a half-open time range START..END (RFC3339 or epoch-ms).
        #[arg(long)]
        time_range: Option<String>,
        /// Chart title (defaults to the series name).
        #[arg(long)]
        title: Option<String>,
        #[arg(long, default_value_t = 960.0)]
        width: f64,
        #[arg(long, default_value_t = 440.0)]
        height: f64,
        /// First forecast window to draw (fan, overlay).
        #[arg(long, default_value_t = 0)]
        window: usize,
        /// How many forecast windows to overlay (overlay; default 8).
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// Metadata, content hash, HDF5 location, and stats for one series.
    #[command(after_help = help::INFO)]
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
    #[command(after_help = help::REMOVE)]
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
    #[command(after_help = help::TRANSFORM)]
    Transform {
        /// Forecast horizon as an ISO-8601 duration, e.g. PT24H.
        #[arg(long)]
        horizon: String,
        /// Forecast interval as an ISO-8601 duration, e.g. PT1H.
        #[arg(long)]
        interval: String,
        /// Restrict to one owner category (Component|SupplementalAttribute).
        #[arg(long)]
        owner_category: Option<String>,
        /// Restrict to one resolution, e.g. PT1H.
        #[arg(long)]
        resolution: Option<String>,
    },
    /// Rename the single series a selector resolves to.
    #[command(after_help = help::RENAME)]
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
    #[command(after_help = help::COPY)]
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
    #[command(after_help = help::REPLACE_OWNER)]
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
    #[command(after_help = help::CLEAR)]
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
    #[command(after_help = help::PERSIST)]
    Persist {
        /// Destination `.h5` path.
        #[arg(long)]
        dest: PathBuf,
        /// Overwrite an existing destination without confirming.
        #[arg(long)]
        force: bool,
        /// Show what would be written without writing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Reclaim space; print the compaction report.
    ///
    /// Rewrites the `.h5` file from the live set and replaces the original, so
    /// deleted data actually leaves the file. Nothing else may have the store
    /// open while this runs.
    #[command(after_help = help::COMPACT)]
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
    #[command(after_help = help::EXPORT)]
    Export {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Directory to write one file per matched series.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Restrict to a half-open time range START..END (RFC3339 or epoch-ms).
        #[arg(long)]
        time_range: Option<String>,
    },
    /// Generate shell completions to stdout.
    ///
    /// For example `infrastore completions zsh`.
    #[command(after_help = help::COMPLETIONS)]
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
    #[command(after_help = help::STATS)]
    Stats,
    /// HDF5 + SQLite paths, on-disk format version, catalog revision, and compression.
    #[command(after_help = help::STORE_INFO)]
    StoreInfo,
    /// Bring a store written by an older build up to this one's catalog revision.
    ///
    /// Every read command opens the store read-only and so cannot upgrade it;
    /// this is the writable open that runs the migration. It is a no-op on a
    /// store that is already current.
    #[command(after_help = help::UPGRADE)]
    Upgrade,
    /// Distinct stored arrays: content hash, HDF5 location, and sharers.
    #[command(after_help = help::ARRAYS)]
    Arrays {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Restrict to arrays whose content hash starts with this hex prefix.
        #[arg(long, value_name = "HEX")]
        data_hash: Option<String>,
    },
    /// Component <-> supplemental-attribute associations.
    #[command(after_help = help::ATTRIBUTES)]
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
    #[command(after_help = help::LINKS)]
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
    #[command(after_help = help::SUMMARY)]
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
    #[command(after_help = help::VERIFY)]
    Verify,
    /// Verify per-resolution static grid consistency.
    #[command(after_help = help::CHECK_CONSISTENCY)]
    CheckConsistency {
        #[arg(long)]
        resolution: Option<String>,
    },
    /// List distinct resolutions and forecast intervals.
    #[command(after_help = help::RESOLUTIONS)]
    Resolutions,
    /// Show the store's forecast parameters.
    #[command(after_help = help::PARAMS)]
    Params {
        #[arg(long)]
        resolution: Option<String>,
        #[arg(long)]
        interval: Option<String>,
    },
    /// Distinct series names matching the selector.
    #[command(after_help = help::NAMES)]
    Names {
        #[command(flatten)]
        selector: SelectorArgs,
    },
    /// Distinct owner types matching the selector.
    #[command(after_help = help::OWNER_TYPES)]
    OwnerTypes {
        #[command(flatten)]
        selector: SelectorArgs,
    },
    /// Distinct owner ids that have a time series.
    #[command(after_help = help::OWNERS)]
    Owners {
        #[command(flatten)]
        selector: SelectorArgs,
    },
    /// Whether anything matches the selector (exit 0 = yes, 1 = no).
    #[command(after_help = help::EXISTS)]
    Exists {
        #[command(flatten)]
        selector: SelectorArgs,
    },
    /// Compare this store against another at the catalog level.
    #[command(after_help = help::DIFF)]
    Diff {
        /// The other store (`.h5`) to compare against.
        #[arg(long)]
        against: PathBuf,
        #[command(flatten)]
        selector: SelectorArgs,
        /// List the identical series too, not just the differences.
        #[arg(long)]
        all: bool,
    },
    /// Attach supplemental attributes to components.
    #[command(after_help = help::ATTACH)]
    Attach {
        #[arg(long)]
        component_id: Option<i64>,
        #[arg(long)]
        component_type: Option<String>,
        #[arg(long)]
        attribute_id: Option<i64>,
        #[arg(long)]
        attribute_type: Option<String>,
        /// Bulk import from a
        /// `component_id,component_type,attribute_id,attribute_type` CSV.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Show what would be attached without changing the store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove supplemental-attribute attachments matching the filter.
    #[command(after_help = help::DETACH)]
    Detach {
        #[arg(long)]
        component_id: Option<i64>,
        #[arg(long)]
        component_type: Option<String>,
        #[arg(long)]
        attribute_id: Option<i64>,
        #[arg(long)]
        attribute_type: Option<String>,
        /// Remove every attachment (required when no filter is given).
        #[arg(long)]
        all: bool,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        force: bool,
        /// Show how many would be detached without changing the store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Add directed parent -> child component links.
    #[command(after_help = help::LINK)]
    Link {
        #[arg(long)]
        parent_id: Option<i64>,
        #[arg(long)]
        parent_type: Option<String>,
        #[arg(long)]
        child_id: Option<i64>,
        #[arg(long)]
        child_type: Option<String>,
        /// Bulk import from a `parent_id,parent_type,child_id,child_type` CSV.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Show what would be linked without changing the store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove parent -> child links matching the filter.
    #[command(after_help = help::UNLINK)]
    Unlink {
        #[arg(long)]
        parent_id: Option<i64>,
        #[arg(long)]
        parent_type: Option<String>,
        #[arg(long)]
        child_id: Option<i64>,
        #[arg(long)]
        child_type: Option<String>,
        /// Remove every link (required when no filter is given).
        #[arg(long)]
        all: bool,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        force: bool,
        /// Show how many would be removed without changing the store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Move a component's associations from one id to another.
    ///
    /// The association counterpart of `replace-owner`, which moves time series.
    #[command(after_help = help::REASSIGN)]
    Reassign {
        #[arg(long)]
        old: i64,
        #[arg(long)]
        new: i64,
        /// Move only the supplemental-attribute attachments.
        #[arg(long)]
        attributes: bool,
        /// Move only the parent/child links.
        #[arg(long)]
        links: bool,
        /// Show how many would move without changing the store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print an example descriptor JSON for a time-series type.
    ///
    /// Durations, `type`, and `owner_category` are printed in the same spelling
    /// the store renders them back in, so a generated descriptor lines up with
    /// `list` / `info` / `export -f json` output for the series it creates.
    #[command(after_help = help::TEMPLATE)]
    Template {
        /// SingleTimeSeries|NonSequentialTimeSeries|PersistentTimeSeries|Deterministic|
        /// Probabilistic|Scenarios
        #[arg(value_name = "TYPE")]
        ts_type: String,
    },
}

/// The stack `real_main` is given, which is more than the 1 MiB Windows hands
/// the main thread by default.
///
/// [`build_command`] assembles every subcommand and every one of their arguments
/// in a single clap-derive-generated call tree, and an unoptimized build gives
/// each of those temporaries its own stack slot instead of reusing one. Measured
/// against a `--help` run of the debug binary, that alone needs between 1 and
/// 2 MiB — so on Windows the CLI overflowed before it could parse anything, on
/// every invocation, while Linux and macOS (8 MiB) were fine. A spawned thread
/// takes its stack size from this constant rather than from the executable
/// header, which is why the work happens on one.
const MAIN_STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() {
    let worker = std::thread::Builder::new()
        .stack_size(MAIN_STACK_SIZE)
        .spawn(real_main)
        .expect("spawning the worker thread");
    if worker.join().is_err() {
        // The panic hook has already printed the message; match the exit code a
        // panicking main would have produced.
        std::process::exit(101);
    }
}

fn real_main() {
    // Parsed through `build_command` rather than `Cli::parse` so the grouped
    // help template is the one a user actually sees.
    let matches = build_command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    init_tracing(cli.log_level.as_deref());
    confirm::set_assume_yes(cli.yes);
    // Before anything parses a timestamp, so an unusable zone is reported now
    // rather than part-way through a CSV.
    if let Err(e) = parse::set_assumed_timezone(cli.assume_timezone.as_deref(), cli.zoneless) {
        output::print_error(cli.format, &e);
        std::process::exit(1);
    }
    if let Err(e) = run(&cli) {
        output::print_error(cli.format, &e);
        std::process::exit(1);
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    match &cli.command {
        Commands::Add {
            descriptor,
            csv,
            inline,
            dry_run,
            replace,
            batch_size,
            quiet,
            compression,
            no_shuffle,
            catalog,
        } => {
            let compression = compression
                .as_deref()
                .map(|spec| parse::parse_compression(spec, !no_shuffle))
                .transpose()?;
            commands::add::run(
                &require_store(cli)?,
                &commands::add::Options {
                    descriptor: descriptor.as_deref(),
                    csv: csv.as_deref(),
                    inline,
                    compression,
                    catalog: *catalog,
                    batch_size: *batch_size,
                    replace: *replace,
                    dry_run: *dry_run,
                    quiet: *quiet,
                    format: cli.format,
                },
            )
        }
        Commands::Init {
            compression,
            no_shuffle,
            catalog,
        } => {
            let compression = compression
                .as_deref()
                .map(|spec| parse::parse_compression(spec, !no_shuffle))
                .transpose()?;
            commands::manage::init(&require_store(cli)?, compression, *catalog, cli.format)
        }
        Commands::Merge {
            from,
            selector,
            replace,
            dry_run,
        } => commands::manage::merge(
            &require_store(cli)?,
            from,
            selector,
            *replace,
            *dry_run,
            cli.format,
        ),
        Commands::List {
            selector,
            limit,
            wide,
        } => commands::show::list(&require_store(cli)?, selector, *limit, *wide, cli.format),
        Commands::Get {
            selector,
            time_range,
            at,
            limit,
            full,
            tail,
            stride,
            plot,
            plot_width,
            window,
            issue_time,
        } => commands::show::get(
            &require_store(cli)?,
            selector,
            &commands::show::GetOptions {
                time_range: time_range.as_deref(),
                at,
                rows: commands::show::RowWindow {
                    limit: *limit,
                    full: *full,
                    tail: *tail,
                    stride: *stride,
                },
                plot: *plot,
                plot_width: *plot_width,
                window: *window,
                issue_time: issue_time.as_deref(),
            },
            cli.format,
        ),
        Commands::Grid {
            selector,
            time_range,
            limit,
            full,
            label,
        } => commands::grid::run(
            &require_store(cli)?,
            selector,
            time_range.as_deref(),
            *limit,
            *full,
            *label,
            cli.format,
        ),
        Commands::Plot {
            selector,
            kind,
            out,
            time_range,
            title,
            width,
            height,
            window,
            limit,
        } => commands::plot::run(
            &require_store(cli)?,
            selector,
            &commands::plot::Options {
                kind: *kind,
                out,
                time_range: time_range.as_deref(),
                title: title.as_deref(),
                width: *width,
                height: *height,
                window: *window,
                limit: *limit,
                format: cli.format,
            },
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
                commands::manage::remove_all(&store, selector, *force, *dry_run, cli.format)
            } else {
                commands::manage::remove(&store, selector, *force, *dry_run, cli.format)
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
            cli.format,
        ),
        Commands::Rename {
            selector,
            new_name,
            dry_run,
        } => commands::manage::rename(
            &require_store(cli)?,
            selector,
            new_name,
            *dry_run,
            cli.format,
        ),
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
            cli.format,
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
            cli.format,
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
            cli.format,
        ),
        Commands::Persist {
            dest,
            force,
            dry_run,
        } => commands::manage::persist(&require_store(cli)?, dest, *force, *dry_run, cli.format),
        Commands::Compact { force } => {
            commands::manage::compact(&require_store(cli)?, *force, cli.format)
        }
        Commands::Export {
            selector,
            dir,
            time_range,
        } => commands::export::run(
            &require_store(cli)?,
            selector,
            dir.as_deref(),
            time_range.as_deref(),
            cli.format,
        ),
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
        Commands::Upgrade => commands::admin::upgrade(&require_store(cli)?, cli.format),
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
        Commands::Names { selector } => {
            commands::discover::names(&require_store(cli)?, selector, cli.format)
        }
        Commands::OwnerTypes { selector } => {
            commands::discover::owner_types(&require_store(cli)?, selector, cli.format)
        }
        Commands::Owners { selector } => {
            commands::discover::owners(&require_store(cli)?, selector, cli.format)
        }
        Commands::Exists { selector } => {
            commands::discover::exists(&require_store(cli)?, selector, cli.format)
        }
        Commands::Diff {
            against,
            selector,
            all,
        } => commands::diff::run(&require_store(cli)?, against, selector, *all, cli.format),
        Commands::Attach {
            component_id,
            component_type,
            attribute_id,
            attribute_type,
            from,
            dry_run,
        } => commands::assoc::attach(
            &require_store(cli)?,
            &commands::assoc::AttachArgs {
                component_id: *component_id,
                component_type: component_type.as_deref(),
                attribute_id: *attribute_id,
                attribute_type: attribute_type.as_deref(),
                from: from.as_deref(),
                dry_run: *dry_run,
                format: cli.format,
            },
        ),
        Commands::Detach {
            component_id,
            component_type,
            attribute_id,
            attribute_type,
            all,
            force,
            dry_run,
        } => commands::assoc::detach(
            &require_store(cli)?,
            *component_id,
            *attribute_id,
            component_type.as_deref(),
            attribute_type.as_deref(),
            *all,
            *force,
            *dry_run,
            cli.format,
        ),
        Commands::Link {
            parent_id,
            parent_type,
            child_id,
            child_type,
            from,
            dry_run,
        } => commands::assoc::link(
            &require_store(cli)?,
            &commands::assoc::LinkArgs {
                parent_id: *parent_id,
                parent_type: parent_type.as_deref(),
                child_id: *child_id,
                child_type: child_type.as_deref(),
                from: from.as_deref(),
                dry_run: *dry_run,
                format: cli.format,
            },
        ),
        Commands::Unlink {
            parent_id,
            parent_type,
            child_id,
            child_type,
            all,
            force,
            dry_run,
        } => commands::assoc::unlink(
            &require_store(cli)?,
            *parent_id,
            *child_id,
            parent_type.as_deref(),
            child_type.as_deref(),
            *all,
            *force,
            *dry_run,
            cli.format,
        ),
        Commands::Reassign {
            old,
            new,
            attributes,
            links,
            dry_run,
        } => commands::assoc::reassign(
            &require_store(cli)?,
            *old,
            *new,
            *attributes,
            *links,
            *dry_run,
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

    /// The `infrastore ...` lines in an examples block, as argv vectors.
    ///
    /// Just enough shell to read what the blocks actually contain: a trailing
    /// `\` continues onto the next line, a ` #` comment and a `>` redirection
    /// are not part of the invocation, and `'load_*'` is one argument. Anything
    /// fancier does not belong in an example.
    fn example_invocations(block: &str) -> Vec<Vec<String>> {
        let mut unwrapped = String::new();
        for line in block.lines() {
            match line.trim().strip_suffix('\\') {
                Some(head) => {
                    unwrapped.push_str(head.trim_end());
                    unwrapped.push(' ');
                }
                None => {
                    unwrapped.push_str(line.trim());
                    unwrapped.push('\n');
                }
            }
        }
        unwrapped
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("infrastore "))
            .map(|l| {
                let l = l.split(" #").next().unwrap_or(l);
                l.split('>')
                    .next()
                    .unwrap_or(l)
                    .split_whitespace()
                    .map(|w| w.trim_matches('\'').to_string())
                    .collect()
            })
            .collect()
    }

    /// Every examples block, labelled by the command it belongs to. The root
    /// help is in here too: it is the first thing a new user sees, and the
    /// grouped listing above it names commands without showing one complete
    /// invocation.
    fn example_blocks() -> Vec<(String, String)> {
        let cmd = Cli::command();
        let mut out = vec![(
            "(root)".to_string(),
            cmd.get_after_help()
                .map(|s| s.to_string())
                .unwrap_or_default(),
        )];
        for sub in cmd.get_subcommands() {
            // clap writes `help` itself; it has no examples to give.
            if sub.get_name() == "help" {
                continue;
            }
            out.push((
                sub.get_name().to_string(),
                sub.get_after_help()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            ));
        }
        out
    }

    /// Every command must end its own `--help` with a worked example.
    ///
    /// The synopsis a command prints tells a reader what its flags are, not what
    /// the command is *for*, and the one whose help stops at the flag list is the
    /// one that gets guessed at. This is the guard that keeps a newly added
    /// command from shipping without one — the same role
    /// [`every_command_appears_in_exactly_one_group`] plays for the index.
    #[test]
    fn every_command_ends_its_help_with_an_example() {
        const MAX: usize = 100;
        for (name, block) in example_blocks() {
            assert!(
                block.starts_with("Examples:"),
                "`infrastore {name} --help` has no Examples: block; add one to src/help.rs"
            );

            // An example has to be an invocation of *this* command, not prose
            // that merely mentions it. (The root's examples run subcommands, so
            // it is exempt from the name check but not from the rest.)
            let invocations = example_invocations(&block);
            assert!(
                !invocations.is_empty(),
                "the {name} examples contain no `infrastore ...` line:\n{block}"
            );
            if name != "(root)" {
                assert!(
                    invocations.iter().any(|argv| argv.contains(&name)),
                    "no example in `infrastore {name} --help` actually runs {name}:\n{block}"
                );
            }

            // Same readable-width rule the grouped listing follows: help that
            // wraps mid-flag is worse than help split across two lines.
            let long: Vec<&str> = block.lines().filter(|l| l.chars().count() > MAX).collect();
            assert!(
                long.is_empty(),
                "these {name} help lines exceed {MAX} columns; wrap them with a trailing \
                 backslash:\n{long:#?}"
            );
        }
    }

    /// And every one of those examples has to parse.
    ///
    /// An example that names a flag the command dropped or renamed is worse than
    /// no example at all — it is documentation that confidently fails. Running
    /// them through the real parser means a flag cannot be renamed without the
    /// examples following it.
    #[test]
    fn every_example_parses_as_a_real_invocation() {
        for (name, block) in example_blocks() {
            for argv in example_invocations(&block) {
                if let Err(e) = build_command().try_get_matches_from(&argv) {
                    panic!(
                        "the {name} example `{}` does not parse:\n{e}",
                        argv.join(" ")
                    );
                }
            }
        }
    }

    /// The documentation pages whose `sh` blocks are real invocations.
    ///
    /// Relative to the crate, because the check is only meaningful run from a
    /// checkout that has the docs beside it.
    const DOC_PAGES: &[&str] = &[
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/src/reference/cli.md"
        ),
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/src/how-to/use-cli.md"
        ),
    ];

    /// The `infrastore ...` lines inside a page's ```sh fences.
    ///
    /// Only `sh` fences: the reference page also carries ```text blocks holding
    /// the synopsis grammar (`--store <PATH>`), which is not meant to parse.
    fn doc_examples(markdown: &str) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        let mut in_sh = false;
        for line in markdown.lines() {
            if line.trim_start().starts_with("```") {
                in_sh = line.trim() == "```sh";
                continue;
            }
            if in_sh {
                out.extend(example_invocations(line));
            }
        }
        out
    }

    /// The documented examples have to parse too, and between them they have to
    /// cover every command.
    ///
    /// The reference page is where someone looks before reaching for `--help`,
    /// so an example there that names a renamed flag misleads exactly the reader
    /// who trusted the docs over the binary. Running them through the real
    /// parser keeps the two honest about each other.
    #[test]
    fn every_command_is_shown_in_the_docs_and_every_doc_example_parses() {
        let mut covered: BTreeSet<String> = BTreeSet::new();
        for page in DOC_PAGES {
            let markdown = match std::fs::read_to_string(page) {
                Ok(t) => t,
                Err(e) => panic!("cannot read {page}: {e}"),
            };
            let examples = doc_examples(&markdown);
            assert!(
                !examples.is_empty(),
                "{page} has no `infrastore ...` examples in its sh blocks"
            );
            for argv in examples {
                if let Err(e) = build_command().try_get_matches_from(&argv) {
                    panic!(
                        "the example `{}` in {page} does not parse:\n{e}",
                        argv.join(" ")
                    );
                }
                covered.extend(argv.into_iter().skip(1));
            }
        }

        let documented: BTreeSet<String> = clap_names()
            .into_iter()
            .filter(|n| n != "help" && !covered.contains(n))
            .collect();
        assert!(
            documented.is_empty(),
            "these commands have no example in {DOC_PAGES:?}: {documented:?}"
        );
    }

    /// The root help must send a reader on to the per-command examples; without
    /// that line they are easy to never discover.
    #[test]
    fn the_root_help_points_at_the_per_command_examples() {
        let mut buf = Vec::new();
        build_command().write_help(&mut buf).expect("help renders");
        let rendered = String::from_utf8(buf).expect("utf8 help");
        assert!(
            rendered.contains("Examples:"),
            "the root --help has no examples:\n{rendered}"
        );
        assert!(
            rendered.contains("<command> --help"),
            "the root --help must point at the per-command examples:\n{rendered}"
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
