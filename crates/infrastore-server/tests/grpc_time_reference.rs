//! The timestamp-spelling semantics, over the wire.
//!
//! A series records how its timestamps were *spelled*, and that spelling is not
//! inert: it survives a read, it is a list filter, and it constrains which query
//! bounds a series will answer. All three cross the gRPC boundary, and none of
//! them was exercised there -- the server suites set `time_reference` and
//! `bounds_zoneless` to `None` everywhere, which tests only that the new fields
//! compile.
//!
//! What makes this worth its own file is that the conversion is lossy in both
//! directions if it is wrong: a spelling dropped in `key_to_pb` reads back as
//! unspecified, and a `bounds_zoneless` dropped in the request turns a refusal
//! into a wrong answer.

use std::time::Duration as StdDuration;

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    Features, NonSequentialTimeSeries, OwnerCategory, SingleTimeSeries, Store, TimeRange,
    TimeReference, TimeSeriesData, TypedArray, create_store,
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
    tokio::time::sleep(StdDuration::from_millis(50)).await;
    format!("http://{local_addr}")
}

/// One hourly series per spelling, each under its own owner id so a filter can
/// pick them apart.
fn store_with_every_spelling() -> Store {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let values = TypedArray::from_f64(vec![4], &[1.0, 2.0, 3.0, 4.0]);

    for (owner, reference) in [
        (1i64, Some(TimeReference::Utc)),
        (2, Some(TimeReference::FixedOffset(-420))),
        (3, Some(TimeReference::Zone("America/Denver".into()))),
        (4, Some(TimeReference::Zoneless)),
        (5, None),
    ] {
        let s = SingleTimeSeries::new(initial, Duration::hours(1), values.clone(), "load");
        let mut data = TimeSeriesData::SingleTimeSeries(s);
        data.set_time_reference(reference);
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
    store
}

#[tokio::test]
async fn every_spelling_survives_the_wire_unchanged() {
    let addr = spawn_server(store_with_every_spelling()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let metas = client
        .list_metadata(None, None, None, None, None, None, None, None, None, None)
        .await
        .unwrap();
    assert_eq!(metas.len(), 5);

    // Each spelling comes back as itself. `None` in particular must stay `None`
    // rather than being promoted to a wall clock or to UTC: it is the claim
    // that nothing was recorded, and a client that re-wrote what it read would
    // otherwise invent one.
    for meta in &metas {
        let expected = match meta.owner_id {
            1 => Some(TimeReference::Utc),
            2 => Some(TimeReference::FixedOffset(-420)),
            3 => Some(TimeReference::Zone("America/Denver".into())),
            4 => Some(TimeReference::Zoneless),
            5 => None,
            other => panic!("unexpected owner {other}"),
        };
        assert_eq!(
            meta.time_reference, expected,
            "owner {} lost its spelling",
            meta.owner_id
        );
    }

    // A row fetched by its own id carries it too, not just a listing's rows.
    let rows = client
        .list_metadata(None, None, None, None, None, None, None, None, None, None)
        .await
        .unwrap();
    let zoneless = rows
        .iter()
        .find(|m| m.owner_id == 4)
        .and_then(|m| m.id)
        .expect("the zoneless series");
    assert_eq!(
        client
            .get_metadata_by_id(zoneless)
            .await
            .unwrap()
            .time_reference,
        Some(TimeReference::Zoneless)
    );
}

#[tokio::test]
async fn the_zoneless_filter_splits_the_two_coherence_groups() {
    let addr = spawn_server(store_with_every_spelling()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let zoneless = client
        .list_metadata(
            None,
            None,
            None,
            None,
            None,
            None,
            Some(true),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(zoneless.len(), 1, "only owner 4 is zoneless");
    assert_eq!(zoneless[0].owner_id, 4);

    // The complement is everything that accepts a zoned bound -- which includes
    // the series that recorded no spelling at all. An unspecified reference is
    // not a floating third group.
    let zoned = client
        .list_metadata(
            None,
            None,
            None,
            None,
            None,
            None,
            Some(false),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let mut owners: Vec<i64> = zoned.iter().map(|m| m.owner_id).collect();
    owners.sort_unstable();
    assert_eq!(owners, vec![1, 2, 3, 5]);
}

#[tokio::test]
async fn a_bound_spelled_the_wrong_way_is_refused_over_the_wire() {
    let addr = spawn_server(store_with_every_spelling()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let rows = client
        .list_metadata(None, None, None, None, None, None, None, None, None, None)
        .await
        .unwrap();
    let by_owner = |owner: i64| {
        rows.iter()
            .find(|m| m.owner_id == owner)
            .and_then(|m| m.id)
            .unwrap_or_else(|| panic!("owner {owner}"))
    };

    let start = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let end = Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap();

    // An instant bound against a wall-clock series has no defined mapping, and
    // the refusal has to survive as a refusal -- a bound whose spelling was
    // dropped in conversion would silently return the wrong two rows instead.
    let err = client
        .read_by_id(by_owner(4), Some(TimeRange::new(start, end)))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("is zoneless") && msg.contains("wall clocks"),
        "expected the category-mismatch refusal, got: {msg}"
    );

    // And the reverse: a wall-clock bound against a series recording instants.
    let err = client
        .read_by_id(by_owner(1), Some(TimeRange::zoneless(start, end)))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("carry no zone") && msg.contains("records instants"),
        "expected the category-mismatch refusal, got: {msg}"
    );

    // Matched both ways, the slice is taken.
    let data = client
        .read_by_id(by_owner(1), Some(TimeRange::new(start, end)))
        .await
        .unwrap();
    assert_eq!(data.as_single().unwrap().length, 2);
    let data = client
        .read_by_id(by_owner(4), Some(TimeRange::zoneless(start, end)))
        .await
        .unwrap();
    assert_eq!(data.as_single().unwrap().length, 2);
}

#[tokio::test]
async fn a_ranged_bulk_read_cannot_span_both_coherence_groups() {
    let addr = spawn_server(store_with_every_spelling()).await;
    let client = RemoteClient::connect(addr).await.unwrap();

    let rows = client
        .list_metadata(None, None, None, None, None, None, None, None, None, None)
        .await
        .unwrap();
    let ids: Vec<_> = rows
        .iter()
        .map(|m| m.id.expect("a served row carries its id"))
        .collect();

    let start = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let end = Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap();

    // The selection holds both a zoneless series and instant-bearing ones, so
    // one bound cannot mean the right thing for all of them.
    let err = client
        .read_by_ids(&ids, Some(TimeRange::new(start, end)))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("mixes zoneless series"),
        "expected the mixed-selection refusal, got: {msg}"
    );
    // The remedy is named, and both offending series are identified -- a
    // message that only said "mixed" would leave the caller to find them.
    assert!(msg.contains("owner 4") && msg.contains("owner 1"), "{msg}");
    assert!(msg.contains("ListFilter::zoneless"), "{msg}");

    // Narrowed to one group it succeeds, which is the constructive remedy the
    // `zoneless` filter exists to provide.
    let zoned: Vec<_> = rows
        .iter()
        .filter(|m| m.owner_id != 4)
        .map(|m| m.id.unwrap())
        .collect();
    let out = client
        .read_by_ids(&zoned, Some(TimeRange::new(start, end)))
        .await
        .unwrap();
    assert_eq!(out.len(), 4);
    for d in &out {
        assert_eq!(d.as_single().unwrap().length, 2);
    }

    // An unranged bulk read over the whole mixed selection is fine: without
    // bounds there is nothing to disagree about.
    assert_eq!(client.read_by_ids(&ids, None).await.unwrap().len(), 5);
}

/// The irregular type carries a spelling on the same terms, and it is the one
/// whose timestamps are stored rather than derived -- so a dropped reference
/// there changes what the vector *means*, not just how it is labelled.
#[tokio::test]
async fn an_irregular_series_carries_its_spelling_too() {
    let mut store = create_store(None, true).unwrap();
    let stamps = vec![
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2024, 1, 1, 5, 0, 0).unwrap(),
    ];
    let nsts = NonSequentialTimeSeries::new(
        stamps,
        TypedArray::from_f64(vec![2], &[1.0, 2.0]),
        "irregular",
    )
    .unwrap();
    let mut data = TimeSeriesData::NonSequentialTimeSeries(nsts);
    data.set_time_reference(Some(TimeReference::Zoneless));
    store
        .add_time_series(
            9,
            "Generator",
            OwnerCategory::Component,
            data,
            Features::new(),
        )
        .unwrap();

    let addr = spawn_server(store).await;
    let client = RemoteClient::connect(addr).await.unwrap();
    let metas = client
        .list_metadata(None, None, None, None, None, None, None, None, None, None)
        .await
        .unwrap();
    assert_eq!(metas[0].time_reference, Some(TimeReference::Zoneless));
}
