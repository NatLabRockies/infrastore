//! Dense forecasts: the long table, out and back.
//!
//! A forecast is stored as a cube and a Parquet file is one flat table, so the
//! export is a *shape change* in a way the static export is not. What is checked
//! here is that the change is lossless — the cube comes back identical — and that
//! the rows carry enough coordinates for the placement not to depend on their
//! order.

use std::sync::Arc;

use arrow::array::{Array, RecordBatch};
use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    Deterministic, Features, OwnerCategory, Probabilistic, Scenarios, TimeReference,
    TimeSeriesData, TimeSeriesMetadata, TypedArray, create_store,
};
use infrastore_parquet::{ImportOptions, read_series, record_batch, schema, write_series};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

/// Store, read back, and export — so the row under test is a real catalog row.
fn stored(data: TimeSeriesData) -> (TimeSeriesMetadata, TimeSeriesData) {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let id = store
        .add_time_series(
            42,
            "Generator",
            OwnerCategory::Component,
            data,
            Features::new(),
        )
        .expect("the forecast should be added");
    let row = store
        .get_metadata_by_id(id)
        .expect("the lookup should succeed")
        .expect("the row was just written");
    let read = store
        .read_by_id(id, infrastore_core::ReadWindow::full())
        .expect("the read should succeed");
    (row, read)
}

/// Two windows of three hourly steps.
///
/// The reference is declared rather than left unset, so a round trip can assert
/// plain equality: an *unspecified* reference comes back as `utc`, which
/// `an_unspecified_reference_comes_back_as_utc` pins on its own.
fn deterministic() -> Deterministic {
    let mut forecast = Deterministic::new(
        t0(),
        Duration::hours(1),
        Duration::hours(3),
        Duration::hours(1),
        2,
        TypedArray::from_f64(vec![3, 2], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        "day_ahead",
    )
    .expect("the forecast should build");
    forecast.time_reference = Some(TimeReference::Utc);
    forecast
}

fn column_names(batch: &RecordBatch) -> Vec<String> {
    batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect()
}

#[test]
fn a_deterministic_forecast_is_a_long_table() {
    let (row, data) = stored(TimeSeriesData::Deterministic(deterministic()));
    let batch = record_batch(&row, &data).expect("the batch should build");

    assert_eq!(
        column_names(&batch),
        vec![
            "issue_time".to_string(),
            "target_time".to_string(),
            "value".to_string()
        ]
    );
    // One row per (window, step): the cube flattened, not one table per window.
    assert_eq!(batch.num_rows(), 6);

    let issue = batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::TimestampMillisecondArray>()
        .expect("a timestamp column");
    let target = batch
        .column(1)
        .as_any()
        .downcast_ref::<arrow::array::TimestampMillisecondArray>()
        .expect("a timestamp column");
    // Window-major, so a GROUP BY issue_time scans contiguously.
    let ms = |h: i64| (t0() + Duration::hours(h)).timestamp_millis();
    assert_eq!(
        issue.values().to_vec(),
        vec![ms(0), ms(0), ms(0), ms(1), ms(1), ms(1)]
    );
    assert_eq!(
        target.values().to_vec(),
        vec![ms(0), ms(1), ms(2), ms(1), ms(2), ms(3)],
        "the second window's steps start at its own issue time"
    );
    // `[H, count]` gathered in window-major order.
    assert_eq!(
        batch
            .column(2)
            .as_any()
            .downcast_ref::<arrow::array::Float64Array>()
            .unwrap()
            .values()
            .to_vec(),
        vec![1.0, 3.0, 5.0, 2.0, 4.0, 6.0]
    );

    let meta = batch.schema().metadata().clone();
    assert_eq!(meta[schema::TIME_SERIES_TYPE], "Deterministic");
    assert_eq!(meta[schema::RESOLUTION], "PT1H");
    assert_eq!(meta[schema::HORIZON], "PT3H");
    assert_eq!(meta[schema::INTERVAL], "PT1H");
    assert_eq!(meta[schema::COUNT], "2");
    assert!(meta.contains_key(schema::INITIAL_TIMESTAMP));
}

#[test]
fn a_deterministic_forecast_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fc.parquet");
    let (row, data) = stored(TimeSeriesData::Deterministic(deterministic()));
    write_series(&path, &row, &data).expect("the file should write");

    let back = read_series(&path, &ImportOptions::default()).expect("the file should import");
    assert_eq!(back.owner_id, Some(42));
    let TimeSeriesData::Deterministic(f) = back.data else {
        panic!("expected a Deterministic, got {:?}", back.data);
    };
    let TimeSeriesData::Deterministic(original) = data else {
        unreachable!()
    };
    assert_eq!(f, original, "the cube comes back identical");
}

#[test]
fn a_probabilistic_forecast_carries_its_percentiles() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("prob.parquet");
    // `[P, H, count]` = [2, 3, 2].
    let values: Vec<f64> = (0..12).map(|i| i as f64).collect();
    let mut forecast = Probabilistic::new(
        t0(),
        Duration::hours(1),
        Duration::hours(3),
        Duration::hours(1),
        2,
        vec![0.1, 0.9],
        TypedArray::from_f64(vec![2, 3, 2], &values),
        "prob",
    )
    .expect("the forecast should build");
    forecast.time_reference = Some(TimeReference::Utc);
    let (row, data) = stored(TimeSeriesData::Probabilistic(forecast.clone()));

    let batch = record_batch(&row, &data).expect("the batch should build");
    assert_eq!(
        column_names(&batch),
        vec![
            "issue_time".to_string(),
            "target_time".to_string(),
            "percentile".to_string(),
            "value".to_string()
        ]
    );
    assert_eq!(batch.num_rows(), 12);
    // One instant's percentiles sit together, which is how a fan chart reads.
    let percentile = batch
        .column(2)
        .as_any()
        .downcast_ref::<arrow::array::Float64Array>()
        .unwrap();
    assert_eq!(percentile.value(0), 0.1);
    assert_eq!(percentile.value(1), 0.9);

    write_series(&path, &row, &data).expect("the file should write");
    let back = read_series(&path, &ImportOptions::default()).expect("the file should import");
    let TimeSeriesData::Probabilistic(f) = back.data else {
        panic!("expected a Probabilistic, got {:?}", back.data);
    };
    assert_eq!(f, forecast);
}

#[test]
fn a_scenarios_forecast_carries_its_trajectory_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("scen.parquet");
    let values: Vec<f64> = (0..12).map(|i| i as f64).collect();
    let mut forecast = Scenarios::new(
        t0(),
        Duration::hours(1),
        Duration::hours(3),
        Duration::hours(1),
        2,
        2,
        TypedArray::from_f64(vec![2, 3, 2], &values),
        "scen",
    )
    .expect("the forecast should build");
    forecast.time_reference = Some(TimeReference::Utc);
    let (row, data) = stored(TimeSeriesData::Scenarios(forecast.clone()));

    let batch = record_batch(&row, &data).expect("the batch should build");
    assert_eq!(
        column_names(&batch),
        vec![
            "issue_time".to_string(),
            "target_time".to_string(),
            "scenario".to_string(),
            "value".to_string()
        ]
    );
    // `scenario_count` is not a catalog column, so the export reads it off the
    // cube; without it the import cannot size the lane axis.
    assert_eq!(batch.schema().metadata()[schema::SCENARIO_COUNT], "2");

    write_series(&path, &row, &data).expect("the file should write");
    let back = read_series(&path, &ImportOptions::default()).expect("the file should import");
    let TimeSeriesData::Scenarios(f) = back.data else {
        panic!("expected a Scenarios, got {:?}", back.data);
    };
    assert_eq!(f, forecast);
}

#[test]
fn a_multidimensional_forecast_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nd.parquet");
    // `[H, count, *E]` = [2, 2, 3].
    let values: Vec<f64> = (0..12).map(|i| i as f64).collect();
    let mut forecast = Deterministic::new(
        t0(),
        Duration::hours(1),
        Duration::hours(2),
        Duration::hours(1),
        2,
        TypedArray::from_f64(vec![2, 2, 3], &values),
        "nd",
    )
    .expect("the forecast should build");
    forecast.time_reference = Some(TimeReference::Utc);
    let (row, data) = stored(TimeSeriesData::Deterministic(forecast.clone()));

    let batch = record_batch(&row, &data).expect("the batch should build");
    // The per-step element shape is one axis further in than the static rule.
    assert_eq!(batch.schema().metadata()[schema::ELEMENT_SHAPE], "[3]");

    write_series(&path, &row, &data).expect("the file should write");
    let back = read_series(&path, &ImportOptions::default()).expect("the file should import");
    let TimeSeriesData::Deterministic(f) = back.data else {
        panic!("expected a Deterministic");
    };
    assert_eq!(f, forecast);
}

#[test]
fn rows_are_placed_by_their_coordinates_not_their_order() {
    // A query engine may rewrite a file in any order; the coordinates are what
    // put a value back where it belongs.
    let dir = tempfile::tempdir().expect("tempdir");
    let (row, data) = stored(TimeSeriesData::Deterministic(deterministic()));
    let batch = record_batch(&row, &data).expect("the batch should build");

    let reversed: Vec<arrow::array::ArrayRef> = batch
        .columns()
        .iter()
        .map(|c| {
            let indices = arrow::array::UInt32Array::from(
                (0..c.len()).rev().map(|i| i as u32).collect::<Vec<_>>(),
            );
            arrow::compute::take(c, &indices, None).expect("take should succeed")
        })
        .collect();
    let shuffled =
        RecordBatch::try_new(batch.schema(), reversed).expect("the batch should rebuild");

    let path = dir.path().join("shuffled.parquet");
    let file = std::fs::File::create(&path).expect("the file should create");
    let mut writer =
        parquet::arrow::ArrowWriter::try_new(file, shuffled.schema(), None).expect("a writer");
    writer.write(&shuffled).expect("the batch should write");
    writer.close().expect("the footer should write");

    let back = read_series(&path, &ImportOptions::default()).expect("the file should import");
    let TimeSeriesData::Deterministic(f) = back.data else {
        panic!("expected a Deterministic");
    };
    let TimeSeriesData::Deterministic(original) = data else {
        unreachable!()
    };
    assert_eq!(f, original);
}

#[test]
fn a_long_table_without_its_grid_is_refused() {
    // The footer's forecast parameters are required: the rows say where a value
    // belongs, not what the grid it belongs to is.
    let dir = tempfile::tempdir().expect("tempdir");
    let (row, data) = stored(TimeSeriesData::Deterministic(deterministic()));
    let batch = record_batch(&row, &data).expect("the batch should build");

    let mut metadata = batch.schema().metadata().clone();
    metadata.remove(schema::INTERVAL);
    let stripped = Arc::new(arrow::datatypes::Schema::new_with_metadata(
        batch.schema().fields().clone(),
        metadata,
    ));
    let batch = RecordBatch::try_new(stripped, batch.columns().to_vec()).expect("rebuild");

    let path = dir.path().join("no_grid.parquet");
    let file = std::fs::File::create(&path).expect("the file should create");
    let mut writer =
        parquet::arrow::ArrowWriter::try_new(file, batch.schema(), None).expect("a writer");
    writer.write(&batch).expect("the batch should write");
    writer.close().expect("the footer should write");

    let err = read_series(&path, &ImportOptions::default()).expect_err("no interval");
    assert!(err.to_string().contains("interval"), "{err}");
}

#[test]
fn a_calendar_horizon_counts_its_steps_by_walking_the_grid() {
    // A month is not a fixed number of milliseconds, so `horizon / resolution`
    // is the wrong arithmetic. Two monthly steps in a two-month horizon.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("monthly.parquet");
    let mut forecast = Deterministic::new(
        Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap(),
        infrastore_core::Period::Months(1),
        infrastore_core::Period::Months(2),
        infrastore_core::Period::Months(1),
        2,
        TypedArray::from_f64(vec![2, 2], &[1.0, 2.0, 3.0, 4.0]),
        "monthly",
    )
    .expect("the forecast should build");
    forecast.time_reference = Some(TimeReference::Utc);
    let (row, data) = stored(TimeSeriesData::Deterministic(forecast.clone()));
    write_series(&path, &row, &data).expect("the file should write");

    let back = read_series(&path, &ImportOptions::default()).expect("the file should import");
    let TimeSeriesData::Deterministic(f) = back.data else {
        panic!("expected a Deterministic");
    };
    assert_eq!(f, forecast);
}

#[test]
fn an_unspecified_reference_comes_back_as_utc() {
    // Not a defect, and worth stating: Arrow's timestamp type has a zone or it
    // has none, and *unspecified* has no third spelling. The export writes a
    // UTC-zoned column for it -- the mapping `to_arrow()` has always used -- so
    // an unspecified reference is promoted to `utc` on the way back. The
    // instants are unchanged; only the label the store records moves from "not
    // stated" to "UTC".
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("unspecified.parquet");
    let mut forecast = deterministic();
    forecast.time_reference = None;
    let (row, data) = stored(TimeSeriesData::Deterministic(forecast));
    write_series(&path, &row, &data).expect("the file should write");

    let back = read_series(&path, &ImportOptions::default()).expect("the file should import");
    assert_eq!(back.data.time_reference(), Some(&TimeReference::Utc));
}
