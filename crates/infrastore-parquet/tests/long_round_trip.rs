//! Export a store to the values/series pairs and read them back.
//!
//! The property under test is that a store survives the trip: every static type,
//! every element type, every timestamp spelling. What is deliberately *not*
//! preserved — the catalog id, a composite series' stored padding — is asserted
//! too, because a silent change there would be worse than a loud one.
//!
//! The forecast tests at the end run the same loop over the three dense kinds,
//! whose partitions carry two more key columns and a cube rather than a vector.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    Dtype, ElementType, FeatureValue, Features, NonSequentialTimeSeries, OwnerCategory, Period,
    PersistentTimeSeries, SingleTimeSeries, TimeReference, TimeSeriesData, TimeSeriesMetadata,
    TypedArray, create_store,
};
use infrastore_parquet::read::{ImportOptions, ImportedSeries};
use infrastore_parquet::{PartitionFiles, partitions, read_partition, write_partitions};

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
    for files in partitions(dir.path()).expect("the directory should list") {
        out.extend(
            read_partition(&files, &ImportOptions::default()).expect("the partition should import"),
        );
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
fn the_id_is_reported_and_then_ignored() {
    let (back, _dir) = round_trip(plain(vec![(1, hourly("load", &[1.0]))]));
    // The file records it, so a --dry-run can say which row it came from.
    assert_eq!(back[0].recorded_id, Some(1));
    // But nothing here files it: `add` never accepts an id, because
    // "never reissued" is a guarantee of the catalog's AUTOINCREMENT.
}

#[test]
fn an_array_that_reappears_is_refused() {
    // The merge join streams, so it cannot stitch a key back together from rows
    // scattered through a file -- and holding a whole file to allow that would
    // give up what makes a large partition importable.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![
        (1, hourly("a", &[1.0, 2.0])),
        (2, hourly("b", &[3.0, 4.0])),
    ]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let files = &report.partitions[0];
    assert_eq!(files.arrays, 2, "two different profiles, two arrays");

    // Reorder the rows so the first array appears on both sides of the second.
    let (schema, batch) = read_one(&files.values_path);
    let indices = arrow::array::UInt32Array::from(vec![0u32, 2, 3, 1]);
    let shuffled: Vec<arrow::array::ArrayRef> = batch
        .columns()
        .iter()
        .map(|c| arrow::compute::take(c, &indices, None).unwrap())
        .collect();
    let shuffled = arrow::array::RecordBatch::try_new(schema.clone(), shuffled).expect("rebuild");
    write_batch(&files.values_path, &shuffled);

    let err = read_partition(&pair(&report), &ImportOptions::default()).expect_err("scattered");
    assert!(err.to_string().contains("returns to array"), "{err}");
    assert!(err.to_string().contains("sorted"), "{err}");
}

#[test]
fn an_edited_value_fails_the_checksum() {
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, hourly("load", &[1.0, 2.0]))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let path = &report.partitions[0].values_path;

    // Change a value without touching the key that names the array, which is
    // what an edit in a query engine looks like.
    let (schema, batch) = read_one(path);
    let mut columns = batch.columns().to_vec();
    let value_index = schema.index_of("value").unwrap();
    columns[value_index] = std::sync::Arc::new(arrow::array::Float64Array::from(vec![9.0, 2.0]));
    let edited = arrow::array::RecordBatch::try_new(schema, columns).expect("rebuild");
    write_batch(path, &edited);

    let err = read_partition(&pair(&report), &ImportOptions::default()).expect_err("checksum");
    assert!(err.to_string().contains("data_hash"), "{err}");
    assert!(err.to_string().contains("--no-checksum"), "{err}");
}

#[test]
fn waiving_the_checksum_is_how_edited_values_get_in() {
    // The pair is still the join key; only its meaning as a content hash is
    // waived. Which is the whole remedy for a file a query engine rewrote.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, hourly("load", &[1.0, 2.0]))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let path = &report.partitions[0].values_path;

    let (schema, batch) = read_one(path);
    let mut columns = batch.columns().to_vec();
    let value_index = schema.index_of("value").unwrap();
    columns[value_index] = std::sync::Arc::new(arrow::array::Float64Array::from(vec![9.0, 2.0]));
    let edited = arrow::array::RecordBatch::try_new(schema, columns).expect("rebuild");
    write_batch(path, &edited);

    let options = ImportOptions {
        skip_checksum: true,
        ..Default::default()
    };
    let back = read_partition(&pair(&report), &options).expect("waived");
    let TimeSeriesData::SingleTimeSeries(s) = &back[0].data else {
        panic!("expected a SingleTimeSeries");
    };
    assert_eq!(s.data.to_f64_vec().unwrap(), vec![9.0, 2.0]);
}

#[test]
fn a_series_row_with_no_values_group_is_refused() {
    // The two halves come from one export; a series row naming an array that is
    // not in the values file means one of them was truncated.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![
        (1, hourly("a", &[1.0, 2.0])),
        (2, hourly("b", &[3.0, 4.0])),
    ]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let path = &report.partitions[0].values_path;

    // Keep only the first array's rows.
    let (schema, batch) = read_one(path);
    let truncated = batch.slice(0, 2);
    write_batch(
        path,
        &arrow::array::RecordBatch::try_new(schema, truncated.columns().to_vec()).unwrap(),
    );

    let err = read_partition(&pair(&report), &ImportOptions::default()).expect_err("dangling");
    assert!(err.to_string().contains("does not hold"), "{err}");
}

#[test]
fn a_values_group_no_series_row_names_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![
        (1, hourly("a", &[1.0, 2.0])),
        (2, hourly("b", &[3.0, 4.0])),
    ]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let path = &report.partitions[0].series_path;

    // Keep only the first catalog row, leaving the second array unclaimed.
    let (schema, batch) = read_one(path);
    let truncated = batch.slice(0, 1);
    write_batch(
        path,
        &arrow::array::RecordBatch::try_new(schema, truncated.columns().to_vec()).unwrap(),
    );

    let err = read_partition(&pair(&report), &ImportOptions::default()).expect_err("dangling");
    assert!(err.to_string().contains("no series row names"), "{err}");
}

#[test]
fn a_series_file_with_no_values_file_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, hourly("load", &[1.0, 2.0]))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    std::fs::remove_file(&report.partitions[0].values_path).unwrap();

    let err = partitions(dir.path()).expect_err("its rows name nothing");
    assert!(err.to_string().contains("has no"), "{err}");
    assert!(err.to_string().contains("not there"), "{err}");
}

#[test]
fn a_partition_can_be_named_by_its_stem() {
    // A stem is neither file: it is the two of them, which is what the CLI's
    // `--parquet <stem>` means.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, hourly("load", &[1.0, 2.0]))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let stem = dir.path().join(&report.partitions[0].stem);

    let found = partitions(&stem).expect("the stem names the pair");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].values, report.partitions[0].values_path);
    assert_eq!(
        found[0].series.as_deref(),
        Some(report.partitions[0].series_path.as_path())
    );
    assert_eq!(
        read_partition(&found[0], &ImportOptions::default())
            .expect("import")
            .len(),
        1
    );
}

#[test]
fn a_thousand_series_share_one_array() {
    // The reason for the layout: a shared profile is written once, and the store
    // it is re-imported into holds it once.
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = [1.0, 2.0, 3.0, 4.0];
    let series = stored(plain(
        (0..1000)
            .map(|owner| (owner, hourly("load", &profile)))
            .collect(),
    ));
    let report = write_partitions(dir.path(), &series).expect("export");
    assert_eq!(report.partitions.len(), 1);
    assert_eq!(report.arrays(), 1, "one profile, one array");
    assert_eq!(report.series(), 1000);
    assert_eq!(report.rows(), 4, "the values file holds the profile once");

    let back = read_partition(&pair(&report), &ImportOptions::default()).expect("import");
    assert_eq!(back.len(), 1000);
    assert_eq!(
        back.iter()
            .map(|s| s.array.clone().expect("keyed"))
            .collect::<std::collections::HashSet<_>>()
            .len(),
        1,
        "every series names the same array"
    );

    let mut store = create_store(None, true).expect("in-memory store");
    for one in back {
        store
            .add_time_series(
                one.owner_id,
                &one.owner_type,
                one.owner_category,
                one.data,
                one.features,
            )
            .expect("add");
    }
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);
}

#[test]
fn a_directory_imports_every_partition_in_it() {
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

    let found = partitions(dir.path()).expect("list");
    assert_eq!(found.len(), 2, "two partitions, four files");
    assert!(found.iter().all(|p| p.series.is_some()), "{found:?}");
    let total: usize = found
        .iter()
        .map(|p| {
            read_partition(p, &ImportOptions::default())
                .expect("import")
                .len()
        })
        .sum();
    assert_eq!(total, 2);
}

#[test]
fn a_directory_with_no_parquet_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = partitions(dir.path()).expect_err("nothing to import");
    assert!(err.to_string().contains("no .parquet"), "{err}");
}

#[test]
fn a_file_from_a_later_format_is_refused_by_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, hourly("load", &[1.0]))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let path = &report.partitions[0].values_path;
    let (schema, batch) = read_one(path);

    let mut metadata = schema.metadata().clone();
    metadata.insert("infrastore.format".into(), "normalized_v99".into());
    let bumped = std::sync::Arc::new(arrow::datatypes::Schema::new_with_metadata(
        schema.fields().clone(),
        metadata,
    ));
    let batch = arrow::array::RecordBatch::try_new(bumped, batch.columns().to_vec()).unwrap();
    write_batch(path, &batch);

    let err =
        read_partition(&pair(&report), &ImportOptions::default()).expect_err("a later format");
    assert!(err.to_string().contains("normalized_v99"), "{err}");
    assert!(err.to_string().contains("normalized_v1"), "{err}");
}

#[test]
fn a_deterministic_single_time_series_points_at_transform() {
    // The type is derived rather than added, so a file naming it is a mistake
    // worth explaining.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, hourly("load", &[1.0]))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    // The type is a series-file column: the values file says nothing about it.
    let path = &report.partitions[0].series_path;
    let (schema, batch) = read_one(path);

    let mut columns = batch.columns().to_vec();
    let index = schema.index_of("time_series_type").unwrap();
    columns[index] = std::sync::Arc::new(arrow::array::StringArray::from(vec![
        "DeterministicSingleTimeSeries",
    ]));
    let edited = arrow::array::RecordBatch::try_new(schema, columns).expect("rebuild");
    write_batch(path, &edited);

    let err =
        read_partition(&pair(&report), &ImportOptions::default()).expect_err("derived, not added");
    assert!(
        err.to_string().contains("transform_single_time_series"),
        "{err}"
    );
}

#[test]
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
    let back = read_lone(&path, &options).expect("a foreign file imports");
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].owner_id, 7);
    assert_eq!(back[0].array, None, "a foreign file carries no array key");
    assert_eq!(back[0].owner_type, "Bus");
    // Evenly spaced rows read as a grid.
    let TimeSeriesData::SingleTimeSeries(s) = &back[0].data else {
        panic!("expected a SingleTimeSeries, got {:?}", back[0].data);
    };
    assert_eq!(s.resolution, Period::Fixed(Duration::hours(1)));
    assert_eq!(s.name, "load");
}

#[test]
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
    let err = read_lone(&path, &options).expect_err("a null is not NaN");
    assert!(err.to_string().contains("nulls"), "{err}");
}

#[test]
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
        let back = read_lone(path, &options).expect("a foreign file imports");
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
fn a_null_in_a_text_or_integer_column_is_refused() {
    // A null is not an absent value: the format writes absent *as* the empty
    // string precisely so that no column is nullable, and a null in an identity
    // column would file the row under a different series.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, hourly("load", &[1.0, 2.0]))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let path = &report.partitions[0].series_path;
    let (schema, batch) = read_one(path);

    let nulled = |column: &str, array: arrow::array::ArrayRef| {
        let index = schema.index_of(column).unwrap();
        let fields: Vec<arrow::datatypes::FieldRef> = schema
            .fields()
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if i == index {
                    std::sync::Arc::new(arrow::datatypes::Field::new(
                        f.name(),
                        f.data_type().clone(),
                        true,
                    ))
                } else {
                    f.clone()
                }
            })
            .collect();
        let mut columns = batch.columns().to_vec();
        columns[index] = array;
        let relaxed = std::sync::Arc::new(arrow::datatypes::Schema::new_with_metadata(
            arrow::datatypes::Fields::from(fields),
            schema.metadata().clone(),
        ));
        let edited = arrow::array::RecordBatch::try_new(relaxed, columns).expect("rebuild");
        write_batch(path, &edited);
        read_partition(&pair(&report), &ImportOptions::default()).expect_err("a null is refused")
    };

    let err = nulled(
        "units",
        std::sync::Arc::new(arrow::array::StringArray::from(vec![None::<&str>])),
    );
    assert!(
        err.to_string().contains("`units` column has nulls"),
        "{err}"
    );

    let err = nulled(
        "owner_id",
        std::sync::Arc::new(arrow::array::Int64Array::from(vec![None::<i64>])),
    );
    assert!(
        err.to_string().contains("`owner_id` column has nulls"),
        "{err}"
    );
}

#[test]
fn a_partition_streams_into_its_sink_one_series_at_a_time() {
    use infrastore_parquet::read_partition_with;

    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![
        (1, hourly("a", &[1.0, 2.0])),
        (2, hourly("b", &[3.0, 4.0])),
        (3, hourly("c", &[5.0, 6.0])),
    ]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let files = pair(&report);

    // Every series reaches the sink, and the count says so. The order is the
    // array key's, not the name's: the merge join walks the values file.
    let mut names = Vec::new();
    let filed = read_partition_with(&files, &ImportOptions::default(), &mut |one| {
        names.push(one.data.name().to_string());
        Ok(())
    })
    .expect("streams");
    assert_eq!(filed, 3);
    names.sort();
    assert_eq!(names, ["a", "b", "c"]);

    // A sink error stops the read where it happened and comes back as it is.
    let mut seen = 0;
    let err = read_partition_with(&files, &ImportOptions::default(), &mut |_| {
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
fn a_contradicting_assertion_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, hourly("load", &[1.0]))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let options = ImportOptions {
        element_type: Some(ElementType::Scalar(Dtype::I64)),
        ..Default::default()
    };
    let err = read_partition(&pair(&report), &options).expect_err("contradiction");
    assert!(err.to_string().contains("asserted"), "{err}");
}

// ---- helpers ---------------------------------------------------------------

/// The one partition an export wrote, as the pair the import takes.
fn pair(report: &infrastore_parquet::ExportReport) -> PartitionFiles {
    let written = &report.partitions[0];
    PartitionFiles {
        stem: written.stem.clone(),
        values: written.values_path.clone(),
        series: Some(written.series_path.clone()),
    }
}

/// A lone Parquet file, which is what a foreign one is.
fn read_lone(
    path: &Path,
    options: &ImportOptions,
) -> Result<Vec<ImportedSeries>, infrastore_core::TimeSeriesError> {
    let found = partitions(path)?;
    assert_eq!(found.len(), 1);
    assert!(found[0].series.is_none(), "a lone file has no series half");
    read_partition(&found[0], options)
}

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
fn forecasts_sharing_a_cube_share_one_values_group() {
    // Normalization is not a static-series trick: a forecast run against a
    // hundred identical units is one cube, and the values file holds it once.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(
        (0..100)
            .map(|owner| (owner, deterministic("day_ahead")))
            .collect(),
    ));
    let report = write_partitions(dir.path(), &series).expect("export");
    assert_eq!(report.arrays(), 1);
    assert_eq!(report.series(), 100);
    assert_eq!(report.rows(), 6, "3 steps x 2 windows, written once");

    let back = read_partition(&pair(&report), &ImportOptions::default()).expect("import");
    assert_eq!(back.len(), 100);
    for one in &back {
        let TimeSeriesData::Deterministic(got) = &one.data else {
            panic!("expected a Deterministic");
        };
        let TimeSeriesData::Deterministic(want) = deterministic("day_ahead") else {
            unreachable!()
        };
        assert_eq!(got, &want);
    }
}

#[test]
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
fn forecast_rows_are_placed_by_coordinates_not_order() {
    // A query engine may rewrite a values file in any order within one array;
    // the coordinates are what put each value back where it belongs.
    let dir = tempfile::tempdir().expect("tempdir");
    let original = deterministic("day_ahead");
    let series = stored(plain(vec![(1, original.clone())]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let path = &report.partitions[0].values_path;
    let (schema, batch) = read_one(path);

    let n = batch.num_rows() as u32;
    let indices = arrow::array::UInt32Array::from((0..n).rev().collect::<Vec<_>>());
    let reversed: Vec<arrow::array::ArrayRef> = batch
        .columns()
        .iter()
        .map(|c| arrow::compute::take(c, &indices, None).unwrap())
        .collect();
    let reversed = arrow::array::RecordBatch::try_new(schema, reversed).expect("rebuild");
    write_batch(path, &reversed);

    // Reversed and still identical, checksum included: the cube is rebuilt from
    // the coordinates, so the bytes it hashes are the ones it started with.
    let back = read_partition(&pair(&report), &ImportOptions::default()).expect("import");
    let (TimeSeriesData::Deterministic(got), TimeSeriesData::Deterministic(want)) =
        (&back[0].data, &original)
    else {
        panic!("expected a Deterministic");
    };
    assert_eq!(got, want);
}

#[test]
fn a_forecast_missing_its_grid_columns_is_refused() {
    // The values rows say where each value belongs, not what the grid it belongs
    // to is; that is the series file's job, and dropping it is not recoverable.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, deterministic("day_ahead"))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let path = &report.partitions[0].series_path;
    let (schema, batch) = read_one(path);

    let keep: Vec<usize> = (0..schema.fields().len())
        .filter(|i| schema.field(*i).name() != "horizon")
        .collect();
    write_batch(path, &batch.project(&keep).expect("project"));

    let err = read_partition(&pair(&report), &ImportOptions::default()).expect_err("no horizon");
    assert!(err.to_string().contains("horizon"), "{err}");
}

#[test]
fn a_forecast_short_of_its_grid_is_refused() {
    // A cube has no hole to leave, so a missing row cannot be filled in. The
    // count is checked before the checksum, which is the more useful message:
    // "the grid holds 6 values" says what to fix, "the hash differs" does not.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, deterministic("day_ahead"))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let path = &report.partitions[0].values_path;
    let (schema, batch) = read_one(path);

    let short = batch.slice(0, batch.num_rows() - 1);
    write_batch(
        path,
        &arrow::array::RecordBatch::try_new(schema, short.columns().to_vec()).unwrap(),
    );

    let err = read_partition(&pair(&report), &ImportOptions::default()).expect_err("a hole");
    assert!(err.to_string().contains("its grid holds"), "{err}");
}

#[test]
fn a_forecast_with_two_rows_for_one_slot_is_refused() {
    // The right number of rows, but one coordinate twice -- so somewhere else
    // has none, and the two rows disagree about the same value.
    let dir = tempfile::tempdir().expect("tempdir");
    let series = stored(plain(vec![(1, deterministic("day_ahead"))]));
    let report = write_partitions(dir.path(), &series).expect("export");
    let path = &report.partitions[0].values_path;
    let (schema, batch) = read_one(path);

    // Repeat row 0 in place of the last one.
    let n = batch.num_rows() as u32;
    let mut indices: Vec<u32> = (0..n - 1).collect();
    indices.push(0);
    let indices = arrow::array::UInt32Array::from(indices);
    let doubled: Vec<arrow::array::ArrayRef> = batch
        .columns()
        .iter()
        .map(|c| arrow::compute::take(c, &indices, None).unwrap())
        .collect();
    write_batch(
        path,
        &arrow::array::RecordBatch::try_new(schema, doubled).expect("rebuild"),
    );

    let err = read_partition(&pair(&report), &ImportOptions::default()).expect_err("a duplicate");
    assert!(err.to_string().contains("two rows for"), "{err}");
}
