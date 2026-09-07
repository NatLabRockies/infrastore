//! Parquet export of the three static types.
//!
//! What is checked here is the **schema**, not the plumbing: the column names
//! and types, the timestamp spelling, the footer keys, and the fact that a
//! forecast is refused rather than mis-shaped. The Python suite
//! (`python/tests/test_arrow.py`) pins the same schema from the other producer,
//! and the two are only interchangeable if both are held to it.

use std::collections::BTreeMap;

use arrow::datatypes::{DataType, TimeUnit};
use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    Deterministic, Dtype, ElementType, FeatureValue, Features, NonSequentialTimeSeries,
    OwnerCategory, Period, PersistentTimeSeries, SingleTimeSeries, TimeReference, TimeSeriesData,
    TimeSeriesId, TimeSeriesMetadata, TimeSeriesType, TypedArray, create_store,
};
use infrastore_parquet::{TIMESTAMP_COLUMN, VALUE_COLUMN, record_batch, schema, write_series};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

fn hourly(values: &[f64]) -> SingleTimeSeries {
    SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![values.len()], values),
        "load",
    )
}

/// A store round trip, so the row under test is a real catalog row with a real
/// id rather than a hand-built struct that could drift from one.
fn stored(data: TimeSeriesData, features: Features) -> (TimeSeriesMetadata, TimeSeriesData) {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let id = store
        .add_time_series(42, "Generator", OwnerCategory::Component, data, features)
        .expect("the series should be added");
    let row = store
        .get_metadata_by_id(id)
        .expect("the lookup should succeed")
        .expect("the row was just written");
    let read = store
        .read_by_id(id, infrastore_core::ReadWindow::full())
        .expect("the read should succeed");
    (row, read)
}

fn footer(batch: &arrow::array::RecordBatch) -> BTreeMap<String, String> {
    batch
        .schema()
        .metadata()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[test]
fn a_single_time_series_is_timestamp_and_value() {
    let (row, data) = stored(
        TimeSeriesData::SingleTimeSeries(hourly(&[1.0, 2.0, 3.0])),
        Features::new(),
    );
    let batch = record_batch(&row, &data).expect("the batch should build");

    assert_eq!(
        batch
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect::<Vec<_>>(),
        vec![TIMESTAMP_COLUMN.to_string(), VALUE_COLUMN.to_string()]
    );
    assert_eq!(batch.num_rows(), 3);
    assert_eq!(
        *batch.schema().field(1).data_type(),
        DataType::Float64,
        "a scalar f64 series is a primitive column"
    );
    // The store has no nulls; NaN is a value.
    assert!(batch.schema().fields().iter().all(|f| !f.is_nullable()));
}

#[test]
fn the_timestamp_column_walks_the_grid() {
    let (row, data) = stored(
        TimeSeriesData::SingleTimeSeries(hourly(&[1.0, 2.0, 3.0])),
        Features::new(),
    );
    let batch = record_batch(&row, &data).expect("the batch should build");
    let column = batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::TimestampMillisecondArray>()
        .expect("a millisecond timestamp column");
    let expected: Vec<i64> = (0..3)
        .map(|k| (t0() + Duration::hours(k)).timestamp_millis())
        .collect();
    assert_eq!(column.values().to_vec(), expected);
}

#[test]
fn a_monthly_grid_steps_on_the_calendar() {
    // The one case a start-plus-k-times-resolution timestamp column would get
    // wrong: `Period::Months` clamps to month end and is not a fixed duration.
    let series = SingleTimeSeries::new(
        Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap(),
        Period::Months(1),
        TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]),
        "monthly",
    );
    let (row, data) = stored(TimeSeriesData::SingleTimeSeries(series), Features::new());
    let batch = record_batch(&row, &data).expect("the batch should build");
    let column = batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::TimestampMillisecondArray>()
        .expect("a millisecond timestamp column");
    let expected: Vec<i64> = [(2024, 1, 31), (2024, 2, 29), (2024, 3, 31)]
        .iter()
        .map(|(y, m, d)| {
            Utc.with_ymd_and_hms(*y, *m, *d, 0, 0, 0)
                .unwrap()
                .timestamp_millis()
        })
        .collect();
    assert_eq!(column.values().to_vec(), expected);
}

#[test]
fn every_dtype_crosses() {
    let cases: Vec<(TypedArray, DataType)> = vec![
        (
            TypedArray::from_slice(vec![2], &[1.0f64, 2.0]).unwrap(),
            DataType::Float64,
        ),
        (
            TypedArray::from_slice(vec![2], &[1.0f32, 2.0]).unwrap(),
            DataType::Float32,
        ),
        (
            TypedArray::from_slice(vec![2], &[1i64, 2]).unwrap(),
            DataType::Int64,
        ),
        (
            TypedArray::from_slice(vec![2], &[1i32, 2]).unwrap(),
            DataType::Int32,
        ),
        (
            TypedArray::from_slice(vec![2], &[1u64, 2]).unwrap(),
            DataType::UInt64,
        ),
        (
            TypedArray::from_slice(vec![2], &[true, false]).unwrap(),
            DataType::Boolean,
        ),
    ];
    for (array, expected) in cases {
        let dtype = array.dtype;
        let series = SingleTimeSeries::new(t0(), Duration::hours(1), array, "load");
        let (row, data) = stored(TimeSeriesData::SingleTimeSeries(series), Features::new());
        let batch = record_batch(&row, &data).expect("the batch should build");
        assert_eq!(
            *batch.schema().field(1).data_type(),
            expected,
            "dtype {dtype:?} should cross as {expected:?}"
        );
    }
}

#[test]
fn a_multidimensional_value_becomes_nested_fixed_size_lists() {
    let values: Vec<f64> = (0..24).map(|i| i as f64).collect();
    let series = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![4, 2, 3], &values),
        "load",
    );
    let (row, data) = stored(TimeSeriesData::SingleTimeSeries(series), Features::new());
    let batch = record_batch(&row, &data).expect("the batch should build");

    // Innermost first, matching the flat row-major buffer.
    let inner = DataType::FixedSizeList(
        std::sync::Arc::new(arrow::datatypes::Field::new(
            "item",
            DataType::Float64,
            false,
        )),
        3,
    );
    let outer = DataType::FixedSizeList(
        std::sync::Arc::new(arrow::datatypes::Field::new("item", inner, false)),
        2,
    );
    assert_eq!(*batch.schema().field(1).data_type(), outer);
    assert_eq!(batch.num_rows(), 4);
    assert_eq!(footer(&batch)[schema::ELEMENT_SHAPE], "[2,3]");
}

#[test]
fn a_composite_element_keeps_its_stored_packing() {
    // `piecewise_linear` is a flat `[1 + 2w]` row, and stays one: the footer's
    // `element_type` is what names it, and every binding has a decoder.
    let values = [2.0, 0.0, 1.0, 1.0, 3.0, 2.0, 0.0, 2.0, 1.0, 4.0];
    let series = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![2, 5], &values),
        "cost",
    )
    .with_element_type(ElementType::PiecewiseLinear);
    let (row, data) = stored(TimeSeriesData::SingleTimeSeries(series), Features::new());
    let batch = record_batch(&row, &data).expect("the batch should build");

    assert_eq!(
        *batch.schema().field(1).data_type(),
        DataType::FixedSizeList(
            std::sync::Arc::new(arrow::datatypes::Field::new(
                "item",
                DataType::Float64,
                false
            )),
            5,
        )
    );
    assert_eq!(footer(&batch)[schema::ELEMENT_TYPE], "piecewise_linear");
}

#[test]
fn the_spelling_of_the_timestamps_survives() {
    let cases = [
        (None, Some("UTC")),
        (Some(TimeReference::Utc), Some("UTC")),
        (Some(TimeReference::Zoneless), None),
        (Some(TimeReference::FixedOffset(-420)), Some("-07:00")),
        (
            Some(TimeReference::Zone("America/Denver".into())),
            Some("America/Denver"),
        ),
    ];
    for (reference, expected) in cases {
        let mut series = hourly(&[1.0, 2.0]);
        series.time_reference = reference.clone();
        let (row, data) = stored(TimeSeriesData::SingleTimeSeries(series), Features::new());
        let batch = record_batch(&row, &data).expect("the batch should build");
        assert_eq!(
            *batch.schema().field(0).data_type(),
            DataType::Timestamp(TimeUnit::Millisecond, expected.map(Into::into)),
            "reference {reference:?} should spell as {expected:?}"
        );
    }
}

#[test]
fn a_zoneless_series_is_told_from_an_unspecified_one_by_the_footer() {
    // Both would produce `timestamp[ms]` with no zone if the zone were the only
    // record, which is why `time_reference` is written explicitly. Only the
    // zoneless one actually does, but the key is what makes it unambiguous.
    let mut series = hourly(&[1.0, 2.0]);
    series.time_reference = Some(TimeReference::Zoneless);
    let (row, data) = stored(TimeSeriesData::SingleTimeSeries(series), Features::new());
    let batch = record_batch(&row, &data).expect("the batch should build");
    assert_eq!(footer(&batch)[schema::TIME_REFERENCE], "zoneless");

    let (row, data) = stored(
        TimeSeriesData::SingleTimeSeries(hourly(&[1.0, 2.0])),
        Features::new(),
    );
    let batch = record_batch(&row, &data).expect("the batch should build");
    assert!(
        !footer(&batch).contains_key(schema::TIME_REFERENCE),
        "an unspecified reference is absent, not written as a literal"
    );
}

#[test]
fn the_footer_describes_the_whole_row() {
    let mut series = hourly(&[1.0, 2.0, 3.0]);
    series.units = Some("MW".into());
    series.quantity_kind = Some("ActivePower".into());
    series.unit_system = Some(infrastore_core::UnitSystem::NaturalUnits);
    series.component_field = Some("max_active_power".into());
    series.application_data = Some(r#"{"k": 1}"#.into());
    series.time_reference = Some(TimeReference::Utc);

    let mut features = Features::new();
    features.insert("model_year".into(), FeatureValue::Int(2030));

    let (row, data) = stored(TimeSeriesData::SingleTimeSeries(series), features);
    let batch = record_batch(&row, &data).expect("the batch should build");
    let meta = footer(&batch);

    assert_eq!(meta[schema::TIME_SERIES_TYPE], "SingleTimeSeries");
    assert_eq!(meta[schema::NAME], "load");
    assert_eq!(meta[schema::ELEMENT_TYPE], "f64");
    assert_eq!(meta[schema::ELEMENT_SHAPE], "[]");
    assert_eq!(meta[schema::RESOLUTION], "PT1H");
    assert_eq!(meta[schema::TIME_REFERENCE], "utc");
    assert_eq!(meta[schema::UNITS], "MW");
    assert_eq!(meta[schema::QUANTITY_KIND], "ActivePower");
    assert_eq!(meta[schema::UNIT_SYSTEM], "natural_units");
    assert_eq!(meta[schema::COMPONENT_FIELD], "max_active_power");
    assert_eq!(meta[schema::APPLICATION_DATA], r#"{"k": 1}"#);
    assert_eq!(meta[schema::OWNER_ID], "42");
    assert_eq!(meta[schema::OWNER_TYPE], "Generator");
    assert_eq!(meta[schema::OWNER_CATEGORY], "Component");
    assert_eq!(meta[schema::FEATURES], r#"{"model_year":2030}"#);
    // The row's own id, for provenance. Ignored on the way back in.
    assert_eq!(meta[schema::ID], "1");

    // Round-tripping the encoded forms gets the values back.
    assert_eq!(
        schema::decode_features(&meta[schema::FEATURES]).unwrap(),
        row.features
    );
    assert_eq!(
        schema::decode_element_shape(&meta[schema::ELEMENT_SHAPE]).unwrap(),
        row.element_shape
    );
}

#[test]
fn an_undeclared_descriptor_is_absent_rather_than_empty() {
    let (row, data) = stored(
        TimeSeriesData::SingleTimeSeries(hourly(&[1.0])),
        Features::new(),
    );
    let meta = footer(&record_batch(&row, &data).expect("the batch should build"));
    for key in [
        schema::UNITS,
        schema::QUANTITY_KIND,
        schema::UNIT_SYSTEM,
        schema::COMPONENT_FIELD,
        schema::APPLICATION_DATA,
    ] {
        assert!(
            !meta.contains_key(key),
            "{key} should be absent, so a reader can ask whether it was declared"
        );
    }
    // These two are facts rather than labels, so they are always written.
    assert_eq!(meta[schema::ELEMENT_SHAPE], "[]");
    assert_eq!(meta[schema::FEATURES], "{}");
}

#[test]
fn the_irregular_types_share_a_shape_and_differ_only_in_the_footer() {
    let stamps: Vec<DateTime<Utc>> = [(2024, 1, 1), (2024, 1, 5), (2024, 3, 9)]
        .iter()
        .map(|(y, m, d)| Utc.with_ymd_and_hms(*y, *m, *d, 0, 0, 0).unwrap())
        .collect();
    let values = TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]);

    let nsts = NonSequentialTimeSeries::new(stamps.clone(), values.clone(), "irregular")
        .expect("an irregular series should build");
    let (row, data) = stored(
        TimeSeriesData::NonSequentialTimeSeries(nsts),
        Features::new(),
    );
    let batch = record_batch(&row, &data).expect("the batch should build");
    let millis: Vec<i64> = stamps.iter().map(|t| t.timestamp_millis()).collect();
    assert_eq!(
        batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::TimestampMillisecondArray>()
            .unwrap()
            .values()
            .to_vec(),
        millis,
        "the timestamp column is the stored vector, not a computed grid"
    );
    let meta = footer(&batch);
    assert_eq!(meta[schema::TIME_SERIES_TYPE], "NonSequentialTimeSeries");
    assert!(
        !meta.contains_key(schema::RESOLUTION),
        "an irregular timeline has no constant step, and its absence says so"
    );

    let pts =
        PersistentTimeSeries::new(stamps, values, "steps").expect("a step function should build");
    let (row, data) = stored(TimeSeriesData::PersistentTimeSeries(pts), Features::new());
    let batch = record_batch(&row, &data).expect("the batch should build");
    // One row per breakpoint, not per instant: a step function is stored
    // sparsely and the table is that sparse form.
    assert_eq!(batch.num_rows(), 3);
    assert_eq!(
        footer(&batch)[schema::TIME_SERIES_TYPE],
        "PersistentTimeSeries",
        "the two share a table shape; only the footer tells them apart"
    );
}

#[test]
fn a_forecast_is_refused_rather_than_mis_shaped() {
    // Two windows of two steps: shape `[horizon_steps, count]`.
    let forecast = Deterministic::new(
        t0(),
        Duration::hours(1),
        Duration::hours(2),
        Duration::hours(1),
        2,
        TypedArray::from_f64(vec![2, 2], &[1.0, 2.0, 3.0, 4.0]),
        "fc",
    )
    .expect("a forecast should build");
    let (row, data) = stored(TimeSeriesData::Deterministic(forecast), Features::new());
    let err = record_batch(&row, &data).expect_err("a forecast is a different table shape");
    assert!(
        matches!(err, infrastore_core::TimeSeriesError::InvalidParameter(_)),
        "got {err:?}"
    );
    assert!(err.to_string().contains("forecast"), "{err}");
}

#[test]
fn a_written_file_reads_back_as_the_same_table() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("load.parquet");
    let mut series = hourly(&[1.0, 2.0, 3.0]);
    series.units = Some("MW".into());
    let (row, data) = stored(TimeSeriesData::SingleTimeSeries(series), Features::new());
    write_series(&path, &row, &data).expect("the file should write");

    let file = std::fs::File::open(&path).expect("the file should open");
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).expect("a valid Parquet file");
    let schema_back = builder.schema().clone();
    let mut reader = builder.build().expect("the reader should build");
    let batch = reader
        .next()
        .expect("one batch")
        .expect("the batch should read");

    assert_eq!(batch.num_rows(), 3);
    assert_eq!(
        batch
            .column(1)
            .as_any()
            .downcast_ref::<arrow::array::Float64Array>()
            .unwrap()
            .values()
            .to_vec(),
        vec![1.0, 2.0, 3.0]
    );
    // The footer survives the round trip, which is the whole reason the
    // descriptors live there rather than in a sidecar.
    assert_eq!(schema_back.metadata()[schema::UNITS], "MW");
    assert_eq!(schema_back.metadata()[schema::NAME], "load");
}

#[test]
fn an_empty_series_writes_an_empty_table() {
    let series = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![0], &[]),
        "empty",
    );
    let (row, data) = stored(TimeSeriesData::SingleTimeSeries(series), Features::new());
    let batch = record_batch(&row, &data).expect("the batch should build");
    assert_eq!(batch.num_rows(), 0);
    assert_eq!(*batch.schema().field(1).data_type(), DataType::Float64);
}

/// A hand-built row is enough to pin the footer's shape without a store, which
/// is what the schema module's own callers do.
#[test]
fn the_footer_can_be_built_from_a_row_alone() {
    let row = TimeSeriesMetadata {
        owner_id: 7,
        owner_type: "Bus".into(),
        owner_category: OwnerCategory::SupplementalAttribute,
        time_series_type: TimeSeriesType::SingleTimeSeries,
        name: "voltage".into(),
        data_hash: [0u8; 32],
        initial_timestamp: Some(t0()),
        resolution: Some(Period::Fixed(Duration::hours(1))),
        length: Some(1),
        horizon: None,
        interval: None,
        count: None,
        timestamps: None,
        features: Features::new(),
        units: None,
        quantity_kind: None,
        unit_system: None,
        time_reference: None,
        component_field: None,
        percentiles: None,
        element_type: ElementType::Scalar(Dtype::F64),
        element_shape: vec![],
        application_data: None,
        id: Some(TimeSeriesId(99)),
    };
    let meta = schema::metadata_for_row(&row);
    assert_eq!(meta[schema::OWNER_CATEGORY], "SupplementalAttribute");
    assert_eq!(meta[schema::ID], "99");
    assert_eq!(
        schema::decode_owner_category(&meta[schema::OWNER_CATEGORY]).unwrap(),
        OwnerCategory::SupplementalAttribute
    );
    assert_eq!(
        schema::decode_time_series_type(&meta[schema::TIME_SERIES_TYPE]).unwrap(),
        TimeSeriesType::SingleTimeSeries
    );
}
