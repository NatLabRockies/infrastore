//! End-to-end gRPC round-trip: spin up a tonic server backed by a real on-disk
//! Store, then drive it through the typed `RemoteClient`. Covers every RPC.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    Dtype, ElementType, FeatureValue, Features, NonSequentialTimeSeries, OwnerCategory, Period,
    PersistentTimeSeries, SingleTimeSeries, Store, TimeSeriesData, TimeSeriesType, TypedArray,
    create_store,
};
use infrastore_server::client::RemoteClient;
use infrastore_server::service::CatalogStoreService;
use tokio::net::TcpListener;

async fn spawn_server(store: Store) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let service = CatalogStoreService::new(store);
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service.into_server())
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    // Tiny pause so the server is listening before the client connects.
    tokio::time::sleep(StdDuration::from_millis(50)).await;
    format!("http://{local_addr}")
}

fn series(initial_year: i32, length: usize, base: f64) -> SingleTimeSeries {
    let initial = Utc.with_ymd_and_hms(initial_year, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let values: Vec<f64> = (0..length).map(|i| base + i as f64).collect();
    let data = TypedArray::from_f64(vec![length], &values);
    SingleTimeSeries::new(initial, resolution, data, "load")
}

fn fixture_store() -> Store {
    let mut store = create_store(None, true).unwrap();
    let s = series(2024, 24, 100.0);
    let mut features: Features = BTreeMap::new();
    features.insert("model_year".into(), FeatureValue::Int(2030));
    store
        .add_time_series(
            42,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s).with_units("MW"),
            features,
        )
        .unwrap();
    let s2 = series(2024, 24, 5.0);
    store
        .add_time_series(
            43,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s2),
            Features::new(),
        )
        .unwrap();
    store
}

/// The catalog ids of every series an owner holds, in catalog order.
///
/// The wire has no key: `list_metadata` is the identify half and every row
/// carries the id the read half takes.
async fn owner_ids(client: &RemoteClient, owner: i64) -> Vec<infrastore_core::TimeSeriesId> {
    owner_rows(client, owner)
        .await
        .into_iter()
        .map(|m| m.id.expect("a served row carries its id"))
        .collect()
}

/// The full catalog rows for one owner.
async fn owner_rows(client: &RemoteClient, owner: i64) -> Vec<infrastore_core::TimeSeriesMetadata> {
    client
        .list_metadata(
            Some(owner),
            Some(OwnerCategory::Component),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap()
}

/// Every row in the store.
async fn all_rows(client: &RemoteClient) -> Vec<infrastore_core::TimeSeriesMetadata> {
    client
        .list_metadata(None, None, None, None, None, None, None, None, None, None)
        .await
        .unwrap()
}

#[tokio::test]
async fn list_and_get_round_trip() {
    let addr = spawn_server(fixture_store()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let metas = client
        .list_metadata(None, None, None, None, None, None, None, None, None, None)
        .await
        .unwrap();
    assert_eq!(metas.len(), 2);

    // Fetch by key.
    let ids = owner_ids(&client, 42).await;
    assert_eq!(ids.len(), 1);
    let data = client.read_by_id(ids[0], None).await.unwrap();
    let single = data.as_single().unwrap();
    assert_eq!(single.length, 24);
    assert_eq!(single.data.to_f64_vec().unwrap()[0], 100.0);
    assert_eq!(single.data.to_f64_vec().unwrap()[23], 123.0);
}

#[tokio::test]
async fn time_range_slicing_over_grpc() {
    let addr = spawn_server(fixture_store()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let ids = owner_ids(&client, 42).await;
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let start = initial + Duration::hours(2);
    let end = initial + Duration::hours(5);
    let data = client
        .read_by_id(ids[0], Some((start, end).into()))
        .await
        .unwrap();
    let single = data.as_single().unwrap();
    assert_eq!(single.length, 3);
    assert_eq!(single.initial_timestamp, start);
    assert_eq!(single.data.to_f64_vec().unwrap(), vec![102.0, 103.0, 104.0]);
}

#[tokio::test]
async fn list_filter_by_features_subset() {
    let addr = spawn_server(fixture_store()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let mut filter: Features = BTreeMap::new();
    filter.insert("model_year".into(), FeatureValue::Int(2030));
    let metas = client
        .list_metadata(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&filter),
        )
        .await
        .unwrap();
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].owner_id, 42);
}

#[tokio::test]
async fn counts_resolutions_has_verify() {
    let addr = spawn_server(fixture_store()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let counts = client.get_counts().await.unwrap();
    assert_eq!(counts.static_time_series, 2);
    assert_eq!(counts.components_with_time_series, 2);

    let resolutions = client.get_resolutions(None).await.unwrap();
    assert_eq!(resolutions, vec![Duration::hours(1)]);

    let resolutions_typed = client
        .get_resolutions(Some(TimeSeriesType::SingleTimeSeries))
        .await
        .unwrap();
    assert_eq!(resolutions_typed, vec![Duration::hours(1)]);

    // The existence probe stays attribute-addressed — it is answered off the
    // catalog indexes without hydrating a row, so it never took an id. Its
    // features match the *whole* set, not a subset: owner 42 carries
    // `model_year`, so omitting it is a question about a series that does not
    // exist, and the answer is `false`.
    let mut features: Features = BTreeMap::new();
    features.insert("model_year".into(), FeatureValue::Int(2030));
    let probe = |features: Features| {
        client.has_any_time_series(
            42,
            OwnerCategory::Component,
            "load",
            Some(TimeSeriesType::SingleTimeSeries),
            Some(Period::Fixed(Duration::hours(1))),
            None,
            features,
        )
    };
    assert!(probe(features).await.unwrap());
    assert!(
        !probe(Features::new()).await.unwrap(),
        "a subset of the feature set is a different series",
    );

    let report = client.verify_integrity().await.unwrap();
    assert!(report.errors.is_empty());
}

#[tokio::test]
async fn missing_key_returns_not_found() {
    let addr = spawn_server(fixture_store()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    // Ids are never reissued, so one that names no row stays stale rather than
    // coming to mean a different series -- a read committed to acting on it is
    // a failure, where `association_exists` is the call that asks.
    let stale = infrastore_core::TimeSeriesId(9_999);
    assert!(!client.association_exists(stale).await.unwrap());
    let err = client.read_by_id(stale, None).await.unwrap_err();
    assert!(matches!(err, infrastore_core::TimeSeriesError::NotFound));
    let err = client.get_metadata_by_id(stale).await.unwrap_err();
    assert!(matches!(err, infrastore_core::TimeSeriesError::NotFound));
}

#[tokio::test]
async fn non_sequential_round_trip_over_grpc() {
    let mut store = fixture_store();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let timestamps = vec![
        initial,
        initial + Duration::hours(5),
        initial + Duration::days(2),
    ];
    let series = NonSequentialTimeSeries::new(
        timestamps.clone(),
        TypedArray::from_f64(vec![3], &[4.0, 5.0, 6.0]),
        "events",
    )
    .unwrap();
    store
        .add_time_series(
            44,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::NonSequentialTimeSeries(series),
            Features::new(),
        )
        .unwrap();

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();
    let ids = owner_ids(&client, 44).await;
    assert_eq!(owner_rows(&client, 44).await[0].resolution, None);
    let got = client
        .read_by_id(
            ids[0],
            Some((initial + Duration::hours(1), initial + Duration::days(3)).into()),
        )
        .await
        .unwrap();
    let irregular = got.as_non_sequential().unwrap();
    assert_eq!(irregular.timestamps, timestamps[1..]);
    assert_eq!(irregular.data.to_f64_vec().unwrap(), vec![5.0, 6.0]);
}

#[tokio::test]
async fn persistent_round_trip_over_grpc() {
    let mut store = fixture_store();
    let month = |m: u32| Utc.with_ymd_and_hms(2024, m, 1, 0, 0, 0).unwrap();
    let breakpoints = vec![month(1), month(4), month(7), month(10)];
    let series = PersistentTimeSeries::new(
        breakpoints.clone(),
        TypedArray::from_f64(vec![4], &[10.0, 40.0, 70.0, 100.0]),
        "gas_price",
    )
    .unwrap()
    .with_units("USD/MMBtu")
    .with_component_field("fuel_cost");
    store
        .add_time_series(
            45,
            "ThermalStandard",
            OwnerCategory::Component,
            TimeSeriesData::PersistentTimeSeries(series),
            Features::new(),
        )
        .unwrap();

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();
    let rows = client
        .list_metadata(
            Some(45),
            Some(OwnerCategory::Component),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    // The type survives the wire, and a step function carries no resolution.
    assert_eq!(
        rows[0].time_series_type,
        TimeSeriesType::PersistentTimeSeries
    );
    assert_eq!(rows[0].resolution, None);
    let id = rows[0].id.expect("a served row carries its catalog id");

    let whole = client.read_by_id(id, None).await.unwrap();
    let step = whole.as_persistent().unwrap();
    assert_eq!(step.timestamps, breakpoints);
    assert_eq!(
        step.data.to_f64_vec().unwrap(),
        vec![10.0, 40.0, 70.0, 100.0]
    );
    assert_eq!(step.units.as_deref(), Some("USD/MMBtu"));
    assert_eq!(step.component_field.as_deref(), Some("fuel_cost"));

    // A range whose start is not a breakpoint still yields the value in force
    // there — the semantic that distinguishes this type — over gRPC too.
    let sliced = client
        .read_by_id(id, Some((month(4) + Duration::days(10), month(9)).into()))
        .await
        .unwrap();
    let sliced = sliced.as_persistent().unwrap();
    assert_eq!(sliced.timestamps, vec![month(4), month(7)]);
    assert_eq!(sliced.data.to_f64_vec().unwrap(), vec![40.0, 70.0]);

    // And the projection read: the value in force at each instant the caller
    // names, gathered in that order. Unsorted, with a repeat -- a gather, not a
    // slice -- and one instant past the last breakpoint, held forward.
    let at = vec![month(9), month(2), month(9), month(12)];
    let projected = client.read_projected(id, &at, false).await.unwrap();
    assert_eq!(projected.shape, vec![4]);
    assert_eq!(
        projected.to_f64_vec().unwrap(),
        vec![70.0, 10.0, 70.0, 100.0]
    );

    // No instants is an empty answer rather than an error.
    let empty = client.read_projected(id, &[], false).await.unwrap();
    assert_eq!(empty.shape, vec![0]);
    assert!(empty.bytes.is_empty());

    // Before the first breakpoint a step function is undefined, and one bad
    // instant fails the whole call rather than shortening the answer.
    let err = client
        .read_projected(id, &[month(2), month(1) - Duration::milliseconds(1)], false)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("before the first breakpoint"),
        "the server's reason should reach the client: {err}"
    );
}

#[tokio::test]
async fn dtype_preserved_over_grpc() {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let mut bytes = Vec::new();
    for v in [10i64, 20, 30] {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    let data = TypedArray::new(Dtype::I64, vec![3], bytes).unwrap();
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                initial, resolution, data, "load",
            )),
            Features::new(),
        )
        .unwrap();

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();
    let ids = owner_ids(&client, 1).await;
    let got = client.read_by_id(ids[0], None).await.unwrap();
    let single = got.as_single().unwrap();
    // dtype + raw bytes survive the round trip.
    assert_eq!(single.data.dtype, Dtype::I64);
    let vals: Vec<i64> = single
        .data
        .bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| i64::from_le_bytes(*c))
        .collect();
    assert_eq!(vals, vec![10, 20, 30]);
}

/// The whole point of `element_type` on the wire: a client that never touches
/// the store's files can still tell a piecewise cost curve from a bare matrix of
/// the same shape, and decode it.
#[tokio::test]
async fn element_type_preserved_over_grpc() {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    // Two timesteps; the widest has 2 points, so the row width is 1 + 2*2 = 5.
    let data = TypedArray::from_f64(
        vec![2, 5],
        &[
            2.0, 0.0, 1.0, 1.0, 3.0, //
            1.0, 0.0, 5.0, 0.0, 0.0,
        ],
    );
    store
        .add(infrastore_core::AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                initial,
                resolution_hour(),
                data,
                "cost",
            ))
            .with_element_type(ElementType::PiecewiseLinear),
        ))
        .unwrap();

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();
    let ids = owner_ids(&client, 1).await;

    let meta = client.get_metadata_by_id(ids[0]).await.unwrap();
    assert_eq!(meta.element_type, ElementType::PiecewiseLinear);
    // The catalog id crosses the wire: a client that stores it as a reference
    // (a generator's cost function naming the series that varies it) reads it
    // from a served row, not just a local one.
    assert_eq!(
        meta.id,
        Some(infrastore_core::TimeSeriesId(1)),
        "a served metadata row must carry the id the catalog filed it under",
    );

    // And a value read carries it too, so decoding needs no second call.
    let got = client.read_by_id(ids[0], None).await.unwrap();
    let single = got.as_single().unwrap();
    let decoded = infrastore_core::decode(&single.data, meta.element_type, 1).unwrap();
    assert_eq!(
        decoded,
        infrastore_core::DecodedValues::PiecewiseLinear(vec![
            vec![
                infrastore_core::XyPoint { x: 0.0, y: 1.0 },
                infrastore_core::XyPoint { x: 1.0, y: 3.0 },
            ],
            vec![infrastore_core::XyPoint { x: 0.0, y: 5.0 }],
        ])
    );
}

fn resolution_hour() -> Duration {
    Duration::hours(1)
}

#[tokio::test]
async fn additive_read_rpcs() {
    let addr = spawn_server(fixture_store()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    // One listing carries everything the five key-shaped listings projected:
    // the identity, the descriptive snapshot (Single -> length), the array's
    // content hash, and the id every read takes.
    let owner_42 = owner_rows(&client, 42).await;
    assert_eq!(owner_42.len(), 1);
    assert_eq!(
        owner_42[0].time_series_type,
        TimeSeriesType::SingleTimeSeries
    );
    assert_eq!(owner_42[0].length, Some(24));
    assert_ne!(owner_42[0].data_hash, [0u8; 32]);
    assert_eq!(owner_42[0].units.as_deref(), Some("MW"));

    // The same row fetched by its own id agrees with the listing's.
    let id = owner_42[0].id.expect("a served row carries its id");
    let meta = client.get_metadata_by_id(id).await.unwrap();
    assert_eq!(meta.units.as_deref(), Some("MW"));
    assert_eq!(meta.id, Some(id));

    // ListMetadataByIds is the same listing addressed by id.
    let rows = all_rows(&client).await;
    assert_eq!(rows.len(), 2);
    let ids: Vec<_> = rows.iter().map(|m| m.id.unwrap()).collect();
    let by_ids = client.list_metadata_by_ids(&ids).await.unwrap();
    assert_eq!(
        by_ids.iter().map(|m| m.id).collect::<Vec<_>>(),
        ids.iter().copied().map(Some).collect::<Vec<_>>(),
        "rows come back in the order the ids were given",
    );

    // Read both series in one call.
    let datas = client.read_by_ids(&ids, None).await.unwrap();
    assert_eq!(datas.len(), 2);

    // Detailed counts + counts by type.
    let detailed = client.time_series_counts_detailed().await.unwrap();
    assert_eq!(detailed.static_time_series_count, 2);
    let by_type = client.counts_by_type().await.unwrap();
    assert_eq!(
        by_type
            .iter()
            .find(|(t, _)| *t == TimeSeriesType::SingleTimeSeries)
            .map(|(_, n)| *n),
        Some(2)
    );

    // Owner ids + intervals + summaries + consistency.
    let owner_ids = client
        .list_owner_ids(OwnerCategory::Component, None, None)
        .await
        .unwrap();
    assert_eq!(owner_ids, vec![42, 43]);
    assert!(client.get_intervals(None).await.unwrap().is_empty());
    // Both series share owner_type/name/shape/grid, so they group into one
    // summary row with count 2.
    let static_summary = client.static_summary().await.unwrap();
    assert_eq!(static_summary.len(), 1);
    assert_eq!(static_summary[0].count, 2);
    assert!(client.forecast_summary().await.unwrap().is_empty());
    let cc = client.check_static_consistency(None).await.unwrap();
    assert_eq!(cc.len(), 1);
    assert_eq!(cc[0].length, 24);
    assert_eq!(cc[0].resolution, Period::Fixed(Duration::hours(1)));
}

/// `component_field` reaches the server as a filter and comes back on the
/// metadata row, so a remote reader can both select by it and read it.
#[tokio::test]
async fn component_field_filters_and_round_trips_over_the_wire() {
    let mut store = create_store(None, true).unwrap();
    for (owner, field) in [
        (1i64, Some("max_active_power")),
        (2, Some("rating")),
        (3, None),
    ] {
        let mut data = TimeSeriesData::SingleTimeSeries(series(2024, 24, owner as f64));
        if let Some(field) = field {
            data = data.with_component_field(field);
        }
        store
            .add_time_series(
                owner,
                "Generator",
                OwnerCategory::Component,
                data,
                Features::new(),
            )
            .unwrap();
    }
    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let metas = client
        .list_metadata(
            None,
            None,
            None,
            None,
            None,
            Some("max_active_power".into()),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].owner_id, 1);
    assert_eq!(
        metas[0].component_field.as_deref(),
        Some("max_active_power")
    );

    // The same predicate naming the other field selects the other owner.
    let rating = client
        .list_metadata(
            None,
            None,
            None,
            None,
            None,
            Some("rating".into()),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(rating.len(), 1);
    assert_eq!(rating[0].owner_id, 2);

    // The row that declares none is unreachable through the filter, and
    // reports it as absent rather than as an empty string.
    assert!(
        client
            .list_metadata(
                None,
                None,
                None,
                None,
                None,
                Some("nothing".into()),
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap()
            .is_empty()
    );
    let all = client
        .list_metadata(
            Some(3),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].component_field, None);
}

/// A value read over gRPC describes its values the way a local one does.
///
/// `ReadByIdResp` carried no unit descriptors, so the encoder dropped `units`,
/// `quantity_kind`, `unit_system` and `component_field` and the decoder wrote
/// `None` back in. The identical `Store::get_time_series` against a local store
/// returns them populated — the core attaches them in `materialize_time_series`
/// precisely so a read round-trips what a write declared. The server tests only
/// ever asserted descriptors through `GetMetadata`, so the gap was invisible.
#[tokio::test]
async fn a_value_read_carries_the_unit_descriptors() {
    let mut store = create_store(None, true).unwrap();
    let described = TimeSeriesData::SingleTimeSeries(series(2024, 24, 100.0))
        .with_units("MW")
        .with_quantity_kind("ActivePower")
        .with_unit_system(infrastore_core::UnitSystem::ComponentBase)
        .with_component_field("max_active_power");
    store
        .add_time_series(
            42,
            "Generator",
            OwnerCategory::Component,
            described,
            Features::new(),
        )
        .unwrap();
    // A second series declaring none of them, to pin that absent stays absent.
    store
        .add_time_series(
            43,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(2024, 24, 5.0)),
            Features::new(),
        )
        .unwrap();

    let local_id = store
        .list_metadata(infrastore_core::ListFilter::new().owner_id(42))
        .unwrap()[0]
        .id
        .expect("a catalog row always carries its id");
    let local_described = store
        .read_by_id(local_id, infrastore_core::ReadWindow::full())
        .unwrap();

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let ids = owner_ids(&client, 42).await;
    let remote = client.read_by_id(ids[0], None).await.unwrap();

    assert_eq!(remote.units(), Some("MW"));
    assert_eq!(remote.quantity_kind(), Some("ActivePower"));
    assert_eq!(
        remote.unit_system(),
        Some(infrastore_core::UnitSystem::ComponentBase)
    );
    assert_eq!(remote.component_field(), Some("max_active_power"));

    // Byte-for-byte the same descriptors the local read reports.
    assert_eq!(remote.units(), local_described.units());
    assert_eq!(remote.quantity_kind(), local_described.quantity_kind());
    assert_eq!(remote.unit_system(), local_described.unit_system());
    assert_eq!(remote.component_field(), local_described.component_field());

    // Unset stays unset rather than becoming an empty string.
    let bare_ids = owner_ids(&client, 43).await;
    let bare = client.read_by_id(bare_ids[0], None).await.unwrap();
    assert_eq!(bare.units(), None);
    assert_eq!(bare.quantity_kind(), None);
    assert_eq!(bare.unit_system(), None);
    assert_eq!(bare.component_field(), None);
}
