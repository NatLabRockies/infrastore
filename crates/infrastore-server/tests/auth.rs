//! Integration tests for the api_key auth interceptor.

use std::time::Duration as StdDuration;

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    Features, OwnerCategory, SingleTimeSeries, Store, TimeSeriesData, TypedArray, create_store,
};
use infrastore_proto::pb::{
    self, CountsReq, GetReq, KeysReq, ListReq, ResolutionsReq, VerifyReq,
    catalog_store_client::CatalogStoreClient,
};
use infrastore_server::auth::ApiKeyInterceptor;
use infrastore_server::service::CatalogStoreService;
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

// ---- Breadth: the interceptor guards every RPC, not just get_counts --------

#[tokio::test]
async fn a_valid_key_works_across_several_rpcs() {
    // `correct_key_succeeds` only exercises `get_counts`. The interceptor is a
    // transport layer, so it applies to every method — but nothing asserted
    // that, and a future per-method wiring change would not have failed a test.
    let addr = spawn_authed(vec!["secret-1".into()]).await;
    let key: MetadataValue<_> = "secret-1".parse().unwrap();
    let mut client = CatalogStoreClient::with_interceptor(
        channel(&addr).await,
        move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("x-api-key", key.clone());
            Ok(req)
        },
    );

    // counts
    assert_eq!(
        client
            .get_counts(CountsReq {})
            .await
            .unwrap()
            .into_inner()
            .static_time_series,
        1
    );
    // list
    assert_eq!(
        client
            .list_time_series(ListReq::default())
            .await
            .unwrap()
            .into_inner()
            .metadata
            .len(),
        1
    );
    // keys, then a get through the key that comes back
    let keys = client
        .get_time_series_keys(KeysReq {
            owner_id: 1,
            owner_category: pb::OwnerCategory::Component as i32,
        })
        .await
        .unwrap()
        .into_inner()
        .keys;
    assert_eq!(keys.len(), 1);
    let resp = client
        .get_time_series(GetReq {
            key: Some(keys[0].clone()),
            start_rfc3339: None,
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.length, 3);
    // resolutions
    assert_eq!(
        client
            .get_resolutions(ResolutionsReq {
                time_series_type: None
            })
            .await
            .unwrap()
            .into_inner()
            .resolution,
        vec!["PT1H".to_string()]
    );
    // verify
    assert!(
        client
            .verify_integrity(VerifyReq {})
            .await
            .unwrap()
            .into_inner()
            .errors
            .is_empty()
    );
}

#[tokio::test]
async fn every_rpc_is_rejected_without_a_key() {
    let addr = spawn_authed(vec!["secret-1".into()]).await;
    let mut client = CatalogStoreClient::new(channel(&addr).await);

    let codes = [
        client
            .get_counts(CountsReq {})
            .await
            .map(|_| ())
            .unwrap_err()
            .code(),
        client
            .list_time_series(ListReq::default())
            .await
            .map(|_| ())
            .unwrap_err()
            .code(),
        client
            .get_time_series_keys(KeysReq {
                owner_id: 1,
                owner_category: pb::OwnerCategory::Component as i32,
            })
            .await
            .map(|_| ())
            .unwrap_err()
            .code(),
        client
            .get_resolutions(ResolutionsReq {
                time_series_type: None,
            })
            .await
            .map(|_| ())
            .unwrap_err()
            .code(),
        client
            .verify_integrity(VerifyReq {})
            .await
            .map(|_| ())
            .unwrap_err()
            .code(),
    ];
    for code in codes {
        assert_eq!(code, tonic::Code::Unauthenticated);
    }
}

#[tokio::test]
async fn a_non_ascii_header_value_is_rejected_as_unauthenticated() {
    // `auth.rs`'s `to_str()` failure arm. `MetadataValue` accepts a non-ASCII
    // string (the bytes go on the wire verbatim), so the server is the layer
    // that has to cope: it cannot compare such a value against the configured
    // keys and must reject it rather than panicking or coercing it.
    let addr = spawn_authed(vec!["secret-1".into()]).await;

    for text in ["sécret", "负荷", "secret-1\u{00ff}"] {
        let value: MetadataValue<tonic::metadata::Ascii> = text
            .parse()
            .unwrap_or_else(|e| panic!("tonic rejected {text:?} client-side: {e}"));
        let mut client = CatalogStoreClient::with_interceptor(
            channel(&addr).await,
            move |mut req: tonic::Request<()>| {
                req.metadata_mut().insert("x-api-key", value.clone());
                Ok(req)
            },
        );
        let err = client.get_counts(CountsReq {}).await.unwrap_err();
        assert_eq!(
            err.code(),
            tonic::Code::Unauthenticated,
            "{text:?} should be rejected, got {err:?}"
        );
    }

    // A `-bin` header is not the one the interceptor reads, so supplying the key
    // that way is a missing-header rejection, not an accepted request.
    let bin = MetadataValue::from_bytes(b"secret-1");
    let mut client = CatalogStoreClient::with_interceptor(
        channel(&addr).await,
        move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert_bin("x-api-key-bin", bin.clone());
            Ok(req)
        },
    );
    let err = client.get_counts(CountsReq {}).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err:?}");
}
