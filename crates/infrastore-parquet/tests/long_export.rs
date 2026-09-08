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

    assert_eq!(report.files.len(), 1, "one partition, one file");
    assert_eq!(report.files[0].series, 3);
    assert_eq!(report.rows(), 6);
    assert_eq!(
        file_names(dir.path()),
        BTreeSet::from(["SingleTimeSeries.f64.utc.parquet".to_string()])
    );

    let (batch, _) = read_file(&report.files[0].path);
    assert_eq!(batch.num_rows(), 6);
    // Contiguous per series, and the whole catalog row travels with each value.
    assert_eq!(
        batch
            .column_by_name("owner_id")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap()
            .values()
            .to_vec(),
        vec![1, 1, 2, 2, 3, 3]
    );
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
    let (batch, _) = read_file(&report.files[0].path);
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
    let (batch, footer) = read_file(&report.files[0].path);

    assert_eq!(strings(&batch, "owner_type"), vec!["Generator"; 2]);
    assert_eq!(strings(&batch, "owner_category"), vec!["Component"; 2]);
    assert_eq!(
        strings(&batch, "time_series_type"),
        vec!["SingleTimeSeries"; 2]
    );
    assert_eq!(strings(&batch, "name"), vec!["load"; 2]);
    assert_eq!(strings(&batch, "resolution"), vec!["PT1H"; 2]);
    assert_eq!(
        strings(&batch, "features"),
        vec![r#"{"model_year":2030}"#; 2]
    );
    assert_eq!(strings(&batch, "element_type"), vec!["f64"; 2]);
    assert_eq!(strings(&batch, "time_reference"), vec!["utc"; 2]);
    assert_eq!(strings(&batch, "units"), vec!["MW"; 2]);
    assert_eq!(strings(&batch, "quantity_kind"), vec!["ActivePower"; 2]);
    assert_eq!(strings(&batch, "unit_system"), vec!["natural_units"; 2]);
    assert_eq!(
        strings(&batch, "component_field"),
        vec!["max_active_power"; 2]
    );
    assert_eq!(strings(&batch, "application_data"), vec![r#"{"k":1}"#; 2]);
    // The id travels for provenance; the import ignores it.
    assert_eq!(
        batch
            .column_by_name("id")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap()
            .value(0),
        1
    );

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
    let (batch, _) = read_file(&report.files[0].path);
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

    assert_eq!(report.files.len(), 5, "every axis of the triple splits");
    assert_eq!(
        file_names(dir.path()),
        BTreeSet::from([
            "SingleTimeSeries.f64.utc.parquet".to_string(),
            "SingleTimeSeries.f64.America_Denver.parquet".to_string(),
            "SingleTimeSeries.f64_3.utc.parquet".to_string(),
            "SingleTimeSeries.i64.utc.parquet".to_string(),
            "NonSequentialTimeSeries.f64.utc.parquet".to_string(),
        ])
    );

    // A zoned file's timestamp column states the zone; an irregular file has no
    // `resolution` column at all, because its type never carries one.
    let zoned_path = dir
        .path()
        .join("SingleTimeSeries.f64.America_Denver.parquet");
    let (batch, _) = read_file(&zoned_path);
    assert_eq!(
        *batch.schema().field(0).data_type(),
        DataType::Timestamp(TimeUnit::Millisecond, Some("America/Denver".into()))
    );
    let irregular_path = dir.path().join("NonSequentialTimeSeries.f64.utc.parquet");
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
    assert_eq!(report.files.len(), 2, "the two share a shape, not a file");
    assert!(
        file_names(dir.path()).contains("PersistentTimeSeries.f64.utc.parquet"),
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

    assert_eq!(report.files.len(), 1, "a composite kind partitions by kind");
    assert_eq!(
        file_names(dir.path()),
        BTreeSet::from(["SingleTimeSeries.piecewise_linear.utc.parquet".to_string()])
    );
    let (batch, footer) = read_file(&report.files[0].path);
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
fn an_empty_series_is_warned_about_rather_than_written() {
    // A long table has one row per value, so a series with no values has no
    // rows to contribute.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(vec![
        (1, TimeSeriesData::SingleTimeSeries(hourly("empty", &[]))),
        (2, TimeSeriesData::SingleTimeSeries(hourly("load", &[1.0]))),
    ]);
    let report = write_partitions(dir.path(), &series).expect("export");
    assert_eq!(report.empty, vec!["empty".to_string()]);
    assert_eq!(report.files.len(), 1);
    assert_eq!(report.rows(), 1);
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
    let (batch, footer) = read_file(&report.files[0].path);
    assert_eq!(footer[table::ROWS_CONTIGUOUS], "true");

    let stamps: Vec<i64> = batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::TimestampMillisecondArray>()
        .unwrap()
        .values()
        .to_vec();
    let owners: Vec<i64> = batch
        .column_by_name("owner_id")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .unwrap()
        .values()
        .to_vec();
    assert_eq!(owners, vec![1, 1, 1, 2, 2, 2], "contiguous per series");
    assert!(stamps[..3].windows(2).all(|w| w[0] < w[1]), "sorted within");
    assert!(stamps[3..].windows(2).all(|w| w[0] < w[1]), "sorted within");
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
        report.files.len(),
        2,
        "a series that declared nothing must not pool with one that declared UTC"
    );
    let names = file_names(dir.path());
    assert!(
        names.contains("SingleTimeSeries.f64.utc.parquet"),
        "{names:?}"
    );
    assert!(
        names.contains("SingleTimeSeries.f64.unspecified.parquet"),
        "{names:?}"
    );

    // Both write a UTC-zoned column -- Arrow has no third spelling -- and the
    // column is what keeps the claim honest.
    let (batch, footer) = read_file(&dir.path().join("SingleTimeSeries.f64.unspecified.parquet"));
    assert_eq!(
        *batch.schema().field(0).data_type(),
        DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()))
    );
    assert_eq!(footer["time_reference"], "unspecified");
    assert_eq!(strings(&batch, "time_reference"), vec!["unspecified"]);
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
    assert!(names.values().all(|n| n.ends_with(".parquet")), "{names:?}");
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

    let name = report.files[0]
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(name, "SingleTimeSeries.f64.offset_minus07_00.parquet");
    // The column still carries the real offset.
    let (batch, footer) = read_file(&report.files[0].path);
    assert_eq!(footer["time_reference"], "-07:00");
    assert_eq!(
        *batch.schema().field(0).data_type(),
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
    let (batch, _) = read_file(&report.files[0].path);
    let expected: Vec<i64> = [(2024, 1, 31), (2024, 2, 29), (2024, 3, 31)]
        .iter()
        .map(|(y, m, d)| {
            Utc.with_ymd_and_hms(*y, *m, *d, 0, 0, 0)
                .unwrap()
                .timestamp_millis()
        })
        .collect();
    assert_eq!(
        batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::TimestampMillisecondArray>()
            .unwrap()
            .values()
            .to_vec(),
        expected
    );
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
        BTreeSet::from(["Deterministic.f64.utc.parquet".to_string()])
    );
    let (batch, footer) = read_file(&report.files[0].path);
    let columns: Vec<String> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    // `issue_time` is why a forecast cannot share a table with a static series.
    assert_eq!(columns[0], "timestamp");
    assert_eq!(columns[1], "issue_time");
    assert!(columns.contains(&"interval".to_string()));
    assert!(columns.contains(&"horizon".to_string()));
    assert_eq!(batch.num_rows(), 4, "2 windows x 2 steps");
    assert_eq!(footer["time_series_type"], "Deterministic");

    // Window-major, so `GROUP BY issue_time` scans contiguously.
    let ms = |h: i64| (t0() + Duration::hours(h)).timestamp_millis();
    let column = |i: usize| {
        batch
            .column(i)
            .as_any()
            .downcast_ref::<arrow::array::TimestampMillisecondArray>()
            .unwrap()
            .values()
            .to_vec()
    };
    assert_eq!(column(1), vec![ms(0), ms(0), ms(1), ms(1)]);
    assert_eq!(column(0), vec![ms(0), ms(1), ms(1), ms(2)]);
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
    assert_eq!(report.files.len(), 2, "two types, two partitions");

    let (batch, _) = read_file(&dir.path().join("Probabilistic.f64.utc.parquet"));
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

    let (batch, _) = read_file(&dir.path().join("Scenarios.f64.utc.parquet"));
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
        BTreeSet::from(["Deterministic.f64_3.utc.parquet".to_string()]),
        "the partition is keyed on the per-step shape"
    );
    let (batch, footer) = read_file(&report.files[0].path);
    assert_eq!(footer["element_shape"], "[3]");
    assert_eq!(batch.num_rows(), 4);
}
