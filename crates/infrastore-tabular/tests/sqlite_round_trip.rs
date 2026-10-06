//! Export a store into SQLite tables and read it back.
//!
//! Every series is compared against what the store hands back for it, so a type,
//! element kind, spelling or descriptor that does not survive the trip fails
//! here by name.

use std::path::Path;

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    Dtype, ElementType, Features, ListFilter, NonSequentialTimeSeries, OwnerCategory,
    PersistentTimeSeries, ReadWindow, SingleTimeSeries, Store, TimeRange, TimeReference,
    TimeSeriesData, TimeSeriesMetadata, TypedArray,
};
use infrastore_tabular::ImportOptions;
use infrastore_tabular::sqlite::{
    export_store, import_store, read_sqlite_partition_with, sqlite_partitions, write_sqlite,
};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

fn spelled(data: TimeSeriesData, reference: Option<TimeReference>) -> TimeSeriesData {
    let mut data = data;
    let descriptors = infrastore_core::Descriptors {
        element_type: data.element_type(),
        units: Some("MW".to_string()),
        quantity_kind: None,
        unit_system: None,
        time_reference: reference,
        component_field: Some("max_active_power".to_string()),
        application_data: None,
    };
    data.set_descriptors(descriptors);
    data
}

fn utc(data: TimeSeriesData) -> TimeSeriesData {
    spelled(data, Some(TimeReference::Utc))
}

fn single(name: &str, array: TypedArray) -> SingleTimeSeries {
    SingleTimeSeries::new(t0(), Duration::hours(1), array, name)
}

fn hourly(name: &str, values: &[f64]) -> TimeSeriesData {
    utc(TimeSeriesData::SingleTimeSeries(single(
        name,
        TypedArray::from_f64(vec![values.len()], values),
    )))
}

/// Every table's name, SQL and rows. Sorted, because the streamed export
/// writes a partition's arrays in storage order rather than key order.
fn dump(db: &Path) -> Vec<(String, String, Vec<Vec<rusqlite::types::Value>>)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut tables = conn
        .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap();
    let tables: Vec<(String, String)> = tables
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    tables
        .into_iter()
        .map(|(name, sql)| {
            let mut stmt = conn
                .prepare(&format!("SELECT * FROM \"{name}\" ORDER BY rowid"))
                .unwrap();
            let width = stmt.column_count();
            let mut rows: Vec<Vec<rusqlite::types::Value>> = stmt
                .query_map([], |r| (0..width).map(|i| r.get(i)).collect())
                .unwrap()
                .map(Result::unwrap)
                .collect();
            rows.sort_by_cached_key(|row| format!("{row:?}"));
            (name, sql, rows)
        })
        .collect()
}

/// The streamed export must write what the in-memory one does, table for table
/// and row for row, and refuse what it refuses.
fn assert_streams_alike(store: &Store, pairs: &[(TimeSeriesMetadata, TimeSeriesData)]) {
    let dir = tempfile::tempdir().unwrap();
    let (whole, streamed) = (dir.path().join("whole.db"), dir.path().join("streamed.db"));
    let whole_result = write_sqlite(&whole, pairs, "p_");
    let streamed_result = export_store(store, ListFilter::new(), None, &streamed, "p_", false);
    match (whole_result, streamed_result) {
        (Ok(_), Ok(_)) => assert_eq!(dump(&streamed), dump(&whole)),
        (Err(a), Err(b)) => assert_eq!(b.to_string(), a.to_string()),
        (a, b) => panic!("the two exports disagree: {a:?} vs {b:?}"),
    }
}

/// Store every item under its own owner, and hand back what the store reads.
///
/// Every selection a test builds this way is also run through
/// [`export_store`], so the streamed path meets each type, element kind and
/// refusal the in-memory one is tested on.
fn stored(items: Vec<TimeSeriesData>) -> Vec<(TimeSeriesMetadata, TimeSeriesData)> {
    let mut store = Store::create(None, true).expect("in-memory store");
    let pairs: Vec<_> = items
        .into_iter()
        .enumerate()
        .map(|(owner, data)| {
            let id = store
                .add_time_series(
                    owner as i64,
                    "Generator",
                    OwnerCategory::Component,
                    data,
                    Features::new(),
                )
                .expect("add");
            let row = store.get_metadata_by_id(id).unwrap().unwrap();
            let values = store
                .read_by_id(id, infrastore_core::ReadWindow::full())
                .unwrap();
            (row, values)
        })
        .collect();
    assert_streams_alike(&store, &pairs);
    pairs
}

fn import(
    db: &Path,
    prefix: &str,
    options: &ImportOptions,
) -> Vec<infrastore_tabular::ImportedSeries> {
    let mut out = Vec::new();
    for partition in sqlite_partitions(db, prefix).expect("partitions") {
        read_sqlite_partition_with(&partition, options, &mut |one| {
            out.push(one);
            Ok(())
        })
        .expect("import");
    }
    out.sort_by_key(|s| s.owner_id);
    out
}

fn table_names(db: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn every_type_and_element_kind_round_trips() {
    let stamps: Vec<DateTime<Utc>> = [0, 1, 5]
        .iter()
        .map(|h| t0() + Duration::hours(*h))
        .collect();
    let forecast_values: Vec<f64> = (0..12).map(f64::from).collect();
    let mut deterministic = infrastore_core::Deterministic::new(
        t0(),
        Duration::hours(1),
        Duration::hours(3),
        Duration::hours(1),
        2,
        TypedArray::from_f64(vec![3, 2], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        "det",
    )
    .unwrap();
    deterministic.time_reference = Some(TimeReference::Utc);
    let mut probabilistic = infrastore_core::Probabilistic::new(
        t0(),
        Duration::hours(1),
        Duration::hours(3),
        Duration::hours(1),
        2,
        vec![0.1, 0.9],
        TypedArray::from_f64(vec![2, 3, 2], &forecast_values),
        "prob",
    )
    .unwrap();
    probabilistic.time_reference = Some(TimeReference::Utc);
    let mut scenarios = infrastore_core::Scenarios::new(
        t0(),
        Duration::hours(1),
        Duration::hours(3),
        Duration::hours(1),
        2,
        2,
        TypedArray::from_f64(vec![2, 3, 2], &forecast_values),
        "scen",
    )
    .unwrap();
    scenarios.time_reference = Some(TimeReference::Utc);

    let items = vec![
        hourly("nan", &[1.0, f64::NAN, -0.5]),
        utc(TimeSeriesData::SingleTimeSeries(single(
            "f32",
            TypedArray::from_slice(vec![2], &[1.5f32, -2.25]).unwrap(),
        ))),
        utc(TimeSeriesData::SingleTimeSeries(single(
            "ints",
            TypedArray::from_slice(vec![2], &[7i32, -8]).unwrap(),
        ))),
        utc(TimeSeriesData::SingleTimeSeries(single(
            "u64",
            TypedArray::from_slice(vec![2], &[7u64, 8]).unwrap(),
        ))),
        utc(TimeSeriesData::SingleTimeSeries(single(
            "flags",
            TypedArray::from_slice(vec![2], &[true, false]).unwrap(),
        ))),
        utc(TimeSeriesData::SingleTimeSeries(single(
            "nested",
            TypedArray::from_f64(vec![2, 2, 3], &(0..12).map(f64::from).collect::<Vec<_>>()),
        ))),
        utc(TimeSeriesData::SingleTimeSeries(
            single(
                "tuple",
                TypedArray::from_f64(vec![2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
            )
            .with_element_type(ElementType::Tuple {
                arity: 3,
                dtype: Dtype::F64,
            }),
        )),
        utc(TimeSeriesData::SingleTimeSeries(
            single("narrow", TypedArray::from_f64(vec![1, 3], &[1.0, 0.0, 5.0]))
                .with_element_type(ElementType::PiecewiseLinear),
        )),
        utc(TimeSeriesData::SingleTimeSeries(
            single(
                "wide",
                TypedArray::from_f64(vec![1, 5], &[2.0, 0.0, 5.0, 1.0, 7.0]),
            )
            .with_element_type(ElementType::PiecewiseLinear),
        )),
        utc(TimeSeriesData::NonSequentialTimeSeries(
            NonSequentialTimeSeries::new(
                stamps.clone(),
                TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]),
                "irregular",
            )
            .unwrap(),
        )),
        utc(TimeSeriesData::PersistentTimeSeries(
            PersistentTimeSeries::new(
                stamps,
                TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]),
                "steps",
            )
            .unwrap(),
        )),
        spelled(hourly("unspecified", &[1.0]), None),
        spelled(hourly("zoneless", &[1.0]), Some(TimeReference::Zoneless)),
        spelled(
            hourly("denver", &[1.0]),
            Some(TimeReference::Zone("America/Denver".to_string())),
        ),
        spelled(
            hourly("offset", &[1.0]),
            Some(TimeReference::FixedOffset(-420)),
        ),
        TimeSeriesData::Deterministic(deterministic),
        TimeSeriesData::Probabilistic(probabilistic),
        TimeSeriesData::Scenarios(scenarios),
    ];
    let series = stored(items);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    write_sqlite(&db, &series, "").expect("export");

    let back = import(&db, "", &ImportOptions::default());
    assert_eq!(back.len(), series.len());
    for (one, (row, want)) in back.iter().zip(&series) {
        assert_eq!(one.owner_id, row.owner_id);
        assert_eq!(&one.data, want, "{} does not survive the trip", row.name);
        assert_eq!(one.recorded_id, row.id.map(|i| i.get()));
    }
}

#[test]
fn a_shared_profile_is_one_array_and_a_prefix_scopes_the_import() {
    let series = stored(vec![
        hourly("load", &[1.0, 2.0]),
        hourly("load", &[1.0, 2.0]),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("CREATE TABLE foo_values (x); CREATE TABLE foo_series (x);")
        .unwrap();

    let written = write_sqlite(&db, &series, "").expect("unprefixed export");
    assert_eq!(
        (written[0].arrays, written[0].series, written[0].rows),
        (1, 2, 2)
    );
    write_sqlite(&db, &series[..1], "run2_").expect("prefixed export beside it");
    assert_eq!(
        table_names(&db),
        [
            "SingleTimeSeries_f64_utc_arrays",
            "SingleTimeSeries_f64_utc_series",
            "SingleTimeSeries_f64_utc_values",
            "SingleTimeSeries_f64_utc_values_key",
            "foo_series",
            "foo_values",
            "run2_SingleTimeSeries_f64_utc_arrays",
            "run2_SingleTimeSeries_f64_utc_series",
            "run2_SingleTimeSeries_f64_utc_values",
            "run2_SingleTimeSeries_f64_utc_values_key",
        ]
    );

    // Each prefix sees exactly its own export, and neither sees `foo_*`.
    assert_eq!(import(&db, "", &ImportOptions::default()).len(), 2);
    assert_eq!(import(&db, "run2_", &ImportOptions::default()).len(), 1);
    let err = sqlite_partitions(&db, "nope_").unwrap_err();
    assert!(err.to_string().contains("\"nope_\""), "{err}");
    let err = write_sqlite(&db, &series, "bad-prefix").unwrap_err();
    assert!(err.to_string().contains("table prefix"), "{err}");

    // A prefix that could read as part of another prefix's tables is refused:
    // without the rules, `""` would pick up `SingleTimeSeries_`'s export, and
    // `""` would read `Deterministic` + a `SingleTimeSeries_…` stem as a
    // `DeterministicSingleTimeSeries` partition.
    for bad in [
        "run3",
        "2_",
        "sqlite_",
        "SQLITE_run_",
        "SingleTimeSeries_",
        "Deterministic",
        "x_Scenarios_",
    ] {
        let err = write_sqlite(&db, &series, bad).unwrap_err();
        assert!(err.to_string().contains("table prefix"), "{bad}: {err}");
        let err = sqlite_partitions(&db, bad).unwrap_err();
        assert!(err.to_string().contains("table prefix"), "{bad}: {err}");
    }
}

#[test]
fn a_discovered_table_name_needing_quotes_is_queried_safely() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TABLE \"SingleTimeSeries_a'b\"\"c_values\" (x);
             CREATE TABLE \"SingleTimeSeries_a'b\"\"c_arrays\" (x);
             CREATE TABLE \"SingleTimeSeries_a'b\"\"c_series\" (time_series_type, element_type);
             INSERT INTO \"SingleTimeSeries_a'b\"\"c_series\" VALUES ('SingleTimeSeries', 'f64');",
        )
        .unwrap();
    let partitions = sqlite_partitions(&db, "").expect("partitions");
    assert_eq!(partitions.len(), 1);
    // Reaches the column check (it would be a SQL syntax error unquoted).
    let err =
        read_sqlite_partition_with(&partitions[0], &ImportOptions::default(), &mut |_| Ok(()))
            .unwrap_err();
    assert!(err.to_string().contains("missing column"), "{err}");
}

#[test]
fn a_collision_writes_nothing() {
    let series = stored(vec![hourly("load", &[1.0]), hourly("other", &[2.0])]);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    write_sqlite(&db, &series[..1], "").expect("first export");
    let before = table_names(&db);

    // The second export would add nothing new but collides on one partition.
    let err = write_sqlite(&db, &series, "").unwrap_err();
    assert!(
        err.to_string().contains("SingleTimeSeries_f64_utc_values"),
        "{err}"
    );
    assert_eq!(table_names(&db), before);
}

#[test]
fn a_failed_export_into_a_new_file_leaves_no_file() {
    let series = stored(vec![utc(TimeSeriesData::SingleTimeSeries(single(
        "big",
        TypedArray::from_slice(vec![1], &[u64::MAX]).unwrap(),
    )))]);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    let err = write_sqlite(&db, &series, "").unwrap_err();
    assert!(err.to_string().contains("does not fit"), "{err}");
    assert!(!db.exists());

    // A file that was already there is never removed, even holding no tables.
    std::fs::write(&db, b"").expect("create an empty database");
    write_sqlite(&db, &series, "").unwrap_err();
    assert!(db.exists(), "a file this call did not create must survive");
}

#[test]
fn the_array_key_is_spelled_once_and_named_by_rowid() {
    let series = stored(vec![
        hourly("a", &[1.0, 2.0]),
        hourly("b", &[1.0, 2.0]),
        hourly("c", &[3.0, 4.0]),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    write_sqlite(&db, &series, "").expect("export");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let columns = |table: &str| -> Vec<String> {
        conn.prepare("SELECT name FROM pragma_table_info(?1)")
            .unwrap()
            .query_map([table], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        columns("SingleTimeSeries_f64_utc_arrays"),
        ["id", "data_hash", "time_axis"]
    );
    assert_eq!(
        columns("SingleTimeSeries_f64_utc_values"),
        ["array_id", "timestamp", "value"]
    );
    assert!(!columns("SingleTimeSeries_f64_utc_series").contains(&"data_hash".to_string()));
    // Two arrays for three series, each id its row's rowid.
    let ids: Vec<(i64, i64)> = conn
        .prepare("SELECT rowid, id FROM SingleTimeSeries_f64_utc_arrays ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(ids, [(1, 1), (2, 2)]);

    // A values row naming an array the arrays table lacks is refused by the
    // import, for a database written where the foreign key was not enforced.
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         INSERT INTO SingleTimeSeries_f64_utc_values VALUES (99, 0, 1.0);",
    )
    .unwrap();
    drop(conn);
    let partition = &sqlite_partitions(&db, "").unwrap()[0];
    let err = read_sqlite_partition_with(partition, &ImportOptions::default(), &mut |_| Ok(()))
        .unwrap_err();
    assert!(err.to_string().contains("`array_id` 99"), "{err}");
}

#[test]
fn an_edited_value_fails_the_checksum_unless_waived() {
    let series = stored(vec![hourly("load", &[1.0, 2.0])]);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    write_sqlite(&db, &series, "").expect("export");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE SingleTimeSeries_f64_utc_values SET value = 9.0 WHERE value = 2.0",
            [],
        )
        .unwrap();

    let partition = &sqlite_partitions(&db, "").unwrap()[0];
    let err = read_sqlite_partition_with(partition, &ImportOptions::default(), &mut |_| Ok(()))
        .unwrap_err();
    assert!(err.to_string().contains("data_hash"), "{err}");

    let waived = ImportOptions {
        skip_checksum: true,
        ..ImportOptions::default()
    };
    let back = import(&db, "", &waived);
    let TimeSeriesData::SingleTimeSeries(s) = &back[0].data else {
        panic!("expected a SingleTimeSeries");
    };
    assert_eq!(s.data.to_f64_vec().unwrap(), vec![1.0, 9.0]);
}

#[test]
fn a_dangling_series_row_is_refused() {
    let series = stored(vec![hourly("load", &[1.0])]);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    write_sqlite(&db, &series, "").expect("export");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM SingleTimeSeries_f64_utc_values", [])
        .unwrap();
    let partition = &sqlite_partitions(&db, "").unwrap()[0];
    let err = read_sqlite_partition_with(partition, &ImportOptions::default(), &mut |_| Ok(()))
        .unwrap_err();
    assert!(err.to_string().contains("does not hold"), "{err}");
}

/// Export `items`, run `sql` against the database, and import everything
/// back, returning the first error discovery or reading reports.
fn import_after(
    items: Vec<TimeSeriesData>,
    sql: &str,
    options: &ImportOptions,
) -> Result<(), String> {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    write_sqlite(&db, &stored(items), "").expect("export");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
    for partition in sqlite_partitions(&db, "").map_err(|e| e.to_string())? {
        read_sqlite_partition_with(&partition, options, &mut |_| Ok(()))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn of<T: infrastore_core::Element>(name: &str, values: &[T]) -> TimeSeriesData {
    utc(TimeSeriesData::SingleTimeSeries(single(
        name,
        TypedArray::from_slice(vec![values.len()], values).unwrap(),
    )))
}

fn tuple3(values: &[f64]) -> TimeSeriesData {
    utc(TimeSeriesData::SingleTimeSeries(
        single(
            "tuple",
            TypedArray::from_f64(vec![values.len() / 3, 3], values),
        )
        .with_element_type(ElementType::Tuple {
            arity: 3,
            dtype: Dtype::F64,
        }),
    ))
}

#[test]
fn hand_edited_tables_are_refused_naming_the_problem() {
    let f64s = || vec![hourly("a", &[1.0, 2.0]), hourly("b", &[3.0, 4.0])];
    let cases: Vec<(&str, Vec<TimeSeriesData>, &str, &str)> = vec![
        // ---- the `value` cells ----
        (
            "a ragged JSON array",
            vec![tuple3(&[1.0, 2.0, 3.0])],
            "UPDATE SingleTimeSeries_tuple3_f64_utc_values SET value = '[1, [2], 3]'",
            "ragged",
        ),
        (
            "two cells of one array with different widths",
            vec![tuple3(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0])],
            "UPDATE SingleTimeSeries_tuple3_f64_utc_values SET value = '[1, 2]' \
             WHERE rowid = 2",
            "different shapes",
        ),
        (
            "a NULL in an integer array",
            vec![of("ints", &[7i32, 8])],
            "UPDATE SingleTimeSeries_i32_utc_values SET value = NULL",
            "not an integer",
        ),
        (
            "a decimal in an integer array",
            vec![of("ints", &[7i32, 8])],
            "UPDATE SingleTimeSeries_i32_utc_values SET value = 1.5",
            "not an integer",
        ),
        (
            "a value too wide for its dtype",
            vec![of("bytes", &[7i8, 8])],
            "UPDATE SingleTimeSeries_i8_utc_values SET value = 1000",
            "does not fit a i8",
        ),
        (
            "a bool other than 0 or 1",
            vec![of("flags", &[true, false])],
            "UPDATE SingleTimeSeries_bool_utc_values SET value = 2",
            "other than 0 or 1",
        ),
        (
            "a blob",
            vec![hourly("a", &[1.0])],
            "UPDATE SingleTimeSeries_f64_utc_values SET value = x'00'",
            "blob",
        ),
        // ---- the tables themselves ----
        (
            "a missing required series column",
            vec![hourly("a", &[1.0])],
            "ALTER TABLE SingleTimeSeries_f64_utc_series DROP COLUMN units",
            "missing column(s) a SingleTimeSeries series table carries: units",
        ),
        (
            "a missing required values column",
            vec![hourly("a", &[1.0])],
            "ALTER TABLE SingleTimeSeries_f64_utc_values DROP COLUMN timestamp",
            "missing column(s) a SingleTimeSeries values table carries: timestamp",
        ),
        (
            "a series table mixing element types",
            f64s(),
            "UPDATE SingleTimeSeries_f64_utc_series SET element_type = 'f32' WHERE name = 'b'",
            "mixes",
        ),
        (
            "a values table with no series table",
            vec![hourly("a", &[1.0])],
            "DROP TABLE SingleTimeSeries_f64_utc_series",
            "but no SingleTimeSeries_f64_utc_series beside it",
        ),
        (
            "an empty series table beside values",
            vec![hourly("a", &[1.0])],
            "DELETE FROM SingleTimeSeries_f64_utc_series",
            "is empty but",
        ),
        (
            "values no series row names",
            f64s(),
            "DELETE FROM SingleTimeSeries_f64_utc_series WHERE name = 'b'",
            "no series row names",
        ),
    ];
    for (what, items, sql, expected) in cases {
        let err = import_after(items, sql, &ImportOptions::default())
            .expect_err(&format!("{what} should be refused"));
        assert!(err.contains(expected), "{what}: {err}");
    }
}

#[test]
fn nested_infinities_round_trip_and_a_scalar_negative_zero_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    let series = stored(vec![tuple3(&[
        f64::INFINITY,
        f64::NEG_INFINITY,
        -0.0,
        1.5,
        f64::NAN,
        2.0,
    ])]);
    write_sqlite(&db, &series, "").expect("export");
    let back = import(&db, "", &ImportOptions::default());
    assert_eq!(back.len(), 1);
    assert_eq!(
        back[0].data, series[0].1,
        "bitwise, so the checksum agreed too"
    );

    let err = write_sqlite(
        &dir.path().join("zero.db"),
        &stored(vec![hourly("z", &[1.0, -0.0])]),
        "",
    )
    .unwrap_err();
    assert!(err.to_string().contains("-0.0"), "{err}");
}

#[test]
fn whole_numbers_in_a_float_array_read_back_as_floats() {
    // JSON `[1, 2, 3]` is integers to a parser; an f64 array takes them as the
    // same values, so even the checksum agrees.
    import_after(
        vec![tuple3(&[1.0, 2.0, 3.0])],
        "UPDATE SingleTimeSeries_tuple3_f64_utc_values SET value = '[1, 2, 3]'",
        &ImportOptions::default(),
    )
    .expect("an integer spelling of the same value is the same value");
}

#[test]
fn one_curve_at_two_paddings_is_one_array() {
    let curve = |name: &str, row: &[f64]| {
        utc(TimeSeriesData::SingleTimeSeries(
            single(name, TypedArray::from_f64(vec![1, row.len()], row))
                .with_element_type(ElementType::PiecewiseLinear),
        ))
    };
    let series = stored(vec![
        curve("narrow", &[1.0, 0.0, 5.0]),
        curve("padded", &[1.0, 0.0, 5.0, 0.0, 0.0]),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    let written = write_sqlite(&db, &series, "").expect("export");
    assert_eq!((written[0].arrays, written[0].series), (1, 2));

    for one in import(&db, "", &ImportOptions::default()) {
        let TimeSeriesData::SingleTimeSeries(s) = &one.data else {
            panic!("expected a SingleTimeSeries");
        };
        assert_eq!(
            s.data.shape,
            vec![1, 3],
            "{} comes back at its own width",
            s.name
        );
        assert_eq!(s.data.to_f64_vec().unwrap(), vec![1.0, 0.0, 5.0]);
    }
}

#[test]
fn partitions_that_flatten_to_one_name_get_distinct_tables() {
    // `Etc/X` and `Etc_X` are different zones and different partitions, but the
    // same table name once `/` becomes `_`.
    let series = stored(vec![
        spelled(
            hourly("slash", &[1.0]),
            Some(TimeReference::Zone("Etc/X".to_string())),
        ),
        spelled(
            hourly("under", &[2.0]),
            Some(TimeReference::Zone("Etc_X".to_string())),
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    let written = write_sqlite(&db, &series, "").expect("export");
    let tables: Vec<&str> = written.iter().map(|t| t.values_table.as_str()).collect();
    assert_eq!(
        tables,
        [
            "SingleTimeSeries_f64_Etc_X_values",
            "SingleTimeSeries_f64_Etc_X_2_values"
        ]
    );
    let back = import(&db, "", &ImportOptions::default());
    for (one, (_, want)) in back.iter().zip(&series) {
        assert_eq!(&one.data, want);
    }
}

#[test]
fn a_collision_is_case_insensitive() {
    let series = stored(vec![hourly("load", &[1.0])]);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("CREATE TABLE singletimeseries_f64_utc_values (x)")
        .unwrap();
    let err = write_sqlite(&db, &series, "").unwrap_err();
    assert!(
        err.to_string().contains("SingleTimeSeries_f64_utc_values"),
        "{err}"
    );
    assert_eq!(table_names(&db), ["singletimeseries_f64_utc_values"]);
}

#[test]
fn an_empty_selection_creates_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");
    assert!(
        write_sqlite(&db, &[], "")
            .expect("nothing to do")
            .is_empty()
    );
    assert!(!db.exists());
}

fn add(store: &mut Store, owner: i64, data: TimeSeriesData) {
    store
        .add_time_series(
            owner,
            "Generator",
            OwnerCategory::Component,
            data,
            Features::new(),
        )
        .expect("add");
}

#[test]
fn a_store_exports_a_selection_and_imports_all_or_nothing() {
    let mut source = Store::create(None, true).unwrap();
    add(&mut source, 1, hourly("load", &[1.0, 2.0, 3.0]));
    add(&mut source, 2, of::<i64>("status", &[1, 0, 1]));
    add(&mut source, 3, hourly("other", &[9.0]));
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");

    // A bad prefix is refused before anything is read or created.
    assert!(export_store(&source, ListFilter::new(), None, &db, "no-dash", false).is_err());
    assert!(!db.exists());

    let filter = ListFilter::new().name_glob("[ls]*");
    let range = TimeRange::spelled(t0() + Duration::hours(1), t0() + Duration::hours(3), false);
    let written = export_store(&source, filter, Some(range), &db, "", false).expect("export");
    assert_eq!(written.iter().map(|t| t.series).sum::<usize>(), 2);
    assert_eq!(written.iter().map(|t| t.rows).sum::<usize>(), 4);

    // The i64 partition sorts after the f64 one, so a clash there fails the
    // import with the f64 series already added -- and it must not stay.
    let mut clash = Store::create(None, true).unwrap();
    add(&mut clash, 2, of::<i64>("status", &[7]));
    assert!(import_store(&mut clash, &db, "", &ImportOptions::default()).is_err());
    assert_eq!(clash.list_metadata(ListFilter::new()).unwrap().len(), 1);

    let mut target = Store::create(None, true).unwrap();
    let ids = import_store(&mut target, &db, "", &ImportOptions::default()).expect("import");
    assert_eq!(ids.len(), 2);
    let read = target.read_by_ids(&ids, ReadWindow::full()).unwrap();
    let TimeSeriesData::SingleTimeSeries(load) = &read[0] else {
        panic!("the f64 partition reads first");
    };
    assert_eq!(load.name, "load");
    assert_eq!(load.initial_timestamp, t0() + Duration::hours(1));
    assert_eq!(load.data, TypedArray::from_f64(vec![2], &[2.0, 3.0]));
}

#[test]
fn a_derived_forecast_is_left_out_so_the_export_reads_back() {
    let mut source = Store::create(None, true).unwrap();
    add(&mut source, 1, hourly("load", &[1.0, 2.0, 3.0, 4.0]));
    source
        .transform_single_time_series(
            Duration::hours(2),
            Duration::hours(1),
            None,
            None,
            Default::default(),
        )
        .expect("transform");
    assert_eq!(source.list_metadata(ListFilter::new()).unwrap().len(), 2);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("out.db");

    // Asking for the derived type by name is refused, not answered with nothing.
    let derived = ListFilter::new()
        .time_series_type(infrastore_core::TimeSeriesType::DeterministicSingleTimeSeries);
    let err = export_store(&source, derived.clone(), None, &db, "", false).unwrap_err();
    assert!(err.to_string().contains("derived"), "{err}");
    assert!(!db.exists());

    let written = export_store(&source, ListFilter::new(), None, &db, "", false).expect("export");
    assert_eq!(written.iter().map(|t| t.series).sum::<usize>(), 1);
    let mut target = Store::create(None, true).unwrap();
    let ids = import_store(&mut target, &db, "", &ImportOptions::default()).expect("import");
    assert_eq!(ids.len(), 1);

    // Asked for, the derived series is written as a partition of its own --
    // by name or with the rest -- and the import refuses that partition.
    let written = export_store(&source, derived, None, &db, "only_", true).expect("by name");
    assert_eq!(written.len(), 1);
    assert_eq!(
        written[0].time_series_type,
        infrastore_core::TimeSeriesType::DeterministicSingleTimeSeries
    );
    let written = export_store(&source, ListFilter::new(), None, &db, "all_", true).expect("all");
    assert_eq!(written.iter().map(|t| t.series).sum::<usize>(), 2);
    let mut target = Store::create(None, true).unwrap();
    let err = import_store(&mut target, &db, "all_", &ImportOptions::default()).unwrap_err();
    assert!(err.to_string().contains("derived"), "{err}");
}
