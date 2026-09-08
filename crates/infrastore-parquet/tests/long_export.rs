//! Partitioned long-table export of the three static types.
//!
//! The properties under test are the ones the format rests on: every column is
//! required, one file never mixes value types or zones, each series' rows are
//! contiguous and sorted, and the filename is safe on every platform CI runs on.

use std::collections::BTreeSet;
use std::path::Path;

use arrow::array::{Array, RecordBatch};
use arrow::datatypes::{DataType, TimeUnit};
use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    Dtype, ElementType, FeatureValue, Features, NonSequentialTimeSeries, OwnerCategory, Period,
    PersistentTimeSeries, SingleTimeSeries, Store, TimeReference, TimeSeriesData,
    TimeSeriesMetadata, TimeSeriesType, TypedArray, create_store,
};
use infrastore_parquet::partition::{PartitionKey, ValueKind, disambiguate, sanitize};
use infrastore_parquet::table;
use infrastore_parquet::write_partitions;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

/// An hourly UTC series. The reference is declared, because every real writer
/// infers one -- Python from `tzinfo`, the CLI from the text -- and a series
/// that declares none is its own partition, which
/// `the_unspecified_reference_is_its_own_partition` covers on purpose.
fn hourly(name: &str, values: &[f64]) -> SingleTimeSeries {
    let mut series = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![values.len()], values),
        name,
    );
    series.time_reference = Some(TimeReference::Utc);
    series
}

/// Stamp a value with a UTC reference, for the fixtures built directly.
fn utc(data: TimeSeriesData) -> TimeSeriesData {
    match data {
        TimeSeriesData::SingleTimeSeries(mut s) => {
            s.time_reference = Some(TimeReference::Utc);
            TimeSeriesData::SingleTimeSeries(s)
        }
        TimeSeriesData::NonSequentialTimeSeries(mut s) => {
            s.time_reference = Some(TimeReference::Utc);
            TimeSeriesData::NonSequentialTimeSeries(s)
        }
        TimeSeriesData::PersistentTimeSeries(mut s) => {
            s.time_reference = Some(TimeReference::Utc);
            TimeSeriesData::PersistentTimeSeries(s)
        }
        other => other,
    }
}

/// Add every series to one store and hand back the `(row, values)` pairs the
/// export takes, so what is exported is what a real read produces.
fn stored(items: Vec<(i64, TimeSeriesData)>) -> Vec<(TimeSeriesMetadata, TimeSeriesData)> {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    read_back(&mut store, items)
}

fn read_back(
    store: &mut Store,
    items: Vec<(i64, TimeSeriesData)>,
) -> Vec<(TimeSeriesMetadata, TimeSeriesData)> {
    let mut out = Vec::new();
    for (owner, data) in items {
        let id = store
            .add_time_series(
                owner,
                "Generator",
                OwnerCategory::Component,
                data,
                Features::new(),
            )
            .expect("the series should be added");
        let row = store
            .get_metadata_by_id(id)
            .expect("the lookup should succeed")
            .expect("the row was just written");
        let values = store
            .read_by_id(id, infrastore_core::ReadWindow::full())
            .expect("the read should succeed");
        out.push((row, values));
    }
    out
}

fn read_file(path: &Path) -> (RecordBatch, std::collections::HashMap<String, String>) {
    let file = std::fs::File::open(path).expect("the file should open");
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).expect("a valid Parquet file");
    let schema = builder.schema().clone();
    let batches: Vec<RecordBatch> = builder
        .build()
        .expect("the reader should build")
        .collect::<Result<_, _>>()
        .expect("every batch should read");
    let batch = arrow::compute::concat_batches(&schema, &batches).expect("concat");
    (batch, schema.metadata().clone())
}

/// The two file names a partition stem produces.
fn values_and_series(stem: &str) -> [String; 2] {
    [
        format!("{stem}.values.parquet"),
        format!("{stem}.series.parquet"),
    ]
}

/// One partition's two halves, read whole.
fn read_partition(
    partition: &infrastore_parquet::WrittenPartition,
) -> (
    RecordBatch,
    RecordBatch,
    std::collections::HashMap<String, String>,
) {
    let (values, footer) = read_file(&partition.values_path);
    let (series, _) = read_file(&partition.series_path);
    (values, series, footer)
}

/// The `timestamp` column's type. Not field 0 any more: the array key leads.
fn stamp_type(batch: &RecordBatch) -> arrow::datatypes::DataType {
    batch
        .schema()
        .field_with_name("timestamp")
        .expect("a timestamp column")
        .data_type()
        .clone()
}

fn stamps(batch: &RecordBatch, name: &str) -> Vec<i64> {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("column {name}"))
        .as_any()
        .downcast_ref::<arrow::array::TimestampMillisecondArray>()
        .expect("a millisecond timestamp column")
        .values()
        .to_vec()
}

fn ints(batch: &RecordBatch, name: &str) -> Vec<i64> {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("column {name}"))
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .expect("an int64 column")
        .values()
        .to_vec()
}

fn file_names(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .expect("the directory should exist")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect()
}

fn strings(batch: &RecordBatch, name: &str) -> Vec<String> {
    let column = batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("column {name}"));
    let array = column
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .expect("a string column");
    (0..array.len())
        .map(|i| array.value(i).to_string())
        .collect()
}

#[test]
fn one_file_holds_many_series() {
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(vec![
        (
            1,
            TimeSeriesData::SingleTimeSeries(hourly("load", &[1.0, 2.0])),
        ),
        (
            2,
            TimeSeriesData::SingleTimeSeries(hourly("load", &[3.0, 4.0])),
        ),
        (
            3,
            TimeSeriesData::SingleTimeSeries(hourly("load", &[5.0, 6.0])),
        ),
    ]);
    let report = write_partitions(dir.path(), &series).expect("the export should succeed");

    assert_eq!(report.partitions.len(), 1, "one partition, one file pair");
    assert_eq!(report.partitions[0].series, 3);
    assert_eq!(report.partitions[0].arrays, 3, "three distinct profiles");
    assert_eq!(report.rows(), 6);
    assert_eq!(
        file_names(dir.path()),
        BTreeSet::from(values_and_series("SingleTimeSeries.f64.utc"))
    );

    let (values, series_rows, _) = read_partition(&report.partitions[0]);
    // The values file holds the arrays and the key that names them, and nothing
    // about who owns them.
    assert_eq!(values.num_rows(), 6);
    assert!(values.column_by_name("owner_id").is_none());
    assert_eq!(series_rows.num_rows(), 3, "one row per series");
    assert_eq!(ints(&series_rows, "owner_id"), vec![1, 2, 3]);
    // Every series row names an array the values file holds.
    let keys: BTreeSet<String> = strings(&values, "data_hash").into_iter().collect();
    for hash in strings(&series_rows, "data_hash") {
        assert!(keys.contains(&hash), "{hash} is in no values group");
    }
}

#[test]
fn one_array_shared_by_many_series_is_written_once() {
    // The whole reason for normalizing: the store holds one array for a
    // thousand components on one profile, and so does the export.
    let dir = tempfile::tempdir().expect("tempdir");
    let shared: Vec<f64> = (0..24).map(|i| i as f64).collect();
    let series = stored(
        (1..=200)
            .map(|owner| {
                (
                    owner,
                    TimeSeriesData::SingleTimeSeries(hourly("load", &shared)),
                )
            })
            .collect(),
    );
    let report = write_partitions(dir.path(), &series).expect("export");

    assert_eq!(report.partitions[0].series, 200);
    assert_eq!(report.partitions[0].arrays, 1, "one array, not two hundred");
    assert_eq!(report.rows(), 24, "24 values, not 4800");

    let (values, series_rows, _) = read_partition(&report.partitions[0]);
    assert_eq!(values.num_rows(), 24);
    assert_eq!(series_rows.num_rows(), 200);
    let distinct: BTreeSet<String> = strings(&values, "data_hash").into_iter().collect();
    assert_eq!(distinct.len(), 1);
}

#[test]
fn every_column_is_required() {
    // The whole reason for partitioning: no nullable columns anywhere.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(vec![(
        1,
        TimeSeriesData::SingleTimeSeries(hourly("load", &[1.0, 2.0])),
    )]);
    let report = write_partitions(dir.path(), &series).expect("the export should succeed");
    let (batch, _) = read_file(&report.partitions[0].values_path);
    for field in batch.schema().fields() {
        assert!(!field.is_nullable(), "column {} is nullable", field.name());
        assert_eq!(batch.column_by_name(field.name()).unwrap().null_count(), 0);
    }
}

#[test]
fn the_catalog_row_becomes_columns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut inner = hourly("load", &[1.0, 2.0]);
    inner.units = Some("MW".into());
    inner.quantity_kind = Some("ActivePower".into());
    inner.unit_system = Some(infrastore_core::UnitSystem::NaturalUnits);
    inner.component_field = Some("max_active_power".into());
    inner.application_data = Some(r#"{"k":1}"#.into());

    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let mut features = Features::new();
    features.insert("model_year".into(), FeatureValue::Int(2030));
    let id = store
        .add_time_series(
            42,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(inner),
            features,
        )
        .expect("the series should be added");
    let row = store.get_metadata_by_id(id).unwrap().unwrap();
    let values = store
        .read_by_id(id, infrastore_core::ReadWindow::full())
        .unwrap();

    let report = write_partitions(dir.path(), &[(row, values)]).expect("export");
    let (values_rows, batch, footer) = read_partition(&report.partitions[0]);
    // One row per series, not per value: that is the point of the split.
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(values_rows.num_rows(), 2);

    assert_eq!(strings(&batch, "owner_type"), vec!["Generator"; 1]);
    assert_eq!(strings(&batch, "owner_category"), vec!["Component"; 1]);
    assert_eq!(
        strings(&batch, "time_series_type"),
        vec!["SingleTimeSeries"; 1]
    );
    assert_eq!(strings(&batch, "name"), vec!["load"; 1]);
    assert_eq!(strings(&batch, "resolution"), vec!["PT1H"; 1]);
    assert_eq!(
        strings(&batch, "features"),
        vec![r#"{"model_year":2030}"#; 1]
    );
    assert_eq!(strings(&batch, "element_type"), vec!["f64"; 1]);
    assert_eq!(strings(&batch, "element_shape"), vec!["[]"; 1]);
    assert_eq!(strings(&batch, "time_reference"), vec!["utc"; 1]);
    assert_eq!(strings(&batch, "units"), vec!["MW"; 1]);
    assert_eq!(strings(&batch, "quantity_kind"), vec!["ActivePower"; 1]);
    assert_eq!(strings(&batch, "unit_system"), vec!["natural_units"; 1]);
    assert_eq!(
        strings(&batch, "component_field"),
        vec!["max_active_power"; 1]
    );
    assert_eq!(strings(&batch, "application_data"), vec![r#"{"k":1}"#; 1]);
    // `initial_timestamp` and `length` are also inside `time_axis`; they are
    // real columns so a reader need not parse one.
    assert_eq!(ints(&batch, "length"), vec![2]);
    assert_eq!(
        strings(&batch, "time_axis"),
        vec!["R2/2024-01-01T00:00:00Z/PT1H".to_string()]
    );
    // The id travels for provenance; the import ignores it.
    assert_eq!(ints(&batch, "id"), vec![1]);

    // The footer states the partition exactly, since the filename does not.
    assert_eq!(footer[table::FORMAT], table::FORMAT_V1);
    assert_eq!(footer[table::ROWS_CONTIGUOUS], "true");
    assert_eq!(footer["time_series_type"], "SingleTimeSeries");
    assert_eq!(footer["element_type"], "f64");
    assert_eq!(footer["element_shape"], "[]");
    assert_eq!(footer["time_reference"], "utc");
}

#[test]
fn an_absent_descriptor_is_the_empty_string() {
    // Not a null: keeping every column required is what the partitioning buys.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(vec![(
        1,
        TimeSeriesData::SingleTimeSeries(hourly("load", &[1.0])),
    )]);
    let report = write_partitions(dir.path(), &series).expect("export");
    let (_, batch, _) = read_partition(&report.partitions[0]);
    for name in [
        "units",
        "quantity_kind",
        "unit_system",
        "component_field",
        "application_data",
    ] {
        assert_eq!(strings(&batch, name), vec![""], "{name}");
    }
}

#[test]
fn the_partition_splits_by_type_value_and_reference() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut zoned = hourly("zoned", &[1.0, 2.0]);
    zoned.time_reference = Some(TimeReference::Zone("America/Denver".into()));
    let wide = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        "wide",
    );
    let ints = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_slice(vec![2], &[1i64, 2]).unwrap(),
        "ints",
    );
    let irregular = NonSequentialTimeSeries::new(
        vec![t0(), t0() + Duration::hours(3)],
        TypedArray::from_f64(vec![2], &[1.0, 2.0]),
        "irregular",
    )
    .unwrap();

    let series = stored(vec![
        (
            1,
            TimeSeriesData::SingleTimeSeries(hourly("load", &[1.0, 2.0])),
        ),
        (2, TimeSeriesData::SingleTimeSeries(zoned)),
        (3, utc(TimeSeriesData::SingleTimeSeries(wide))),
        (4, utc(TimeSeriesData::SingleTimeSeries(ints))),
        (5, utc(TimeSeriesData::NonSequentialTimeSeries(irregular))),
    ]);
    let report = write_partitions(dir.path(), &series).expect("export");

    assert_eq!(
        report.partitions.len(),
        5,
        "every axis of the triple splits"
    );
    assert_eq!(
        file_names(dir.path()),
        [
            "SingleTimeSeries.f64.utc",
            "SingleTimeSeries.f64.America_Denver",
            "SingleTimeSeries.f64_3.utc",
            "SingleTimeSeries.i64.utc",
            "NonSequentialTimeSeries.f64.utc",
        ]
        .iter()
        .flat_map(|stem| values_and_series(stem))
        .collect::<BTreeSet<String>>()
    );

    // A zoned file's timestamp column states the zone; an irregular file has no
    // `resolution` column at all, because its type never carries one.
    let zoned_path = dir
        .path()
        .join("SingleTimeSeries.f64.America_Denver.values.parquet");
    let (batch, _) = read_file(&zoned_path);
    assert_eq!(
        stamp_type(&batch),
        DataType::Timestamp(TimeUnit::Millisecond, Some("America/Denver".into()))
    );
    let irregular_path = dir
        .path()
        .join("NonSequentialTimeSeries.f64.utc.values.parquet");
    let (batch, _) = read_file(&irregular_path);
    assert!(batch.column_by_name("resolution").is_none());
}

#[test]
fn a_persistent_series_is_told_from_an_irregular_one_by_its_partition() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stamps = vec![t0(), t0() + Duration::hours(3)];
    let values = TypedArray::from_f64(vec![2], &[1.0, 2.0]);
    let series = stored(vec![
        (
            1,
            utc(TimeSeriesData::NonSequentialTimeSeries(
                NonSequentialTimeSeries::new(stamps.clone(), values.clone(), "irregular").unwrap(),
            )),
        ),
        (
            2,
            utc(TimeSeriesData::PersistentTimeSeries(
                PersistentTimeSeries::new(stamps, values, "steps").unwrap(),
            )),
        ),
    ]);
    let report = write_partitions(dir.path(), &series).expect("export");
    assert_eq!(
        report.partitions.len(),
        2,
        "the two share a shape, not a file"
    );
    assert!(
        file_names(dir.path()).contains("PersistentTimeSeries.f64.utc.values.parquet"),
        "{:?}",
        file_names(dir.path())
    );
}

#[test]
fn composites_share_a_file_and_are_repadded_to_its_widest() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Two curves of different widths: `[n, x1, y1, ...]` padded per series.
    let narrow = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![1, 3], &[1.0, 0.0, 5.0]),
        "narrow",
    )
    .with_element_type(ElementType::PiecewiseLinear);
    let wide = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![1, 5], &[2.0, 0.0, 5.0, 1.0, 7.0]),
        "wide",
    )
    .with_element_type(ElementType::PiecewiseLinear);

    let series = stored(vec![
        (1, utc(TimeSeriesData::SingleTimeSeries(narrow))),
        (2, utc(TimeSeriesData::SingleTimeSeries(wide))),
    ]);
    let report = write_partitions(dir.path(), &series).expect("export");

    assert_eq!(
        report.partitions.len(),
        1,
        "a composite kind partitions by kind"
    );
    assert_eq!(
        file_names(dir.path()),
        BTreeSet::from(values_and_series("SingleTimeSeries.piecewise_linear.utc"))
    );
    let (batch, footer) = read_file(&report.partitions[0].values_path);
    assert_eq!(footer["element_shape"], "[5]", "the file's widest");
    let list = batch
        .column_by_name("value")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow::array::FixedSizeListArray>()
        .expect("a fixed-size list");
    assert_eq!(list.value_length(), 5);
    // The narrow row was padded with zeros; its leading `n` still says one point.
    let flat = list
        .values()
        .as_any()
        .downcast_ref::<arrow::array::Float64Array>()
        .unwrap();
    assert_eq!(
        (0..5).map(|i| flat.value(i)).collect::<Vec<_>>(),
        vec![1.0, 0.0, 5.0, 0.0, 0.0]
    );
}

#[test]
fn an_empty_series_fails_the_export_and_writes_nothing() {
    // A values file has one row per value, so an empty series would be a series
    // row with no values group -- which is also what a truncated file looks
    // like. Refusing here keeps the import's rule simple, and refusing *before*
    // anything is written leaves the destination as it was found.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(vec![
        (1, TimeSeriesData::SingleTimeSeries(hourly("empty", &[]))),
        (2, TimeSeriesData::SingleTimeSeries(hourly("load", &[1.0]))),
        (
            3,
            TimeSeriesData::SingleTimeSeries(hourly("also_empty", &[])),
        ),
    ]);
    let err = write_partitions(dir.path(), &series).expect_err("an empty series is refused");
    let message = err.to_string();
    // Every one of them, so the caller fixes the selection once rather than
    // running into them one at a time.
    assert!(message.contains("'empty'"), "{message}");
    assert!(message.contains("'also_empty'"), "{message}");
    assert!(message.contains("Narrow the selection"), "{message}");
    assert!(
        file_names(dir.path()).is_empty(),
        "nothing may be written: {:?}",
        file_names(dir.path())
    );
}

#[test]
fn rows_are_contiguous_and_sorted_by_time() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stamps = vec![t0() + Duration::hours(5), t0(), t0() + Duration::hours(2)];
    // The store keeps an irregular series' own order, which must be increasing;
    // a `SingleTimeSeries` walks its grid. Either way the file is time-sorted.
    let sorted = {
        let mut s = stamps.clone();
        s.sort();
        s
    };
    let series = stored(vec![
        (
            1,
            utc(TimeSeriesData::NonSequentialTimeSeries(
                NonSequentialTimeSeries::new(
                    sorted.clone(),
                    TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]),
                    "a",
                )
                .unwrap(),
            )),
        ),
        (
            2,
            utc(TimeSeriesData::NonSequentialTimeSeries(
                NonSequentialTimeSeries::new(
                    sorted.clone(),
                    TypedArray::from_f64(vec![3], &[4.0, 5.0, 6.0]),
                    "b",
                )
                .unwrap(),
            )),
        ),
    ]);
    let report = write_partitions(dir.path(), &series).expect("export");
    let (batch, series_rows, footer) = read_partition(&report.partitions[0]);
    assert_eq!(footer[table::ROWS_CONTIGUOUS], "true");

    let stamps: Vec<i64> = batch
        .column_by_name("timestamp")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow::array::TimestampMillisecondArray>()
        .unwrap()
        .values()
        .to_vec();
    // Two series, two different sets of values, so two array keys -- contiguous
    // and in key order, with the rows inside each sorted by time.
    let keys = strings(&batch, "data_hash");
    assert_eq!(keys.len(), 6);
    assert_eq!(keys[0], keys[1]);
    assert_eq!(keys[1], keys[2]);
    assert_ne!(keys[2], keys[3], "a new array starts here");
    assert_eq!(keys[3], keys[5]);
    assert!(keys[0] < keys[3], "keys ascend");
    assert!(stamps[..3].windows(2).all(|w| w[0] < w[1]), "sorted within");
    assert!(stamps[3..].windows(2).all(|w| w[0] < w[1]), "sorted within");
    // The series file is sorted the same way, so a reader walks both together.
    assert_eq!(
        strings(&series_rows, "data_hash"),
        vec![keys[0].clone(), keys[3].clone()]
    );
}

#[test]
fn the_unspecified_reference_is_its_own_partition() {
    let dir = tempfile::tempdir().expect("tempdir");
    let declared = hourly("declared", &[1.0]);
    assert_eq!(declared.time_reference, Some(TimeReference::Utc));
    // Built without the helper, which declares UTC: this one declares nothing,
    // which is a different claim and so a different partition.
    let undeclared = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![1], &[1.0]),
        "undeclared",
    );
    assert_eq!(undeclared.time_reference, None);

    let series = stored(vec![
        (1, TimeSeriesData::SingleTimeSeries(declared)),
        (2, TimeSeriesData::SingleTimeSeries(undeclared)),
    ]);
    let report = write_partitions(dir.path(), &series).expect("export");
    assert_eq!(
        report.partitions.len(),
        2,
        "a series that declared nothing must not pool with one that declared UTC"
    );
    let names = file_names(dir.path());
    assert!(
        names.contains("SingleTimeSeries.f64.utc.values.parquet"),
        "{names:?}"
    );
    assert!(
        names.contains("SingleTimeSeries.f64.unspecified.values.parquet"),
        "{names:?}"
    );

    // Both write a UTC-zoned column -- Arrow has no third spelling -- and the
    // column is what keeps the claim honest.
    let (batch, footer) = read_file(
        &dir.path()
            .join("SingleTimeSeries.f64.unspecified.values.parquet"),
    );
    assert_eq!(
        stamp_type(&batch),
        DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()))
    );
    assert_eq!(footer["time_reference"], "unspecified");
    let (_, series_rows, _) = read_partition(
        report
            .partitions
            .iter()
            .find(|p| p.reference == "unspecified")
            .unwrap(),
    );
    assert_eq!(strings(&series_rows, "time_reference"), vec!["unspecified"]);
}

// ---- Slugs -----------------------------------------------------------------

#[test]
fn a_slug_is_a_legal_path_component_everywhere() {
    // Linux forbids only `/` and NUL; macOS adds `:` in some layers; Windows
    // forbids `<>:"/\|?*`, control characters, a trailing dot or space, and the
    // device names. A slug has to clear all three.
    let hostile = [
        "America/Denver",
        "-07:00",
        "a<b>c",
        "q\"r",
        "back\\slash",
        "pipe|bar",
        "what?",
        "star*",
        "tab\there",
        "trailing.",
        "",
        "CON",
        "nul",
        "lpt9",
        "Ünïcødé/zone",
    ];
    for raw in hostile {
        let slug = sanitize(raw);
        assert!(!slug.is_empty(), "{raw:?} slugged to nothing");
        assert!(
            slug.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'),
            "{raw:?} -> {slug:?} keeps an unsafe character"
        );
        assert!(
            !slug.trim_matches('.').is_empty(),
            "{raw:?} -> {slug:?} is all dots"
        );
        assert!(
            !slug.ends_with(' ') && !slug.ends_with('.'),
            "{raw:?} -> {slug:?} ends with a character Windows strips"
        );
        let stem = slug.trim_matches('.').to_ascii_uppercase();
        for device in ["CON", "PRN", "AUX", "NUL", "COM1", "LPT9"] {
            assert_ne!(stem, device, "{raw:?} -> {slug:?} is a Windows device name");
        }
    }
}

#[test]
fn a_slug_is_one_way_and_the_footer_is_the_truth() {
    // Two zones that flatten alike are a real possibility, which is why nothing
    // parses a partition back out of a filename.
    assert_eq!(sanitize("a/b"), sanitize("a_b"));
}

#[test]
fn a_directory_that_already_holds_parquet_files_is_refused() {
    // `add --parquet <dir>` imports every file it finds, so a narrower export
    // over an earlier one would leave stale partitions for the import to file.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(vec![(
        1,
        TimeSeriesData::SingleTimeSeries(hourly("load", &[1.0, 2.0])),
    )]);
    write_partitions(dir.path(), &series).expect("first export");
    let err = write_partitions(dir.path(), &series).expect_err("second export into the same dir");
    assert!(err.to_string().contains("already holds"), "{err}");
    assert!(err.to_string().contains(".parquet"), "{err}");
    // A sibling non-parquet file is not in the way.
    let clean = tempfile::tempdir().expect("tempdir");
    std::fs::write(clean.path().join("notes.txt"), b"x").unwrap();
    write_partitions(clean.path(), &series).expect("a stray text file does not block the export");
}

#[test]
fn row_groups_are_cut_on_series_boundaries() {
    // 900k rows of one series then 200k of another: the second must not be
    // split 100k/100k across the target just because the first fell short of
    // it. The first group is cut at the boundary, the second holds the rest.
    let dir = tempfile::tempdir().expect("tempdir");
    let big: Vec<f64> = (0..900_000).map(|i| i as f64).collect();
    let small: Vec<f64> = (0..200_000).map(|i| -(i as f64)).collect();
    let series = stored(vec![
        (1, TimeSeriesData::SingleTimeSeries(hourly("a", &big))),
        (2, TimeSeriesData::SingleTimeSeries(hourly("b", &small))),
    ]);
    let report = write_partitions(dir.path(), &series).expect("export");
    assert_eq!(report.partitions.len(), 1);
    let file = std::fs::File::open(&report.partitions[0].values_path).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    let rows: Vec<i64> = builder
        .metadata()
        .row_groups()
        .iter()
        .map(|g| g.num_rows())
        .collect();
    assert_eq!(rows, vec![900_000, 200_000], "{rows:?}");
}

#[test]
fn a_series_larger_than_the_target_is_the_only_one_split() {
    let dir = tempfile::tempdir().expect("tempdir");
    let small: Vec<f64> = (0..10).map(|i| i as f64).collect();
    let huge: Vec<f64> = (0..1_500_000).map(|i| i as f64).collect();
    let series = stored(vec![
        (1, TimeSeriesData::SingleTimeSeries(hourly("a", &small))),
        (2, TimeSeriesData::SingleTimeSeries(hourly("b", &huge))),
    ]);
    let report = write_partitions(dir.path(), &series).expect("export");
    let file = std::fs::File::open(&report.partitions[0].values_path).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    let rows: Vec<i64> = builder
        .metadata()
        .row_groups()
        .iter()
        .map(|g| g.num_rows())
        .collect();
    // The ten-row series gets a group of its own rather than sharing one with
    // the first tenth of the next; the huge one is split at the target.
    assert_eq!(rows, vec![10, 1_000_000, 500_000], "{rows:?}");
}

#[test]
fn colliding_partitions_get_distinct_files() {
    // The consequence of a one-way slug: without this the second file would
    // silently overwrite the first.
    let key = |zone: &str| PartitionKey {
        time_series_type: TimeSeriesType::SingleTimeSeries,
        value_kind: ValueKind::Dense {
            dtype: Dtype::F64,
            shape: vec![],
        },
        time_reference: Some(TimeReference::Zone(zone.into())),
    };
    let keys = vec![key("a/b"), key("a_b")];
    let names = disambiguate(&keys);
    let distinct: BTreeSet<&String> = names.values().collect();
    assert_eq!(distinct.len(), 2, "{names:?}");
    assert!(
        names.values().all(|n| !n.ends_with(".parquet")),
        "a stem, not a file name: {names:?}"
    );
}

#[test]
fn partitions_that_share_a_slug_are_still_two_partitions() {
    // `Ord` must agree with `Eq`: the zones `a/b` and `a_b` flatten to one
    // filename, but they want different timestamp columns, and a BTreeMap keyed
    // on a slug-derived order would pool them into one file.
    let key = |zone: &str| PartitionKey {
        time_series_type: TimeSeriesType::SingleTimeSeries,
        value_kind: ValueKind::Dense {
            dtype: Dtype::F64,
            shape: vec![],
        },
        time_reference: Some(TimeReference::Zone(zone.into())),
    };
    assert_ne!(key("a/b").cmp(&key("a_b")), std::cmp::Ordering::Equal);
    let set: BTreeSet<PartitionKey> = [key("a/b"), key("a_b")].into_iter().collect();
    assert_eq!(set.len(), 2);
}

#[test]
fn a_collision_suffix_is_itself_reserved() {
    // Two keys slug to `...a_b...`; the second is moved to `_2`. A third key
    // whose natural slug is already the `_2` name must not overwrite it.
    let key = |zone: &str| PartitionKey {
        time_series_type: TimeSeriesType::SingleTimeSeries,
        value_kind: ValueKind::Dense {
            dtype: Dtype::F64,
            shape: vec![],
        },
        time_reference: Some(TimeReference::Zone(zone.into())),
    };
    // The zone `a_b_2` slugs to exactly the name the collision suffix produces.
    let natural_2 = key("a_b_2").stem();
    let keys = vec![key("a/b"), key("a_b"), key("a_b_2")];
    let names = disambiguate(&keys);
    let distinct: BTreeSet<&String> = names.values().collect();
    assert_eq!(distinct.len(), 3, "{names:?}");
    assert_eq!(
        names.values().filter(|n| **n == natural_2).count(),
        1,
        "{names:?}"
    );
}

#[test]
fn naming_is_stable_across_runs() {
    let key = |zone: &str| PartitionKey {
        time_series_type: TimeSeriesType::SingleTimeSeries,
        value_kind: ValueKind::Dense {
            dtype: Dtype::F64,
            shape: vec![],
        },
        time_reference: Some(TimeReference::Zone(zone.into())),
    };
    let forward = disambiguate(&[key("a/b"), key("a_b")]);
    let backward = disambiguate(&[key("a_b"), key("a/b")]);
    assert_eq!(forward, backward, "input order must not change the names");
}

#[test]
fn value_slugs_avoid_the_canonical_punctuation() {
    // `tuple(3,f64)` and `[2, 3]` are the canonical spellings and both carry
    // characters a filename should not.
    assert_eq!(
        ValueKind::Tuple {
            arity: 3,
            dtype: Dtype::F64
        }
        .slug(),
        "tuple3_f64"
    );
    assert_eq!(
        ValueKind::Dense {
            dtype: Dtype::F64,
            shape: vec![2, 3]
        }
        .slug(),
        "f64_2x3"
    );
    assert_eq!(
        ValueKind::Composite(ElementType::PiecewiseLinear).slug(),
        "piecewise_linear"
    );
}

#[test]
fn an_offset_reference_slugs_without_a_colon_or_a_leading_dash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut series = hourly("load", &[1.0]);
    series.time_reference = Some(TimeReference::FixedOffset(-420));
    let stored = stored(vec![(1, TimeSeriesData::SingleTimeSeries(series))]);
    let report = write_partitions(dir.path(), &stored).expect("export");

    let name = report.partitions[0]
        .values_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        name,
        "SingleTimeSeries.f64.offset_minus07_00.values.parquet"
    );
    // The column still carries the real offset.
    let (batch, footer) = read_file(&report.partitions[0].values_path);
    assert_eq!(footer["time_reference"], "-07:00");
    assert_eq!(
        stamp_type(&batch),
        DataType::Timestamp(TimeUnit::Millisecond, Some("-07:00".into()))
    );
}

#[test]
fn a_monthly_grid_is_materialized_on_the_calendar() {
    // The case a start-plus-k-times-resolution timestamp column gets wrong.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = SingleTimeSeries::new(
        Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap(),
        Period::Months(1),
        TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]),
        "monthly",
    );
    let stored = stored(vec![(1, utc(TimeSeriesData::SingleTimeSeries(series)))]);
    let report = write_partitions(dir.path(), &stored).expect("export");
    let (batch, _) = read_file(&report.partitions[0].values_path);
    let expected: Vec<i64> = [(2024, 1, 31), (2024, 2, 29), (2024, 3, 31)]
        .iter()
        .map(|(y, m, d)| {
            Utc.with_ymd_and_hms(*y, *m, *d, 0, 0, 0)
                .unwrap()
                .timestamp_millis()
        })
        .collect();
    assert_eq!(stamps(&batch, "timestamp"), expected);
}

#[test]
fn a_forecast_gets_key_columns_a_static_series_does_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut forecast = infrastore_core::Deterministic::new(
        t0(),
        Duration::hours(1),
        Duration::hours(2),
        Duration::hours(1),
        2,
        TypedArray::from_f64(vec![2, 2], &[1.0, 2.0, 3.0, 4.0]),
        "fc",
    )
    .expect("the forecast should build");
    forecast.time_reference = Some(TimeReference::Utc);
    let series = stored(vec![(1, TimeSeriesData::Deterministic(forecast))]);
    let report = write_partitions(dir.path(), &series).expect("export");

    assert_eq!(
        file_names(dir.path()),
        BTreeSet::from(values_and_series("Deterministic.f64.utc"))
    );
    let (batch, footer) = read_file(&report.partitions[0].values_path);
    let columns: Vec<String> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    // The array key leads, then the time columns. `issue_time` is why a forecast
    // cannot share a values file with a static series.
    assert_eq!(columns[0], "data_hash");
    assert_eq!(columns[1], "time_axis");
    assert_eq!(columns[2], "timestamp");
    assert_eq!(columns[3], "issue_time");
    // `interval` and `horizon` describe the series, so they are in the other
    // half -- but the values file's `time_axis` still carries them, because two
    // forecasts sharing an array and an anchor but not a horizon have different
    // target-time rows.
    assert!(!columns.contains(&"interval".to_string()));
    let (_, series_rows, _) = read_partition(&report.partitions[0]);
    assert_eq!(strings(&series_rows, "interval"), vec!["PT1H"]);
    assert_eq!(strings(&series_rows, "horizon"), vec!["PT2H"]);
    assert_eq!(ints(&series_rows, "count"), vec![2]);
    assert_eq!(
        strings(&series_rows, "time_axis"),
        vec!["R2/2024-01-01T00:00:00Z/PT1H/PT2H/PT1H".to_string()]
    );
    assert_eq!(batch.num_rows(), 4, "2 windows x 2 steps");
    assert_eq!(footer["time_series_type"], "Deterministic");

    // Window-major, so `GROUP BY issue_time` scans contiguously.
    let ms = |h: i64| (t0() + Duration::hours(h)).timestamp_millis();
    assert_eq!(
        stamps(&batch, "issue_time"),
        vec![ms(0), ms(0), ms(1), ms(1)]
    );
    assert_eq!(
        stamps(&batch, "timestamp"),
        vec![ms(0), ms(1), ms(1), ms(2)]
    );
    // `[H, count]` gathered into window-major order.
    assert_eq!(
        batch
            .column_by_name("value")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::Float64Array>()
            .unwrap()
            .values()
            .to_vec(),
        vec![1.0, 3.0, 2.0, 4.0]
    );
}

#[test]
fn the_lane_column_names_the_third_axis() {
    let dir = tempfile::tempdir().expect("tempdir");
    let values: Vec<f64> = (0..8).map(|i| i as f64).collect();
    let mut prob = infrastore_core::Probabilistic::new(
        t0(),
        Duration::hours(1),
        Duration::hours(2),
        Duration::hours(1),
        2,
        vec![0.1, 0.9],
        TypedArray::from_f64(vec![2, 2, 2], &values),
        "prob",
    )
    .expect("the forecast should build");
    prob.time_reference = Some(TimeReference::Utc);
    let mut scen = infrastore_core::Scenarios::new(
        t0(),
        Duration::hours(1),
        Duration::hours(2),
        Duration::hours(1),
        2,
        2,
        TypedArray::from_f64(vec![2, 2, 2], &values),
        "scen",
    )
    .expect("the forecast should build");
    scen.time_reference = Some(TimeReference::Utc);

    let series = stored(vec![
        (1, TimeSeriesData::Probabilistic(prob)),
        (2, TimeSeriesData::Scenarios(scen)),
    ]);
    let report = write_partitions(dir.path(), &series).expect("export");
    assert_eq!(report.partitions.len(), 2, "two types, two partitions");

    let (batch, _) = read_file(&dir.path().join("Probabilistic.f64.utc.values.parquet"));
    let percentile = batch
        .column_by_name("percentile")
        .expect("a Probabilistic carries its percentiles")
        .as_any()
        .downcast_ref::<arrow::array::Float64Array>()
        .unwrap();
    // One instant's percentiles sit together, which is how a fan chart reads.
    assert_eq!(percentile.value(0), 0.1);
    assert_eq!(percentile.value(1), 0.9);
    assert!(batch.column_by_name("scenario").is_none());

    let (batch, _) = read_file(&dir.path().join("Scenarios.f64.utc.values.parquet"));
    let scenario = batch
        .column_by_name("scenario")
        .expect("a Scenarios carries its trajectory index")
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .unwrap();
    assert_eq!(scenario.values().to_vec(), vec![0, 1, 0, 1, 0, 1, 0, 1]);
    assert!(batch.column_by_name("percentile").is_none());
}

#[test]
fn a_forecasts_per_step_shape_is_not_the_catalogs_element_shape() {
    // The catalog stores `TypedArray::element_shape` -- everything after the
    // leading axis -- which for a `[H, count, *E]` cube is `[count, *E]`. Using
    // it as the partition's value shape would claim the window count is part of
    // one timestep.
    let dir = tempfile::tempdir().expect("tempdir");
    let values: Vec<f64> = (0..12).map(|i| i as f64).collect();
    let mut forecast = infrastore_core::Deterministic::new(
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
    let series = stored(vec![(1, TimeSeriesData::Deterministic(forecast))]);
    assert_eq!(
        series[0].0.element_shape,
        vec![2, 3],
        "the catalog counts from the wrong axis for a forecast"
    );

    let report = write_partitions(dir.path(), &series).expect("export");
    assert_eq!(
        file_names(dir.path()),
        BTreeSet::from(values_and_series("Deterministic.f64_3.utc")),
        "the partition is keyed on the per-step shape"
    );
    let (batch, footer) = read_file(&report.partitions[0].values_path);
    assert_eq!(footer["element_shape"], "[3]");
    assert_eq!(batch.num_rows(), 4);
}

// ---- The array key ---------------------------------------------------------

/// The `time_axis` a stored series ends up with, without exporting it.
fn axis_of(data: TimeSeriesData) -> String {
    let series = stored(vec![(1, data)]);
    infrastore_parquet::table::time_axis_of(&series[0].0).expect("a time axis")
}

#[test]
fn a_regular_grid_spells_its_axis_as_a_repeating_interval() {
    // ISO 8601's own notation for "n repetitions from here, this far apart",
    // which is exactly what a `SingleTimeSeries` grid is.
    assert_eq!(
        axis_of(utc(TimeSeriesData::SingleTimeSeries(
            SingleTimeSeries::new(
                t0(),
                Duration::hours(1),
                TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]),
                "load",
            )
        ))),
        "R3/2024-01-01T00:00:00Z/PT1H"
    );
    // A calendar period keeps its own spelling, since it is not a duration.
    assert_eq!(
        axis_of(utc(TimeSeriesData::SingleTimeSeries(
            SingleTimeSeries::new(
                Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap(),
                Period::Months(1),
                TypedArray::from_f64(vec![2], &[1.0, 2.0]),
                "monthly",
            )
        ))),
        "R2/2024-01-31T00:00:00Z/P1M"
    );
}

#[test]
fn an_irregular_axis_is_the_catalogs_own_timestamps_hash() {
    // Not a rendering of the timestamps: the catalog's key for the axis, so the
    // column joins against `time_series_readable.timestamps_hash`.
    let stamps = vec![t0(), t0() + Duration::hours(5)];
    let axis = axis_of(utc(TimeSeriesData::NonSequentialTimeSeries(
        NonSequentialTimeSeries::new(
            stamps.clone(),
            TypedArray::from_f64(vec![2], &[1.0, 2.0]),
            "irregular",
        )
        .unwrap(),
    )));
    assert_eq!(
        axis,
        infrastore_core::hash_hex(&infrastore_core::timestamps_hash(&stamps))
    );
    assert_eq!(axis.len(), 64, "hex of a 32-byte hash");
}

#[test]
fn a_forecast_axis_carries_its_horizon_as_well_as_its_interval() {
    // The horizon's own step decides how many `target_time` rows a window has,
    // so two forecasts sharing an array, an anchor and an interval but not a
    // horizon are different tables.
    let forecast = |horizon: Duration| {
        let mut f = infrastore_core::Deterministic::new(
            t0(),
            Duration::hours(1),
            horizon,
            Duration::hours(1),
            2,
            TypedArray::from_f64(
                vec![horizon.num_hours() as usize, 2],
                &vec![1.0; horizon.num_hours() as usize * 2],
            ),
            "fc",
        )
        .expect("the forecast should build");
        f.time_reference = Some(TimeReference::Utc);
        TimeSeriesData::Deterministic(f)
    };
    assert_eq!(
        axis_of(forecast(Duration::hours(2))),
        "R2/2024-01-01T00:00:00Z/PT1H/PT2H/PT1H"
    );
    assert_ne!(
        axis_of(forecast(Duration::hours(2))),
        axis_of(forecast(Duration::hours(3)))
    );
}

#[test]
fn one_array_on_two_anchors_is_two_keys() {
    // `data_hash` covers the bytes and not the axis: the same profile anchored
    // on two years is one stored array with two different timestamp columns, so
    // it must be two values groups.
    let dir = tempfile::tempdir().expect("tempdir");
    let values = [1.0, 2.0, 3.0];
    let anchored = |year: i32| {
        let mut s = SingleTimeSeries::new(
            Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).unwrap(),
            Duration::hours(1),
            TypedArray::from_f64(vec![3], &values),
            "load",
        );
        s.time_reference = Some(TimeReference::Utc);
        TimeSeriesData::SingleTimeSeries(s)
    };
    let series = stored(vec![(1, anchored(2024)), (2, anchored(2025))]);
    let report = write_partitions(dir.path(), &series).expect("export");

    assert_eq!(report.partitions[0].arrays, 2, "same bytes, two axes");
    assert_eq!(report.partitions[0].rows, 6);
    let (values_rows, series_rows, _) = read_partition(&report.partitions[0]);
    // One `data_hash`, two `time_axis` values -- which is why the key is a pair.
    let hashes: BTreeSet<String> = strings(&values_rows, "data_hash").into_iter().collect();
    assert_eq!(hashes.len(), 1);
    let axes: BTreeSet<String> = strings(&series_rows, "time_axis").into_iter().collect();
    assert_eq!(axes.len(), 2, "{axes:?}");
}

#[test]
fn two_irregular_series_on_different_axes_are_two_keys() {
    // The case the project's own docs call out: identical values on different
    // axes share one stored array, and only `timestamps_hash` tells them apart.
    let dir = tempfile::tempdir().expect("tempdir");
    let values = TypedArray::from_f64(vec![2], &[1.0, 2.0]);
    let on = |offset: i64| {
        utc(TimeSeriesData::NonSequentialTimeSeries(
            NonSequentialTimeSeries::new(
                vec![
                    t0() + Duration::hours(offset),
                    t0() + Duration::hours(offset + 5),
                ],
                values.clone(),
                "irregular",
            )
            .unwrap(),
        ))
    };
    let series = stored(vec![(1, on(0)), (2, on(1))]);
    let report = write_partitions(dir.path(), &series).expect("export");
    assert_eq!(report.partitions[0].arrays, 2);
    let (values_rows, _, _) = read_partition(&report.partitions[0]);
    let hashes: BTreeSet<String> = strings(&values_rows, "data_hash").into_iter().collect();
    assert_eq!(hashes.len(), 1, "one stored array");
    let axes: BTreeSet<String> = strings(&values_rows, "time_axis").into_iter().collect();
    assert_eq!(axes.len(), 2, "two axes");
}

#[test]
fn the_two_halves_say_which_they_are() {
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(vec![(
        1,
        TimeSeriesData::SingleTimeSeries(hourly("load", &[1.0])),
    )]);
    let report = write_partitions(dir.path(), &series).expect("export");
    let (_, values_footer) = read_file(&report.partitions[0].values_path);
    let (_, series_footer) = read_file(&report.partitions[0].series_path);
    assert_eq!(values_footer[table::FORMAT], table::FORMAT_V1);
    assert_eq!(values_footer[table::ROLE], table::ROLE_VALUES);
    assert_eq!(series_footer[table::ROLE], table::ROLE_SERIES);
    // Both halves state the partition, so either can be opened alone.
    assert_eq!(
        values_footer["time_reference"],
        series_footer["time_reference"]
    );
}
