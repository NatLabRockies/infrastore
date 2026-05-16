//! JSON descriptor: the human-authored file that describes a time series whose
//! numeric values live in a companion CSV.
//!
//! A descriptor file may be a single JSON object (one series) or a JSON array
//! of objects (batch add).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use time_series_store_core::{
    AddRequest, Deterministic, Features, NonSequentialTimeSeries, Probabilistic, Scenarios,
    SingleTimeSeries, TimeSeriesData, TimeSeriesType,
};

use crate::csv_io::{self, CsvData};
use crate::parse;

/// One time-series description. Field presence is validated per `type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub owner_uuid: String,
    pub owner_type: String,
    #[serde(default = "default_owner_category")]
    pub owner_category: String,
    pub name: String,
    #[serde(rename = "type")]
    pub ts_type: String,
    pub dtype: String,
    pub units: Option<String>,
    pub scaling_factor_multiplier: Option<String>,
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
        let dtype = parse::parse_dtype(&self.dtype)?;
        let ts_type = parse::parse_ts_type(&self.ts_type)?;
        let owner_category = parse::parse_owner_category(&self.owner_category)?;
        let per_step: usize = self.element_shape.iter().product::<usize>().max(1);
        let csv_path = self.csv_path(base_dir, override_csv)?;
        let needs_timestamps = ts_type == TimeSeriesType::NonSequentialTimeSeries;
        let csv = csv_io::read_csv(&csv_path, self.has_header, needs_timestamps)?;

        let data = self.build_data(ts_type, dtype, per_step, &csv)?;

        Ok(AddRequest {
            owner_uuid: self.owner_uuid.clone(),
            owner_type: self.owner_type.clone(),
            owner_category,
            name: self.name.clone(),
            data,
            features: self.features()?,
            units: self.units.clone(),
            scaling_factor_multiplier: self.scaling_factor_multiplier.clone(),
            logical_type: None,
        })
    }

    fn build_data(
        &self,
        ts_type: TimeSeriesType,
        dtype: time_series_store_core::Dtype,
        per_step: usize,
        csv: &CsvData,
    ) -> Result<TimeSeriesData, String> {
        let elem = &self.element_shape;
        match ts_type {
            TimeSeriesType::SingleTimeSeries => {
                let (initial, resolution) = self.regular_params()?;
                let length = self.steps_from_values(csv.values.len(), per_step)?;
                let shape = with_elem(vec![length], elem);
                let arr = csv_io::build_typed_array(dtype, shape, &csv.values)?;
                Ok(TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                    initial, resolution, arr,
                )))
            }
            TimeSeriesType::NonSequentialTimeSeries => {
                let timestamps = csv
                    .timestamps
                    .iter()
                    .map(|s| parse::parse_timestamp(s))
                    .collect::<Result<Vec<_>, _>>()?;
                let length = timestamps.len();
                let shape = with_elem(vec![length], elem);
                let arr = csv_io::build_typed_array(dtype, shape, &csv.values)?;
                let ns = NonSequentialTimeSeries::new(timestamps, arr)?;
                Ok(TimeSeriesData::NonSequentialTimeSeries(ns))
            }
            TimeSeriesType::Deterministic => {
                let (initial, resolution) = self.regular_params()?;
                let horizon = self.duration_field("horizon")?;
                let interval = self.duration_field("interval")?;
                let count = self.usize_field("count", self.count)?;
                let h = parse::horizon_steps(horizon, resolution)?;
                let shape = with_elem(vec![h, count], elem);
                let arr = csv_io::build_typed_array(dtype, shape, &csv.values)?;
                let det = Deterministic::new(initial, resolution, horizon, interval, count, arr)?;
                Ok(TimeSeriesData::Deterministic(det))
            }
            TimeSeriesType::Probabilistic => {
                let (initial, resolution) = self.regular_params()?;
                let horizon = self.duration_field("horizon")?;
                let interval = self.duration_field("interval")?;
                let count = self.usize_field("count", self.count)?;
                let percentiles = self
                    .percentiles
                    .clone()
                    .ok_or_else(|| "Probabilistic requires `percentiles`".to_string())?;
                let h = parse::horizon_steps(horizon, resolution)?;
                let shape = with_elem(vec![percentiles.len(), h, count], elem);
                let arr = csv_io::build_typed_array(dtype, shape, &csv.values)?;
                let prob = Probabilistic::new(
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    percentiles,
                    arr,
                )?;
                Ok(TimeSeriesData::Probabilistic(prob))
            }
            TimeSeriesType::Scenarios => {
                let (initial, resolution) = self.regular_params()?;
                let horizon = self.duration_field("horizon")?;
                let interval = self.duration_field("interval")?;
                let count = self.usize_field("count", self.count)?;
                let h = parse::horizon_steps(horizon, resolution)?;
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
                let arr = csv_io::build_typed_array(dtype, shape, &csv.values)?;
                let scen = Scenarios::new(
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    scenario_count,
                    arr,
                )?;
                Ok(TimeSeriesData::Scenarios(scen))
            }
            TimeSeriesType::DeterministicSingleTimeSeries => Err(
                "DeterministicSingleTimeSeries cannot be added from CSV; add a SingleTimeSeries \
                 then run `tss transform`"
                    .to_string(),
            ),
        }
    }

    fn regular_params(&self) -> Result<(chrono::DateTime<chrono::Utc>, chrono::Duration), String> {
        let initial = self
            .initial_timestamp
            .as_ref()
            .ok_or_else(|| format!("series '{}' requires `initial_timestamp`", self.name))?;
        let initial = parse::parse_timestamp(initial)?;
        let resolution = self.duration_field("resolution")?;
        Ok((initial, resolution))
    }

    fn duration_field(&self, field: &str) -> Result<chrono::Duration, String> {
        let raw = match field {
            "resolution" => &self.resolution,
            "horizon" => &self.horizon,
            "interval" => &self.interval,
            _ => unreachable!(),
        };
        let raw = raw
            .as_ref()
            .ok_or_else(|| format!("series '{}' requires `{field}`", self.name))?;
        parse::parse_duration(raw)
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
