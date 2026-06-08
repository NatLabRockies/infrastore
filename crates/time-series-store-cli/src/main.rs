//! `tss` — a command-line tool for loading and inspecting a time-series-store
//! directly on disk (NetCDF + SQLite). Output conventions mirror the sibling
//! `torc` CLI: a global `-f/--format table|json|csv`.

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

/// Help styling matching the `../torc` CLI: green bold headers/usage, cyan literals.
const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().bold())
    .usage(AnsiColor::Green.on_default().bold())
    .literal(AnsiColor::Cyan.on_default().bold())
    .placeholder(AnsiColor::Cyan.on_default());

#[derive(Parser, Debug)]
#[command(
    name = "tss",
    version,
    about = "Load and inspect a time-series-store on disk",
    styles = HELP_STYLES
)]
struct Cli {
    /// Path to the NetCDF store file (.nc). The SQLite catalog is derived automatically.
    #[arg(long, global = true)]
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
    /// Remove a single time series.
    Remove {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        force: bool,
    },
    /// Derive DeterministicSingleTimeSeries from stored SingleTimeSeries.
    Transform {
        /// Forecast horizon, e.g. 24h.
        #[arg(long)]
        horizon: String,
        /// Forecast interval, e.g. 1h.
        #[arg(long)]
        interval: String,
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
        Commands::Add { descriptor, csv } => {
            commands::add::run(&require_store(cli)?, descriptor, csv.as_deref())
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
        Commands::Remove { selector, force } => {
            commands::manage::remove(&require_store(cli)?, selector, *force)
        }
        Commands::Transform { horizon, interval } => {
            commands::manage::transform(&require_store(cli)?, horizon, interval)
        }
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
