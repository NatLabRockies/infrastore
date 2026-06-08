//! gRPC integration tests for forecast read path (Deterministic / Probabilistic / Scenarios).
//!
//! Each test builds a store via `add_time_series`, spins up a tonic server, drives
//! it through `RemoteClient::get_time_series`, and asserts the full-fidelity
//! round-trip including a `time_range` window-selection read.

use std::time::Duration as StdDuration;

use chrono::{Duration, TimeZone, Utc};
use time_series_store_core::{
    Deterministic, Features, OwnerCategory, Probabilistic, Scenarios, Store, TimeSeriesData,
    TypedArray, create_store,
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

    tokio::time::sleep(StdDuration::from_millis(50)).await;
    format!("http://{local_addr}")
}

/// Build an f64 TypedArray with sequential values starting from `base`.
fn seq_f64(shape: Vec<usize>, base: f64) -> TypedArray {
    let n: usize = shape.iter().product();
    let vals: Vec<f64> = (0..n).map(|i| base + i as f64).collect();
    TypedArray::from_f64(shape, &vals)
}

fn add_time_series(store: &mut Store, owner: &str, data: TimeSeriesData) {
    store
        .add_time_series(
            owner,
            "Generator",
            OwnerCategory::Component,
            "price",
            data,
            Features::new(),
            None,
            None,
        )
        .unwrap();
}

fn add_det_forecast(store: &mut Store) {
    // shape [H=4, count=6, elem] — scalar, 6 windows, 4-step horizon, 2h interval
    let data = seq_f64(vec![4, 6], 1.0);
    let det = Deterministic::new(
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        Duration::hours(1),
        Duration::hours(4),
        Duration::hours(2),
        6,
        data,
    )
    .unwrap();
    add_time_series(store, "det-owner", TimeSeriesData::Deterministic(det));
}

fn add_prob_forecast(store: &mut Store) {
    // shape [P=3, H=4, count=5] — 3 percentiles, 4-step horizon, 5 windows
    let data = seq_f64(vec![3, 4, 5], 10.0);
    let prob = Probabilistic::new(
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        Duration::hours(1),
        Duration::hours(4),
        Duration::hours(2),
        5,
        vec![10.0, 50.0, 90.0],
        data,
    )
    .unwrap();
    add_time_series(store, "prob-owner", TimeSeriesData::Probabilistic(prob));
}

fn add_scen_forecast(store: &mut Store) {
    // shape [S=4, H=3, count=5] — 4 scenarios, 3-step horizon, 5 windows
    let data = seq_f64(vec![4, 3, 5], 20.0);
    let scen = Scenarios::new(
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        Duration::hours(1),
        Duration::hours(3),
        Duration::hours(2),
        5,
        4,
        data,
    )
    .unwrap();
    add_time_series(store, "scen-owner", TimeSeriesData::Scenarios(scen));
}

// ---- Deterministic ----

#[tokio::test]
async fn deterministic_full_round_trip_over_grpc() {
    let mut store = create_store(None, true).unwrap();
    add_det_forecast(&mut store);

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let keys = client
        .get_time_series_keys("det-owner".to_string())
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);

    let data = client.get_time_series(&keys[0], None).await.unwrap();
    let det = data.as_deterministic().expect("expected Deterministic");
    assert_eq!(det.count, 6);
    assert_eq!(det.data.shape, vec![4, 6]);
    assert_eq!(det.horizon, Duration::hours(4));
    assert_eq!(det.interval, Duration::hours(2));
    assert_eq!(det.resolution, Duration::hours(1));
    assert_eq!(
        det.initial_timestamp,
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
    );
    // First value is base=1.0
    assert_eq!(det.data.to_f64_vec().unwrap()[0], 1.0);
}

#[tokio::test]
async fn deterministic_time_range_over_grpc() {
    let mut store = create_store(None, true).unwrap();
    add_det_forecast(&mut store);

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let keys = client
        .get_time_series_keys("det-owner".to_string())
        .await
        .unwrap();

    // Windows start at t0 + k*2h. Select windows 2,3,4 (start=t0+4h, end=t0+10h).
    let t0 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let start = t0 + Duration::hours(4); // window index 2
    let end = t0 + Duration::hours(10); // window index 5 (exclusive)

    let data = client
        .get_time_series(&keys[0], Some((start, end)))
        .await
        .unwrap();
    let det = data.as_deterministic().expect("expected Deterministic");
    assert_eq!(
        det.count, 3,
        "should have selected 3 windows (indices 2,3,4)"
    );
    assert_eq!(det.data.shape, vec![4, 3]);
    assert_eq!(det.initial_timestamp, start);
    assert_eq!(det.horizon, Duration::hours(4));
}

// ---- Probabilistic ----

#[tokio::test]
async fn probabilistic_full_round_trip_over_grpc() {
    let mut store = create_store(None, true).unwrap();
    add_prob_forecast(&mut store);

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let keys = client
        .get_time_series_keys("prob-owner".to_string())
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);

    let data = client.get_time_series(&keys[0], None).await.unwrap();
    let prob = data.as_probabilistic().expect("expected Probabilistic");
    assert_eq!(prob.count, 5);
    assert_eq!(prob.data.shape, vec![3, 4, 5]);
    assert_eq!(prob.percentiles, vec![10.0, 50.0, 90.0]);
    assert_eq!(prob.horizon, Duration::hours(4));
    assert_eq!(prob.interval, Duration::hours(2));
    assert_eq!(prob.resolution, Duration::hours(1));
    assert_eq!(prob.data.to_f64_vec().unwrap()[0], 10.0);
}

#[tokio::test]
async fn probabilistic_time_range_over_grpc() {
    let mut store = create_store(None, true).unwrap();
    add_prob_forecast(&mut store);

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let keys = client
        .get_time_series_keys("prob-owner".to_string())
        .await
        .unwrap();

    // Windows start at t0 + k*2h. Select windows 1,2 (start=t0+2h, end=t0+6h).
    let t0 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let start = t0 + Duration::hours(2);
    let end = t0 + Duration::hours(6);

    let data = client
        .get_time_series(&keys[0], Some((start, end)))
        .await
        .unwrap();
    let prob = data.as_probabilistic().expect("expected Probabilistic");
    assert_eq!(
        prob.count, 2,
        "should have selected 2 windows (indices 1,2)"
    );
    assert_eq!(prob.data.shape, vec![3, 4, 2]);
    assert_eq!(prob.percentiles, vec![10.0, 50.0, 90.0]);
    assert_eq!(prob.initial_timestamp, start);
}

// ---- Scenarios ----

#[tokio::test]
async fn scenarios_full_round_trip_over_grpc() {
    let mut store = create_store(None, true).unwrap();
    add_scen_forecast(&mut store);

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let keys = client
        .get_time_series_keys("scen-owner".to_string())
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);

    let data = client.get_time_series(&keys[0], None).await.unwrap();
    let scen = data.as_scenarios().expect("expected Scenarios");
    assert_eq!(scen.count, 5);
    assert_eq!(scen.scenario_count, 4);
    assert_eq!(scen.data.shape, vec![4, 3, 5]);
    assert_eq!(scen.horizon, Duration::hours(3));
    assert_eq!(scen.interval, Duration::hours(2));
    assert_eq!(scen.resolution, Duration::hours(1));
    assert_eq!(scen.data.to_f64_vec().unwrap()[0], 20.0);
}

#[tokio::test]
async fn scenarios_time_range_over_grpc() {
    let mut store = create_store(None, true).unwrap();
    add_scen_forecast(&mut store);

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let keys = client
        .get_time_series_keys("scen-owner".to_string())
        .await
        .unwrap();

    // Windows start at t0 + k*2h. Select windows 0,1 (start=t0, end=t0+4h).
    let t0 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let start = t0;
    let end = t0 + Duration::hours(4);

    let data = client
        .get_time_series(&keys[0], Some((start, end)))
        .await
        .unwrap();
    let scen = data.as_scenarios().expect("expected Scenarios");
    assert_eq!(
        scen.count, 2,
        "should have selected 2 windows (indices 0,1)"
    );
    assert_eq!(scen.data.shape, vec![4, 3, 2]);
    assert_eq!(scen.scenario_count, 4);
    assert_eq!(scen.initial_timestamp, t0);
}
