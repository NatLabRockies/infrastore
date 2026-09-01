//! JSON descriptor: the human-authored file that describes a time series whose
//! numeric values live in a companion CSV.
//!
//! A descriptor file may be a single JSON object (one series) or a JSON array
//! of objects (batch add).

use std::collections::BTreeMap;
use std::path::Path;

use infrastore_core::{
    AddRequest, Descriptors, Deterministic, ElementType, Features, NonSequentialTimeSeries,
    PersistentTimeSeries, Probabilistic, Scenarios, SingleTimeSeries, TimeReference,
    TimeSeriesData, TimeSeriesType, UnitSystem,
};
use serde::Deserialize;

use crate::csv_io::{self, CsvData};
use crate::parse;

/// Which shape the companion CSV's *columns* are in.
///
/// Orthogonal to [`CsvLayout`], which is about the leading timestamp columns:
/// this decides whether the value block describes one series or many.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColumnLayout {
    /// Every value column belongs to the one series the descriptor names, and
    /// together they are its per-timestep element. The hand-authored form, and
    /// what `template` prints.
    #[default]
    Long,
    /// Every value column is a *separate* scalar series, sharing this
    /// descriptor's name, type, resolution, and units, and differing only by
    /// owner. This is the canonical power-systems file
    /// (`timestamp,gen_001,gen_002,...`) and the shape `infrastore grid`
    /// writes back out.
    Wide,
}

/// How a wide CSV's column headers map to the `i64` owner ids the store keys on.
///
/// Wide headers are component *names*; the store has no name→id table, so the
/// mapping has to be an input. A sidecar CSV is the batch form, the inline
/// object the one-off, and `owner_id_from: "header"` the case where the headers
/// already are ids.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OwnerMap {
    /// Path to a `column,owner_id[,owner_type]` CSV, relative to the descriptor.
    Path(String),
    /// `{"gen_001": 42, ...}` written straight into the descriptor.
    Inline(BTreeMap<String, i64>),
}

/// One time-series description. Field presence is validated per `type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    /// Required in the `long` layout; rejected in `wide`, where each column
    /// carries its own owner.
    pub owner_id: Option<i64>,
    /// Required in `long`. In `wide` it is the default for columns whose
    /// `owner_map` row does not name one.
    pub owner_type: Option<String>,
    #[serde(default = "default_owner_category")]
    pub owner_category: String,
    pub name: String,
    #[serde(rename = "type")]
    pub ts_type: String,
    /// Canonical `element_type` string: a dtype spelling (`f64`, `i64`, ...)
    /// for plain numbers, else `tuple(N,dtype)` or a function-data kind. The
    /// physical dtype the CSV cells are parsed as is derived from it.
    pub element_type: String,
    pub units: Option<String>,
    /// What kind of physical quantity the values measure (e.g. `"ActivePower"`).
    /// Free-form; QUDT `QuantityKind` local names are the recommended vocabulary.
    pub quantity_kind: Option<String>,
    /// `"natural_units"` or `"component_base"`. Absent means unspecified.
    pub unit_system: Option<String>,
    /// How this series' timestamps are spelled: `"utc"`, `"zoneless"`, a fixed
    /// offset (`"-07:00"`), or an IANA zone name (`"America/Denver"`).
    ///
    /// Normally absent, and then *inferred* from the timestamps the ingest
    /// actually reads — an offset in the CSV text is preserved, a `Z` records
    /// UTC, and a zoneless column records whatever `--assume-timezone` /
    /// `--zoneless` said. Set it only to declare a spelling the text cannot
    /// carry; it overrides the inference for the whole series.
    pub time_reference: Option<String>,
    /// The field on the owning component whose value these values are the
    /// time-varying form of (e.g. `"max_active_power"`). Free-form.
    pub component_field: Option<String>,
    /// Opaque, package-owned payload stored verbatim on the metadata row.
    pub application_data: Option<String>,
    /// CSV data path, relative to the descriptor file. May be overridden by `--csv`.
    pub csv: Option<String>,
    #[serde(default)]
    pub element_shape: Vec<usize>,
    #[serde(default)]
    pub features: BTreeMap<String, serde_json::Value>,

    // Type-specific.
    pub initial_timestamp: Option<String>,
    pub resolution: Option<String>,
    pub horizon: Option<String>,
    pub interval: Option<String>,
    pub count: Option<usize>,
    pub percentiles: Option<Vec<f64>>,
    pub scenario_count: Option<usize>,

    // Wide layout.
    #[serde(default)]
    pub layout: ColumnLayout,
    /// Column header -> owner id, as a sidecar CSV path or an inline object.
    pub owner_map: Option<OwnerMap>,
    /// `"header"` when the column headers already are integer owner ids.
    pub owner_id_from: Option<String>,
}

fn default_owner_category() -> String {
    infrastore_core::OwnerCategory::Component
        .as_str()
        .to_string()
}

/// The physical shape of a companion CSV, decided by [`csv_layout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsvLayout {
    /// Every column is a value, flattened row-major into the array. The
    /// hand-authored form, and what `template` documents.
    Values,
    /// A leading `timestamp` column, then values.
    Timestamped,
    /// Leading `issue_time` and `target_time` columns, then one value column per
    /// (leading series x element) — the form `export -f csv` writes for a dense
    /// forecast. Rows run window-major; see [`forecast_values_from_rows`].
    ForecastTimestamped,
}

/// Check a timestamped CSV against the grid its descriptor declares.
///
/// The regular types take their timeline from `initial_timestamp` + `resolution`
/// and store no timestamps of their own, so the column an exported file carries
/// has nowhere to go. It was therefore stripped and dropped — parsed for the
/// irregular types, discarded for these — which made the round trip `export`
/// advertises silently lossy: feeding a slice back under a descriptor naming a
/// different grid relocated every value onto it, and a column of outright
/// garbage was accepted without a word.
///
/// A timestamp column is a claim about the data, so it is checked rather than
/// ignored: the file has to describe the same grid the descriptor does.
fn check_regular_grid(
    timestamps: &[String],
    initial: chrono::DateTime<chrono::Utc>,
    resolution: infrastore_core::Period,
    length: usize,
    name: &str,
) -> Result<(), String> {
    if timestamps.len() != length {
        return Err(format!(
            "series '{name}': the CSV has {} timestamps but {length} time steps of values",
            timestamps.len()
        ));
    }
    for (i, raw) in timestamps.iter().enumerate() {
        let got = parse::parse_timestamp(raw)
            .map_err(|e| format!("series '{name}', row {}: {e}", i + 1))?;
        let want = resolution
            .add_to(initial, i as i64)
            .ok_or_else(|| format!("series '{name}': the declared grid overflows at step {i}"))?;
        if got != want {
            return Err(format!(
                "series '{name}', row {}: the CSV says {} but the declared grid \
                 (initial_timestamp {}, resolution {}) puts step {i} at {}. Adjust \
                 `initial_timestamp`/`resolution` to match the file, or drop the \
                 timestamp column to accept the declared grid.",
                i + 1,
                got.to_rfc3339(),
                initial.to_rfc3339(),
                resolution.to_iso8601(),
                want.to_rfc3339(),
            ));
        }
    }
    Ok(())
}

impl CsvLayout {
    /// How many leading columns to strip before the value block.
    fn leading_cols(self) -> usize {
        match self {
            CsvLayout::Values => 0,
            CsvLayout::Timestamped => 1,
            CsvLayout::ForecastTimestamped => 2,
        }
    }
}

/// The two static types that carry their instants explicitly rather than on a
/// grid: `NonSequentialTimeSeries` and `PersistentTimeSeries`.
///
/// The CLI treats them identically on the ingest side — same CSV shape, same
/// timestamp column, same spelling inference — because the difference between
/// them is what a *read* between those instants means, which no ingest path
/// sees.
fn is_irregular_static(ts_type: TimeSeriesType) -> bool {
    matches!(
        ts_type,
        TimeSeriesType::NonSequentialTimeSeries | TimeSeriesType::PersistentTimeSeries
    )
}

/// Which physical layout a companion CSV is in, read off its header row.
///
/// `export` writes timestamps for every type, so detecting the layout from the
/// header is what lets an exported file be fed straight back to `add` while a
/// hand-written value-only CSV keeps meaning what it did.
///
/// This is why the header row is mandatory: the detection has no other input,
/// and guessing wrong on a forecast transposes its axes silently rather than
/// failing.
fn csv_layout(header: &[String], ts_type: TimeSeriesType) -> CsvLayout {
    let col = |i: usize| header.get(i).map(|s| s.trim().to_ascii_lowercase());
    let first_is = |name: &str| col(0).as_deref() == Some(name);

    match ts_type {
        // Both irregular static types carry their instants explicitly, so both
        // read a `timestamp,value...` CSV.
        TimeSeriesType::NonSequentialTimeSeries | TimeSeriesType::PersistentTimeSeries => {
            CsvLayout::Timestamped
        }
        TimeSeriesType::SingleTimeSeries => {
            if first_is("timestamp") {
                CsvLayout::Timestamped
            } else {
                CsvLayout::Values
            }
        }
        // Deterministic / Probabilistic / Scenarios.
        _ => {
            if first_is("issue_time") && col(1).as_deref() == Some("target_time") {
                CsvLayout::ForecastTimestamped
            } else {
                CsvLayout::Values
            }
        }
    }
}

/// Reject a CSV whose "header" is really its first row of data.
///
/// The header row is mandatory, which on its own would trade one silent failure
/// for another: hand a header-less file to `add` and the CSV reader eats row one
/// as column names, storing a series quietly one element short. Nothing
/// downstream can catch that — the remaining rows parse fine and the length is
/// whatever it is.
///
/// A header cell that parses as a value of the declared dtype is the signature
/// of that mistake, so it is worth one cheap check. Only the *value* columns are
/// tested: in a timestamped layout the leading column of a header-less file
/// holds timestamps, which are not dtype-parseable and would mask the rest.
fn reject_headerless(
    csv_path: &Path,
    header: &[String],
    layout: CsvLayout,
    dtype: infrastore_core::Dtype,
) -> Result<(), String> {
    let values = &header[header.len().min(layout.leading_cols())..];
    if values.is_empty() || !values.iter().all(|cell| csv_io::parses_as(dtype, cell)) {
        return Ok(());
    }
    Err(format!(
        "the first row of {} ({}) is {} data, not a header. Every data CSV must \
         start with a header row — add one (e.g. `value`, or `timestamp,value`), or \
         delete the row if it is a stray value.",
        csv_path.display(),
        values.join(","),
        dtype.as_str(),
    ))
}

/// The forecast value cells in stored-array order, whichever layout the CSV is
/// in. A value-only CSV is already in array order and passes straight through.
fn forecast_values(
    csv: &CsvData,
    layout: CsvLayout,
    num_series: usize,
    horizon_len: usize,
    count: usize,
    per_step: usize,
) -> Result<Vec<String>, String> {
    match layout {
        CsvLayout::ForecastTimestamped => {
            forecast_values_from_rows(csv, num_series, horizon_len, count, per_step)
        }
        _ => Ok(csv.values.clone()),
    }
}

/// Re-order a timestamped forecast CSV's value cells into the stored array's
/// layout.
///
/// `export` emits one row per `(window, horizon step)` with the leading series
/// (percentiles / scenarios) spread across columns, because that is what reads
/// well next to an `issue_time`/`target_time` pair. The array is stored
/// `[series, horizon, count, element]`. Those two orders differ by a transpose,
/// so the cells cannot simply be concatenated — doing that silently scrambles
/// the forecast rather than failing.
///
/// Row `r` is window `c = r / horizon_len`, step `h = r % horizon_len`; column
/// `k` within a row is series `s = k / per_step`, element `j = k % per_step`.
fn forecast_values_from_rows(
    csv: &CsvData,
    num_series: usize,
    horizon_len: usize,
    count: usize,
    per_step: usize,
) -> Result<Vec<String>, String> {
    let expected_rows = count * horizon_len;
    let expected_width = num_series * per_step;
    if csv.rows != expected_rows || csv.row_width != expected_width {
        return Err(format!(
            "timestamped forecast CSV has {} rows x {} value columns, expected \
             {expected_rows} x {expected_width} (count {count} x horizon steps \
             {horizon_len}, series {num_series} x element {per_step})",
            csv.rows, csv.row_width
        ));
    }
    let mut out = vec![String::new(); expected_rows * expected_width];
    for r in 0..expected_rows {
        let c = r / horizon_len;
        let h = r % horizon_len;
        for k in 0..expected_width {
            let s = k / per_step;
            let j = k % per_step;
            let dst = (((s * horizon_len + h) * count) + c) * per_step + j;
            out[dst] = csv.values[r * expected_width + k].clone();
        }
    }
    Ok(out)
}

/// Load one or more descriptors from a JSON file.
///
/// A root JSON object is a single series; a root JSON array is a batch add.
pub fn load(path: &Path) -> Result<Vec<Descriptor>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("reading descriptor {}: {e}", path.display()))?;
    parse_descriptors(&text, &path.display().to_string())
}

/// [`load`] from an already-open reader, for `--descriptor -`.
///
/// `label` stands in for the path in error messages, so a piped descriptor
/// reports `<stdin>` rather than a path that does not exist.
pub fn load_reader(mut reader: impl std::io::Read, label: &str) -> Result<Vec<Descriptor>, String> {
    let mut text = String::new();
    reader
        .read_to_string(&mut text)
        .map_err(|e| format!("reading descriptor {label}: {e}"))?;
    parse_descriptors(&text, label)
}

fn parse_descriptors(text: &str, path: &str) -> Result<Vec<Descriptor>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("parsing descriptor {path}: {e}"))?;

    match &value {
        serde_json::Value::Array(arr) => {
            if arr.is_empty() {
                return Err(format!("descriptor {path} is an empty array"));
            }
            let series: Vec<Descriptor> = arr
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    serde_json::from_value(v.clone())
                        .map_err(|e| format!("parsing descriptor[{i}] in {path}: {}", explain(e)))
                })
                .collect::<Result<_, _>>()?;
            Ok(series)
        }
        serde_json::Value::Object(_) => {
            let one: Descriptor = serde_json::from_value(value)
                .map_err(|e| format!("parsing descriptor {path}: {}", explain(e)))?;
            Ok(vec![one])
        }
        _ => Err(format!("descriptor {path} must be a JSON object or array")),
    }
}

/// A serde error, plus a migration note for a retired field.
///
/// `deny_unknown_fields` reports a retired key as `unknown field ...`, which is
/// accurate but does not tell a reader carrying an older descriptor what to do
/// instead.
fn explain(e: serde_json::Error) -> String {
    let msg = e.to_string();
    if msg.contains("has_header") {
        return format!(
            "{msg}\n  note: `has_header` was removed — every data CSV must now have a \
             header row. Drop the key; if the CSV has no header, add one (e.g. `value`, \
             or `timestamp,value`)."
        );
    }
    msg
}

impl Descriptor {
    /// Resolve the CSV path against the descriptor's directory, honoring an override.
    fn csv_path(
        &self,
        base_dir: Option<&Path>,
        override_csv: Option<&Path>,
    ) -> Result<std::path::PathBuf, String> {
        if let Some(p) = override_csv {
            return Ok(p.to_path_buf());
        }
        let rel = self.csv.as_ref().ok_or_else(|| {
            format!(
                "series '{}' has no csv path (add \"csv\": \"path/to/data.csv\" or pass --csv)",
                self.name
            )
        })?;
        Ok(match base_dir {
            Some(dir) => dir.join(rel),
            None => std::path::PathBuf::from(rel),
        })
    }

    fn features(&self) -> Result<Features, String> {
        let mut out = Features::new();
        for (k, v) in &self.features {
            out.insert(k.clone(), parse::feature_from_json(k, v)?);
        }
        Ok(out)
    }

    /// The raw text of the timestamp this series is anchored on: the
    /// descriptor's `initial_timestamp` for a regular grid, or the first row of
    /// the CSV's timestamp column for an irregular one (which has no
    /// `initial_timestamp` to speak of).
    fn anchor_timestamp<'a>(
        &'a self,
        ts_type: TimeSeriesType,
        csv: &'a CsvData,
    ) -> Option<&'a str> {
        if is_irregular_static(ts_type) {
            return csv.first_timestamp();
        }
        self.initial_timestamp.as_deref()
    }

    /// Warn when the CSV's timestamps do not all agree on a spelling.
    ///
    /// A series records one reference, taken from its anchor, so a file whose
    /// rows carry different offsets stores every instant correctly and renders
    /// them all at the anchor's offset. Nothing is lost — the instants are
    /// exact — but a row written `12:00-06:00` reads back `11:00-07:00`, and
    /// silently changing a wall clock is the one thing this whole feature
    /// exists to stop.
    ///
    /// The remedy is to name the zone (`"time_reference": "America/Denver"`),
    /// which renders each instant in that zone and reproduces both wall clocks
    /// exactly. So this warns and names it rather than refusing: the offsets in
    /// the file are real and the instants they name are unambiguous, which is
    /// not the caller error a hard failure would imply.
    ///
    /// Skipped when the descriptor states a `time_reference`, which is the
    /// caller having already decided. Short-circuits on the first disagreement,
    /// so an agreeing file pays one parse per row and a disagreeing one stops
    /// early.
    fn warn_on_mixed_spellings(&self, ts_type: TimeSeriesType, csv: &CsvData) {
        if self.time_reference.is_some() || !is_irregular_static(ts_type) {
            return;
        }
        let timestamps = csv.timestamps();
        let Some(anchor_text) = timestamps.first() else {
            return;
        };
        let Ok((_, anchor)) = parse::parse_timestamp_with_reference(anchor_text) else {
            return;
        };
        let odd = timestamps.iter().enumerate().skip(1).find(|(_, raw)| {
            matches!(parse::parse_timestamp_with_reference(raw), Ok((_, r)) if r != anchor)
        });
        if let Some((row, raw)) = odd {
            eprintln!(
                "{}",
                crate::color::dim_err(&format!(
                    "warning: series '{}': row {} is spelled {:?} but the series is anchored on \
                     {:?}. Every instant is stored exactly, but the series records one spelling, \
                     so that row reads back at {}. Set \"time_reference\" to the IANA zone (e.g. \
                     \"America/Denver\") to render each instant in that zone and reproduce both \
                     wall clocks.",
                    self.name,
                    row + 1,
                    raw.trim(),
                    anchor_text.trim(),
                    anchor.as_storage_string(),
                ))
            );
        }
    }

    /// The timestamp spelling this series records.
    ///
    /// An explicit `time_reference` wins; otherwise it is read off the
    /// timestamps this ingest actually parsed, since the spelling is a property
    /// of the *text* and the descriptor never sees it. `first_timestamp` is the
    /// raw text of whichever timestamp the series is anchored on: the
    /// descriptor's `initial_timestamp` for a regular grid, the first row of the
    /// timestamp column for an irregular one.
    ///
    /// A series takes one spelling, so only the anchor is consulted. A file
    /// whose offsets change part-way (local civil time across a DST boundary) is
    /// exactly the case `--assume-timezone <IANA name>` exists for: the zone
    /// renders every row correctly, where the offset of the first row would be
    /// an hour wrong after the transition.
    fn time_reference(
        &self,
        first_timestamp: Option<&str>,
    ) -> Result<Option<TimeReference>, String> {
        if let Some(spelling) = self.time_reference.as_deref() {
            let reference = TimeReference::parse(spelling)
                .map_err(|e| format!("series '{}': invalid time_reference: {e}", self.name))?;
            // The core validates a zone name's *shape* and never resolves it,
            // so `America/Dever` reaches storage intact. This is the layer with
            // a tz database, and it was the only spelling the CLI let through
            // in silence: the same typo passed to `--assume-timezone` is a hard
            // error. Warn rather than refuse -- the store deliberately accepts
            // a name its database has not heard of yet, which is what keeps a
            // zone IANA added last month usable before this build catches up.
            if !crate::fields::zone_is_known(&reference) {
                eprintln!(
                    "{}",
                    crate::color::dim_err(&format!(
                        "warning: series '{}': time_reference \"{spelling}\" is not a zone \
                         this build's tz database recognizes. It is stored as given -- a real \
                         name this build predates still works -- but a typo will only surface \
                         when something tries to render it. `store-info` lists it the same way.",
                        self.name
                    ))
                );
            }
            return Ok(Some(reference));
        }
        match first_timestamp {
            Some(raw) => Ok(Some(parse::parse_timestamp_with_reference(raw)?.1)),
            None => Ok(None),
        }
    }

    /// The descriptive attributes this descriptor declares, which are set on
    /// the series rather than on the request.
    ///
    /// `unit_system` is validated here rather than by serde so an unknown
    /// spelling names the valid ones in the error, instead of producing serde's
    /// "unknown variant" against a field the user cannot see the type of.
    fn descriptors(&self, element_type: ElementType) -> Result<Descriptors, String> {
        let unit_system = match self.unit_system.as_deref() {
            None => None,
            Some(s) => Some(UnitSystem::parse(s).ok_or_else(|| {
                format!("invalid unit_system {s:?}; expected natural_units or component_base")
            })?),
        };
        Ok(Descriptors {
            element_type,
            units: self.units.clone(),
            quantity_kind: self.quantity_kind.clone(),
            unit_system,
            // Filled in per series from the timestamps actually ingested, not
            // from the descriptor: the spelling is a property of the text the
            // CSV (or `initial_timestamp`) carried, and the descriptor never
            // sees it. See `Descriptor::time_reference`.
            time_reference: None,
            component_field: self.component_field.clone(),
            application_data: self.application_data.clone(),
        })
    }

    /// Build the core [`AddRequest`]s this descriptor describes by reading the
    /// companion CSV and assembling the matching [`TimeSeriesData`] variants.
    ///
    /// A `long` descriptor yields exactly one request; a `wide` one yields one
    /// per value column. Callers therefore cannot assume a 1:1 descriptor →
    /// series relationship — which is the whole point of the wide layout.
    pub fn to_add_requests(
        &self,
        base_dir: Option<&Path>,
        override_csv: Option<&Path>,
    ) -> Result<Vec<AddRequest>, String> {
        match self.layout {
            ColumnLayout::Long => Ok(vec![self.long_request(base_dir, override_csv)?]),
            ColumnLayout::Wide => self.wide_requests(base_dir, override_csv),
        }
    }

    fn long_request(
        &self,
        base_dir: Option<&Path>,
        override_csv: Option<&Path>,
    ) -> Result<AddRequest, String> {
        for (field, present) in [
            ("owner_map", self.owner_map.is_some()),
            ("owner_id_from", self.owner_id_from.is_some()),
        ] {
            if present {
                return Err(format!(
                    "series '{}' sets `{field}`, which only applies to \"layout\": \"wide\"",
                    self.name
                ));
            }
        }
        let owner_id = self
            .owner_id
            .ok_or_else(|| format!("series '{}' requires `owner_id`", self.name))?;
        let owner_type = self
            .owner_type
            .clone()
            .ok_or_else(|| format!("series '{}' requires `owner_type`", self.name))?;

        let element_type = parse::parse_element_type(&self.element_type)?;
        let dtype = element_type.physical_dtype();
        let ts_type = parse::parse_ts_type(&self.ts_type)?;
        let owner_category = parse::parse_owner_category(&self.owner_category)?;
        let per_step: usize = self.element_shape.iter().product::<usize>().max(1);
        let csv_path = self.csv_path(base_dir, override_csv)?;
        let header = csv_io::read_header(&csv_path)?;
        let layout = csv_layout(&header, ts_type);
        reject_headerless(&csv_path, &header, layout, dtype)?;
        let csv = csv_io::read_csv(&csv_path, layout.leading_cols())?;

        let mut data = self.build_data(ts_type, dtype, per_step, &csv, layout)?;
        // The descriptor's `element_type`, `units`, and `application_data` describe the
        // series, so they are set on it rather than on the request.
        data.set_descriptors(self.descriptors(element_type)?);
        self.warn_on_mixed_spellings(ts_type, &csv);
        data.set_time_reference(self.time_reference(self.anchor_timestamp(ts_type, &csv))?);

        Ok(AddRequest {
            owner_id,
            owner_type,
            owner_category,
            data,
            features: self.features()?,
        })
    }

    /// One request per value column of a wide CSV.
    ///
    /// Restricted to the two static types and to scalar elements. A forecast's
    /// value block is already three axes deep before any per-column split, and
    /// a multidimensional element would need a second header row to say which
    /// column belongs to which `(owner, element)` pair — neither is expressible
    /// in `timestamp,gen_001,...`, so both are rejected rather than guessed at.
    fn wide_requests(
        &self,
        base_dir: Option<&Path>,
        override_csv: Option<&Path>,
    ) -> Result<Vec<AddRequest>, String> {
        let ts_type = parse::parse_ts_type(&self.ts_type)?;
        if !matches!(
            ts_type,
            TimeSeriesType::SingleTimeSeries
                | TimeSeriesType::NonSequentialTimeSeries
                | TimeSeriesType::PersistentTimeSeries
        ) {
            return Err(format!(
                "\"layout\": \"wide\" holds one scalar series per column, so it covers the \
                 static types (SingleTimeSeries, NonSequentialTimeSeries, \
                 PersistentTimeSeries) only; '{}' is {}",
                self.name,
                ts_type.as_str()
            ));
        }
        if !self.element_shape.is_empty() {
            return Err(format!(
                "series '{}': \"layout\": \"wide\" gives each column one scalar per \
                 timestep, so `element_shape` must be omitted (got {:?})",
                self.name, self.element_shape
            ));
        }
        if self.owner_id.is_some() {
            return Err(format!(
                "series '{}': a wide descriptor takes its owner ids from `owner_map` / \
                 `owner_id_from`, so `owner_id` must be omitted",
                self.name
            ));
        }
        let element_type = parse::parse_element_type(&self.element_type)?;
        let dtype = element_type.physical_dtype();
        let owner_category = parse::parse_owner_category(&self.owner_category)?;
        let features = self.features()?;

        let csv_path = self.csv_path(base_dir, override_csv)?;
        let header = csv_io::read_header(&csv_path)?;
        // The header is the column→owner mapping's left-hand side, so unlike the
        // long layout there is nothing to detect: a leading `timestamp` column
        // is stripped, everything after it is a series. `reject_headerless` is
        // deliberately not run — with `owner_id_from: "header"` the headers are
        // integers, which parse as every numeric dtype and would trip it.
        let has_timestamps = header
            .first()
            .is_some_and(|c| c.trim().eq_ignore_ascii_case("timestamp"));
        if is_irregular_static(ts_type) && !has_timestamps {
            return Err(format!(
                "{}: a wide {} CSV must start with a `timestamp` column (its timestamps \
                 are explicit, not a grid)",
                csv_path.display(),
                ts_type.as_str()
            ));
        }
        let leading = usize::from(has_timestamps);
        let columns: Vec<String> = header[leading.min(header.len())..].to_vec();
        if columns.is_empty() {
            return Err(format!(
                "{} has no value columns; a wide CSV is `timestamp,<col>,<col>,...`",
                csv_path.display()
            ));
        }
        if let Some(dup) = first_duplicate(&columns) {
            return Err(format!(
                "{} has two columns named '{dup}'; wide column headers identify owners \
                 and must be distinct",
                csv_path.display()
            ));
        }
        let owners = self.resolve_owners(&columns, base_dir, &csv_path)?;

        let csv = csv_io::read_csv(&csv_path, leading)?;
        if csv.rows > 0 && csv.row_width != columns.len() {
            return Err(format!(
                "{} has {} value columns in its header but {} in its rows",
                csv_path.display(),
                columns.len(),
                csv.row_width
            ));
        }
        let timestamps = if has_timestamps && is_irregular_static(ts_type) {
            Some(
                csv.timestamps()
                    .iter()
                    .map(|s| parse::parse_timestamp(s))
                    .collect::<Result<Vec<_>, _>>()?,
            )
        } else {
            None
        };
        // A wide `SingleTimeSeries` file carries the same timestamp column, and
        // it describes one grid for every column in the file, so it is checked
        // once here rather than per owner.
        if has_timestamps && ts_type == TimeSeriesType::SingleTimeSeries {
            let (initial, resolution) = self.regular_params()?;
            check_regular_grid(&csv.timestamps(), initial, resolution, csv.rows, &self.name)?;
        }

        // One anchor for the whole file: every column of a wide CSV shares the
        // timestamp column, so every series it yields shares one spelling.
        self.warn_on_mixed_spellings(ts_type, &csv);
        let anchor = self.anchor_timestamp(ts_type, &csv);
        let time_reference = self.time_reference(anchor)?;

        let mut out = Vec::with_capacity(columns.len());
        for (j, (owner_id, owner_type)) in owners.into_iter().enumerate() {
            let cells: Vec<String> = (0..csv.rows)
                .map(|r| csv.values[r * csv.row_width + j].clone())
                .collect();
            let arr = csv_io::build_typed_array(dtype, vec![csv.rows], &cells)
                .map_err(|e| format!("column '{}': {e}", columns[j]))?;
            let mut data = match &timestamps {
                Some(ts) if ts_type == TimeSeriesType::PersistentTimeSeries => {
                    TimeSeriesData::PersistentTimeSeries(
                        PersistentTimeSeries::new(ts.clone(), arr, &self.name)
                            .map_err(|e| format!("column '{}': {e}", columns[j]))?,
                    )
                }
                Some(ts) => TimeSeriesData::NonSequentialTimeSeries(
                    NonSequentialTimeSeries::new(ts.clone(), arr, &self.name)
                        .map_err(|e| format!("column '{}': {e}", columns[j]))?,
                ),
                None => {
                    let (initial, resolution) = self.regular_params()?;
                    TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                        initial, resolution, arr, &self.name,
                    ))
                }
            };
            data.set_descriptors(self.descriptors(element_type)?);
            data.set_time_reference(time_reference.clone());
            out.push(AddRequest {
                owner_id,
                owner_type,
                owner_category,
                data,
                features: features.clone(),
            });
        }
        Ok(out)
    }

    /// `(owner_id, owner_type)` for each wide column, in column order.
    fn resolve_owners(
        &self,
        columns: &[String],
        base_dir: Option<&Path>,
        csv_path: &Path,
    ) -> Result<Vec<(i64, String)>, String> {
        let default_type = || {
            self.owner_type.clone().ok_or_else(|| {
                format!(
                    "series '{}' requires `owner_type` (the type every wide column gets \
                     unless its owner_map row names one)",
                    self.name
                )
            })
        };
        match (&self.owner_id_from, &self.owner_map) {
            (Some(_), Some(_)) => Err(format!(
                "series '{}' sets both `owner_id_from` and `owner_map`; use one",
                self.name
            )),
            (Some(from), None) => {
                if from != "header" {
                    return Err(format!(
                        "series '{}': invalid `owner_id_from` '{from}' (the only value is \
                         \"header\", for a CSV whose column headers already are owner ids)",
                        self.name
                    ));
                }
                let owner_type = default_type()?;
                columns
                    .iter()
                    .map(|c| {
                        c.trim()
                            .parse::<i64>()
                            .map(|id| (id, owner_type.clone()))
                            .map_err(|_| {
                                format!(
                                    "{}: column header '{c}' is not an integer owner id. \
                                     Drop \"owner_id_from\": \"header\" and supply an \
                                     `owner_map` instead.",
                                    csv_path.display()
                                )
                            })
                    })
                    .collect()
            }
            (None, Some(map)) => {
                let table = self.load_owner_map(map, base_dir)?;
                let mut out = Vec::with_capacity(columns.len());
                let mut missing = Vec::new();
                for c in columns {
                    match table.get(c.trim()) {
                        Some((id, ty)) => out.push((
                            *id,
                            match ty {
                                Some(t) => t.clone(),
                                None => default_type()?,
                            },
                        )),
                        None => missing.push(c.clone()),
                    }
                }
                if !missing.is_empty() {
                    // Naming the columns is the whole value here: a 500-column
                    // load that stops at "some column is unmapped" leaves the
                    // caller diffing two files by hand.
                    const MAX: usize = 10;
                    let shown: Vec<&str> = missing.iter().take(MAX).map(String::as_str).collect();
                    let more = missing.len().saturating_sub(MAX);
                    return Err(format!(
                        "{} of {}'s columns are not in the owner_map: {}{}",
                        missing.len(),
                        csv_path.display(),
                        shown.join(", "),
                        if more > 0 {
                            format!(", ... and {more} more")
                        } else {
                            String::new()
                        }
                    ));
                }
                Ok(out)
            }
            (None, None) => Err(format!(
                "series '{}': \"layout\": \"wide\" needs a column->owner mapping. Add \
                 \"owner_map\": \"components.csv\" (a `column,owner_id[,owner_type]` file), \
                 an inline \"owner_map\": {{\"gen_001\": 42}}, or \"owner_id_from\": \
                 \"header\" if the headers already are owner ids.",
                self.name
            )),
        }
    }

    /// The owner map as `column -> (owner_id, owner_type?)`.
    fn load_owner_map(
        &self,
        map: &OwnerMap,
        base_dir: Option<&Path>,
    ) -> Result<BTreeMap<String, (i64, Option<String>)>, String> {
        match map {
            OwnerMap::Inline(entries) => Ok(entries
                .iter()
                .map(|(k, v)| (k.trim().to_string(), (*v, None)))
                .collect()),
            OwnerMap::Path(rel) => {
                let path = match base_dir {
                    Some(dir) => dir.join(rel),
                    None => std::path::PathBuf::from(rel),
                };
                read_owner_map_csv(&path)
            }
        }
    }

    fn build_data(
        &self,
        ts_type: TimeSeriesType,
        dtype: infrastore_core::Dtype,
        per_step: usize,
        csv: &CsvData,
        layout: CsvLayout,
    ) -> Result<TimeSeriesData, String> {
        let elem = &self.element_shape;
        match ts_type {
            TimeSeriesType::SingleTimeSeries => {
                let (initial, resolution) = self.regular_params()?;
                let length = self.steps_from_values(csv.values.len(), per_step)?;
                if layout == CsvLayout::Timestamped {
                    check_regular_grid(&csv.timestamps(), initial, resolution, length, &self.name)?;
                }
                let shape = with_elem(vec![length], elem);
                let arr = csv_io::build_typed_array(dtype, shape, &csv.values)?;
                Ok(TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                    initial, resolution, arr, &self.name,
                )))
            }
            TimeSeriesType::NonSequentialTimeSeries => {
                let timestamps = csv
                    .timestamps()
                    .iter()
                    .map(|s| parse::parse_timestamp(s))
                    .collect::<Result<Vec<_>, _>>()?;
                let length = timestamps.len();
                let shape = with_elem(vec![length], elem);
                let arr = csv_io::build_typed_array(dtype, shape, &csv.values)?;
                let ns = NonSequentialTimeSeries::new(timestamps, arr, &self.name)?;
                Ok(TimeSeriesData::NonSequentialTimeSeries(ns))
            }
            TimeSeriesType::PersistentTimeSeries => {
                // Byte-for-byte the NonSequentialTimeSeries arm above: the CSV
                // says the same thing either way, and the two types differ only
                // in what a read between those instants means.
                let timestamps = csv
                    .timestamps()
                    .iter()
                    .map(|s| parse::parse_timestamp(s))
                    .collect::<Result<Vec<_>, _>>()?;
                let length = timestamps.len();
                let shape = with_elem(vec![length], elem);
                let arr = csv_io::build_typed_array(dtype, shape, &csv.values)?;
                let p = PersistentTimeSeries::new(timestamps, arr, &self.name)?;
                Ok(TimeSeriesData::PersistentTimeSeries(p))
            }
            TimeSeriesType::Deterministic => {
                let (initial, resolution) = self.regular_params()?;
                let horizon = self.period_field("horizon")?;
                let interval = self.period_field("interval")?;
                let count = self.usize_field("count", self.count)?;
                let h = parse::period_horizon_steps(horizon, resolution)?;
                let shape = with_elem(vec![h, count], elem);
                let values = forecast_values(csv, layout, 1, h, count, per_step)?;
                let arr = csv_io::build_typed_array(dtype, shape, &values)?;
                let det = Deterministic::new(
                    initial, resolution, horizon, interval, count, arr, &self.name,
                )?;
                Ok(TimeSeriesData::Deterministic(det))
            }
            TimeSeriesType::Probabilistic => {
                let (initial, resolution) = self.regular_params()?;
                let horizon = self.period_field("horizon")?;
                let interval = self.period_field("interval")?;
                let count = self.usize_field("count", self.count)?;
                let percentiles = self
                    .percentiles
                    .clone()
                    .ok_or_else(|| "Probabilistic requires `percentiles`".to_string())?;
                let h = parse::period_horizon_steps(horizon, resolution)?;
                let shape = with_elem(vec![percentiles.len(), h, count], elem);
                let values = forecast_values(csv, layout, percentiles.len(), h, count, per_step)?;
                let arr = csv_io::build_typed_array(dtype, shape, &values)?;
                let prob = Probabilistic::new(
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    percentiles,
                    arr,
                    &self.name,
                )?;
                Ok(TimeSeriesData::Probabilistic(prob))
            }
            TimeSeriesType::Scenarios => {
                let (initial, resolution) = self.regular_params()?;
                let horizon = self.period_field("horizon")?;
                let interval = self.period_field("interval")?;
                let count = self.usize_field("count", self.count)?;
                let h = parse::period_horizon_steps(horizon, resolution)?;
                let denom = h * count * per_step;
                let scenario_count = match self.scenario_count {
                    Some(s) => s,
                    None => {
                        if denom == 0 || !csv.values.len().is_multiple_of(denom) {
                            return Err(format!(
                                "cannot infer scenario_count: {} values is not divisible by H*count*element ({denom})",
                                csv.values.len()
                            ));
                        }
                        csv.values.len() / denom
                    }
                };
                let shape = with_elem(vec![scenario_count, h, count], elem);
                let values = forecast_values(csv, layout, scenario_count, h, count, per_step)?;
                let arr = csv_io::build_typed_array(dtype, shape, &values)?;
                let scen = Scenarios::new(
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    scenario_count,
                    arr,
                    &self.name,
                )?;
                Ok(TimeSeriesData::Scenarios(scen))
            }
            TimeSeriesType::DeterministicSingleTimeSeries => Err(
                "DeterministicSingleTimeSeries cannot be added from CSV; add a SingleTimeSeries \
                 then run `infrastore transform`"
                    .to_string(),
            ),
        }
    }

    fn regular_params(
        &self,
    ) -> Result<(chrono::DateTime<chrono::Utc>, infrastore_core::Period), String> {
        let initial = self
            .initial_timestamp
            .as_ref()
            .ok_or_else(|| format!("series '{}' requires `initial_timestamp`", self.name))?;
        let initial = parse::parse_timestamp(initial)?;
        let resolution = self.period_field("resolution")?;
        Ok((initial, resolution))
    }

    fn period_field(&self, field: &str) -> Result<infrastore_core::Period, String> {
        let raw = match field {
            "resolution" => &self.resolution,
            "horizon" => &self.horizon,
            "interval" => &self.interval,
            _ => unreachable!(),
        };
        let raw = raw
            .as_ref()
            .ok_or_else(|| format!("series '{}' requires `{field}`", self.name))?;
        parse::parse_period(raw)
    }

    fn usize_field(&self, field: &str, value: Option<usize>) -> Result<usize, String> {
        value.ok_or_else(|| format!("series '{}' requires `{field}`", self.name))
    }

    fn steps_from_values(&self, total: usize, per_step: usize) -> Result<usize, String> {
        if per_step == 0 {
            return Err("element_shape must not contain a zero dimension".to_string());
        }
        if !total.is_multiple_of(per_step) {
            return Err(format!(
                "{total} values is not divisible by per-step element count {per_step}"
            ));
        }
        Ok(total / per_step)
    }
}

/// Append the trailing element shape to a leading shape.
fn with_elem(mut leading: Vec<usize>, elem: &[usize]) -> Vec<usize> {
    leading.extend_from_slice(elem);
    leading
}

/// The first value that appears twice, trimmed. `None` when all are distinct.
fn first_duplicate(values: &[String]) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    values
        .iter()
        .map(|v| v.trim())
        .find(|v| !seen.insert(*v))
        .map(str::to_string)
}

/// Read a `column,owner_id[,owner_type]` sidecar into `column -> (id, type?)`.
///
/// The header row is mandatory and its names are checked, because the file is
/// two or three same-shaped columns of text: a header-less file would silently
/// map the literal column named `column` to the id `owner_id` and then report
/// every real column as unmapped.
fn read_owner_map_csv(path: &Path) -> Result<BTreeMap<String, (i64, Option<String>)>, String> {
    let header = csv_io::read_header(path)?;
    let normalized: Vec<String> = header
        .iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .collect();
    let has_type = match normalized.as_slice() {
        [c, o] if c == "column" && o == "owner_id" => false,
        [c, o, t] if c == "column" && o == "owner_id" && t == "owner_type" => true,
        _ => {
            return Err(format!(
                "{}: an owner map must start with the header `column,owner_id` or \
                 `column,owner_id,owner_type` (found `{}`)",
                path.display(),
                header.join(",")
            ));
        }
    };

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(false)
        .trim(csv::Trim::All)
        .from_path(path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    let mut out: BTreeMap<String, (i64, Option<String>)> = BTreeMap::new();
    for (row, record) in reader.records().enumerate() {
        let record =
            record.map_err(|e| format!("reading {} row {}: {e}", path.display(), row + 1))?;
        let column = record.get(0).unwrap_or_default().trim().to_string();
        let raw_id = record.get(1).unwrap_or_default().trim();
        let owner_id = raw_id.parse::<i64>().map_err(|_| {
            format!(
                "{} row {}: owner_id '{raw_id}' is not an integer",
                path.display(),
                row + 1
            )
        })?;
        let owner_type = if has_type {
            record
                .get(2)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        } else {
            None
        };
        if out.insert(column.clone(), (owner_id, owner_type)).is_some() {
            return Err(format!("{} maps column '{column}' twice", path.display()));
        }
    }
    Ok(out)
}
