//! The `add` command: load one or more series from a descriptor JSON + CSV, or
//! from flags for a one-off.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use infrastore_core::{AddRequest, Compression, Store, TimeSeriesData};
use serde_json::{Value, json};

use crate::descriptor::{ColumnLayout, Descriptor, OwnerMap};
use crate::output::{self, Format};
use crate::store_access::{self, CatalogChoice};
use crate::{color, descriptor, parse};

/// Above this many series, the per-series `added ...` lines are replaced by a
/// progress counter and the closing summary.
///
/// A load of a handful of series wants the listing — it is the confirmation
/// that the descriptor said what its author meant. A 5000-series load wants
/// neither 5000 lines of scrollback nor silence while it runs.
const PER_SERIES_LIST_MAX: usize = 20;

/// The `--descriptor` value that means "read the JSON from stdin".
const STDIN: &str = "-";

/// One written series, as both output forms report it.
///
/// The `id` is the point: it is the durable handle a caller records in its own
/// model, and `--id` on `get`/`info` is how it comes back. A load that printed
/// only names would leave the caller re-listing the store to find what it just
/// wrote.
struct AddedRow {
    id: i64,
    time_series_type: &'static str,
    name: String,
    owner_id: i64,
}

impl AddedRow {
    fn line(&self) -> String {
        format!(
            "added {} '{}' (owner {}) as id {}",
            self.time_series_type, self.name, self.owner_id, self.id
        )
    }

    fn json(&self) -> Value {
        json!({
            "id": self.id,
            "time_series_type": self.time_series_type,
            "name": self.name,
            "owner_id": self.owner_id,
        })
    }
}

/// A descriptor written as flags instead of a file.
///
/// Deliberately a mirror of [`Descriptor`]'s fields rather than a second, terser
/// schema: the inline form is a shortcut for authoring one descriptor, not a
/// different way to describe a series, so anything expressible in one file is
/// expressible here and both go down the same code path.
#[derive(Debug, Clone, clap::Args)]
pub struct InlineArgs {
    /// Owner ID (long layout).
    #[arg(long, help_heading = "Inline descriptor")]
    pub owner_id: Option<i64>,
    /// Owner type, e.g. Generator.
    #[arg(long, help_heading = "Inline descriptor")]
    pub owner_type: Option<String>,
    /// Owner category (Component|SupplementalAttribute); defaults to Component.
    #[arg(long, help_heading = "Inline descriptor")]
    pub owner_category: Option<String>,
    /// Time series name.
    #[arg(long, help_heading = "Inline descriptor")]
    pub name: Option<String>,
    /// Time series type.
    #[arg(long = "type", value_name = "TYPE", help_heading = "Inline descriptor")]
    pub ts_type: Option<String>,
    /// Element type, e.g. f64 or tuple(3,f64).
    #[arg(long, help_heading = "Inline descriptor")]
    pub element_type: Option<String>,
    /// Units label, e.g. MW.
    #[arg(long, help_heading = "Inline descriptor")]
    pub units: Option<String>,
    /// Quantity kind the values measure, e.g. ActivePower.
    #[arg(long, help_heading = "Inline descriptor")]
    pub quantity_kind: Option<String>,
    /// Unit basis: natural_units or component_base.
    #[arg(long, help_heading = "Inline descriptor")]
    pub unit_system: Option<String>,
    /// Declare how the timestamps are spelled: utc, zoneless, a fixed offset
    /// like -07:00, or an IANA zone name like America/Denver.
    ///
    /// Normally unnecessary — the spelling is read off the timestamps
    /// themselves, with --assume-timezone / --zoneless deciding what a zoneless
    /// column means. Set this only to declare a spelling the text cannot carry.
    // `allow_hyphen_values` so a western offset can be written the obvious way.
    #[arg(long, help_heading = "Inline descriptor", allow_hyphen_values = true)]
    pub time_reference: Option<String>,
    /// Component field these values vary over time, e.g. max_active_power.
    #[arg(long, help_heading = "Inline descriptor")]
    pub component_field: Option<String>,
    /// Opaque, package-owned payload.
    #[arg(long, help_heading = "Inline descriptor")]
    pub application_data: Option<String>,
    /// Per-timestep element shape, repeatable: --element-shape 3 --element-shape 2.
    #[arg(long, help_heading = "Inline descriptor")]
    pub element_shape: Vec<usize>,
    /// Feature, repeatable: key=value.
    #[arg(
        long = "feature",
        value_name = "KEY=VALUE",
        help_heading = "Inline descriptor"
    )]
    pub feature: Vec<String>,
    /// First timestamp (RFC3339 or epoch-ms).
    #[arg(long, help_heading = "Inline descriptor")]
    pub initial_timestamp: Option<String>,
    /// Resolution as an ISO-8601 duration, e.g. PT1H.
    #[arg(long, help_heading = "Inline descriptor")]
    pub resolution: Option<String>,
    /// Forecast horizon as an ISO-8601 duration.
    #[arg(long, help_heading = "Inline descriptor")]
    pub horizon: Option<String>,
    /// Forecast interval as an ISO-8601 duration.
    #[arg(long, help_heading = "Inline descriptor")]
    pub interval: Option<String>,
    /// Forecast window count.
    #[arg(long, help_heading = "Inline descriptor")]
    pub count: Option<usize>,
    /// Percentile, repeatable (Probabilistic).
    #[arg(
        long = "percentile",
        value_name = "P",
        help_heading = "Inline descriptor"
    )]
    pub percentile: Vec<f64>,
    /// Scenario count (Scenarios).
    #[arg(long, help_heading = "Inline descriptor")]
    pub scenario_count: Option<usize>,
    /// CSV column layout: long (default) or wide.
    #[arg(long, value_name = "LAYOUT", help_heading = "Inline descriptor")]
    pub layout: Option<String>,
    /// Wide layout: path to a `column,owner_id[,owner_type]` CSV.
    #[arg(long, help_heading = "Inline descriptor")]
    pub owner_map: Option<String>,
    /// Wide layout: `header` when the column headers already are owner ids.
    #[arg(long, value_name = "SOURCE", help_heading = "Inline descriptor")]
    pub owner_id_from: Option<String>,
}

impl InlineArgs {
    /// Whether any inline field was supplied, i.e. whether the caller meant the
    /// inline form at all.
    pub fn any_set(&self) -> bool {
        self.owner_id.is_some()
            || self.owner_type.is_some()
            || self.owner_category.is_some()
            || self.name.is_some()
            || self.ts_type.is_some()
            || self.element_type.is_some()
            || self.units.is_some()
            || self.quantity_kind.is_some()
            || self.unit_system.is_some()
            || self.time_reference.is_some()
            || self.component_field.is_some()
            || self.application_data.is_some()
            || !self.element_shape.is_empty()
            || !self.feature.is_empty()
            || self.initial_timestamp.is_some()
            || self.resolution.is_some()
            || self.horizon.is_some()
            || self.interval.is_some()
            || self.count.is_some()
            || !self.percentile.is_empty()
            || self.scenario_count.is_some()
            || self.layout.is_some()
            || self.owner_map.is_some()
            || self.owner_id_from.is_some()
    }

    fn to_descriptor(&self, csv: &Path) -> Result<Descriptor, String> {
        let layout = match self.layout.as_deref() {
            None | Some("long") => ColumnLayout::Long,
            Some("wide") => ColumnLayout::Wide,
            Some(other) => return Err(format!("invalid --layout '{other}' (use long or wide)")),
        };
        let features = self
            .feature
            .iter()
            .map(|pair| parse::parse_feature_kv(pair))
            .collect::<Result<infrastore_core::Features, String>>()?;
        let features = infrastore_core::features_to_plain(&features)
            .into_iter()
            .collect();
        Ok(Descriptor {
            owner_id: self.owner_id,
            owner_type: self.owner_type.clone(),
            owner_category: self.owner_category.clone().unwrap_or_else(|| {
                infrastore_core::OwnerCategory::Component
                    .as_str()
                    .to_string()
            }),
            name: self.name.clone().ok_or("an inline add requires --name")?,
            ts_type: self.ts_type.clone().ok_or_else(|| {
                format!("an inline add requires --type ({})", parse::TS_TYPE_NAMES)
            })?,
            element_type: self
                .element_type
                .clone()
                .ok_or("an inline add requires --element-type (e.g. f64)")?,
            units: self.units.clone(),
            quantity_kind: self.quantity_kind.clone(),
            unit_system: self.unit_system.clone(),
            time_reference: self.time_reference.clone(),
            component_field: self.component_field.clone(),
            application_data: self.application_data.clone(),
            csv: Some(csv.display().to_string()),
            element_shape: self.element_shape.clone(),
            features,
            initial_timestamp: self.initial_timestamp.clone(),
            resolution: self.resolution.clone(),
            horizon: self.horizon.clone(),
            interval: self.interval.clone(),
            count: self.count,
            percentiles: (!self.percentile.is_empty()).then(|| self.percentile.clone()),
            scenario_count: self.scenario_count,
            layout,
            owner_map: self.owner_map.clone().map(OwnerMap::Path),
            owner_id_from: self.owner_id_from.clone(),
        })
    }
}

// The `add` command's flags, handed to [`run`] as parsed.
//
// `--parquet` is mutually exclusive with the descriptor and CSV forms, because
// a partition's series file *is* the descriptor. `--no-checksum` is read only
// by the Parquet path, so a build without that feature parses a flag it does
// not use — as it does every other `--parquet` argument, which beats a flag
// that vanishes from `--help`.
#[derive(Debug, clap::Args)]
pub struct AddArgs {
    /// Descriptor JSON describing the series (single object or array of
    /// objects). `-` reads it from stdin.
    #[arg(long)]
    pub descriptor: Option<PathBuf>,
    /// CSV data path. With --descriptor it overrides the descriptor's own
    /// (single-series descriptors only); without one it starts an inline add.
    #[arg(long)]
    pub csv: Option<PathBuf>,
    /// Parquet partition to load, repeatable: a file, a directory, or a
    /// partition stem. A pair written by `export -f parquet` is
    /// self-describing and needs no other flag; a foreign values file needs
    /// at least --owner-id and --owner-type.
    #[arg(long, value_name = "PATH")]
    pub parquet: Vec<PathBuf>,
    /// Waive the data_hash check on a --parquet load, for values edited in a
    /// query engine without the hash being recomputed.
    #[arg(long)]
    #[cfg_attr(not(feature = "parquet"), allow(dead_code))]
    pub no_checksum: bool,
    #[command(flatten)]
    pub inline: InlineArgs,
    /// Resolve every descriptor and print what would be written, without
    /// opening the store.
    #[arg(long)]
    pub dry_run: bool,
    /// Remove any series that already has one of these identities first.
    #[arg(long)]
    pub replace: bool,
    /// Commit every N series instead of the whole load in one transaction.
    #[arg(long, value_name = "N")]
    pub batch_size: Option<usize>,
    /// Print nothing but errors.
    #[arg(long, short = 'q')]
    pub quiet: bool,
    /// Compression for a store created by this command: none, deflate, or
    /// deflate:LEVEL (0-9). Errors if the store already exists.
    #[arg(long)]
    pub compression: Option<String>,
    /// Disable byte-shuffle for deflate compression (only with --compression).
    #[arg(long)]
    pub no_shuffle: bool,
    /// Where the SQLite catalog lives while the store is open.
    #[arg(long, value_name = "MODE", default_value_t = CatalogChoice::Attached)]
    pub catalog: CatalogChoice,
}

/// `compression` is `--compression` / `--no-shuffle` already parsed.
pub fn run(
    store_path: &Path,
    opts: &AddArgs,
    compression: Option<Compression>,
    format: Format,
) -> Result<(), String> {
    // Resolved up front, because a Parquet file *is* the descriptor: there is
    // nothing to load from a JSON file and nothing for a relative `csv` path to
    // sit beside.
    let (parquet, descriptors, base_dir, csv_override) = if opts.parquet.is_empty() {
        let (descriptors, base_dir, csv_override) = load_descriptors(opts)?;
        (None, descriptors, base_dir, csv_override)
    } else {
        (Some(parquet_import(opts)?), Vec::new(), None, None)
    };

    if opts.dry_run {
        return match &parquet {
            Some((setup, files)) => report_parquet_dry_run(&parquet_dry_run(setup, files)?, format),
            None => dry_run(&descriptors, base_dir.as_deref(), csv_override, format),
        };
    }

    let batch = opts.batch_size.unwrap_or(usize::MAX);
    if batch == 0 {
        return Err("--batch-size must be at least 1".to_string());
    }

    // The store is opened lazily, on the first batch that actually has
    // something to write. `add` creates the store when it is missing, so
    // opening it up front would leave an empty artifact behind whenever a
    // descriptor turns out not to resolve — and a failed load that silently
    // creates a store is how you end up with an empty one you then trust.
    let mut store: Option<Store> = None;
    let open = |compression, catalog| -> Result<Store, String> {
        store_access::open_writable_with(store_path, compression, catalog)
    };
    let mut progress = Progress::new(
        parquet
            .as_ref()
            .map_or(descriptors.len(), |(_, files)| files.len()),
        opts.quiet,
    );
    let mut pending: Vec<AddRequest> = Vec::new();
    let mut added: Vec<AddedRow> = Vec::new();
    let mut total = 0usize;

    // The load runs to completion or to its first error; either way the catalog
    // is written out below. Nothing between here and there may return early.
    let loaded = (|| -> Result<(), String> {
        // One transaction per Parquet file, whatever --batch-size says: a
        // directory import is all-or-nothing *per file*, so a partition that
        // fails leaves the ones already committed alone. Each file is streamed
        // into its transaction one series at a time -- a partition can be
        // larger than memory, and holding it would defeat the format.
        if let Some((setup, files)) = &parquet {
            for (i, file) in files.iter().enumerate() {
                total += import_parquet_partition(
                    file,
                    setup,
                    &mut store,
                    &|| open(compression, opts.catalog),
                    opts.replace,
                    &mut added,
                )?;
                progress.tick(i + 1, total);
            }
        }
        for (i, desc) in descriptors.iter().enumerate() {
            pending.extend(desc.to_add_requests(base_dir.as_deref(), csv_override)?);
            progress.tick(i + 1, total + pending.len());
            // `>=` rather than `==`: a wide descriptor contributes many requests
            // at once, so the batch can overshoot the size in one step. Chunks
            // are separate transactions, which is the trade the flag exists to
            // make.
            if pending.len() >= batch {
                if store.is_none() {
                    store = Some(open(compression, opts.catalog)?);
                }
                let store = store.as_mut().expect("just opened");
                total += flush(store, &mut pending, opts.replace, &mut added)?;
            }
        }
        if !pending.is_empty() && store.is_none() {
            store = Some(open(compression, opts.catalog)?);
        }
        if let Some(store) = store.as_mut() {
            total += flush(store, &mut pending, opts.replace, &mut added)?;
        }
        Ok(())
    })();

    if let Some(store) = store.as_mut() {
        // `persist_catalog` rather than `flush`, because one of these is a
        // per-process store. An in-memory catalog that is never written before
        // this command exits is not "not yet durable" — it is gone, and every
        // array this load streamed to the HDF5 file is unreachable. For an
        // attached catalog this *is* `flush`.
        //
        // On the failure path too, and that is the point. Creating the store
        // stamps the HDF5 half immediately, while an in-memory catalog writes no
        // `.sqlite` until this call, so returning early on a mid-load error
        // would leave a stamped array file with no catalog beside it — the
        // terminal `MismatchedArtifact` state, recoverable only by deleting the
        // file. Every batch that did commit is all-or-nothing, so what we write
        // here is a valid store holding exactly the batches that succeeded.
        let persisted = store.persist_catalog().map_err(|e| e.to_string());
        // The load error is the one that explains what went wrong; a persist
        // failure on top of it is a consequence, not the cause.
        if loaded.is_ok() {
            persisted?;
        }
    }
    loaded?;
    progress.finish();

    if opts.quiet {
        return Ok(());
    }
    let listed = total <= PER_SERIES_LIST_MAX;
    crate::output::report(
        format,
        || {
            serde_json::json!({
                "added": total,
                "store": store_path.display().to_string(),
                // Same threshold as the human listing: a bulk load of 100k series
                // should report its count, not echo every row back.
                "series": listed.then(|| added.iter().map(AddedRow::json).collect::<Vec<_>>()),
            })
        },
        || {
            if listed {
                for row in &added {
                    println!("{}", row.line());
                }
            }
            println!(
                "{}",
                color::header(&format!(
                    "Added {total} time series to {}.",
                    store_path.display()
                ))
            );
        },
    )
}

/// Write one batch, returning how many series it held.
fn flush(
    store: &mut Store,
    pending: &mut Vec<AddRequest>,
    replace: bool,
    added: &mut Vec<AddedRow>,
) -> Result<usize, String> {
    if pending.is_empty() {
        return Ok(0);
    }
    let requests = std::mem::take(pending);
    // The reporting fields, captured before the requests are consumed by the write.
    let echo: Vec<(&'static str, String, i64)> = requests
        .iter()
        .map(|r| {
            (
                r.data.time_series_type().as_str(),
                r.data.name().to_string(),
                r.owner_id,
            )
        })
        .collect();
    if replace {
        // Remove-then-add per identity, so a re-run of a load leaves either the
        // new series or the old one — never neither. Identities the store does
        // not hold are skipped rather than failing the batch: `--replace` says
        // "replace it if it is there", which has to hold on the first load into
        // an empty store and on a descriptor that adds a series alongside ones
        // it is replacing.
        let filters: Vec<infrastore_core::ListFilter> =
            requests.iter().map(request_identity).collect();
        store_access::remove_existing(store, &filters)?;
    }
    let keys = store
        .add_time_series_bulk(requests)
        .map_err(|e| e.to_string())?;
    for (id, (ts_type, name, owner_id)) in keys.iter().zip(&echo) {
        added.push(AddedRow {
            id: id.get(),
            time_series_type: ts_type,
            name: name.clone(),
            owner_id: *owner_id,
        });
    }
    Ok(keys.len())
}

/// The identity a request will be stored under, for `--replace`.
fn request_identity(req: &AddRequest) -> infrastore_core::ListFilter {
    let (resolution, interval) = match &req.data {
        TimeSeriesData::SingleTimeSeries(s) => (Some(s.resolution), None),
        TimeSeriesData::NonSequentialTimeSeries(_) | TimeSeriesData::PersistentTimeSeries(_) => {
            (None, None)
        }
        TimeSeriesData::Deterministic(d) => (Some(d.resolution), Some(d.interval)),
        TimeSeriesData::Probabilistic(p) => (Some(p.resolution), Some(p.interval)),
        TimeSeriesData::Scenarios(s) => (Some(s.resolution), Some(s.interval)),
    };
    infrastore_core::ListFilter {
        owner_id: Some(req.owner_id),
        owner_category: Some(req.owner_category),
        time_series_type: Some(req.data.time_series_type()),
        name: Some(req.data.name().to_string()),
        resolution,
        interval,
        features: Some(req.features.clone()),
        features_exact: true,
        ..Default::default()
    }
}

/// `--dry-run`: resolve every descriptor and print what would be written,
/// without opening the store at all.
///
/// This reads each CSV in full, because the errors worth catching before a
/// multi-GB load are exactly the ones only the data reveals — a value count that
/// does not divide by the element shape, a cell that will not parse as the
/// declared dtype, a wide column with no owner.
fn dry_run(
    descriptors: &[Descriptor],
    base_dir: Option<&Path>,
    csv_override: Option<&Path>,
    format: Format,
) -> Result<(), String> {
    let mut requests = Vec::new();
    for desc in descriptors {
        requests.extend(desc.to_add_requests(base_dir, csv_override)?);
    }
    report_dry_run(&requests, format)
}

/// Render what a load would write, whatever resolved the requests.
fn report_dry_run(requests: &[AddRequest], format: Format) -> Result<(), String> {
    let headers: Vec<String> = [
        "Owner",
        "Owner Type",
        "Category",
        "Type",
        "Name",
        "Features",
        "Element Type",
        "Shape",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let rows: Vec<Vec<String>> = requests
        .iter()
        .map(|r| {
            let arr = r.data.array();
            vec![
                r.owner_id.to_string(),
                r.owner_type.clone(),
                r.owner_category.as_str().to_string(),
                r.data.time_series_type().as_str().to_string(),
                r.data.name().to_string(),
                crate::fields::features_str(&r.features),
                r.data.element_type().to_string(),
                format!("{:?}", arr.shape),
            ]
        })
        .collect();

    match format {
        f if f.is_json() => {
            let items: Vec<Value> = requests
                .iter()
                .map(|r| {
                    let arr = r.data.array();
                    json!({
                        "owner_id": r.owner_id,
                        "owner_type": r.owner_type,
                        "owner_category": r.owner_category.as_str(),
                        "type": r.data.time_series_type().as_str(),
                        "name": r.data.name(),
                        "features": crate::fields::features_json(&r.features),
                        "element_type": r.data.element_type().to_string(),
                        "dtype": arr.dtype.as_str(),
                        "shape": arr.shape,
                    })
                })
                .collect();
            output::print_items(f, &items)?;
        }
        Format::Csv => output::display_csv_rows(&headers, &rows)?,
        _ => {
            output::display_table_dyn(&headers, &rows);
            println!(
                "{}",
                color::header(&format!(
                    "Would add {} time series. Nothing was written.",
                    requests.len()
                ))
            );
        }
    }
    Ok(())
}

/// The descriptors to load, the directory their relative `csv` paths resolve
/// against, and the `--csv` override if there is one.
type Loaded<'a> = (Vec<Descriptor>, Option<PathBuf>, Option<&'a Path>);

/// Resolve the descriptors plus the directory their relative `csv` paths are
/// against, from whichever input form was used.
fn load_descriptors(opts: &AddArgs) -> Result<Loaded<'_>, String> {
    match (opts.descriptor.as_deref(), opts.csv.as_deref()) {
        (Some(path), csv) => {
            if opts.inline.any_set() {
                return Err(
                    "--descriptor describes the series itself; drop the inline flags (or drop \
                     --descriptor and pass them all)"
                        .to_string(),
                );
            }
            let (descriptors, base_dir) = if path.as_os_str() == STDIN {
                // Relative `csv` paths in a piped descriptor resolve against the
                // working directory: there is no file for them to sit beside.
                (
                    descriptor::load_reader(std::io::stdin().lock(), "<stdin>")?,
                    None,
                )
            } else {
                (
                    descriptor::load(path)?,
                    path.parent().map(Path::to_path_buf),
                )
            };
            if csv.is_some() && descriptors.len() > 1 {
                return Err("--csv cannot be used with an array descriptor".to_string());
            }
            Ok((descriptors, base_dir, csv))
        }
        (None, Some(csv)) => {
            // Inline paths are already relative to the working directory.
            Ok((vec![opts.inline.to_descriptor(csv)?], None, None))
        }
        (None, None) => Err(
            "add needs either --descriptor <path.json> (or - for stdin) or --csv <path.csv> \
             with the inline flags (--owner-id, --name, --type, --element-type, ...)"
                .to_string(),
        ),
    }
}

/// A one-line, terminal-only progress counter on stderr.
///
/// stderr rather than stdout so it never lands in a redirected `-f json`/`-f
/// csv` capture, and terminal-only so a log file does not collect one line per
/// carriage return.
struct Progress {
    total: usize,
    enabled: bool,
}

impl Progress {
    fn new(total: usize, quiet: bool) -> Self {
        Self {
            total,
            enabled: !quiet && std::io::stderr().is_terminal(),
        }
    }

    fn tick(&mut self, descriptor: usize, series: usize) {
        if !self.enabled {
            return;
        }
        let mut err = std::io::stderr();
        let _ = write!(
            err,
            "\rloading descriptor {descriptor}/{} ({series} series)\x1b[K",
            self.total
        );
        let _ = err.flush();
    }

    fn finish(&mut self) {
        if !self.enabled {
            return;
        }
        let mut err = std::io::stderr();
        let _ = write!(err, "\r\x1b[K");
        let _ = err.flush();
    }
}

// ---- Parquet ----------------------------------------------------------------

/// What every `--parquet` partition is read with: the inline flags, resolved
/// once.
///
/// A partition written by `export -f parquet` carries the whole catalog row in
/// its series file, so this needs no other flag. A **foreign** values file —
/// anything else's Parquet, or one of ours with no series file beside it —
/// carries less, and `--owner-id`, `--owner-type` and `--name` supply what is
/// missing.
///
/// The inline flags override a column for every series in the file, with one
/// exception that follows the project's usual rule: `--element-type` is an
/// **assertion**. It is how `tuple(3,f64)` gets named for a file whose bytes
/// cannot say whether a `FixedSizeList<double>[3]` is a tuple or a dense row,
/// and a contradiction is an error rather than a silent replacement.
#[cfg(feature = "parquet")]
struct ParquetImport {
    options: infrastore_parquet::read::ImportOptions,
    features: Option<infrastore_core::Features>,
    owner_category: Option<infrastore_core::OwnerCategory>,
}

#[cfg(feature = "parquet")]
impl ParquetImport {
    /// One read series as the request that files it.
    fn request(&self, one: infrastore_parquet::ImportedSeries) -> AddRequest {
        AddRequest {
            owner_id: one.owner_id,
            owner_type: one.owner_type,
            owner_category: self.owner_category.unwrap_or(one.owner_category),
            data: one.data,
            // The flags replace the file's map rather than merging into it: a
            // feature set is part of a series' identity, so a half-replaced one
            // would file the row under an identity nobody named.
            features: self.features.clone().unwrap_or(one.features),
        }
    }
}

/// Resolve the flags and discover every file, before anything is read.
///
/// A path may be a **file, a directory, or a partition stem**; a directory takes
/// every partition in it, sorted, pairing each `.values.parquet` with the
/// `.series.parquet` beside it. Discovery is separate from reading so that a
/// malformed later partition is found while its predecessors are being
/// committed, not before any of them is: each partition is its own transaction
/// (see [`import_parquet_partition`]), and a directory import is all-or-nothing
/// per partition rather than across the whole directory. Re-running after a fix
/// does not redo the committed ones.
#[cfg(feature = "parquet")]
fn parquet_import(
    opts: &AddArgs,
) -> Result<(ParquetImport, Vec<infrastore_parquet::PartitionFiles>), String> {
    if opts.descriptor.is_some() || opts.csv.is_some() {
        return Err(
            "--parquet carries its own descriptors; drop --descriptor and --csv".to_string(),
        );
    }
    refuse_grid_flags(&opts.inline)?;
    let setup = ParquetImport {
        options: parquet_options(opts)?,
        features: inline_features(opts)?,
        owner_category: opts
            .inline
            .owner_category
            .as_deref()
            .map(parse::parse_owner_category)
            .transpose()?,
    };
    let mut files = Vec::new();
    for path in &opts.parquet {
        files.extend(infrastore_parquet::partitions(path).map_err(|e| e.to_string())?);
    }
    Ok((setup, files))
}

/// Stream one partition into the store as one transaction.
///
/// Each series is added the moment the merge join finishes it, inside a
/// transaction opened on the first one and committed after the last, so the
/// partition is all-or-nothing without ever being in memory at once. A read or
/// write error rolls the transaction back, leaving the store as it was before
/// this partition and every earlier partition's commit intact.
///
/// The store is still opened lazily, on the first series: `add` creates a
/// missing store, and a partition that turns out to hold nothing readable
/// should not leave an empty artifact behind.
#[cfg(feature = "parquet")]
fn import_parquet_partition(
    file: &infrastore_parquet::PartitionFiles,
    setup: &ParquetImport,
    store: &mut Option<Store>,
    open: &dyn Fn() -> Result<Store, String>,
    replace: bool,
    added: &mut Vec<AddedRow>,
) -> Result<usize, String> {
    // The sink speaks the core's error type. A failure that starts life as a
    // String (opening the store, the `--replace` removal) is parked here and
    // reported in place of the placeholder the sink returns for it.
    let mut side_error: Option<String> = None;
    let mut in_transaction = false;
    let mut count = 0usize;

    let streamed = infrastore_parquet::read_partition_with(file, &setup.options, &mut |one| {
        let request = setup.request(one);
        if store.is_none() {
            match open() {
                Ok(opened) => *store = Some(opened),
                Err(message) => {
                    side_error = Some(message);
                    return Err(infrastore_core::TimeSeriesError::InvalidParameter(
                        "store could not be opened".to_string(),
                    ));
                }
            }
        }
        let target = store.as_mut().expect("just opened");
        if !in_transaction {
            target.begin_transaction()?;
            in_transaction = true;
        }
        if replace
            && let Err(message) =
                store_access::remove_existing(target, &[request_identity(&request)])
        {
            side_error = Some(message);
            return Err(infrastore_core::TimeSeriesError::InvalidParameter(
                "replacement failed".to_string(),
            ));
        }
        let echo = (
            request.data.time_series_type().as_str(),
            request.data.name().to_string(),
            request.owner_id,
        );
        let id = target.add(request)?;
        added.push(AddedRow {
            id: id.get(),
            time_series_type: echo.0,
            name: echo.1,
            owner_id: echo.2,
        });
        count += 1;
        Ok(())
    });

    let outcome = match streamed {
        Ok(_) => Ok(count),
        Err(e) => Err(side_error
            .take()
            .unwrap_or_else(|| format!("reading {}: {e}", file.label()))),
    };
    if in_transaction {
        let target = store.as_mut().expect("a transaction is open on it");
        match &outcome {
            Ok(_) => target.commit_transaction().map_err(|e| e.to_string())?,
            Err(_) => {
                if let Err(e) = target.rollback_transaction() {
                    tracing::warn!(error = %e, partition = %file.label(), "rolling back the partition's transaction failed");
                }
            }
        }
    }
    outcome
}

/// Collect what a load would write, per partition, without writing.
///
/// Still streamed: each series is reduced to its summary line as it is read, so
/// a dry run over a large directory costs no more memory than the load. The
/// distinct-array count is the one number this layout exists for: a thousand
/// components sharing one profile is a thousand series over **one** array.
#[cfg(feature = "parquet")]
fn parquet_dry_run(
    setup: &ParquetImport,
    files: &[infrastore_parquet::PartitionFiles],
) -> Result<Vec<ParquetBatch>, String> {
    let mut batches = Vec::with_capacity(files.len());
    for file in files {
        let mut series = Vec::new();
        let mut ignored_ids = Vec::new();
        let mut arrays = std::collections::HashSet::new();
        infrastore_parquet::read_partition_with(file, &setup.options, &mut |one| {
            if let Some(id) = one.recorded_id {
                ignored_ids.push(id);
            }
            if let Some(key) = one.array.clone() {
                arrays.insert(key);
            }
            series.push(ParquetSummary::of(&setup.request(one)));
            Ok(())
        })
        .map_err(|e| format!("reading {}: {e}", file.label()))?;
        batches.push(ParquetBatch {
            label: file.label(),
            arrays: arrays.len(),
            series,
            ignored_ids,
        });
    }
    Ok(batches)
}

/// One partition's worth of a dry run: what it holds and what was thrown away.
pub struct ParquetBatch {
    /// The partition's stem, or a foreign file's path.
    pub label: String,
    /// How many **distinct** arrays its series read between them.
    pub arrays: usize,
    pub series: Vec<ParquetSummary>,
    /// The ids the file recorded, reported at `--dry-run` and then dropped.
    ///
    /// `add` never accepts an id: "never reissued" is a guarantee of the
    /// catalog's `AUTOINCREMENT`, and a caller free to name one could re-file a
    /// retired id. The destination assigns fresh ones.
    pub ignored_ids: Vec<i64>,
}

/// One series of a dry run, reduced to what the report prints so the values
/// can be dropped as soon as they are read.
pub struct ParquetSummary {
    pub owner_id: i64,
    pub owner_type: String,
    pub owner_category: &'static str,
    pub time_series_type: &'static str,
    pub name: String,
    pub element_type: String,
    pub features: Value,
    pub empty_descriptors: Vec<&'static str>,
}

#[cfg(feature = "parquet")]
impl ParquetSummary {
    fn of(request: &AddRequest) -> Self {
        Self {
            owner_id: request.owner_id,
            owner_type: request.owner_type.clone(),
            owner_category: request.owner_category.as_str(),
            time_series_type: request.data.time_series_type().as_str(),
            name: request.data.name().to_string(),
            element_type: request.data.element_type().to_string(),
            features: crate::fields::features_json(&request.features),
            empty_descriptors: empty_descriptors(request),
        }
    }
}

/// The inline flags that describe a **grid**, which a Parquet import never
/// takes.
///
/// The values imply the grid — a `SingleTimeSeries`' anchor is its first
/// timestamp and a forecast's windows are its `issue_time` column — so a flag
/// naming one is either redundant or a contradiction the import would have to
/// adjudicate. `--layout` and its two companions describe a CSV's column shape,
/// which a Parquet file does not have. Each is refused by name rather than
/// silently dropped, which is what they were.
///
/// See Finding 7.27: a foreign *forecast* — a values file with an `issue_time`
/// column and no series file — is not supported, and these flags are what would
/// have to describe one.
#[cfg(feature = "parquet")]
fn refuse_grid_flags(inline: &InlineArgs) -> Result<(), String> {
    let mut named: Vec<&str> = Vec::new();
    if inline.initial_timestamp.is_some() {
        named.push("--initial-timestamp");
    }
    if inline.interval.is_some() {
        named.push("--interval");
    }
    if inline.horizon.is_some() {
        named.push("--horizon");
    }
    if inline.count.is_some() {
        named.push("--count");
    }
    if !inline.percentile.is_empty() {
        named.push("--percentile");
    }
    if inline.scenario_count.is_some() {
        named.push("--scenario-count");
    }
    for (present, flag) in [
        (inline.layout.is_some(), "--layout"),
        (inline.owner_map.is_some(), "--owner-map"),
        (inline.owner_id_from.is_some(), "--owner-id-from"),
    ] {
        if present {
            named.push(flag);
        }
    }
    if named.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} {} not apply to --parquet: the rows carry the grid ({} is in the values file, and \
         a forecast's windows are its issue_time column), and --layout describes a CSV's \
         columns. Drop {}.",
        named.join(", "),
        if named.len() == 1 { "does" } else { "do" },
        "every timestamp",
        if named.len() == 1 { "it" } else { "them" },
    ))
}

#[cfg(feature = "parquet")]
fn parquet_options(opts: &AddArgs) -> Result<infrastore_parquet::read::ImportOptions, String> {
    Ok(infrastore_parquet::read::ImportOptions {
        time_series_type: opts
            .inline
            .ts_type
            .as_deref()
            .map(parse::parse_ts_type)
            .transpose()?,
        element_type: opts
            .inline
            .element_type
            .as_deref()
            .map(parse::parse_element_type)
            .transpose()?,
        // An assertion, like `--element-type`: the `value` column's nesting
        // already states the per-step shape, so this agrees with it or errors.
        element_shape: (!opts.inline.element_shape.is_empty())
            .then(|| opts.inline.element_shape.clone()),
        // An assertion against the `resolution` column, and the grid itself for
        // a foreign file that records none.
        resolution: opts
            .inline
            .resolution
            .as_deref()
            .map(parse::parse_period)
            .transpose()?,
        name: opts.inline.name.clone(),
        owner_id: opts.inline.owner_id,
        owner_type: opts.inline.owner_type.clone(),
        owner_category: opts
            .inline
            .owner_category
            .as_deref()
            .map(parse::parse_owner_category)
            .transpose()?,
        time_reference: opts
            .inline
            .time_reference
            .as_deref()
            .map(|spelling| {
                infrastore_core::TimeReference::parse(spelling)
                    .map_err(|e| format!("invalid --time-reference: {e}"))
            })
            .transpose()?,
        features: inline_features(opts)?,
        // Overrides, applied to every series in the partition, as §2.10 promises
        // of an inline flag. An empty string clears the descriptor, since that
        // is how this format spells "absent" in the first place.
        units: opts.inline.units.clone(),
        quantity_kind: opts.inline.quantity_kind.clone(),
        unit_system: opts
            .inline
            .unit_system
            .as_deref()
            .map(parse::parse_unit_system)
            .transpose()?,
        component_field: opts.inline.component_field.clone(),
        application_data: opts.inline.application_data.clone(),
        skip_checksum: opts.no_checksum,
    })
}

#[cfg(feature = "parquet")]
fn inline_features(opts: &AddArgs) -> Result<Option<infrastore_core::Features>, String> {
    if opts.inline.feature.is_empty() {
        return Ok(None);
    }
    let mut features = infrastore_core::Features::new();
    for pair in &opts.inline.feature {
        let (k, v) = parse::parse_feature_kv(pair)?;
        features.insert(k, v);
    }
    Ok(Some(features))
}

#[cfg(not(feature = "parquet"))]
struct ParquetImport;

#[cfg(not(feature = "parquet"))]
fn parquet_import(_opts: &AddArgs) -> Result<(ParquetImport, Vec<PathBuf>), String> {
    Err(crate::commands::without_parquet())
}

#[cfg(not(feature = "parquet"))]
fn import_parquet_partition(
    _file: &Path,
    _setup: &ParquetImport,
    _store: &mut Option<Store>,
    _open: &dyn Fn() -> Result<Store, String>,
    _replace: bool,
    _added: &mut Vec<AddedRow>,
) -> Result<usize, String> {
    Err(crate::commands::without_parquet())
}

#[cfg(not(feature = "parquet"))]
fn parquet_dry_run(
    _setup: &ParquetImport,
    _files: &[PathBuf],
) -> Result<Vec<ParquetBatch>, String> {
    Err(crate::commands::without_parquet())
}

/// Report what a Parquet load would write, per partition.
///
/// Per partition rather than flattened, because a directory import is one
/// transaction per partition and a caller deciding whether to run it wants to
/// see that shape.
/// The ignored ids are reported here and nowhere else: this is the only moment
/// where saying "the file names id 7 and the store will not use it" is useful.
fn report_parquet_dry_run(batches: &[ParquetBatch], format: Format) -> Result<(), String> {
    let total: usize = batches.iter().map(|b| b.series.len()).sum();
    match format {
        f if f.is_json() => {
            let items: Vec<Value> = batches
                .iter()
                .map(|b| {
                    json!({
                        "file": b.label,
                        "series": b.series.len(),
                        "arrays": b.arrays,
                        "ignored_ids": b.ignored_ids,
                        "matches": b
                            .series
                            .iter()
                            .map(|r| json!({
                                "owner_id": r.owner_id,
                                "owner_type": r.owner_type,
                                "owner_category": r.owner_category,
                                "type": r.time_series_type,
                                "name": r.name,
                                "element_type": r.element_type,
                                "features": r.features,
                                "empty_descriptors": r.empty_descriptors,
                            }))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect();
            output::print_value(
                f,
                &json!({ "dry_run": true, "would_add": total, "files": items }),
            )
        }
        _ => {
            for one in batches {
                println!(
                    "{} — {} series over {} arrays{}",
                    one.label,
                    one.series.len(),
                    one.arrays,
                    if one.ignored_ids.is_empty() {
                        String::new()
                    } else {
                        format!(", ignoring {} recorded ids", one.ignored_ids.len())
                    }
                );
                for one in &one.series {
                    let note = if one.empty_descriptors.is_empty() {
                        String::new()
                    } else {
                        format!("  (no {})", one.empty_descriptors.join(", "))
                    };
                    println!(
                        "  - owner={} type={} name={}{note}",
                        one.owner_id, one.time_series_type, one.name,
                    );
                }
            }
            println!(
                "{}",
                color::header(&format!(
                    "Would add {total} time series. Nothing was written."
                ))
            );
            Ok(())
        }
    }
}

/// Which free-form descriptors the file left empty.
///
/// An empty string is how the format writes "absent", so a file that genuinely
/// stored an empty string is indistinguishable from one that stored nothing.
/// Naming them at `--dry-run` is what makes that visible before it is committed.
#[cfg(feature = "parquet")]
fn empty_descriptors(request: &AddRequest) -> Vec<&'static str> {
    let mut out = Vec::new();
    if request.data.units().is_none() {
        out.push("units");
    }
    if request.data.quantity_kind().is_none() {
        out.push("quantity_kind");
    }
    if request.data.unit_system().is_none() {
        out.push("unit_system");
    }
    if request.data.component_field().is_none() {
        out.push("component_field");
    }
    if request.data.application_data().is_none() {
        out.push("application_data");
    }
    out
}
