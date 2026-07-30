//! JSON descriptor: the human-authored file that describes a time series whose
//! numeric values live in a companion CSV.
//!
//! A descriptor file may be a single JSON object (one series) or a JSON array
//! of objects (batch add).

use std::collections::BTreeMap;
use std::path::Path;

use infrastore_core::{
    AddRequest, Deterministic, Features, NonSequentialTimeSeries, Probabilistic, Scenarios,
    SingleTimeSeries, TimeSeriesData, TimeSeriesType,
};
use serde::Deserialize;

use crate::csv_io::{self, CsvData};
use crate::parse;

/// One time-series description. Field presence is validated per `type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub owner_id: i64,
    pub owner_type: String,
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
    /// Opaque, package-owned extension payload stored verbatim on the metadata row.
    pub ext: Option<String>,
    /// CSV data path, relative to the descriptor file. May be overridden by `--csv`.
    pub csv: Option<String>,
    #[serde(default = "default_true")]
    pub has_header: bool,
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
}

fn default_true() -> bool {
    true
}
fn default_owner_category() -> String {
    "component".to_string()
}

/// The physical shape of a companion CSV, decided by [`Descriptor::csv_layout`].
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
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("parsing descriptor {}: {e}", path.display()))?;

    match &value {
        serde_json::Value::Array(arr) => {
            if arr.is_empty() {
                return Err(format!("descriptor {} is an empty array", path.display()));
            }
            let series: Vec<Descriptor> = arr
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    serde_json::from_value(v.clone())
                        .map_err(|e| format!("parsing descriptor[{i}] in {}: {e}", path.display()))
                })
                .collect::<Result<_, _>>()?;
            Ok(series)
        }
        serde_json::Value::Object(_) => {
            let one: Descriptor = serde_json::from_value(value)
                .map_err(|e| format!("parsing descriptor {}: {e}", path.display()))?;
            Ok(vec![one])
        }
        _ => Err(format!(
            "descriptor {} must be a JSON object or array",
            path.display()
        )),
    }
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

    /// Build a core [`AddRequest`] by reading the companion CSV and assembling
    /// the matching [`TimeSeriesData`] variant.
    pub fn to_add_request(
        &self,
        base_dir: Option<&Path>,
        override_csv: Option<&Path>,
    ) -> Result<AddRequest, String> {
        let element_type = parse::parse_element_type(&self.element_type)?;
        let dtype = element_type.physical_dtype();
        let ts_type = parse::parse_ts_type(&self.ts_type)?;
        let owner_category = parse::parse_owner_category(&self.owner_category)?;
        let per_step: usize = self.element_shape.iter().product::<usize>().max(1);
        let csv_path = self.csv_path(base_dir, override_csv)?;
        let layout = self.csv_layout(&csv_path, ts_type)?;
        let csv = csv_io::read_csv(&csv_path, self.has_header, layout.leading_cols())?;

        let data = self.build_data(ts_type, dtype, per_step, &csv, layout)?;

        Ok(AddRequest {
            owner_id: self.owner_id,
            owner_type: self.owner_type.clone(),
            owner_category,
            data,
            features: self.features()?,
            units: self.units.clone(),
            element_type: Some(element_type),
            ext: self.ext.clone(),
        })
    }

    /// Which physical layout the companion CSV is in.
    ///
    /// `add` originally required each type's rawest form: bare values for a
    /// SingleTimeSeries or a forecast, `timestamp,value...` for a
    /// NonSequentialTimeSeries. But `export` writes timestamps for every type —
    /// they are the useful part of the output — so an exported file could not be
    /// fed back in, even though `export` is meant to be `add`'s inverse.
    /// Detecting the shape from the header closes that loop without a new flag
    /// and without changing what a hand-written value-only CSV means.
    fn csv_layout(&self, csv_path: &Path, ts_type: TimeSeriesType) -> Result<CsvLayout, String> {
        // Without a header there is nothing to detect, so the historical
        // per-type default stands.
        let header = csv_io::read_header(csv_path, self.has_header)?;
        let col = |i: usize| header.get(i).map(|s| s.trim().to_ascii_lowercase());
        let first_is = |name: &str| col(0).as_deref() == Some(name);

        Ok(match ts_type {
            TimeSeriesType::NonSequentialTimeSeries => CsvLayout::Timestamped,
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
        })
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
