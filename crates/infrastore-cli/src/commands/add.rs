//! The `add` command: load one or more series from a descriptor JSON + CSV, or
//! from flags for a one-off.

use std::collections::BTreeMap;
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
        let mut features = BTreeMap::new();
        for pair in &self.feature {
            let (k, v) = parse::parse_feature_kv(pair)?;
            features.insert(k, crate::fields::feature_value_json(&v));
        }
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

/// Everything `add` was asked to do, assembled by `main`.
pub struct Options<'a> {
    pub descriptor: Option<&'a Path>,
    pub csv: Option<&'a Path>,
    pub inline: &'a InlineArgs,
    pub compression: Option<Compression>,
    pub catalog: CatalogChoice,
    pub batch_size: Option<usize>,
    pub replace: bool,
    pub dry_run: bool,
    pub quiet: bool,
    pub format: Format,
}

pub fn run(store_path: &Path, opts: &Options<'_>) -> Result<(), String> {
    let (descriptors, base_dir, csv_override) = load_descriptors(opts)?;

    if opts.dry_run {
        return dry_run(&descriptors, base_dir.as_deref(), csv_override, opts.format);
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
    let mut progress = Progress::new(descriptors.len(), opts.quiet);
    let mut pending: Vec<AddRequest> = Vec::new();
    let mut added: Vec<AddedRow> = Vec::new();
    let mut total = 0usize;

    // The load runs to completion or to its first error; either way the catalog
    // is written out below. Nothing between here and there may return early.
    let loaded = (|| -> Result<(), String> {
        for (i, desc) in descriptors.iter().enumerate() {
            pending.extend(desc.to_add_requests(base_dir.as_deref(), csv_override)?);
            progress.tick(i + 1, total + pending.len());
            // `>=` rather than `==`: a wide descriptor contributes many requests
            // at once, so the batch can overshoot the size in one step. Chunks
            // are separate transactions, which is the trade the flag exists to
            // make.
            if pending.len() >= batch {
                if store.is_none() {
                    store = Some(open(opts.compression, opts.catalog)?);
                }
                let store = store.as_mut().expect("just opened");
                total += flush(store, &mut pending, opts.replace, &mut added)?;
            }
        }
        if !pending.is_empty() && store.is_none() {
            store = Some(open(opts.compression, opts.catalog)?);
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
        opts.format,
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
        TimeSeriesData::NonSequentialTimeSeries(_) => (None, None),
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
            let arr = data_array(&r.data);
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
                    let arr = data_array(&r.data);
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

fn data_array(d: &TimeSeriesData) -> &infrastore_core::TypedArray {
    match d {
        TimeSeriesData::SingleTimeSeries(s) => &s.data,
        TimeSeriesData::NonSequentialTimeSeries(s) => &s.data,
        TimeSeriesData::Deterministic(d) => &d.data,
        TimeSeriesData::Probabilistic(p) => &p.data,
        TimeSeriesData::Scenarios(s) => &s.data,
    }
}

/// The descriptors to load, the directory their relative `csv` paths resolve
/// against, and the `--csv` override if there is one.
type Loaded<'a> = (Vec<Descriptor>, Option<PathBuf>, Option<&'a Path>);

/// Resolve the descriptors plus the directory their relative `csv` paths are
/// against, from whichever input form was used.
fn load_descriptors<'a>(opts: &'a Options<'a>) -> Result<Loaded<'a>, String> {
    match (opts.descriptor, opts.csv) {
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
