//! Export to the partitioned files and read them back.
//!
//! The property under test is that a store survives the trip: every static type,
//! every element type, every timestamp spelling.
//!
//! **Every test here is `#[ignore]`d for one commit.** The export now writes the
//! normalized values/series pair (§2.12 phase 1) and the reader that pairs them
//! lands in phase 2; ignoring rather than deleting keeps what these assert
//! visible, and phase 2 lifts the attribute rather than reinventing them. What is deliberately *not*
//! preserved — the catalog id, a composite series' stored padding — is asserted
//! too, because a silent change there would be worse than a loud one.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    Dtype, ElementType, FeatureValue, Features, NonSequentialTimeSeries, OwnerCategory, Period,
    PersistentTimeSeries, SingleTimeSeries, TimeReference, TimeSeriesData, TimeSeriesMetadata,
    TypedArray, create_store,
};
use infrastore_parquet::read::{ImportOptions, ImportedSeries};
use infrastore_parquet::{parquet_files, read_file, write_partitions};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

fn utc(data: TimeSeriesData) -> TimeSeriesData {
    let mut data = data;
    let mut descriptors = descriptors_of(&data);
    descriptors.time_reference = Some(TimeReference::Utc);
    data.set_descriptors(descriptors);
    data
}

fn descriptors_of(data: &TimeSeriesData) -> infrastore_core::Descriptors {
    infrastore_core::Descriptors {
        element_type: data.element_type(),
        units: None,
        quantity_kind: None,
        unit_system: None,
        time_reference: data.time_reference().cloned(),
        component_field: None,
        application_data: None,
    }
}

fn hourly(name: &str, values: &[f64]) -> TimeSeriesData {
    utc(TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![values.len()], values),
        name,
    )))
}

/// Store every item, export the store, and read it back.
///
/// The whole loop, because the interesting failures are in the seams: what the
/// catalog stores, what the file says, and what comes back.
fn round_trip(
    items: Vec<(i64, TimeSeriesData, Features)>,
) -> (Vec<ImportedSeries>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(items);
    let report = write_partitions(dir.path(), &series).expect("the export should succeed");
    assert!(
        !report.partitions.is_empty(),
        "something should have been written"
    );

    let mut out = Vec::new();
    for file in parquet_files(dir.path()).expect("the directory should list") {
        out.extend(read_file(&file, &ImportOptions::default()).expect("the file should import"));
    }
    out.sort_by_key(|s| (s.owner_id, s.data.name().to_string()));
    (out, dir)
}

fn stored(
    items: Vec<(i64, TimeSeriesData, Features)>,
) -> Vec<(TimeSeriesMetadata, TimeSeriesData)> {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let mut out = Vec::new();
    for (owner, data, features) in items {
        let id = store
            .add_time_series(owner, "Generator", OwnerCategory::Component, data, features)
            .expect("the series should be added");
        let row = store.get_metadata_by_id(id).unwrap().unwrap();
        let values = store
            .read_by_id(id, infrastore_core::ReadWindow::full())
            .unwrap();
        out.push((row, values));
    }
    out
}

fn plain(items: Vec<(i64, TimeSeriesData)>) -> Vec<(i64, TimeSeriesData, Features)> {
    items
        .into_iter()
        .map(|(o, d)| (o, d, Features::new()))
        .collect()
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_store_survives_the_round_trip() {
    let (back, _dir) = round_trip(plain(vec![
        (1, hourly("load", &[1.0, 2.0, 3.0])),
        (2, hourly("load", &[4.0, 5.0, 6.0])),
    ]));
    assert_eq!(back.len(), 2);
    for (series, expected) in back.iter().zip([[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]) {
        assert_eq!(series.owner_type, "Generator");
        assert_eq!(series.owner_category, OwnerCategory::Component);
        let TimeSeriesData::SingleTimeSeries(s) = &series.data else {
            panic!("expected a SingleTimeSeries, got {:?}", series.data);
        };
        assert_eq!(s.name, "load");
        assert_eq!(s.initial_timestamp, t0());
        assert_eq!(s.resolution, Period::Fixed(Duration::hours(1)));
        assert_eq!(s.data.to_f64_vec().unwrap(), expected.to_vec());
        assert_eq!(s.time_reference, Some(TimeReference::Utc));
    }
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn the_values_round_trip_exactly() {
    // The improvement over CSV, where a float passes through decimal text.
    let awkward = [
        std::f64::consts::PI,
        1e-300,
        -0.0,
        f64::MAX,
        f64::MIN_POSITIVE,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    let (back, _dir) = round_trip(plain(vec![(1, hourly("load", &awkward))]));
    let TimeSeriesData::SingleTimeSeries(s) = &back[0].data else {
        panic!("expected a SingleTimeSeries");
    };
    for (got, want) in s.data.to_f64_vec().unwrap().iter().zip(&awkward) {
        assert_eq!(got.to_bits(), want.to_bits(), "{got} vs {want}");
    }
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn every_descriptor_comes_back() {
    let mut inner = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![2], &[1.0, 2.0]),
        "load",
    );
    inner.units = Some("MW".into());
    inner.quantity_kind = Some("ActivePower".into());
    inner.unit_system = Some(infrastore_core::UnitSystem::NaturalUnits);
    inner.component_field = Some("max_active_power".into());
    inner.application_data = Some(r#"{"k":1}"#.into());
    inner.time_reference = Some(TimeReference::Utc);

    let mut features = Features::new();
    features.insert("model_year".into(), FeatureValue::Int(2030));
    features.insert("scenario".into(), FeatureValue::Str("high".into()));

    let (back, _dir) = round_trip(vec![(
        42,
        TimeSeriesData::SingleTimeSeries(inner),
        features.clone(),
    )]);
    assert_eq!(back[0].owner_id, 42);
    assert_eq!(back[0].features, features);
    let TimeSeriesData::SingleTimeSeries(s) = &back[0].data else {
        panic!("expected a SingleTimeSeries");
    };
    assert_eq!(s.units.as_deref(), Some("MW"));
    assert_eq!(s.quantity_kind.as_deref(), Some("ActivePower"));
    assert_eq!(
        s.unit_system,
        Some(infrastore_core::UnitSystem::NaturalUnits)
    );
    assert_eq!(s.component_field.as_deref(), Some("max_active_power"));
    assert_eq!(s.application_data.as_deref(), Some(r#"{"k":1}"#));
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn an_absent_descriptor_stays_absent() {
    // Written as the empty string so every column can be required, and mapped
    // back to absent here.
    let (back, _dir) = round_trip(plain(vec![(1, hourly("load", &[1.0]))]));
    let TimeSeriesData::SingleTimeSeries(s) = &back[0].data else {
        panic!("expected a SingleTimeSeries");
    };
    assert_eq!(s.units, None);
    assert_eq!(s.quantity_kind, None);
    assert_eq!(s.unit_system, None);
    assert_eq!(s.component_field, None);
    assert_eq!(s.application_data, None);
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_stored_empty_string_reads_back_as_absent() {
    // The one documented consequence of §2.7. Asserted rather than left to be
    // discovered, because it is a real (if small) loss.
    let mut inner = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![1], &[1.0]),
        "load",
    );
    inner.units = Some(String::new());
    inner.time_reference = Some(TimeReference::Utc);
    let (back, _dir) = round_trip(plain(vec![(1, TimeSeriesData::SingleTimeSeries(inner))]));
    let TimeSeriesData::SingleTimeSeries(s) = &back[0].data else {
        panic!("expected a SingleTimeSeries");
    };
    assert_eq!(
        s.units, None,
        "an empty string is indistinguishable from absent"
    );
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn the_irregular_types_keep_their_own_reading() {
    let stamps = vec![t0(), t0() + Duration::hours(1), t0() + Duration::hours(5)];
    let values = TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]);
    let (back, _dir) = round_trip(plain(vec![
        (
            1,
            utc(TimeSeriesData::NonSequentialTimeSeries(
                NonSequentialTimeSeries::new(stamps.clone(), values.clone(), "irregular").unwrap(),
            )),
        ),
        (
            2,
            utc(TimeSeriesData::PersistentTimeSeries(
                PersistentTimeSeries::new(stamps.clone(), values, "steps").unwrap(),
            )),
        ),
    ]));
    assert_eq!(back.len(), 2);
    let TimeSeriesData::NonSequentialTimeSeries(a) = &back[0].data else {
        panic!("expected a NonSequentialTimeSeries, got {:?}", back[0].data);
    };
    assert_eq!(a.timestamps, stamps);
    assert!(
        matches!(back[1].data, TimeSeriesData::PersistentTimeSeries(_)),
        "the two share a shape; only the column tells them apart: {:?}",
        back[1].data
    );
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn every_element_type_round_trips() {
    // §2.5's whole table, in one export.
    let dense = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        "dense",
    );
    let nested = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(
            vec![2, 2, 3],
            &(0..12).map(|i| i as f64).collect::<Vec<_>>(),
        ),
        "nested",
    );
    let tuple = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        "tuple",
    )
    .with_element_type(ElementType::Tuple {
        arity: 3,
        dtype: Dtype::F64,
    });
    let linear = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![2, 2], &[1.0, 2.0, 3.0, 4.0]),
        "linear",
    )
    .with_element_type(ElementType::LinearFunction);
    let ints = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_slice(vec![2], &[7i32, 8]).unwrap(),
        "ints",
    );
    let flags = SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_slice(vec![2], &[true, false]).unwrap(),
        "flags",
    );

    let (back, _dir) = round_trip(plain(vec![
        (1, utc(TimeSeriesData::SingleTimeSeries(dense))),
        (2, utc(TimeSeriesData::SingleTimeSeries(nested))),
        (3, utc(TimeSeriesData::SingleTimeSeries(tuple))),
        (4, utc(TimeSeriesData::SingleTimeSeries(linear))),
        (5, utc(TimeSeriesData::SingleTimeSeries(ints))),
        (6, utc(TimeSeriesData::SingleTimeSeries(flags))),
    ]));
    let by_name: BTreeMap<String, &ImportedSeries> = back
        .iter()
        .map(|s| (s.data.name().to_string(), s))
        .collect();

    let shape_of = |name: &str| match &by_name[name].data {
        TimeSeriesData::SingleTimeSeries(s) => (s.data.shape.clone(), s.element_type),
        other => panic!("expected a SingleTimeSeries, got {other:?}"),
    };
    assert_eq!(
        shape_of("dense"),
        (vec![2, 3], ElementType::Scalar(Dtype::F64))
    );
    assert_eq!(
        shape_of("nested"),
        (vec![2, 2, 3], ElementType::Scalar(Dtype::F64))
    );
    assert_eq!(
        shape_of("tuple"),
        (
            vec![2, 3],
            ElementType::Tuple {
                arity: 3,
                dtype: Dtype::F64
            }
        )
    );
    assert_eq!(
        shape_of("linear"),
        (vec![2, 2], ElementType::LinearFunction)
    );
    assert_eq!(shape_of("ints"), (vec![2], ElementType::Scalar(Dtype::I32)));
    assert_eq!(
        shape_of("flags"),
        (vec![2], ElementType::Scalar(Dtype::Bool))
    );
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_repadded_composite_comes_back_at_its_own_width() {
    // Two curves share a file and are padded to its widest; the import shrinks
    // each back to the width its own points need. The values are what must
    // survive, not the padding.
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

    let (back, _dir) = round_trip(plain(vec![
        (1, utc(TimeSeriesData::SingleTimeSeries(narrow))),
        (2, utc(TimeSeriesData::SingleTimeSeries(wide))),
    ]));
    let TimeSeriesData::SingleTimeSeries(a) = &back[0].data else {
        panic!("expected a SingleTimeSeries");
    };
    assert_eq!(a.data.shape, vec![1, 3], "the narrow curve is narrow again");
    assert_eq!(a.data.to_f64_vec().unwrap(), vec![1.0, 0.0, 5.0]);
    let TimeSeriesData::SingleTimeSeries(b) = &back[1].data else {
        panic!("expected a SingleTimeSeries");
    };
    assert_eq!(b.data.shape, vec![1, 5]);
    assert_eq!(b.data.to_f64_vec().unwrap(), vec![2.0, 0.0, 5.0, 1.0, 7.0]);
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn every_spelling_round_trips_including_unspecified() {
    let spelling = |name: &str, reference: Option<TimeReference>| {
        let mut inner = SingleTimeSeries::new(
            t0(),
            Duration::hours(1),
            TypedArray::from_f64(vec![2], &[1.0, 2.0]),
            name,
        );
        inner.time_reference = reference;
        TimeSeriesData::SingleTimeSeries(inner)
    };
    let cases = [
        ("utc", Some(TimeReference::Utc)),
        ("zoneless", Some(TimeReference::Zoneless)),
        ("offset", Some(TimeReference::FixedOffset(-420))),
        ("zone", Some(TimeReference::Zone("America/Denver".into()))),
        ("unspecified", None),
    ];
    let items = cases
        .iter()
        .enumerate()
        .map(|(i, (name, r))| (i as i64 + 1, spelling(name, r.clone())))
        .collect();
    let (back, _dir) = round_trip(plain(items));

    for (series, (name, expected)) in back.iter().zip(&cases) {
        let TimeSeriesData::SingleTimeSeries(s) = &series.data else {
            panic!("expected a SingleTimeSeries");
        };
        assert_eq!(&s.name, name);
        assert_eq!(
            &s.time_reference, expected,
            "{name} did not survive the round trip"
        );
    }
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_monthly_grid_survives_its_calendar() {
    let series = SingleTimeSeries::new(
        Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap(),
        Period::Months(1),
        TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]),
        "monthly",
    );
    let (back, _dir) = round_trip(plain(vec![(
        1,
        utc(TimeSeriesData::SingleTimeSeries(series)),
    )]));
    let TimeSeriesData::SingleTimeSeries(s) = &back[0].data else {
        panic!("expected a SingleTimeSeries");
    };
    assert_eq!(s.resolution, Period::Months(1));
    assert_eq!(
        s.initial_timestamp,
        Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap()
    );
    assert_eq!(s.length, 3);
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn the_id_is_reported_and_then_ignored() {
    let (back, _dir) = round_trip(plain(vec![(1, hourly("load", &[1.0]))]));
    // The file records it, so a --dry-run can say which row it came from.
    assert_eq!(back[0].recorded_id, Some(1));
    // But nothing here files it: `add` never accepts an id, because
    // "never reissued" is a guarantee of the catalog's AUTOINCREMENT.
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_series_that_reappears_is_refused() {
    // The import streams, so it cannot stitch a series back together from rows
    // scattered through a file -- and holding everything to allow that would
    // give up what makes a large file importable.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![
        (1, hourly("a", &[1.0, 2.0])),
        (2, hourly("b", &[3.0, 4.0])),
    ]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let path = &report.partitions[0].values_path;

    // Reorder the rows so owner 1 appears on both sides of owner 2. The
    // `data_hash` column goes first, so the contiguity refusal is what surfaces
    // rather than the truncated first group failing its checksum -- both are
    // real, and this test is about the one that explains the fix.
    let (schema, batch) = read_one(path);
    let keep: Vec<usize> = (0..schema.fields().len())
        .filter(|i| schema.field(*i).name() != "data_hash")
        .collect();
    let batch = batch.project(&keep).expect("project");
    let indices = arrow::array::UInt32Array::from(vec![0u32, 2, 3, 1]);
    let shuffled: Vec<arrow::array::ArrayRef> = batch
        .columns()
        .iter()
        .map(|c| arrow::compute::take(c, &indices, None).unwrap())
        .collect();
    let shuffled = arrow::array::RecordBatch::try_new(batch.schema(), shuffled).expect("rebuild");

    let scrambled = dir.path().join("scrambled.parquet");
    write_batch(&scrambled, &shuffled);
    let err = read_file(&scrambled, &ImportOptions::default()).expect_err("not contiguous");
    assert!(err.to_string().contains("appears again"), "{err}");
    assert!(err.to_string().contains("sorted"), "{err}");
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn an_edited_value_fails_the_checksum() {
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, hourly("load", &[1.0, 2.0]))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let path = &report.partitions[0].values_path;

    let (schema, batch) = read_one(path);
    // Change a value without touching the recorded hash, which is what an edit
    // in a query engine looks like if the column is kept.
    let mut columns = batch.columns().to_vec();
    let value_index = schema.index_of("value").unwrap();
    columns[value_index] = std::sync::Arc::new(arrow::array::Float64Array::from(vec![9.0, 2.0]));
    let edited = arrow::array::RecordBatch::try_new(batch.schema(), columns).expect("rebuild");
    let edited_path = dir.path().join("edited.parquet");
    write_batch(&edited_path, &edited);

    let err = read_file(&edited_path, &ImportOptions::default()).expect_err("checksum");
    assert!(err.to_string().contains("data_hash"), "{err}");
    assert!(err.to_string().contains("drop"), "{err}");
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn dropping_the_hash_column_is_how_edited_values_get_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, hourly("load", &[1.0, 2.0]))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let (schema, batch) = read_one(&report.partitions[0].values_path);

    let keep: Vec<usize> = (0..schema.fields().len())
        .filter(|i| schema.field(*i).name() != "data_hash")
        .collect();
    let projected = batch.project(&keep).expect("project");
    let edited_values = arrow::array::Float64Array::from(vec![9.0, 2.0]);
    let mut columns = projected.columns().to_vec();
    let value_index = projected.schema().index_of("value").unwrap();
    columns[value_index] = std::sync::Arc::new(edited_values);
    let edited = arrow::array::RecordBatch::try_new(projected.schema(), columns).expect("rebuild");

    let path = dir.path().join("no_hash.parquet");
    write_batch(&path, &edited);
    let back = read_file(&path, &ImportOptions::default()).expect("no column, no checksum");
    let TimeSeriesData::SingleTimeSeries(s) = &back[0].data else {
        panic!("expected a SingleTimeSeries");
    };
    assert_eq!(s.data.to_f64_vec().unwrap(), vec![9.0, 2.0]);
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_directory_imports_every_file_in_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![
        (1, hourly("load", &[1.0])),
        (
            2,
            utc(TimeSeriesData::NonSequentialTimeSeries(
                NonSequentialTimeSeries::new(
                    vec![t0(), t0() + Duration::hours(5)],
                    TypedArray::from_f64(vec![2], &[1.0, 2.0]),
                    "irregular",
                )
                .unwrap(),
            )),
        ),
    ]));
    let report = write_partitions(dir.path(), &series).expect("export");
    assert_eq!(report.partitions.len(), 2);

    let files = parquet_files(dir.path()).expect("list");
    assert_eq!(files.len(), 2);
    let total: usize = files
        .iter()
        .map(|f| {
            read_file(f, &ImportOptions::default())
                .expect("import")
                .len()
        })
        .sum();
    assert_eq!(total, 2);
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_directory_with_no_parquet_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = parquet_files(dir.path()).expect_err("nothing to import");
    assert!(err.to_string().contains("no .parquet"), "{err}");
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_file_from_a_later_format_is_refused_by_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, hourly("load", &[1.0]))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let (schema, batch) = read_one(&report.partitions[0].values_path);

    let mut metadata = schema.metadata().clone();
    metadata.insert("infrastore.format".into(), "long_table_v99".into());
    let bumped = std::sync::Arc::new(arrow::datatypes::Schema::new_with_metadata(
        schema.fields().clone(),
        metadata,
    ));
    let batch = arrow::array::RecordBatch::try_new(bumped, batch.columns().to_vec()).unwrap();
    let path = dir.path().join("future.parquet");
    write_batch(&path, &batch);

    let err = read_file(&path, &ImportOptions::default()).expect_err("a later format");
    assert!(err.to_string().contains("long_table_v99"), "{err}");
    assert!(err.to_string().contains("long_table_v1"), "{err}");
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_deterministic_single_time_series_points_at_transform() {
    // The type is derived rather than added, so a file naming it is a mistake
    // worth explaining.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, hourly("load", &[1.0]))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let (schema, batch) = read_one(&report.partitions[0].values_path);

    let mut columns = batch.columns().to_vec();
    let index = schema.index_of("time_series_type").unwrap();
    columns[index] = std::sync::Arc::new(arrow::array::StringArray::from(vec![
        "DeterministicSingleTimeSeries",
    ]));
    let edited = arrow::array::RecordBatch::try_new(batch.schema(), columns).expect("rebuild");
    let path = dir.path().join("dst.parquet");
    write_batch(&path, &edited);

    let err = read_file(&path, &ImportOptions::default()).expect_err("derived, not added");
    assert!(
        err.to_string().contains("transform_single_time_series"),
        "{err}"
    );
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_foreign_file_infers_what_it_does_not_say() {
    use arrow::array::{ArrayRef, Float64Array, RecordBatch, TimestampMillisecondArray};
    use arrow::datatypes::{Field, Schema};

    let dir = tempfile::tempdir().expect("tempdir");
    let stamps: ArrayRef = std::sync::Arc::new(
        TimestampMillisecondArray::from(vec![
            t0().timestamp_millis(),
            t0().timestamp_millis() + 3_600_000,
        ])
        .with_timezone("UTC"),
    );
    let values: ArrayRef = std::sync::Arc::new(Float64Array::from(vec![1.0, 2.0]));
    let schema = Schema::new(vec![
        Field::new("timestamp", stamps.data_type().clone(), false),
        Field::new("value", values.data_type().clone(), false),
    ]);
    let batch =
        RecordBatch::try_new(std::sync::Arc::new(schema), vec![stamps, values]).expect("batch");
    let path = dir.path().join("foreign.parquet");
    write_batch(&path, &batch);

    // Nothing but the two columns: the name and owner have to be supplied.
    let options = ImportOptions {
        name: Some("load".into()),
        owner_id: Some(7),
        owner_type: Some("Bus".into()),
        ..Default::default()
    };
    let back = read_file(&path, &options).expect("a foreign file imports");
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].owner_id, 7);
    assert_eq!(back[0].owner_type, "Bus");
    // Evenly spaced rows read as a grid.
    let TimeSeriesData::SingleTimeSeries(s) = &back[0].data else {
        panic!("expected a SingleTimeSeries, got {:?}", back[0].data);
    };
    assert_eq!(s.resolution, Period::Fixed(Duration::hours(1)));
    assert_eq!(s.name, "load");
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn nulls_are_refused_rather_than_coerced() {
    use arrow::array::{ArrayRef, Float64Array, RecordBatch, TimestampMillisecondArray};
    use arrow::datatypes::{Field, Schema};

    let dir = tempfile::tempdir().expect("tempdir");
    let stamps: ArrayRef = std::sync::Arc::new(
        TimestampMillisecondArray::from(vec![
            t0().timestamp_millis(),
            t0().timestamp_millis() + 3_600_000,
        ])
        .with_timezone("UTC"),
    );
    let values: ArrayRef = std::sync::Arc::new(Float64Array::from(vec![Some(1.0), None]));
    let schema = Schema::new(vec![
        Field::new("timestamp", stamps.data_type().clone(), false),
        Field::new("value", values.data_type().clone(), true),
    ]);
    let batch =
        RecordBatch::try_new(std::sync::Arc::new(schema), vec![stamps, values]).expect("batch");
    let path = dir.path().join("nulls.parquet");
    write_batch(&path, &batch);

    let options = ImportOptions {
        name: Some("load".into()),
        owner_id: Some(1),
        owner_type: Some("Bus".into()),
        ..Default::default()
    };
    let err = read_file(&path, &options).expect_err("a null is not NaN");
    assert!(err.to_string().contains("nulls"), "{err}");
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_foreign_file_takes_its_spelling_from_the_arrow_zone() {
    use arrow::array::{ArrayRef, Float64Array, RecordBatch, TimestampMillisecondArray};
    use arrow::datatypes::{Field, Schema};

    let dir = tempfile::tempdir().expect("tempdir");
    let options = ImportOptions {
        name: Some("load".into()),
        owner_id: Some(7),
        owner_type: Some("Bus".into()),
        ..Default::default()
    };
    let write = |file: &str, zone: Option<&str>| {
        let mut stamps = TimestampMillisecondArray::from(vec![
            t0().timestamp_millis(),
            t0().timestamp_millis() + 3_600_000,
        ]);
        if let Some(z) = zone {
            stamps = stamps.with_timezone(z);
        }
        let stamps: ArrayRef = std::sync::Arc::new(stamps);
        let values: ArrayRef = std::sync::Arc::new(Float64Array::from(vec![1.0, 2.0]));
        let schema = Schema::new(vec![
            Field::new("timestamp", stamps.data_type().clone(), false),
            Field::new("value", values.data_type().clone(), false),
        ]);
        let batch =
            RecordBatch::try_new(std::sync::Arc::new(schema), vec![stamps, values]).expect("batch");
        let path = dir.path().join(file);
        write_batch(&path, &batch);
        path
    };
    let reference = |path: &Path| {
        let back = read_file(path, &options).expect("a foreign file imports");
        let TimeSeriesData::SingleTimeSeries(s) = &back[0].data else {
            panic!("expected a SingleTimeSeries, got {:?}", back[0].data);
        };
        s.time_reference.clone()
    };
    // No footer, no column: the zone is all there is, and it is not nothing.
    assert_eq!(
        reference(&write("utc.parquet", Some("UTC"))),
        Some(TimeReference::Utc)
    );
    assert_eq!(
        reference(&write("denver.parquet", Some("America/Denver"))),
        Some(TimeReference::Zone("America/Denver".into()))
    );
    assert_eq!(
        reference(&write("naive.parquet", None)),
        Some(TimeReference::Zoneless)
    );
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_null_in_a_text_or_integer_column_is_refused() {
    use arrow::array::{
        ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray, TimestampMillisecondArray,
    };
    use arrow::datatypes::{Field, Schema};

    let dir = tempfile::tempdir().expect("tempdir");
    let stamps = || -> ArrayRef {
        std::sync::Arc::new(
            TimestampMillisecondArray::from(vec![
                t0().timestamp_millis(),
                t0().timestamp_millis() + 3_600_000,
            ])
            .with_timezone("UTC"),
        )
    };
    let values = || -> ArrayRef { std::sync::Arc::new(Float64Array::from(vec![1.0, 2.0])) };
    let options = ImportOptions {
        name: Some("load".into()),
        owner_id: Some(1),
        owner_type: Some("Bus".into()),
        ..Default::default()
    };

    // A null descriptor is not an absent one: absent is the empty string.
    let units: ArrayRef = std::sync::Arc::new(StringArray::from(vec![Some("MW"), None]));
    let schema = Schema::new(vec![
        Field::new("timestamp", stamps().data_type().clone(), false),
        Field::new("value", values().data_type().clone(), false),
        Field::new("units", units.data_type().clone(), true),
    ]);
    let batch = RecordBatch::try_new(std::sync::Arc::new(schema), vec![stamps(), values(), units])
        .expect("batch");
    let path = dir.path().join("null_text.parquet");
    write_batch(&path, &batch);
    let err = read_file(&path, &options).expect_err("a null descriptor is refused");
    assert!(
        err.to_string().contains("`units` column has nulls"),
        "{err}"
    );

    // A null identity column would file the rows under a different series.
    let owner: ArrayRef = std::sync::Arc::new(Int64Array::from(vec![Some(1), None]));
    let schema = Schema::new(vec![
        Field::new("timestamp", stamps().data_type().clone(), false),
        Field::new("value", values().data_type().clone(), false),
        Field::new("owner_id", owner.data_type().clone(), true),
    ]);
    let batch = RecordBatch::try_new(std::sync::Arc::new(schema), vec![stamps(), values(), owner])
        .expect("batch");
    let path = dir.path().join("null_int.parquet");
    write_batch(&path, &batch);
    let err = read_file(&path, &options).expect_err("a null owner is refused");
    assert!(
        err.to_string().contains("`owner_id` column has nulls"),
        "{err}"
    );
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_file_streams_into_its_sink_one_series_at_a_time() {
    use infrastore_parquet::read_file_with;

    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![
        (1, hourly("a", &[1.0, 2.0])),
        (2, hourly("b", &[3.0, 4.0])),
        (3, hourly("c", &[5.0, 6.0])),
    ]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let path = &report.partitions[0].values_path;

    // Every series reaches the sink, in file order, and the count says so.
    let mut names = Vec::new();
    let filed = read_file_with(path, &ImportOptions::default(), &mut |one| {
        names.push(one.data.name().to_string());
        Ok(())
    })
    .expect("streams");
    assert_eq!(filed, 3);
    assert_eq!(names, ["a", "b", "c"]);

    // A sink error stops the read where it happened and comes back as it is.
    let mut seen = 0;
    let err = read_file_with(path, &ImportOptions::default(), &mut |_| {
        seen += 1;
        if seen == 2 {
            Err(infrastore_core::TimeSeriesError::InvalidParameter(
                "the sink said no".into(),
            ))
        } else {
            Ok(())
        }
    })
    .expect_err("the sink's error propagates");
    assert!(err.to_string().contains("the sink said no"), "{err}");
    assert_eq!(seen, 2, "nothing is read past the failure");
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_contradicting_assertion_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, hourly("load", &[1.0]))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let options = ImportOptions {
        element_type: Some(ElementType::Scalar(Dtype::I64)),
        ..Default::default()
    };
    let err = read_file(&report.partitions[0].values_path, &options).expect_err("contradiction");
    assert!(err.to_string().contains("asserted"), "{err}");
}

// ---- helpers ---------------------------------------------------------------

fn read_one(path: &Path) -> (arrow::datatypes::SchemaRef, arrow::array::RecordBatch) {
    let file = std::fs::File::open(path).unwrap();
    let builder =
        parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    let schema = builder.schema().clone();
    let batches: Vec<arrow::array::RecordBatch> =
        builder.build().unwrap().collect::<Result<_, _>>().unwrap();
    let batch = arrow::compute::concat_batches(&schema, &batches).unwrap();
    (schema, batch)
}

fn write_batch(path: &Path, batch: &arrow::array::RecordBatch) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = parquet::arrow::ArrowWriter::try_new(file, batch.schema(), None).unwrap();
    writer.write(batch).unwrap();
    writer.close().unwrap();
}

// ---- Dense forecasts --------------------------------------------------------

fn deterministic(name: &str) -> TimeSeriesData {
    let mut forecast = infrastore_core::Deterministic::new(
        t0(),
        Duration::hours(1),
        Duration::hours(3),
        Duration::hours(1),
        2,
        TypedArray::from_f64(vec![3, 2], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        name,
    )
    .expect("the forecast should build");
    forecast.time_reference = Some(TimeReference::Utc);
    TimeSeriesData::Deterministic(forecast)
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_deterministic_forecast_round_trips() {
    let original = deterministic("day_ahead");
    let (back, _dir) = round_trip(plain(vec![(1, original.clone())]));
    assert_eq!(back.len(), 1);
    let (TimeSeriesData::Deterministic(got), TimeSeriesData::Deterministic(want)) =
        (&back[0].data, &original)
    else {
        panic!("expected a Deterministic, got {:?}", back[0].data);
    };
    assert_eq!(got, want, "the cube comes back identical");
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_probabilistic_forecast_keeps_its_percentiles() {
    // The core requires percentiles to be strictly increasing, so the import can
    // sort the lane labels -- which is what makes it independent of the row
    // order a query engine happened to leave behind.
    let values: Vec<f64> = (0..12).map(|i| i as f64).collect();
    let mut forecast = infrastore_core::Probabilistic::new(
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

    let (back, _dir) = round_trip(plain(vec![(
        1,
        TimeSeriesData::Probabilistic(forecast.clone()),
    )]));
    let TimeSeriesData::Probabilistic(got) = &back[0].data else {
        panic!("expected a Probabilistic, got {:?}", back[0].data);
    };
    assert_eq!(got.percentiles, vec![0.1, 0.9]);
    assert_eq!(got, &forecast);
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_scenarios_forecast_round_trips() {
    let values: Vec<f64> = (0..12).map(|i| i as f64).collect();
    let mut forecast = infrastore_core::Scenarios::new(
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

    let (back, _dir) = round_trip(plain(vec![(
        1,
        TimeSeriesData::Scenarios(forecast.clone()),
    )]));
    let TimeSeriesData::Scenarios(got) = &back[0].data else {
        panic!("expected a Scenarios, got {:?}", back[0].data);
    };
    assert_eq!(got, &forecast);
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_multidimensional_forecast_round_trips() {
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

    let (back, _dir) = round_trip(plain(vec![(
        1,
        TimeSeriesData::Deterministic(forecast.clone()),
    )]));
    let TimeSeriesData::Deterministic(got) = &back[0].data else {
        panic!("expected a Deterministic");
    };
    assert_eq!(got, &forecast);
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_calendar_horizon_counts_its_steps_by_walking_the_grid() {
    // A month is not a fixed number of milliseconds, so `horizon / resolution`
    // is the wrong arithmetic.
    let mut forecast = infrastore_core::Deterministic::new(
        Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap(),
        Period::Months(1),
        Period::Months(2),
        Period::Months(1),
        2,
        TypedArray::from_f64(vec![2, 2], &[1.0, 2.0, 3.0, 4.0]),
        "monthly",
    )
    .expect("the forecast should build");
    forecast.time_reference = Some(TimeReference::Utc);

    let (back, _dir) = round_trip(plain(vec![(
        1,
        TimeSeriesData::Deterministic(forecast.clone()),
    )]));
    let TimeSeriesData::Deterministic(got) = &back[0].data else {
        panic!("expected a Deterministic");
    };
    assert_eq!(got, &forecast);
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn forecast_rows_are_placed_by_coordinates_not_order() {
    // A query engine may rewrite a file in any order within a series; the
    // coordinates are what put each value back where it belongs.
    let dir = tempfile::tempdir().expect("tempdir");
    let original = deterministic("day_ahead");
    let series = stored(plain(vec![(1, original.clone())]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let (schema, batch) = read_one(&report.partitions[0].values_path);

    let n = batch.num_rows() as u32;
    let indices = arrow::array::UInt32Array::from((0..n).rev().collect::<Vec<_>>());
    let reversed: Vec<arrow::array::ArrayRef> = batch
        .columns()
        .iter()
        .map(|c| arrow::compute::take(c, &indices, None).unwrap())
        .collect();
    let reversed = arrow::array::RecordBatch::try_new(schema.clone(), reversed).expect("rebuild");
    let path = dir.path().join("reversed.parquet");
    write_batch(&path, &reversed);

    let back = read_file(&path, &ImportOptions::default()).expect("import");
    let (TimeSeriesData::Deterministic(got), TimeSeriesData::Deterministic(want)) =
        (&back[0].data, &original)
    else {
        panic!("expected a Deterministic");
    };
    assert_eq!(got, want);
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_forecast_missing_its_grid_columns_is_refused() {
    // The rows say where a value belongs, not what the grid it belongs to is.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, deterministic("day_ahead"))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let (schema, batch) = read_one(&report.partitions[0].values_path);

    let keep: Vec<usize> = (0..schema.fields().len())
        .filter(|i| schema.field(*i).name() != "horizon")
        .collect();
    let projected = batch.project(&keep).expect("project");
    let path = dir.path().join("no_horizon.parquet");
    write_batch(&path, &projected);

    let err = read_file(&path, &ImportOptions::default()).expect_err("no horizon");
    assert!(err.to_string().contains("horizon"), "{err}");
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_forecast_short_of_its_grid_is_refused() {
    // A cube has no hole to leave, so a missing row cannot be filled in. The
    // count is checked first, which is the more useful message.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, deterministic("day_ahead"))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let (schema, batch) = read_one(&report.partitions[0].values_path);

    let keep: Vec<usize> = (0..schema.fields().len())
        .filter(|i| schema.field(*i).name() != "data_hash")
        .collect();
    let projected = batch.project(&keep).expect("project");
    let short = projected.slice(0, projected.num_rows() - 1);
    let path = dir.path().join("short.parquet");
    write_batch(&path, &short);

    let err = read_file(&path, &ImportOptions::default()).expect_err("a hole");
    assert!(err.to_string().contains("its grid holds"), "{err}");
}

#[test]
#[ignore = "the normalized reader lands in the next commit (FEATURE_PLAN.md §2.12 phase 2)"]
fn a_forecast_with_two_rows_for_one_slot_is_refused() {
    // The right number of rows, but one coordinate twice -- so somewhere else
    // has none, and the two rows disagree about the same value.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, deterministic("day_ahead"))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let (schema, batch) = read_one(&report.partitions[0].values_path);

    let keep: Vec<usize> = (0..schema.fields().len())
        .filter(|i| schema.field(*i).name() != "data_hash")
        .collect();
    let projected = batch.project(&keep).expect("project");
    let n = projected.num_rows() as u32;
    // Repeat row 0 in place of the last one.
    let mut indices: Vec<u32> = (0..n - 1).collect();
    indices.push(0);
    let indices = arrow::array::UInt32Array::from(indices);
    let doubled: Vec<arrow::array::ArrayRef> = projected
        .columns()
        .iter()
        .map(|c| arrow::compute::take(c, &indices, None).unwrap())
        .collect();
    let doubled = arrow::array::RecordBatch::try_new(projected.schema(), doubled).expect("rebuild");
    let path = dir.path().join("doubled.parquet");
    write_batch(&path, &doubled);

    let err = read_file(&path, &ImportOptions::default()).expect_err("a duplicate");
    assert!(err.to_string().contains("two rows for"), "{err}");
}
