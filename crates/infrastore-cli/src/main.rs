//! `infrastore` — a command-line tool for loading and inspecting a infrastore store
//! directly on disk (NetCDF + SQLite).

mod color;
mod commands;
mod csv_io;
mod descriptor;
mod output;
mod parse;
mod select;
mod store_access;

use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Styles};
use clap::{Parser, Subcommand};

use output::Format;
use select::SelectorArgs;

/// Help styling: green bold headers/usage, cyan literals.
const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().bold())
    .usage(AnsiColor::Green.on_default().bold())
    .literal(AnsiColor::Cyan.on_default().bold())
    .placeholder(AnsiColor::Cyan.on_default());

#[derive(Parser, Debug)]
#[command(
    name = "infrastore",
    version,
    about = "Load and inspect an infrastore store on disk",
    styles = HELP_STYLES
)]
struct Cli {
    /// Path to the NetCDF store file (.nc). The SQLite catalog is derived
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
    /// Add one or more time series from a descriptor JSON + CSV data.
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
    List(SelectorArgs),
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
    /// Show metadata and numeric stats for a single time series.
    Info {
        #[command(flatten)]
        selector: SelectorArgs,
    },
    /// Remove time series. A selector resolving to one series removes that one;
    /// with `--all` a selector may match several.
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
    /// Persist the store to a new NetCDF + SQLite artifact.
    Persist {
        /// Destination `.nc` path.
        #[arg(long)]
        dest: PathBuf,
    },
    /// Reclaim reusable space and print the compaction report.
    Compact {
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        force: bool,
    },
    /// Export series values to CSV or JSON files (the read-direction inverse
    /// of `add`). One file per matched series into --dir, or stdout when the
    /// selector matches exactly one series.
    Export {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Directory to write one file per matched series.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Generate shell completions to stdout (e.g. `infrastore completions zsh`).
    Completions {
        /// Shell to generate for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Overall counts, detailed counts, per-type counts, distinct arrays.
    Stats,
    /// Grouped static and/or forecast summaries.
    Summary {
        #[arg(long)]
        static_only: bool,
        #[arg(long)]
        forecast_only: bool,
    },
    /// Verify stored array integrity; nonzero exit if errors are present.
    ///
    /// Recomputes each stored array's content hash and reports the ones that
    /// disagree with the hash recorded alongside them. This checks the NetCDF
    /// half of the store only: the SQLite catalog is not inspected, so a clean
    /// report does not mean the store as a whole is sound. A catalog that is
    /// corrupted, truncated, or paired with the wrong .nc file still verifies
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
    let cli = Cli::parse();
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
        Commands::List(selector) => {
            commands::show::list(&require_store(cli)?, selector, cli.format)
        }
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
        Commands::Info { selector } => {
            commands::show::info(&require_store(cli)?, selector, cli.format)
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
            use clap::CommandFactory;
            clap_complete::generate(
                *shell,
                &mut Cli::command(),
                "infrastore",
                &mut std::io::stdout(),
            );
            Ok(())
        }
        Commands::Stats => commands::admin::stats(&require_store(cli)?, cli.format),
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
        .ok_or_else(|| "missing --store <path.nc>".to_string())
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
