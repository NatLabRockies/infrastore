//! Parquet import: reading a file this crate wrote, and reading a foreign one.
//!
//! The two are one code path, so most of these fix the *inference* rules — what
//! the import concludes when the footer is silent — and the refusals, which are
//! where a wrong answer would be worse than no answer.

use std::path::Path;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, FixedSizeListArray, Float64Array, Int64Array, RecordBatch,
    TimestampMicrosecondArray, TimestampMillisecondArray, TimestampNanosecondArray,
    TimestampSecondArray,
};
use arrow::datatypes::{DataType, Field, Schema};
use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    Dtype, ElementType, FeatureValue, Features, NonSequentialTimeSeries, OwnerCategory, Period,
    PersistentTimeSeries, SingleTimeSeries, TimeReference, TimeSeriesData, TimeSeriesError,
    TimeSeriesMetadata, TimeSeriesType, TypedArray, create_store,
};
use infrastore_parquet::{ImportOptions, read_series, write_series};
use parquet::arrow::ArrowWriter;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

/// Write a series through the store and out to Parquet, so what is imported is
/// exactly what the export produces.
fn exported(dir: &Path, data: TimeSeriesData, features: Features) -> std::path::PathBuf {
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
    let path = dir.join("series.parquet");
    write_series(&path, &row, &read).expect("the file should write");
    path
}

fn hourly(values: &[f64]) -> SingleTimeSeries {
    SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![values.len()], values),
        "load",
    )
}

/// A file with **no footer at all** — what a foreign writer produces.
fn foreign(dir: &Path, name: &str, timestamps: ArrayRef, values: ArrayRef) -> std::path::PathBuf {
    // Nullability follows the array, so a null-bearing fixture is writable at
    // all -- the import's refusal is what is under test, not Arrow's.
    let schema = Schema::new(vec![
        Field::new(
            "timestamp",
            timestamps.data_type().clone(),
            timestamps.null_count() > 0,
        ),
        Field::new("value", values.data_type().clone(), values.null_count() > 0),
    ]);
    let batch = RecordBatch::try_new(Arc::new(schema), vec![timestamps, values])
        .expect("the batch should build");
    let path = dir.join(name);
    let file = std::fs::File::create(&path).expect("the file should create");
    let mut writer = ArrowWriter::try_new(file, batch.schema(), None).expect("a writer");
    writer.write(&batch).expect("the batch should write");
    writer.close().expect("the footer should write");
    path
}

fn millis(count: i64, step_ms: i64) -> ArrayRef {
    Arc::new(TimestampMillisecondArray::from(
        (0..count)
            .map(|k| t0().timestamp_millis() + k * step_ms)
            .collect::<Vec<_>>(),
    ))
}

#[test]
fn an_exported_file_round_trips_with_no_flags() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut series = hourly(&[1.0, 2.0, 3.0]);
    series.units = Some("MW".into());
    series.quantity_kind = Some("ActivePower".into());
    series.component_field = Some("max_active_power".into());
    series.application_data = Some(r#"{"k":1}"#.into());
    series.time_reference = Some(TimeReference::Utc);
    let mut features = Features::new();
    features.insert("model_year".into(), FeatureValue::Int(2030));

    let path = exported(
        dir.path(),
        TimeSeriesData::SingleTimeSeries(series),
        features.clone(),
    );
    let back = read_series(&path, &ImportOptions::default()).expect("the file should import");

    assert_eq!(back.owner_id, Some(42));
    assert_eq!(back.owner_type.as_deref(), Some("Generator"));
    assert_eq!(back.owner_category, Some(OwnerCategory::Component));
    assert_eq!(back.features, features);
    // The recorded id is surfaced for reporting and never used to file the row.
    assert_eq!(back.recorded_id, Some(1));

    let TimeSeriesData::SingleTimeSeries(s) = back.data else {
        panic!("expected a SingleTimeSeries, got {:?}", back.data);
    };
    assert_eq!(s.name, "load");
    assert_eq!(s.initial_timestamp, t0());
    assert_eq!(s.resolution, Period::Fixed(Duration::hours(1)));
    assert_eq!(s.data.to_f64_vec().unwrap(), vec![1.0, 2.0, 3.0]);
    assert_eq!(s.units.as_deref(), Some("MW"));
    assert_eq!(s.quantity_kind.as_deref(), Some("ActivePower"));
    assert_eq!(s.component_field.as_deref(), Some("max_active_power"));
    assert_eq!(s.application_data.as_deref(), Some(r#"{"k":1}"#));
    assert_eq!(s.time_reference, Some(TimeReference::Utc));
}

#[test]
fn the_values_round_trip_exactly() {
    // The improvement over CSV, where a float passes through decimal text.
    let dir = tempfile::tempdir().expect("tempdir");
    let awkward = [
        std::f64::consts::PI,
        1e-300,
        -0.0,
        f64::MAX,
        f64::MIN_POSITIVE,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    let path = exported(
        dir.path(),
        TimeSeriesData::SingleTimeSeries(hourly(&awkward)),
        Features::new(),
    );
    let back = read_series(&path, &ImportOptions::default()).expect("the file should import");
    let TimeSeriesData::SingleTimeSeries(s) = back.data else {
        panic!("expected a SingleTimeSeries");
    };
    let values = s.data.to_f64_vec().unwrap();
    for (got, want) in values.iter().zip(&awkward) {
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "{got} should be bit-identical to {want}"
        );
    }
}

#[test]
fn the_irregular_types_keep_their_own_reading() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stamps: Vec<DateTime<Utc>> = [(2024, 1, 1), (2024, 1, 5), (2024, 3, 9)]
        .iter()
        .map(|(y, m, d)| Utc.with_ymd_and_hms(*y, *m, *d, 0, 0, 0).unwrap())
        .collect();
    let values = TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]);

    let path = exported(
        dir.path(),
        TimeSeriesData::NonSequentialTimeSeries(
            NonSequentialTimeSeries::new(stamps.clone(), values.clone(), "irregular").unwrap(),
        ),
        Features::new(),
    );
    let back = read_series(&path, &ImportOptions::default()).expect("the file should import");
    let TimeSeriesData::NonSequentialTimeSeries(s) = back.data else {
        panic!("expected a NonSequentialTimeSeries, got {:?}", back.data);
    };
    assert_eq!(s.timestamps, stamps);

    // The two share a table shape, so only the footer keeps them apart.
    let dir2 = tempfile::tempdir().expect("tempdir");
    let path = exported(
        dir2.path(),
        TimeSeriesData::PersistentTimeSeries(
            PersistentTimeSeries::new(stamps.clone(), values, "steps").unwrap(),
        ),
        Features::new(),
    );
    let back = read_series(&path, &ImportOptions::default()).expect("the file should import");
    assert!(
        matches!(back.data, TimeSeriesData::PersistentTimeSeries(_)),
        "got {:?}",
        back.data
    );
}

#[test]
fn a_multidimensional_value_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let values: Vec<f64> = (0..24).map(|i| i as f64).collect();
    let series = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![4, 2, 3], &values),
        "load",
    );
    let path = exported(
        dir.path(),
        TimeSeriesData::SingleTimeSeries(series),
        Features::new(),
    );
    let back = read_series(&path, &ImportOptions::default()).expect("the file should import");
    let TimeSeriesData::SingleTimeSeries(s) = back.data else {
        panic!("expected a SingleTimeSeries");
    };
    assert_eq!(s.data.shape, vec![4, 2, 3]);
    assert_eq!(s.data.to_f64_vec().unwrap(), values);
}

#[test]
fn a_composite_element_type_comes_back_as_itself() {
    let dir = tempfile::tempdir().expect("tempdir");
    let values = [2.0, 0.0, 1.0, 1.0, 3.0, 2.0, 0.0, 2.0, 1.0, 4.0];
    let series = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![2, 5], &values),
        "cost",
    )
    .with_element_type(ElementType::PiecewiseLinear);
    let path = exported(
        dir.path(),
        TimeSeriesData::SingleTimeSeries(series),
        Features::new(),
    );
    let back = read_series(&path, &ImportOptions::default()).expect("the file should import");
    assert_eq!(back.data.element_type(), ElementType::PiecewiseLinear);
}

// ---- Foreign files ---------------------------------------------------------

#[test]
fn a_foreign_grid_infers_a_single_time_series() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = foreign(
        dir.path(),
        "grid.parquet",
        millis(3, 3_600_000),
        Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0])),
    );
    let options = ImportOptions {
        name: Some("load".into()),
        ..Default::default()
    };
    let back = read_series(&path, &options).expect("the file should import");
    let TimeSeriesData::SingleTimeSeries(s) = back.data else {
        panic!("evenly spaced rows read as a grid");
    };
    assert_eq!(s.resolution, Period::Fixed(Duration::hours(1)));
    assert_eq!(back.owner_id, None, "a foreign file names no owner");
}

#[test]
fn foreign_rows_that_are_not_a_grid_infer_the_irregular_type() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stamps: ArrayRef = Arc::new(TimestampMillisecondArray::from(vec![
        t0().timestamp_millis(),
        t0().timestamp_millis() + 3_600_000,
        t0().timestamp_millis() + 10_800_000,
    ]));
    let path = foreign(
        dir.path(),
        "ragged.parquet",
        stamps,
        Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0])),
    );
    let options = ImportOptions {
        name: Some("irregular".into()),
        ..Default::default()
    };
    let back = read_series(&path, &options).expect("the file should import");
    assert!(
        matches!(back.data, TimeSeriesData::NonSequentialTimeSeries(_)),
        "got {:?}",
        back.data
    );
}

#[test]
fn a_persistent_reading_must_be_named_never_inferred() {
    // Structurally identical to the irregular type, so inferring it would be
    // guessing what the values mean.
    let dir = tempfile::tempdir().expect("tempdir");
    let stamps: ArrayRef = Arc::new(TimestampMillisecondArray::from(vec![
        t0().timestamp_millis(),
        t0().timestamp_millis() + 3_600_000,
        t0().timestamp_millis() + 10_800_000,
    ]));
    let path = foreign(
        dir.path(),
        "steps.parquet",
        stamps,
        Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0])),
    );
    let options = ImportOptions {
        name: Some("steps".into()),
        time_series_type: Some(TimeSeriesType::PersistentTimeSeries),
        ..Default::default()
    };
    let back = read_series(&path, &options).expect("the file should import");
    assert!(matches!(back.data, TimeSeriesData::PersistentTimeSeries(_)));
}

#[test]
fn a_fixed_size_list_infers_dense_and_an_assertion_makes_it_a_tuple() {
    let dir = tempfile::tempdir().expect("tempdir");
    let leaf: ArrayRef = Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]));
    let field = Arc::new(Field::new("item", DataType::Float64, false));
    let values: ArrayRef = Arc::new(FixedSizeListArray::try_new(field, 3, leaf, None).unwrap());
    let path = foreign(dir.path(), "wide.parquet", millis(2, 3_600_000), values);

    // Dense is the weaker claim, and the bytes cannot say more.
    let options = ImportOptions {
        name: Some("load".into()),
        ..Default::default()
    };
    let back = read_series(&path, &options).expect("the file should import");
    assert_eq!(back.data.element_type(), ElementType::Scalar(Dtype::F64));

    // The assertion states the stronger one.
    let options = ImportOptions {
        name: Some("load".into()),
        element_type: Some(ElementType::Tuple {
            arity: 3,
            dtype: Dtype::F64,
        }),
        ..Default::default()
    };
    let back = read_series(&path, &options).expect("the file should import");
    assert_eq!(
        back.data.element_type(),
        ElementType::Tuple {
            arity: 3,
            dtype: Dtype::F64
        }
    );
}

#[test]
fn a_contradicting_assertion_is_an_error_not_an_override() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = exported(
        dir.path(),
        TimeSeriesData::SingleTimeSeries(hourly(&[1.0, 2.0])),
        Features::new(),
    );
    let options = ImportOptions {
        element_type: Some(ElementType::Scalar(Dtype::I64)),
        ..Default::default()
    };
    let err = read_series(&path, &options).expect_err("a contradiction is an error");
    assert!(err.to_string().contains("asserted"), "{err}");

    let options = ImportOptions {
        time_series_type: Some(TimeSeriesType::NonSequentialTimeSeries),
        ..Default::default()
    };
    let err = read_series(&path, &options).expect_err("a contradiction is an error");
    assert!(err.to_string().contains("asserted"), "{err}");
}

#[test]
fn an_unzoned_foreign_column_reads_as_zoneless() {
    // Arrow cannot tell `zoneless` from *unspecified*, which is exactly why the
    // footer spells it out. For a file with no footer, a naive timestamp is a
    // wall clock — the reading Python and the CLI already take.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = foreign(
        dir.path(),
        "naive.parquet",
        millis(2, 3_600_000),
        Arc::new(Float64Array::from(vec![1.0, 2.0])),
    );
    let options = ImportOptions {
        name: Some("load".into()),
        ..Default::default()
    };
    let back = read_series(&path, &options).expect("the file should import");
    assert_eq!(back.data.time_reference(), Some(&TimeReference::Zoneless));
}

#[test]
fn a_zoned_foreign_column_keeps_its_zone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stamps: ArrayRef = Arc::new(
        TimestampMillisecondArray::from(vec![
            t0().timestamp_millis(),
            t0().timestamp_millis() + 3_600_000,
        ])
        .with_timezone("America/Denver"),
    );
    let path = foreign(
        dir.path(),
        "zoned.parquet",
        stamps,
        Arc::new(Float64Array::from(vec![1.0, 2.0])),
    );
    let options = ImportOptions {
        name: Some("load".into()),
        ..Default::default()
    };
    let back = read_series(&path, &options).expect("the file should import");
    assert_eq!(
        back.data.time_reference(),
        Some(&TimeReference::Zone("America/Denver".into()))
    );
}

// ---- Timestamp precision ---------------------------------------------------

#[test]
fn seconds_and_milliseconds_cross_as_they_are() {
    let dir = tempfile::tempdir().expect("tempdir");
    let seconds: ArrayRef = Arc::new(TimestampSecondArray::from(vec![
        t0().timestamp(),
        t0().timestamp() + 3600,
    ]));
    let path = foreign(
        dir.path(),
        "secs.parquet",
        seconds,
        Arc::new(Float64Array::from(vec![1.0, 2.0])),
    );
    let options = ImportOptions {
        name: Some("load".into()),
        ..Default::default()
    };
    let back = read_series(&path, &options).expect("the file should import");
    let TimeSeriesData::SingleTimeSeries(s) = back.data else {
        panic!("expected a SingleTimeSeries");
    };
    assert_eq!(s.initial_timestamp, t0());
}

#[test]
fn a_finer_timestamp_is_refused_unless_it_is_a_whole_millisecond() {
    let dir = tempfile::tempdir().expect("tempdir");
    let options = ImportOptions {
        name: Some("load".into()),
        ..Default::default()
    };

    // Whole milliseconds expressed in microseconds: accepted.
    let micros: ArrayRef = Arc::new(TimestampMicrosecondArray::from(vec![
        t0().timestamp_millis() * 1_000,
        (t0().timestamp_millis() + 3_600_000) * 1_000,
    ]));
    let path = foreign(
        dir.path(),
        "us_ok.parquet",
        micros,
        Arc::new(Float64Array::from(vec![1.0, 2.0])),
    );
    read_series(&path, &options).expect("whole milliseconds in microseconds are fine");

    // Half a millisecond: refused rather than rounded, the same rule the write
    // path enforces on every instant the store records.
    let micros: ArrayRef = Arc::new(TimestampMicrosecondArray::from(vec![
        t0().timestamp_millis() * 1_000 + 500,
        (t0().timestamp_millis() + 3_600_000) * 1_000 + 500,
    ]));
    let path = foreign(
        dir.path(),
        "us_bad.parquet",
        micros,
        Arc::new(Float64Array::from(vec![1.0, 2.0])),
    );
    let err = read_series(&path, &options).expect_err("a fractional millisecond is refused");
    assert!(
        matches!(err, TimeSeriesError::InvalidParameter(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains("whole millisecond"), "{err}");

    // And the same for nanoseconds.
    let nanos: ArrayRef = Arc::new(TimestampNanosecondArray::from(vec![
        t0().timestamp_millis() * 1_000_000 + 1,
        (t0().timestamp_millis() + 3_600_000) * 1_000_000,
    ]));
    let path = foreign(
        dir.path(),
        "ns_bad.parquet",
        nanos,
        Arc::new(Float64Array::from(vec![1.0, 2.0])),
    );
    let err = read_series(&path, &options).expect_err("a sub-millisecond nanosecond is refused");
    assert!(err.to_string().contains("whole millisecond"), "{err}");
}

// ---- Refusals --------------------------------------------------------------

#[test]
fn nulls_are_refused_rather_than_coerced() {
    let dir = tempfile::tempdir().expect("tempdir");
    let values: ArrayRef = Arc::new(Float64Array::from(vec![Some(1.0), None, Some(3.0)]));
    let path = foreign(dir.path(), "nulls.parquet", millis(3, 3_600_000), values);
    let options = ImportOptions {
        name: Some("load".into()),
        ..Default::default()
    };
    let err = read_series(&path, &options).expect_err("a null is not NaN");
    assert!(err.to_string().contains("nulls"), "{err}");
}

#[test]
fn a_file_without_the_expected_columns_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = Schema::new(vec![Field::new("t", DataType::Int64, false)]);
    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![Arc::new(Int64Array::from(vec![1i64, 2])) as ArrayRef],
    )
    .unwrap();
    let path = dir.path().join("wrong.parquet");
    let file = std::fs::File::create(&path).unwrap();
    let mut writer = ArrowWriter::try_new(file, batch.schema(), None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();

    let err = read_series(&path, &ImportOptions::default()).expect_err("no timestamp column");
    assert!(err.to_string().contains("timestamp"), "{err}");
}

#[test]
fn a_nameless_file_is_refused_because_a_name_is_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = foreign(
        dir.path(),
        "anon.parquet",
        millis(2, 3_600_000),
        Arc::new(Float64Array::from(vec![1.0, 2.0])),
    );
    let err = read_series(&path, &ImportOptions::default()).expect_err("a name is required");
    assert!(err.to_string().contains("name"), "{err}");
}

#[test]
fn an_empty_regular_series_has_no_anchor_and_says_so() {
    // The timestamp column *is* a `SingleTimeSeries`' anchor, so an empty table
    // has nowhere to put it. The irregular types need no anchor and are fine.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = exported(
        dir.path(),
        TimeSeriesData::SingleTimeSeries(hourly(&[])),
        Features::new(),
    );
    let err = read_series(&path, &ImportOptions::default()).expect_err("no anchor");
    assert!(err.to_string().contains("anchored"), "{err}");
}

#[test]
fn timestamps_that_leave_the_declared_grid_are_refused() {
    // A file whose footer claims PT1H but whose rows do not walk one.
    let dir = tempfile::tempdir().expect("tempdir");
    let row = TimeSeriesMetadata {
        owner_id: 1,
        owner_type: "Generator".into(),
        owner_category: OwnerCategory::Component,
        time_series_type: TimeSeriesType::SingleTimeSeries,
        name: "load".into(),
        data_hash: [0u8; 32],
        initial_timestamp: Some(t0()),
        resolution: Some(Period::Fixed(Duration::hours(1))),
        length: Some(3),
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
        id: None,
    };
    // Hand-built so the timestamps disagree with the footer's resolution.
    let footer = infrastore_parquet::schema::metadata_for_row(&row);
    let stamps: ArrayRef = Arc::new(
        TimestampMillisecondArray::from(vec![
            t0().timestamp_millis(),
            t0().timestamp_millis() + 3_600_000,
            t0().timestamp_millis() + 10_800_000,
        ])
        .with_timezone("UTC"),
    );
    let values: ArrayRef = Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0]));
    let schema = Schema::new_with_metadata(
        vec![
            Field::new("timestamp", stamps.data_type().clone(), false),
            Field::new("value", values.data_type().clone(), false),
        ],
        footer.into_iter().collect(),
    );
    let batch = RecordBatch::try_new(Arc::new(schema), vec![stamps, values]).unwrap();
    let path = dir.path().join("offgrid.parquet");
    let file = std::fs::File::create(&path).unwrap();
    let mut writer = ArrowWriter::try_new(file, batch.schema(), None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();

    let err = read_series(&path, &ImportOptions::default()).expect_err("row 2 is off the grid");
    assert!(err.to_string().contains("grid"), "{err}");
}
