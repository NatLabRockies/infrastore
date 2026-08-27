//! Independent sidecar serde: the store's HDF5 array layout and its SQLite
//! association rows, each built and inspected without going through the half
//! of the `Store` API that would make the round trip a tautology.
//!
//! `sidecar_reads_back_through_the_public_api` builds a store's on-disk pair
//! directly — the HDF5 datasets via `hdf5_metno`, the catalog rows via raw
//! SQL against the documented schema (`docs/src/reference/file-format.md`) —
//! and then exercises *deserialization* by opening it with `open_store` and
//! reading it back through the public API. `public_api_writes_the_documented_layout`
//! runs the other direction: writes through `add_time_series` /
//! `add_parent_child_association`, then inspects the raw `.h5` and `.sqlite`
//! files directly to check *serialization* against the same spec.
//!
//! Everything is generated in a tempdir; no binary fixtures are checked in.

use chrono::{Duration, TimeZone, Utc};
use hdf5_metno as h5;
use sha2::{Digest, Sha256};

use infrastore_core::{
    CatalogMode, Compression, Deterministic, Dtype, ElementType, Features, OwnerCategory,
    ParentChildAssociation, ParentChildFilter, SingleTimeSeries, TimeSeriesData, TimeSeriesType,
    TypedArray, array_hash, catalog_sqlite_path, create_store, create_store_with_catalog, hash_hex,
    open_store,
};

/// SHA-256 of an empty `Features` map, reproducing the domain documented for
/// `feature_sets` ("an empty feature map stores no rows") and implemented in
/// `crate::hash::features_hash`: `b"features\0"` then the entry count as a
/// little-endian `u64`, with no per-entry bytes for zero entries.
fn empty_features_hash() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"features\0");
    hasher.update(0u64.to_le_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// One row of `time_series_associations`, hand-inserted with raw SQL. Mirrors
/// the column list `MetadataStore::insert_batched` writes (`src/metadata.rs`)
/// and the schema in `docs/src/reference/file-format.md#time_series_associations`.
struct RawAssocRow<'a> {
    owner_id: i64,
    owner_type: &'a str,
    owner_category: i64,
    time_series_type: i64,
    name: &'a str,
    data_hash: [u8; 32],
    initial_timestamp: String,
    resolution: String,
    length: i64,
    horizon: Option<String>,
    interval: Option<String>,
    count: Option<i64>,
    units: Option<&'a str>,
    element_type: String,
    element_shape_json: String,
}

fn insert_association_row(conn: &rusqlite::Connection, row: &RawAssocRow) {
    conn.execute(
        "INSERT INTO time_series_associations
         (owner_id, owner_type, owner_category, time_series_type, name, data_hash,
          initial_timestamp, resolution, length, horizon, interval, count,
          timestamps_hash, units, quantity_kind, unit_system, component_field,
          percentiles_json, element_type, element_shape, application_data, features_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, ?13, NULL, NULL, NULL,
                 NULL, ?14, ?15, NULL, ?16)",
        rusqlite::params![
            row.owner_id,
            row.owner_type,
            row.owner_category,
            row.time_series_type,
            row.name,
            row.data_hash.as_slice(),
            row.initial_timestamp,
            row.resolution,
            row.length,
            row.horizon,
            row.interval,
            row.count,
            row.units,
            row.element_type,
            row.element_shape_json,
            empty_features_hash().as_slice(),
        ],
    )
    .unwrap();
}

/// Build (and validate, via the public `TypedArray::from_f64`) the two arrays
/// this test writes: a scalar static series and a `[H, count]` dense forecast
/// window, so leading-dims geometry is exercised on both directions.
fn static_values() -> Vec<f64> {
    vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0]
}

fn forecast_values(horizon_len: usize, count: usize) -> Vec<f64> {
    (0..horizon_len * count).map(|i| 100.0 + i as f64).collect()
}

/// Deserialization: an HDF5 sidecar built directly with `hdf5_metno`, plus a
/// hand-inserted `time_series_associations` row and a hand-inserted
/// `parent_child_associations` row, must read back through the public
/// `Store` API exactly.
#[test]
fn sidecar_reads_back_through_the_public_api() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidecar.h5");
    let sqlite_path = catalog_sqlite_path(&path);

    // Bootstrap a valid, empty, correctly-stamped pair through the public API
    // — this is boilerplate (root attributes, the `time_series/single` group,
    // the paired generation stamp), not the layout under test. Everything
    // that *is* under test — the array datasets and the association rows — is
    // added below directly against the HDF5 file and the SQLite catalog.
    {
        let mut store =
            create_store_with_catalog(Some(&path), false, Compression::None, CatalogMode::Attached)
                .unwrap();
        store.flush().unwrap();
    }

    let initial_timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

    // A static SingleTimeSeries-shaped array, packed layout: `sts_f64_s_6_PT1H`
    // plus its `_h` hash companion, one column wide.
    let static_values = static_values();
    let static_array = TypedArray::from_f64(vec![static_values.len()], &static_values);
    let static_hash = array_hash(&static_array);
    let sts_name = format!("sts_f64_s_{}_PT1H", static_values.len());
    let sts_hash_name = format!("{sts_name}_h");

    // A dense-forecast-shaped array `[H, count]`, standalone layout:
    // `arr_{hex_hash}`. H=3 window steps at PT1H resolution (horizon PT3H),
    // 2 windows.
    let (horizon_len, count) = (3usize, 2usize);
    let forecast_values = forecast_values(horizon_len, count);
    let forecast_array = TypedArray::from_f64(vec![horizon_len, count], &forecast_values);
    let forecast_hash = array_hash(&forecast_array);
    let arr_name = format!("arr_{}", hash_hex(&forecast_hash));

    {
        let f = h5::File::open_rw(&path).unwrap();
        let single = f.group("time_series/single").unwrap();

        let sts_ds = single
            .new_dataset::<f64>()
            .shape(vec![static_values.len(), 1])
            .chunk(vec![1, 1])
            .create(sts_name.as_str())
            .unwrap();
        sts_ds.write_raw(&static_values).unwrap();
        let hash_ds = single
            .new_dataset::<u8>()
            .shape(vec![1, 64])
            .create(sts_hash_name.as_str())
            .unwrap();
        hash_ds
            .write_raw(hash_hex(&static_hash).as_bytes())
            .unwrap();

        let arr_ds = single
            .new_dataset::<f64>()
            .shape(vec![horizon_len, count])
            .create(arr_name.as_str())
            .unwrap();
        arr_ds.write_raw(&forecast_values).unwrap();
    }

    {
        let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
        insert_association_row(
            &conn,
            &RawAssocRow {
                owner_id: 1,
                owner_type: "Generator",
                owner_category: OwnerCategory::Component.code(),
                time_series_type: TimeSeriesType::SingleTimeSeries.code(),
                name: "load",
                data_hash: static_hash,
                initial_timestamp: initial_timestamp.to_rfc3339(),
                resolution: "PT1H".to_string(),
                length: static_values.len() as i64,
                horizon: None,
                interval: None,
                count: None,
                units: Some("MW"),
                element_type: ElementType::Scalar(Dtype::F64).to_string(),
                element_shape_json: serde_json::to_string(&Vec::<usize>::new()).unwrap(),
            },
        );
        insert_association_row(
            &conn,
            &RawAssocRow {
                owner_id: 1,
                owner_type: "Generator",
                owner_category: OwnerCategory::Component.code(),
                time_series_type: TimeSeriesType::Deterministic.code(),
                name: "forecast",
                data_hash: forecast_hash,
                initial_timestamp: initial_timestamp.to_rfc3339(),
                resolution: "PT1H".to_string(),
                // `length` for a forecast is the array's own leading dim (H),
                // matching `forecast_metadata`'s `Some(data.length())`.
                length: horizon_len as i64,
                horizon: Some("PT3H".to_string()),
                interval: Some("PT1H".to_string()),
                count: Some(count as i64),
                units: None,
                element_type: ElementType::Scalar(Dtype::F64).to_string(),
                // `TypedArray::element_shape()` is the trailing dims after the
                // leading time axis, which for `[H, count]` is `[count]`.
                element_shape_json: serde_json::to_string(&[count]).unwrap(),
            },
        );
        conn.execute(
            "INSERT INTO parent_child_associations (parent_id, parent_type, child_id, child_type)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![10i64, "Generator", 20i64, "Bus"],
        )
        .unwrap();
    }

    // Deserialization: read the hand-built sidecar back through the public API.
    let store = open_store(&path, true).unwrap();

    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "integrity errors: {:?}", report.errors);

    let keys = store
        .get_time_series_keys(1, OwnerCategory::Component)
        .unwrap();
    assert_eq!(keys.len(), 2, "{keys:?}");

    let single_key = keys
        .iter()
        .find(|k| k.identity().name == "load")
        .expect("the static series");
    let single_data = store.get_time_series(single_key.identity(), None).unwrap();
    let single = single_data.as_single().expect("SingleTimeSeries");
    assert_eq!(single.data.to_f64_vec().unwrap(), static_values);
    assert_eq!(single.initial_timestamp, initial_timestamp);
    let single_meta = store.get_metadata(single_key.identity()).unwrap();
    assert_eq!(single_meta.units, Some("MW".to_string()));
    assert_eq!(single_meta.data_hash, static_hash);

    let forecast_key = keys
        .iter()
        .find(|k| k.identity().name == "forecast")
        .expect("the forecast");
    let forecast_data = store
        .get_time_series(forecast_key.identity(), None)
        .unwrap();
    let det = forecast_data.as_deterministic().expect("Deterministic");
    assert_eq!(det.data.to_f64_vec().unwrap(), forecast_values);
    assert_eq!(det.count, count);
    assert_eq!(det.horizon.to_iso8601(), "PT3H");
    assert_eq!(det.interval.to_iso8601(), "PT1H");
    assert_eq!(det.resolution.to_iso8601(), "PT1H");

    let edges = store
        .list_parent_child_associations(&ParentChildFilter::new().parent_id(10))
        .unwrap();
    assert_eq!(
        edges,
        vec![ParentChildAssociation {
            parent_id: 10,
            parent_type: "Generator".to_string(),
            child_id: 20,
            child_type: "Bus".to_string(),
            id: None,
        }]
    );
}

/// Serialization: writing through the public API must produce the documented
/// layout, checked by reading the raw `.h5` and `.sqlite` files directly
/// rather than through the store's own reader.
#[test]
fn public_api_writes_the_documented_layout() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let initial_timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

    let static_values = static_values();
    let (horizon_len, count) = (3usize, 2usize);
    let forecast_values = forecast_values(horizon_len, count);

    let (static_hash, forecast_hash) = {
        let mut store = create_store(Some(&path), false).unwrap();

        let single = SingleTimeSeries::new(
            initial_timestamp,
            Duration::hours(1),
            TypedArray::from_f64(vec![static_values.len()], &static_values),
            "load",
        );
        let static_hash = array_hash(&single.data);
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(single).with_units("MW"),
                Features::new(),
            )
            .unwrap();

        let det = Deterministic::new(
            initial_timestamp,
            Duration::hours(1),
            Duration::hours(horizon_len as i64),
            Duration::hours(1),
            count,
            TypedArray::from_f64(vec![horizon_len, count], &forecast_values),
            "forecast",
        )
        .unwrap();
        let forecast_hash = array_hash(&det.data);
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::Deterministic(det),
                Features::new(),
            )
            .unwrap();

        store
            .add_parent_child_association(ParentChildAssociation {
                parent_id: 10,
                parent_type: "Generator".to_string(),
                child_id: 20,
                child_type: "Bus".to_string(),
                id: None,
            })
            .unwrap();

        store.flush().unwrap();
        (static_hash, forecast_hash)
    };

    // --- HDF5 half, read directly -------------------------------------------
    let f = h5::File::open(&path).unwrap();
    assert_eq!(
        f.attr("storage_backend")
            .unwrap()
            .read_scalar::<h5::types::VarLenUnicode>()
            .unwrap()
            .to_string(),
        "hdf5"
    );

    let single_group = f.group("time_series/single").unwrap();
    let members = single_group.member_names().unwrap();

    // The packed static pool: name encodes dtype/shape/length/resolution
    // exactly (`sts_f64_s_{length}_PT1H`), with no spill since this is the
    // only write to it.
    let sts_name = format!("sts_f64_s_{}_PT1H", static_values.len());
    assert!(
        members.contains(&sts_name),
        "expected {sts_name} among {members:?}"
    );
    let sts_ds = single_group.dataset(&sts_name).unwrap();
    let sts_shape = sts_ds.shape();
    assert_eq!(sts_shape[0], static_values.len());
    let cols = sts_shape[1];
    let hash_ds = single_group.dataset(&format!("{sts_name}_h")).unwrap();
    assert_eq!(hash_ds.shape(), vec![cols, 64]);

    // Locate the column this array landed in by scanning the hash companion —
    // the store is free to size the pool wider than one column.
    let hash_bytes = hash_ds.read_raw::<u8>().unwrap();
    let expected_hex = hash_hex(&static_hash);
    let col = (0..cols)
        .find(|&c| &hash_bytes[c * 64..(c + 1) * 64] == expected_hex.as_bytes())
        .expect("the static array's hash must be recorded in the hash companion");
    let flat = sts_ds.read_raw::<f64>().unwrap();
    let column: Vec<f64> = (0..static_values.len())
        .map(|t| flat[t * cols + col])
        .collect();
    assert_eq!(column, static_values);

    // The standalone forecast array: name is the hash outright, shape is
    // `[H, count]`.
    let arr_name = format!("arr_{}", hash_hex(&forecast_hash));
    assert!(
        members.contains(&arr_name),
        "expected {arr_name} among {members:?}"
    );
    let arr_ds = single_group.dataset(&arr_name).unwrap();
    assert_eq!(arr_ds.shape(), vec![horizon_len, count]);
    assert_eq!(arr_ds.read_raw::<f64>().unwrap(), forecast_values);

    // --- SQLite half, read directly -----------------------------------------
    let conn = rusqlite::Connection::open(catalog_sqlite_path(&path)).unwrap();

    let (owner_category, ts_type, resolution, length, element_type, element_shape, data_hash): (
        i64,
        i64,
        String,
        i64,
        String,
        String,
        Vec<u8>,
    ) = conn
        .query_row(
            "SELECT owner_category, time_series_type, resolution, length, element_type,
                     element_shape, data_hash
             FROM time_series_associations WHERE name = 'load'",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(owner_category, OwnerCategory::Component.code());
    assert_eq!(ts_type, TimeSeriesType::SingleTimeSeries.code());
    assert_eq!(resolution, "PT1H");
    assert_eq!(length, static_values.len() as i64);
    assert_eq!(element_type, "f64");
    assert_eq!(element_shape, "[]");
    assert_eq!(data_hash, static_hash.to_vec());

    let (horizon, interval, fc_count, fc_element_shape, fc_data_hash): (
        String,
        String,
        i64,
        String,
        Vec<u8>,
    ) = conn
        .query_row(
            "SELECT horizon, interval, count, element_shape, data_hash
             FROM time_series_associations WHERE name = 'forecast'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(horizon, "PT3H");
    assert_eq!(interval, "PT1H");
    assert_eq!(fc_count, count as i64);
    // Trailing dims after the leading `H` axis: just `[count]` here.
    assert_eq!(fc_element_shape, serde_json::to_string(&[count]).unwrap());
    assert_eq!(fc_data_hash, forecast_hash.to_vec());

    let (parent_id, parent_type, child_id, child_type): (i64, String, i64, String) = conn
        .query_row(
            "SELECT parent_id, parent_type, child_id, child_type FROM parent_child_associations",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (
            parent_id,
            parent_type.as_str(),
            child_id,
            child_type.as_str()
        ),
        (10, "Generator", 20, "Bus")
    );
}
