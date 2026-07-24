//! Integration tests for the api_key auth interceptor.

use std::time::Duration as StdDuration;

use castore_core::{
    Features, OwnerCategory, SingleTimeSeries, Store, TimeSeriesData, TypedArray, create_store,
};
use castore_proto::pb::{CountsReq, catalog_store_client::CatalogStoreClient};
use castore_server::auth::ApiKeyInterceptor;
use castore_server::service::CatalogStoreService;
use chrono::{Duration, TimeZone, Utc};
use tokio::net::TcpListener;
use tonic::metadata::MetadataValue;
use tonic::service::InterceptorLayer;
use tonic::transport::Channel;

fn make_store() -> Store {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let data = TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]);
    let s = SingleTimeSeries::new(initial, resolution, data, "load");
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s),
            Features::new(),
            None,
        )
        .unwrap();
    store
}

async fn spawn_authed(keys: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let interceptor = ApiKeyInterceptor::new(keys);
    let service = CatalogStoreService::new(make_store());
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .layer(InterceptorLayer::new(interceptor))
            .add_service(service.into_server())
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(StdDuration::from_millis(50)).await;
    format!("http://{local_addr}")
}

async fn channel(addr: &str) -> Channel {
    Channel::from_shared(addr.to_string())
        .unwrap()
        .connect()
        .await
        .unwrap()
}

#[tokio::test]
async fn missing_header_is_rejected() {
    let addr = spawn_authed(vec!["secret-1".into()]).await;
    let mut client = CatalogStoreClient::new(channel(&addr).await);
    let err = client.get_counts(CountsReq {}).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn wrong_key_is_rejected() {
    let addr = spawn_authed(vec!["secret-1".into()]).await;
    let key: MetadataValue<_> = "nope".parse().unwrap();
    let mut client = CatalogStoreClient::with_interceptor(
        channel(&addr).await,
        move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("x-api-key", key.clone());
            Ok(req)
        },
    );
    let err = client.get_counts(CountsReq {}).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn correct_key_succeeds() {
    let addr = spawn_authed(vec!["secret-1".into(), "secret-2".into()]).await;
    let key: MetadataValue<_> = "secret-2".parse().unwrap();
    let mut client = CatalogStoreClient::with_interceptor(
        channel(&addr).await,
        move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("x-api-key", key.clone());
            Ok(req)
        },
    );
    let resp = client.get_counts(CountsReq {}).await.unwrap().into_inner();
    assert_eq!(resp.static_time_series, 1);
}
