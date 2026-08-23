//! Request-validation, empty-result, and error-mapping coverage for the gRPC
//! server.
//!
//! `grpc_round_trip.rs` drives the happy path of every RPC through the typed
//! `RemoteClient`. This file drives the *raw* generated client instead, because
//! the malformed requests below cannot be built through the typed wrapper — it
//! constructs well-formed messages by construction. Each case asserts the
//! `tonic::Code` the server returns, which is the contract every non-Rust client
//! sees.
//!
//! Two additional groups: empty results (an RPC that legitimately matches
//! nothing must return an empty message, not `NotFound`), and the client's
//! `map_status` table, driven end-to-end since the function is private.

use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    Deterministic, Features, OwnerCategory, Period, SingleTimeSeries, Store, TimeSeriesData,
    TimeSeriesError, TimeSeriesType, TypedArray, create_store,
};
use infrastore_proto::pb::{
    self, BulkReadReq, GetReq, HasReq, IntervalsReq, KeyReq, KeysReq, ListOwnerIdsReq, ListReq,
    ResolutionsReq, catalog_store_client::CatalogStoreClient,
};
use infrastore_server::client::RemoteClient;
use infrastore_server::service::CatalogStoreService;
use tokio::net::TcpListener;
use tonic::transport::Channel;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn spawn(store: Store) -> String {
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
    tokio::time::sleep(StdDuration::from_millis(50)).await;
    format!("http://{local_addr}")
}

/// Like [`spawn`], but with an explicit `BulkRead` key ceiling.
async fn spawn_with_bulk_limit(store: Store, max_keys: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let service = CatalogStoreService::new(store).with_max_bulk_read_keys(max_keys);
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

async fn raw_client(addr: &str) -> CatalogStoreClient<Channel> {
    let channel = Channel::from_shared(addr.to_string())
        .unwrap()
        .connect()
        .await
        .unwrap();
    CatalogStoreClient::new(channel)
}

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

fn sts(name: &str, base: f64, length: usize) -> SingleTimeSeries {
    let values: Vec<f64> = (0..length).map(|i| base + i as f64).collect();
    SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![length], &values),
        name,
    )
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
        .unwrap();
}

/// One SingleTimeSeries owned by 42.
fn populated_store() -> Store {
    let mut store = create_store(None, true).unwrap();
    add(
        &mut store,
        42,
        TimeSeriesData::SingleTimeSeries(sts("load", 100.0, 24)),
    );
    store
}

fn empty_store() -> Store {
    create_store(None, true).unwrap()
}

/// A well-formed key message for the series in `populated_store`.
fn good_key() -> pb::TimeSeriesKey {
    pb::TimeSeriesKey {
        owner_id: 42,
        owner_category: pb::OwnerCategory::Component as i32,
        time_series_type: pb::TimeSeriesType::SingleTimeSeries as i32,
        name: "load".into(),
        resolution: "PT1H".into(),
        interval: String::new(),
        features: Some(pb::Features::default()),
        initial_timestamp_rfc3339: None,
        length: None,
        horizon: None,
        count: None,
        time_reference: None,
    }
}

// ---------------------------------------------------------------------------
// 3.3a Request validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_key_is_invalid_argument_on_every_key_taking_rpc() {
    let addr = spawn(populated_store()).await;
    let mut client = raw_client(&addr).await;

    let err = client
        .get_time_series(GetReq {
            key: None,
            start_rfc3339: None,
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    assert!(!err.message().is_empty());

    let err = client
        .has_time_series(HasReq { key: None })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    let err = client.get_metadata(KeyReq { key: None }).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
}

#[tokio::test]
async fn a_one_sided_time_range_is_invalid_argument() {
    let addr = spawn(populated_store()).await;
    let mut client = raw_client(&addr).await;

    // start without end.
    let err = client
        .get_time_series(GetReq {
            key: Some(good_key()),
            start_rfc3339: Some(t0().to_rfc3339()),
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    // end without start.
    let err = client
        .get_time_series(GetReq {
            key: Some(good_key()),
            start_rfc3339: None,
            end_rfc3339: Some(t0().to_rfc3339()),
            bounds_zoneless: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    // Both absent is the full read, and both present is a slice.
    assert!(
        client
            .get_time_series(GetReq {
                key: Some(good_key()),
                start_rfc3339: None,
                end_rfc3339: None,
                bounds_zoneless: None,
            })
            .await
            .is_ok()
    );
    assert!(
        client
            .get_time_series(GetReq {
                key: Some(good_key()),
                start_rfc3339: Some(t0().to_rfc3339()),
                end_rfc3339: Some((t0() + Duration::hours(2)).to_rfc3339()),
                bounds_zoneless: None,
            })
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_malformed_rfc3339_timestamp_is_invalid_argument() {
    let addr = spawn(populated_store()).await;
    let mut client = raw_client(&addr).await;

    for (start, end) in [
        (Some("not a timestamp".to_string()), Some(t0().to_rfc3339())),
        (Some(t0().to_rfc3339()), Some("also not".to_string())),
        // A date with no time zone is not RFC3339.
        (Some("2024-01-01".to_string()), Some(t0().to_rfc3339())),
        // A plausible-looking but invalid month.
        (
            Some("2024-13-01T00:00:00Z".to_string()),
            Some(t0().to_rfc3339()),
        ),
    ] {
        let err = client
            .get_time_series(GetReq {
                key: Some(good_key()),
                start_rfc3339: start.clone(),
                end_rfc3339: end.clone(),
                bounds_zoneless: None,
            })
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            tonic::Code::InvalidArgument,
            "start={start:?} end={end:?}: {err:?}"
        );
    }
}

#[tokio::test]
async fn an_unparseable_iso_period_is_invalid_argument() {
    let addr = spawn(populated_store()).await;
    let mut client = raw_client(&addr).await;

    // In a key.
    let mut key = good_key();
    key.resolution = "not-a-period".into();
    let err = client
        .get_time_series(GetReq {
            key: Some(key),
            start_rfc3339: None,
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    // In a filter.
    let err = client
        .list_time_series(ListReq {
            resolution: Some("PT?H".into()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    // In list_owner_ids.
    let err = client
        .list_owner_ids(ListOwnerIdsReq {
            owner_category: pb::OwnerCategory::Component as i32,
            time_series_type: None,
            resolution: Some("garbage".into()),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
}

#[tokio::test]
async fn an_unknown_owner_category_enum_int_is_invalid_argument() {
    let addr = spawn(populated_store()).await;
    let mut client = raw_client(&addr).await;

    let mut key = good_key();
    key.owner_category = 999;
    let err = client
        .get_time_series(GetReq {
            key: Some(key),
            start_rfc3339: None,
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    let err = client
        .get_time_series_keys(KeysReq {
            owner_id: 42,
            owner_category: 999,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    let err = client
        .list_owner_ids(ListOwnerIdsReq {
            owner_category: 999,
            time_series_type: None,
            resolution: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    let err = client
        .list_time_series(ListReq {
            owner_category: Some(999),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
}

#[tokio::test]
async fn an_unknown_time_series_type_enum_int_is_invalid_argument() {
    let addr = spawn(populated_store()).await;
    let mut client = raw_client(&addr).await;

    let mut key = good_key();
    key.time_series_type = 999;
    let err = client
        .get_time_series(GetReq {
            key: Some(key),
            start_rfc3339: None,
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    let err = client
        .list_time_series(ListReq {
            time_series_type: Some(999),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    let err = client
        .get_resolutions(ResolutionsReq {
            time_series_type: Some(999),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    let err = client
        .get_intervals(IntervalsReq {
            time_series_type: Some(999),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
}

#[tokio::test]
async fn a_resolve_forecast_key_request_with_no_requested_type_is_invalid_argument() {
    let addr = spawn(populated_store()).await;
    let mut client = raw_client(&addr).await;

    let err = client
        .resolve_forecast_key(pb::ResolveForecastKeyReq {
            owner_id: 42,
            owner_category: pb::OwnerCategory::Component as i32,
            name: "load".into(),
            requested: None,
            resolution: None,
            interval: None,
            features: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
}

// ---------------------------------------------------------------------------
// 3.3b Empty results
// ---------------------------------------------------------------------------

#[tokio::test]
async fn listing_an_empty_store_returns_empty_messages_not_errors() {
    let addr = spawn(empty_store()).await;
    let client = RemoteClient::connect(addr.clone()).await.unwrap();

    assert!(
        client
            .list_time_series(None, None, None, None, None, None, None, None, None, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        client
            .list_keys(
                None, None, None, None, None, None, None, None, None, None, false
            )
            .await
            .unwrap()
            .is_empty()
    );
    assert!(client.get_resolutions(None).await.unwrap().is_empty());
    assert!(client.get_intervals(None).await.unwrap().is_empty());
    assert!(client.static_summary().await.unwrap().is_empty());
    assert!(client.forecast_summary().await.unwrap().is_empty());
    assert!(client.counts_by_type().await.unwrap().is_empty());
    assert!(
        client
            .list_owner_ids(OwnerCategory::Component, None, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        client
            .check_static_consistency(None)
            .await
            .unwrap()
            .is_empty()
    );

    // Counts on an empty store are all zero, not an error.
    let counts = client.get_counts().await.unwrap();
    assert_eq!(counts.static_time_series, 0);
    assert_eq!(counts.forecasts, 0);
    assert_eq!(counts.components_with_time_series, 0);

    let detailed = client.time_series_counts_detailed().await.unwrap();
    assert_eq!(detailed.static_time_series_count, 0);
    assert_eq!(detailed.forecast_count, 0);
    assert_eq!(detailed.components_with_time_series, 0);

    // And integrity holds.
    assert!(client.verify_integrity().await.unwrap().errors.is_empty());
}

#[tokio::test]
async fn a_filter_matching_nothing_returns_an_empty_list() {
    let addr = spawn(populated_store()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    assert!(
        client
            .list_time_series(
                Some(999),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None
            )
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        client
            .list_time_series(
                None,
                None,
                None,
                None,
                Some("no_such_name".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap()
            .is_empty()
    );
    // Keys for an owner that has none.
    assert!(
        client
            .get_time_series_keys(999, OwnerCategory::Component)
            .await
            .unwrap()
            .is_empty()
    );
    // The right owner id under the wrong category.
    assert!(
        client
            .get_time_series_keys(42, OwnerCategory::SupplementalAttribute)
            .await
            .unwrap()
            .is_empty()
    );
    // An empty owner-id list for a category with nothing in it.
    assert!(
        client
            .list_owner_ids(OwnerCategory::SupplementalAttribute, None, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn has_time_series_returns_false_rather_than_not_found() {
    let addr = spawn(populated_store()).await;
    let mut client = raw_client(&addr).await;

    // Present.
    let resp = client
        .has_time_series(HasReq {
            key: Some(good_key()),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(resp.present);

    // Absent: `false`, not a NotFound status.
    let mut absent = good_key();
    absent.name = "no_such_name".into();
    let resp = client
        .has_time_series(HasReq { key: Some(absent) })
        .await
        .unwrap()
        .into_inner();
    assert!(!resp.present);

    // The same through the typed client.
    let client = RemoteClient::connect(addr).await.unwrap();
    let mut absent = infrastore_proto::convert::key_from_pb(good_key()).unwrap();
    absent.name = "no_such_name".into();
    assert!(!client.has_time_series(&absent).await.unwrap());
}

#[tokio::test]
async fn getting_a_missing_key_is_not_found() {
    let addr = spawn(populated_store()).await;
    let mut raw = raw_client(&addr).await;

    let mut absent = good_key();
    absent.name = "no_such_name".into();
    let err = raw
        .get_time_series(GetReq {
            key: Some(absent.clone()),
            start_rfc3339: None,
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound, "{err:?}");

    let err = raw
        .get_metadata(KeyReq { key: Some(absent) })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound, "{err:?}");
}

// ---------------------------------------------------------------------------
// 3.3c BulkRead edges
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bulk_read_with_an_empty_key_list_returns_no_items() {
    let addr = spawn(populated_store()).await;
    let mut raw = raw_client(&addr).await;
    let resp = raw
        .bulk_read(BulkReadReq {
            keys: Vec::new(),
            start_rfc3339: None,
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(resp.items.is_empty());

    // And through the typed client.
    let client = RemoteClient::connect(addr).await.unwrap();
    assert!(client.bulk_read(&[], None).await.unwrap().is_empty());
}

#[tokio::test]
async fn bulk_read_fails_the_whole_batch_on_one_missing_key() {
    let mut store = create_store(None, true).unwrap();
    add(
        &mut store,
        1,
        TimeSeriesData::SingleTimeSeries(sts("load", 1.0, 4)),
    );
    add(
        &mut store,
        2,
        TimeSeriesData::SingleTimeSeries(sts("load", 2.0, 4)),
    );
    let addr = spawn(store).await;
    let mut raw = raw_client(&addr).await;

    let present = |owner: i64| pb::TimeSeriesKey {
        owner_id: owner,
        ..good_key()
    };
    let mut absent = present(1);
    absent.name = "no_such_name".into();

    // All present: two items back, in request order.
    let resp = raw
        .bulk_read(BulkReadReq {
            keys: vec![present(1), present(2)],
            start_rfc3339: None,
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.items.len(), 2);

    // One missing among N fails the whole call rather than returning a short
    // list the caller would silently mis-index.
    for keys in [
        vec![absent.clone(), present(1)],
        vec![present(1), absent.clone()],
        vec![present(1), absent.clone(), present(2)],
    ] {
        let err = raw
            .bulk_read(BulkReadReq {
                keys,
                start_rfc3339: None,
                end_rfc3339: None,
                bounds_zoneless: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound, "{err:?}");
    }
}

#[tokio::test]
async fn bulk_read_returns_duplicate_keys_once_each() {
    let addr = spawn(populated_store()).await;
    let mut raw = raw_client(&addr).await;

    let resp = raw
        .bulk_read(BulkReadReq {
            keys: vec![good_key(), good_key(), good_key()],
            start_rfc3339: None,
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap()
        .into_inner();
    // One item per requested key, positionally: duplicates are not collapsed.
    assert_eq!(resp.items.len(), 3);
    assert_eq!(resp.items[0].value_bytes, resp.items[1].value_bytes);
    assert_eq!(resp.items[1].value_bytes, resp.items[2].value_bytes);
}

#[tokio::test]
async fn a_time_range_applies_to_every_key_in_a_bulk_read() {
    let mut store = create_store(None, true).unwrap();
    add(
        &mut store,
        1,
        TimeSeriesData::SingleTimeSeries(sts("load", 100.0, 8)),
    );
    add(
        &mut store,
        2,
        TimeSeriesData::SingleTimeSeries(sts("load", 200.0, 8)),
    );
    let addr = spawn(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let key = |owner: i64| infrastore_core::KeyIdentity {
        owner_id: owner,
        owner_category: OwnerCategory::Component,
        time_series_type: TimeSeriesType::SingleTimeSeries,
        name: "load".into(),
        resolution: Some(Period::fixed(Duration::hours(1))),
        interval: None,
        features: Features::new(),
    };
    let k1 = key(1);
    let k2 = key(2);
    let range = (t0() + Duration::hours(2), t0() + Duration::hours(5));

    let items = client
        .bulk_read(&[&k1, &k2], Some(range.into()))
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    for (i, base) in [(0usize, 100.0f64), (1, 200.0)] {
        let single = items[i].as_single().unwrap();
        assert_eq!(single.length, 3, "item {i}");
        assert_eq!(single.initial_timestamp, range.0, "item {i}");
        assert_eq!(
            single.data.to_f64_vec().unwrap(),
            vec![base + 2.0, base + 3.0, base + 4.0],
            "item {i}"
        );
    }

    // FINDING F9: `BulkReadResp` items carry no name, so
    // `RemoteClient::bulk_read` fills in the empty string — unlike
    // `get_time_series`, which knows the name from the key it was given. Pinned,
    // not fixed: the caller already holds the keys positionally, but the
    // asymmetry with `get_time_series` is a trap.
    for (i, item) in items.iter().enumerate() {
        assert_eq!(item.as_single().unwrap().name, "", "item {i}");
    }

    // Apart from the name, the slice matches the per-key read.
    for (i, k) in [&k1, &k2].iter().enumerate() {
        let per_key = client.get_time_series(k, Some(range.into())).await.unwrap();
        let (bulk, single) = (items[i].as_single().unwrap(), per_key.as_single().unwrap());
        assert_eq!(bulk.data, single.data, "item {i}");
        assert_eq!(bulk.initial_timestamp, single.initial_timestamp, "item {i}");
        assert_eq!(bulk.resolution, single.resolution, "item {i}");
        assert_eq!(bulk.length, single.length, "item {i}");
        assert_eq!(
            single.name, "load",
            "item {i}: get_time_series keeps the name"
        );
    }
}

// ---------------------------------------------------------------------------
// 3.3d A calendar (Months) series end-to-end over the wire
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_monthly_series_survives_the_wire_as_a_calendar_period() {
    // `Period::Months` is not equal to any `Fixed` span, so a wire encoding that
    // lost the distinction would silently turn a monthly series into a
    // fixed-span one. Nothing else exercises a calendar period over gRPC.
    let initial = Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap();
    let mut store = create_store(None, true).unwrap();
    let values: Vec<f64> = (0..12).map(|i| 100.0 + i as f64).collect();
    add(
        &mut store,
        7,
        TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
            initial,
            Period::Months(1),
            TypedArray::from_f64(vec![12], &values),
            "monthly_load",
        )),
    );
    // Plus a monthly Deterministic so horizon/interval travel too.
    let det_values: Vec<f64> = (0..3 * 4).map(|i| i as f64).collect();
    add(
        &mut store,
        8,
        TimeSeriesData::Deterministic(
            Deterministic::new(
                initial,
                Period::Months(1),
                Period::Months(3),
                Period::Months(1),
                4,
                TypedArray::from_f64(vec![3, 4], &det_values),
                "monthly_fc",
            )
            .unwrap(),
        ),
    );

    let addr = spawn(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    // The ISO string survives, and decodes as a calendar period.
    let resolutions = client.get_resolutions(None).await.unwrap();
    assert_eq!(resolutions, vec![Period::Months(1)]);
    assert!(resolutions[0].is_irregular());
    assert_eq!(
        client.get_intervals(None).await.unwrap(),
        vec![Period::Months(1)]
    );

    // Metadata rows carry it.
    let metas = client
        .list_time_series(
            Some(7),
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
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].resolution, Some(Period::Months(1)));

    // Keys carry it, and the full key decodes.
    let keys = client
        .get_time_series_keys(8, OwnerCategory::Component)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].resolution(), Some(Period::Months(1)));
    assert_eq!(keys[0].interval(), Some(Period::Months(1)));

    // A get returns the calendar periods and the right values.
    let data = client
        .get_time_series(keys[0].identity(), None)
        .await
        .unwrap();
    let det = data.as_deterministic().unwrap();
    assert_eq!(det.resolution, Period::Months(1));
    assert_eq!(det.horizon, Period::Months(3));
    assert_eq!(det.interval, Period::Months(1));
    assert_eq!(det.count, 4);
    assert_eq!(det.data.to_f64_vec().unwrap(), det_values);

    // A calendar-boundary window selection over the wire: February 15 is
    // window 1, and the span to March 15 is 29 days in this leap year.
    let feb = Utc.with_ymd_and_hms(2024, 2, 15, 0, 0, 0).unwrap();
    let mar = Utc.with_ymd_and_hms(2024, 3, 15, 0, 0, 0).unwrap();
    let sliced = client
        .get_time_series(keys[0].identity(), Some((feb, mar).into()))
        .await
        .unwrap();
    let det = sliced.as_deterministic().unwrap();
    assert_eq!(det.count, 1);
    assert_eq!(det.initial_timestamp, feb);

    // Filtering by the calendar resolution matches; a fixed span does not.
    assert_eq!(
        client
            .list_time_series(
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(Period::Months(1)),
                None,
                None,
            )
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(
        client
            .list_time_series(
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(Period::fixed(Duration::days(30))),
                None,
                None,
            )
            .await
            .unwrap()
            .is_empty(),
        "a fixed 30-day resolution must not match a monthly series"
    );

    // And the summary rows.
    let rows = client.forecast_summary().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].resolution, Some(Period::Months(1)));
    assert_eq!(rows[0].horizon, Some(Period::Months(3)));
    assert_eq!(rows[0].interval, Some(Period::Months(1)));
}

// ---------------------------------------------------------------------------
// 3.5 The client's status -> error table
// ---------------------------------------------------------------------------

#[tokio::test]
async fn map_status_translates_not_found_and_invalid_argument() {
    let addr = spawn(populated_store()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    // NotFound -> TimeSeriesError::NotFound (payload-free).
    let mut absent = infrastore_proto::convert::key_from_pb(good_key()).unwrap();
    absent.name = "no_such_name".into();
    let err = client.get_time_series(&absent, None).await.unwrap_err();
    assert!(matches!(err, TimeSeriesError::NotFound), "{err:?}");

    // InvalidArgument -> InvalidParameter, carrying the server's message.
    let key = infrastore_proto::convert::key_from_pb(good_key()).unwrap();
    let backwards = (t0() + Duration::hours(5), t0());
    let err = client
        .get_time_series(&key, Some(backwards.into()))
        .await
        .unwrap_err();
    match err {
        TimeSeriesError::InvalidParameter(msg) => {
            assert!(!msg.is_empty(), "the server's message must be carried over")
        }
        other => panic!("expected InvalidParameter, got {other:?}"),
    }
}

/// Reject every request with a fixed status, so the client's mapping of codes
/// the real service does not produce can still be driven end-to-end.
#[derive(Clone)]
struct RejectWith(tonic::Code);

impl tonic::service::Interceptor for RejectWith {
    fn call(&mut self, _req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        Err(tonic::Status::new(self.0, "synthetic failure"))
    }
}

async fn spawn_rejecting(code: tonic::Code) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let service = CatalogStoreService::new(populated_store());
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .layer(tonic::service::InterceptorLayer::new(RejectWith(code)))
            .add_service(service.into_server())
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(StdDuration::from_millis(50)).await;
    format!("http://{local_addr}")
}

#[tokio::test]
async fn map_status_translates_the_remaining_codes() {
    // The full table. `AlreadyExists` and `ReadOnly`/`FailedPrecondition` cannot
    // arise from a read-only service, and `Internal` needs a Sqlite/Io failure,
    // so they are driven through an interceptor that rejects with a fixed code.
    for (code, expect) in [
        (tonic::Code::AlreadyExists, "duplicate"),
        (tonic::Code::DataLoss, "integrity"),
        (tonic::Code::FailedPrecondition, "invalid_parameter"),
        (tonic::Code::Unauthenticated, "connection"),
    ] {
        let addr = spawn_rejecting(code).await;
        let client = RemoteClient::connect(addr).await.unwrap();
        let err = client.get_counts().await.unwrap_err();
        let got = match &err {
            TimeSeriesError::DuplicateTimeSeries => "duplicate",
            TimeSeriesError::IntegrityError(_) => "integrity",
            TimeSeriesError::InvalidParameter(_) => "invalid_parameter",
            TimeSeriesError::ConnectionError(_) => "connection",
            other => panic!("unexpected mapping for {code:?}: {other:?}"),
        };
        assert_eq!(got, expect, "{code:?} mapped to {err:?}");
    }
}

#[tokio::test]
async fn internal_and_unavailable_collapse_into_connection_error() {
    // PIN the documented lossy collapse: every code outside the explicit table
    // becomes `ConnectionError`, so a server-side `Internal` (a Sqlite or IO
    // failure) is indistinguishable from a transport `Unavailable`. This is
    // intentional — the client cannot act differently on either — and the code
    // name is preserved in the message so a human reading a log still can.
    for code in [tonic::Code::Internal, tonic::Code::Unavailable] {
        let addr = spawn_rejecting(code).await;
        let client = RemoteClient::connect(addr).await.unwrap();
        let err = client.get_counts().await.unwrap_err();
        match err {
            TimeSeriesError::ConnectionError(msg) => {
                assert!(
                    msg.contains("synthetic failure"),
                    "{code:?}: the message must be preserved, got {msg:?}"
                );
                assert!(
                    !msg.is_empty(),
                    "{code:?}: the code name must be recoverable from the message"
                );
            }
            other => panic!("expected ConnectionError for {code:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn connecting_to_a_dead_address_is_a_connection_error() {
    // Bind then immediately drop the listener so the port is (almost certainly)
    // unused. Whether the failure surfaces at connect or on the first RPC, it
    // must be a ConnectionError either way.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    match RemoteClient::connect(format!("http://{addr}")).await {
        Err(TimeSeriesError::ConnectionError(msg)) => assert!(!msg.is_empty()),
        Err(other) => panic!("expected ConnectionError, got {other:?}"),
        Ok(client) => {
            let err = client.get_counts().await.unwrap_err();
            assert!(
                matches!(err, TimeSeriesError::ConnectionError(_)),
                "expected ConnectionError, got {err:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The read-only server refuses nothing it should serve
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_read_rpc_answers_on_a_populated_store() {
    // A breadth check that no read RPC errors on a normal store — the
    // complement of the empty-store matrix above.
    let addr = spawn(populated_store()).await;
    let client = RemoteClient::connect(addr).await.unwrap();
    let key = infrastore_proto::convert::key_from_pb(good_key()).unwrap();

    assert_eq!(
        client
            .list_time_series(None, None, None, None, None, None, None, None, None, None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        client
            .list_keys(
                None, None, None, None, None, None, None, None, None, None, true
            )
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(client.get_time_series(&key, None).await.is_ok());
    assert!(client.get_metadata(&key).await.is_ok());
    assert!(client.has_time_series(&key).await.unwrap());
    assert_eq!(client.bulk_read(&[&key], None).await.unwrap().len(), 1);
    assert_eq!(
        client.get_resolutions(None).await.unwrap(),
        vec![Period::fixed(Duration::hours(1))]
    );
    assert!(client.get_intervals(None).await.unwrap().is_empty());
    assert_eq!(client.get_counts().await.unwrap().static_time_series, 1);
    assert_eq!(
        client.counts_by_type().await.unwrap(),
        vec![(TimeSeriesType::SingleTimeSeries, 1)]
    );
    assert_eq!(
        client
            .list_owner_ids(OwnerCategory::Component, None, None)
            .await
            .unwrap(),
        vec![42]
    );
    assert_eq!(client.static_summary().await.unwrap().len(), 1);
    assert!(client.forecast_summary().await.unwrap().is_empty());
    assert_eq!(
        client.check_static_consistency(None).await.unwrap().len(),
        1
    );
    assert_eq!(
        client
            .time_series_counts_detailed()
            .await
            .unwrap()
            .static_time_series_count,
        1
    );
    assert!(client.verify_integrity().await.unwrap().errors.is_empty());
    assert!(
        client
            .get_forecast_parameters(None, None)
            .await
            .unwrap()
            .horizon
            .is_none()
    );
}

/// `BulkRead` is the one RPC whose cost the caller picks, so it has a ceiling.
///
/// It returns a full copy of a series per key and deliberately does not collapse
/// duplicates (see `bulk_read_returns_duplicate_keys_once_each`), while a key
/// encodes in well under 70 bytes. A request inside tonic's 4 MiB decode limit
/// could therefore name a couple of hundred thousand keys, and the handler
/// materialized every one before writing any response -- a 900 KB request
/// measured an 822 MB response off a 16 KB store, unauthenticated under the
/// default `auth = "none"`.
#[tokio::test]
async fn bulk_read_refuses_more_keys_than_the_server_allows() {
    let addr = spawn_with_bulk_limit(populated_store(), 3).await;
    let mut raw = raw_client(&addr).await;

    // At the limit: still served.
    let resp = raw
        .bulk_read(BulkReadReq {
            keys: vec![good_key(), good_key(), good_key()],
            start_rfc3339: None,
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.items.len(), 3);

    // One over: refused, and refused as a resource limit rather than as bad
    // input, so a client can tell "split the request" from "this key is wrong".
    let err = raw
        .bulk_read(BulkReadReq {
            keys: vec![good_key(), good_key(), good_key(), good_key()],
            start_rfc3339: None,
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::ResourceExhausted, "{err:?}");
    assert!(err.message().contains("split the request"), "{err:?}");
    // The message is read by a person, so it is checked as one: a substring
    // assertion passes happily over a run of stray whitespace left by a wrapped
    // string literal, which is exactly what it used to carry.
    assert_eq!(
        err.message(),
        "bulk_read requested 4 keys, more than this server's limit of 3; split the request",
        "{err:?}"
    );

    // The keys are never even decoded, so a request that is both oversized and
    // malformed reports the size -- the cheap check runs first.
    let err = raw
        .bulk_read(BulkReadReq {
            keys: vec![
                good_key(),
                good_key(),
                good_key(),
                pb::TimeSeriesKey::default(),
            ],
            start_rfc3339: None,
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::ResourceExhausted, "{err:?}");
}

/// The shipped default is high enough not to get in the way of real batching.
#[tokio::test]
async fn the_default_bulk_read_limit_admits_an_ordinary_batch() {
    let addr = spawn(populated_store()).await;
    let mut raw = raw_client(&addr).await;
    let keys = vec![good_key(); 512];
    let resp = raw
        .bulk_read(BulkReadReq {
            keys,
            start_rfc3339: None,
            end_rfc3339: None,
            bounds_zoneless: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.items.len(), 512);
}
