//! What every writer of the layout does before it touches a file: group a
//! selection by partition and by array, and walk one array's value rows in the
//! order the layout emits them.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use infrastore_core::{Period, TimeSeriesData, TimeSeriesMetadata, TimeSeriesType, TypedArray};

use crate::layout::{self, ArrayKey};
use crate::partition::{PartitionKey, ValueKind};
use crate::schema;
use crate::{Result, unsupported};

/// A selection grouped the way the layout writes it: by partition, then by the
/// array each series reads, with every array's series in id order. Indexes are
/// into the slice handed to [`plan`].
pub type Plan = BTreeMap<PartitionKey, BTreeMap<ArrayKey, Vec<usize>>>;

/// Group `series` by partition and array key. `BTreeMap` is the sort both
/// halves of a partition promise; within a key, series are ordered by id.
pub fn plan(series: &[(TimeSeriesMetadata, TimeSeriesData)]) -> Result<Plan> {
    let mut out = Plan::new();
    for (index, (row, data)) in series.iter().enumerate() {
        out.entry(partition_of(row))
            .or_default()
            .entry(array_key(row, data)?)
            .or_default()
            .push(index);
    }
    for arrays in out.values_mut() {
        for members in arrays.values_mut() {
            members.sort_by_key(|i| series[*i].0.id.map_or(0, |id| id.get()));
        }
    }
    Ok(out)
}

/// Refuse a selection holding an empty series, naming every one.
///
/// A series with no values has a catalog row and a zero-length array; the format
/// could represent it as a series row whose values group has no rows, but that
/// would make "a series row with no values group" legal on import too, and that
/// is the shape a truncated or half-written file takes.
pub fn refuse_empty(series: &[(TimeSeriesMetadata, TimeSeriesData)]) -> Result<()> {
    let empty: Vec<String> = series
        .iter()
        .map(|(row, data)| Ok((row, row_count(row, data)?)))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, n)| *n == 0)
        .map(|(row, _)| format!("'{}' (owner {})", row.name, row.owner_id))
        .collect();
    if empty.is_empty() {
        return Ok(());
    }
    Err(unsupported(format!(
        "{} of the selected series hold no values and cannot be exported: {}. A values table \
         has one row per value, so an empty series would be a series row with no values \
         group -- which is also what a truncated file looks like. Narrow the selection past \
         them.",
        empty.len(),
        empty.join(", ")
    )))
}

/// The array a series reads: its content hash and its time axis.
///
/// The hash is the **canonical** one, which for a composite kind is the
/// minimum-width re-encoding rather than the stored bytes — so two series whose
/// curves are the same points at different paddings share one values group, which
/// is the whole point of normalizing.
pub fn array_key(row: &TimeSeriesMetadata, data: &TimeSeriesData) -> Result<ArrayKey> {
    let array = series_rows(row, data)?.array;
    let leading = leading_shape(row, array);
    Ok(ArrayKey {
        data_hash: layout::canonical_hash(array, row.element_type, &leading)?,
        time_axis: layout::time_axis_of(data)?,
    })
}

/// The partition a catalog row belongs to.
pub fn partition_of(row: &TimeSeriesMetadata) -> PartitionKey {
    PartitionKey {
        time_series_type: row.time_series_type,
        value_kind: ValueKind::of(row.element_type, &per_step_shape(row)),
        time_reference: row.time_reference.clone(),
    }
}

/// The **per-step** element shape, which for a forecast is not the catalog's
/// `element_shape`.
///
/// The catalog stores `TypedArray::element_shape` — everything after the leading
/// axis — which is the per-step shape only for a static series. A forecast's
/// cube is `[H, count, *E]` or `[lanes, H, count, *E]`, so its per-step shape is
/// what follows *all* the leading axes. Getting this wrong would put a forecast
/// in a partition whose `value` column claims the window count is part of one
/// timestep.
pub fn per_step_shape(row: &TimeSeriesMetadata) -> Vec<usize> {
    let skip = row.time_series_type.leading_dims().saturating_sub(1);
    row.element_shape.get(skip..).unwrap_or_default().to_vec()
}

/// The per-series text columns, one per name `series_text_columns` lists.
///
/// Absent free-form descriptors are the **empty string**, which is what keeps
/// every column required. A stored empty string therefore reads back as absent;
/// §2.8 of the plan documents that, and it is the price of not having five
/// nullable columns.
pub fn descriptor_row(
    ts_type: TimeSeriesType,
    row: &TimeSeriesMetadata,
) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = vec![
        (schema::OWNER_TYPE, row.owner_type.clone()),
        (
            schema::OWNER_CATEGORY,
            row.owner_category.as_str().to_string(),
        ),
        (
            schema::TIME_SERIES_TYPE,
            row.time_series_type.as_str().to_string(),
        ),
        (schema::NAME, row.name.clone()),
    ];
    if ts_type == TimeSeriesType::SingleTimeSeries || ts_type.is_forecast() {
        out.push((
            schema::RESOLUTION,
            row.resolution.map(|p| p.to_iso8601()).unwrap_or_default(),
        ));
    }
    if ts_type.is_forecast() {
        out.push((
            schema::INTERVAL,
            row.interval.map(|p| p.to_iso8601()).unwrap_or_default(),
        ));
        out.push((
            schema::HORIZON,
            row.horizon.map(|p| p.to_iso8601()).unwrap_or_default(),
        ));
    }
    out.extend([
        (schema::FEATURES, schema::encode_features(&row.features)),
        (schema::ELEMENT_TYPE, row.element_type.to_string()),
        (
            schema::ELEMENT_SHAPE,
            schema::encode_element_shape(&row.element_shape),
        ),
        (
            schema::TIME_REFERENCE,
            layout::reference_literal(row.time_reference.as_ref()),
        ),
        (schema::UNITS, row.units.clone().unwrap_or_default()),
        (
            schema::QUANTITY_KIND,
            row.quantity_kind.clone().unwrap_or_default(),
        ),
        (
            schema::UNIT_SYSTEM,
            row.unit_system.map_or("", |u| u.as_str()).to_string(),
        ),
        (
            schema::COMPONENT_FIELD,
            row.component_field.clone().unwrap_or_default(),
        ),
        (
            schema::APPLICATION_DATA,
            row.application_data.clone().unwrap_or_default(),
        ),
    ]);
    out
}

/// How many rows a series contributes.
pub fn row_count(row: &TimeSeriesMetadata, data: &TimeSeriesData) -> Result<usize> {
    Ok(series_rows(row, data)?.target.len())
}

/// One array's value rows, before they are columns: where each value sits on the
/// grids, and where it sits in the stored array.
pub struct SeriesRows<'a> {
    /// Empty for a static series, which has no windows.
    pub issue: Vec<i64>,
    pub target: Vec<i64>,
    /// The lane column, whichever the type has. Empty for `Deterministic`.
    pub percentile: Vec<f64>,
    pub scenario: Vec<i64>,
    /// The element index in `array` each row draws from, in emission order.
    ///
    /// An index rather than a slice because a forecast's rows are a *permutation*
    /// of the stored cube: the file is window-major and the cube is step-major,
    /// so nothing can be copied contiguously.
    pub offsets: Vec<usize>,
    pub array: &'a TypedArray,
    /// Elements per row.
    pub per_step: usize,
}

/// The rows one series contributes, in the order the file emits them.
///
/// Static rows are the array in order. A forecast's are window-major, then step,
/// then lane — so `GROUP BY issue_time` scans contiguously and one instant's
/// percentiles sit together, which is how a fan chart reads a row.
pub fn series_rows<'a>(
    row: &TimeSeriesMetadata,
    data: &'a TimeSeriesData,
) -> Result<SeriesRows<'a>> {
    let per_step = per_step_shape(row).iter().product::<usize>().max(1);
    if !data.time_series_type().is_forecast() {
        let (timestamps, array) = static_parts(data)?;
        let n = timestamps.len();
        return Ok(SeriesRows {
            issue: Vec::new(),
            target: timestamps.iter().map(|t| t.timestamp_millis()).collect(),
            percentile: Vec::new(),
            scenario: Vec::new(),
            offsets: (0..n).collect(),
            array,
            per_step,
        });
    }

    let ForecastParts {
        array,
        lanes,
        percentiles,
        grid,
    } = forecast_parts(data)?;
    let leading = data.time_series_type().leading_dims();
    let horizon_steps = array.shape[leading - 2];
    let count = grid.count;
    let lane_count = lanes.unwrap_or(1);

    let rows = count * horizon_steps * lane_count;
    let mut out = SeriesRows {
        issue: Vec::with_capacity(rows),
        target: Vec::with_capacity(rows),
        percentile: Vec::new(),
        scenario: Vec::new(),
        offsets: Vec::with_capacity(rows),
        array,
        per_step,
    };
    for window in 0..count {
        let issued = grid.issue_time(window)?;
        for step in 0..horizon_steps {
            let at = grid.target_time(issued, step)?;
            for lane in 0..lane_count {
                out.issue.push(issued.timestamp_millis());
                out.target.push(at.timestamp_millis());
                if let Some(percentiles) = &percentiles {
                    out.percentile.push(percentiles[lane]);
                } else if lanes.is_some() {
                    out.scenario.push(lane as i64);
                }
                out.offsets
                    .push((lane * horizon_steps + step) * count + window);
            }
        }
    }
    Ok(out)
}

/// A forecast's cube, its lane axis, its percentile labels, and its two grids.
struct ForecastParts<'a> {
    array: &'a TypedArray,
    /// How wide the lane axis is; `None` for `Deterministic`, which has none.
    lanes: Option<usize>,
    /// The percentile labels, for the one type that has them.
    percentiles: Option<Vec<f64>>,
    grid: ForecastGrid,
}

fn forecast_parts(data: &TimeSeriesData) -> Result<ForecastParts<'_>> {
    let grid_of = |initial, resolution, interval, count| ForecastGrid {
        initial_timestamp: initial,
        resolution,
        interval,
        count,
    };
    Ok(match data {
        TimeSeriesData::Deterministic(f) => ForecastParts {
            array: &f.data,
            lanes: None,
            percentiles: None,
            grid: grid_of(f.initial_timestamp, f.resolution, f.interval, f.count),
        },
        TimeSeriesData::Probabilistic(f) => ForecastParts {
            array: &f.data,
            lanes: Some(f.percentiles.len()),
            percentiles: Some(f.percentiles.clone()),
            grid: grid_of(f.initial_timestamp, f.resolution, f.interval, f.count),
        },
        TimeSeriesData::Scenarios(f) => ForecastParts {
            array: &f.data,
            lanes: Some(f.scenario_count),
            percentiles: None,
            grid: grid_of(f.initial_timestamp, f.resolution, f.interval, f.count),
        },
        other => {
            return Err(unsupported(format!(
                "{} is not a dense forecast",
                other.time_series_type().as_str()
            )));
        }
    })
}

/// The two grids a forecast value sits on: windows step by `interval`, and the
/// steps inside one window step by `resolution`.
struct ForecastGrid {
    initial_timestamp: DateTime<Utc>,
    resolution: Period,
    interval: Period,
    count: usize,
}

impl ForecastGrid {
    fn issue_time(&self, window: usize) -> Result<DateTime<Utc>> {
        self.interval
            .add_to(self.initial_timestamp, window as i64)
            .ok_or_else(|| unsupported(format!("forecast window {window} overflows the calendar")))
    }

    fn target_time(&self, issue: DateTime<Utc>, step: usize) -> Result<DateTime<Utc>> {
        self.resolution
            .add_to(issue, step as i64)
            .ok_or_else(|| unsupported(format!("forecast step {step} overflows the calendar")))
    }
}

/// A static series' timestamps and values.
///
/// A `SingleTimeSeries` materializes its grid, calendar-aware, which is why this
/// cannot be `initial + k * resolution`. The two irregular types hand back the
/// vector they store; for a `PersistentTimeSeries` those are **breakpoints**, so
/// the table is the sparse step function as stored.
fn static_parts(data: &TimeSeriesData) -> Result<(Vec<DateTime<Utc>>, &TypedArray)> {
    match data {
        TimeSeriesData::SingleTimeSeries(s) => Ok((s.timestamps().collect(), &s.data)),
        TimeSeriesData::NonSequentialTimeSeries(s) => Ok((s.timestamps.clone(), &s.data)),
        TimeSeriesData::PersistentTimeSeries(s) => Ok((s.timestamps.clone(), &s.data)),
        other => Err(unsupported(format!(
            "{} is not a static series",
            other.time_series_type().as_str()
        ))),
    }
}
/// The leading axes of a stored array — what precedes the per-step element
/// shape — as `encode` wants them.
///
/// `[length]` for a static series, `[H, count]` for a `Deterministic`,
/// `[lanes, H, count]` for the two with a third axis. Only the composite
/// canonicalization uses it, and only to re-encode at minimum width.
pub fn leading_shape(row: &TimeSeriesMetadata, array: &TypedArray) -> Vec<usize> {
    let leading = row.time_series_type.leading_dims().min(array.shape.len());
    array.shape[..leading].to_vec()
}
