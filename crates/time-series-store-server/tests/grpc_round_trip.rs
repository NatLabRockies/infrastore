//! End-to-end gRPC round-trip: spin up a tonic server backed by a real on-disk
//! Store, then drive it through the typed `RemoteClient`. Covers every RPC.

use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

use chrono::{Duration, TimeZone, Utc};
use time_series_store_core::{
    Dtype, FeatureValue, Features, NonSequentialTimeSeries, OwnerCategory, SingleTimeSeries, Store,
    TimeSeriesData, TimeSeriesType, TypedArray, create_store,
};
use time_series_store_server::client::RemoteClient;
use time_series_store_server::service::TimeSeriesStoreService;
use tokio::net::TcpListener;

async fn spawn_server(store: Store) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let service = TimeSeriesStoreService::new(store);
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
            TimeSeriesData::SingleTimeSeries(s),
            features,
            Some("MW".into()),
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
            None,
        )
        .unwrap();
    store
}

#[tokio::test]
async fn list_and_get_round_trip() {
    let addr = spawn_server(fixture_store()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let metas = client
        .list_time_series(None, None, None, None, None, None, None)
        .await
        .unwrap();
    assert_eq!(metas.len(), 2);

    // Fetch by key.
    let keys = client
        .get_time_series_keys(42, OwnerCategory::Component)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    let data = client.get_time_series(&keys[0], None).await.unwrap();
    let single = data.as_single().unwrap();
    assert_eq!(single.length, 24);
    assert_eq!(single.data.to_f64_vec().unwrap()[0], 100.0);
    assert_eq!(single.data.to_f64_vec().unwrap()[23], 123.0);
}

#[tokio::test]
async fn time_range_slicing_over_grpc() {
    let addr = spawn_server(fixture_store()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let keys = client
        .get_time_series_keys(42, OwnerCategory::Component)
        .await
        .unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let start = initial + Duration::hours(2);
    let end = initial + Duration::hours(5);
    let data = client
        .get_time_series(&keys[0], Some((start, end)))
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
        .list_time_series(None, None, None, None, None, None, Some(&filter))
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

    let keys = client
        .get_time_series_keys(42, OwnerCategory::Component)
        .await
        .unwrap();
    let present = client.has_time_series(&keys[0]).await.unwrap();
    assert!(present);

    let report = client.verify_integrity().await.unwrap();
    assert!(report.errors.is_empty());
}

#[tokio::test]
async fn missing_key_returns_not_found() {
    let addr = spawn_server(fixture_store()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let bogus_key = time_series_store_core::TimeSeriesKey {
        owner_id: 999,
        owner_category: OwnerCategory::Component,
        time_series_type: TimeSeriesType::SingleTimeSeries,
        name: "load".into(),
        resolution: Some(Duration::hours(1)),
        features: Features::new(),
    };
    let err = client.get_time_series(&bogus_key, None).await.unwrap_err();
    assert!(matches!(
        err,
        time_series_store_core::TimeSeriesError::NotFound
    ));
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
            None,
        )
        .unwrap();

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();
    let keys = client
        .get_time_series_keys(44, OwnerCategory::Component)
        .await
        .unwrap();
    assert_eq!(keys[0].resolution, None);
    let got = client
        .get_time_series(
            &keys[0],
            Some((initial + Duration::hours(1), initial + Duration::days(3))),
        )
        .await
        .unwrap();
    let irregular = got.as_non_sequential().unwrap();
    assert_eq!(irregular.timestamps, timestamps[1..]);
    assert_eq!(irregular.data.to_f64_vec().unwrap(), vec![5.0, 6.0]);
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
            None,
        )
        .unwrap();

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();
    let keys = client
        .get_time_series_keys(1, OwnerCategory::Component)
        .await
        .unwrap();
    let got = client.get_time_series(&keys[0], None).await.unwrap();
    let single = got.as_single().unwrap();
    // dtype + raw bytes survive the round trip.
    assert_eq!(single.data.dtype, Dtype::I64);
    let vals: Vec<i64> = single
        .data
        .bytes
        .chunks_exact(8)
        .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(vals, vec![10, 20, 30]);
}
