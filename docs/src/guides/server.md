# gRPC Server & Client Guide

The gRPC server exposes a store for remote, **read-only** access. Writes always require local
filesystem access, so the service offers only list/get/keys/resolutions/counts/exists/verify. This
guide covers running the server and talking to it from Rust. For the wire contract see the
[gRPC API reference](../reference/grpc-api.md); for the config file see
[Server Configuration](../reference/server-config.md).

## When to Use It

Use the server to fan out reads of an existing store to many clients or across the network — for
example, serving a published dataset to analysis jobs. A single writer produces the `.h5` +
`.h5.sqlite` pair locally; the server then reads it and answers queries. It never modifies the
files.

## Run the Server

1. Produce a store with any binding and `flush()` it to disk.
2. Write a config (start from `examples/server.toml`):

   ```toml
   [server]
   host = "0.0.0.0"
   port = 50051

   [data]
   files = ["./system.h5"]   # the .h5.sqlite catalog must sit beside it

   [authentication]
   method = "none"
   ```

3. Launch:

   ```sh
   cargo run -p infrastore-server -- --config my_server.toml
   # or, from a release build:
   ./target/release/infrastore-server --config my_server.toml
   ```

On startup the server validates the auth section, opens the first `[data].files` entry read-only,
and serves the `CatalogStore` service on `host:port`. Set `RUST_LOG=debug` for verbose logs.

v0 serves the **first** `[data].files` entry; multi-file serving is reserved for later.

### Check that it is up

With [`grpcurl`](https://github.com/fullstorydev/grpcurl) and the proto file:

```sh
grpcurl -plaintext -proto proto/infrastore/v1/store.proto \
  127.0.0.1:50051 infrastore.v1.CatalogStore/GetCounts
```

Add `-H 'x-api-key: replace-me-with-a-secret'` when authentication is enabled. The equivalent from
Rust is `RemoteClient::connect(...).get_counts()` — see [The Rust Client](#the-rust-client) below.

## The Rust Client

`RemoteClient` mirrors the read methods of `Store` and returns the same core types. gRPC status
codes are mapped back onto `TimeSeriesError`, so remote and local calls surface the same error
taxonomy.

```rust
use infrastore_core::OwnerCategory;
use infrastore_server::client::RemoteClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = RemoteClient::connect("http://127.0.0.1:50051".into()).await?;

    let counts = client.get_counts().await?;
    println!("{} static series", counts.static_time_series);

    // Identify, then act. The owner is the (owner_id, owner_category) pair, and
    // every row carries the catalog id that addresses it.
    let rows = client
        .list_metadata(
            Some(42),
            Some(OwnerCategory::Component),
            None, None, None, None, None, None, None, None,
        )
        .await?;
    if let Some(id) = rows.first().and_then(|m| m.id) {
        let data = client.read_by_id(id, None).await?;
        println!("read {} values", data.as_single().unwrap().length);
    }
    Ok(())
}
```

A series is addressed by its catalog **association id**, never by a key: `list_metadata` (or
`list_metadata_by_ids`, for ids a caller already recorded) hands back rows carrying `id`, and
`read_by_id` / `read_by_ids` / `get_metadata_by_id` take it. `association_exists` answers whether a
stored reference still resolves without fetching its row — the cheap way to validate a whole model
on load.

Available methods: `connect`, `from_channel`, `list_metadata`, `list_metadata_by_ids`,
`get_metadata_by_id`, `association_exists`, `has_any_time_series`, `read_by_id`, `read_by_ids`,
`get_resolutions`, `get_intervals`, `get_counts`, `counts_by_type`, `time_series_counts_detailed`,
`get_forecast_parameters`, `list_owner_ids`, `static_summary`, `forecast_summary`,
`check_static_consistency`, `verify_integrity`.

## Authentication

To require an API key, configure the server:

```toml
[authentication]
method = "api_key"
keys = ["replace-me-with-a-secret-1", "replace-me-with-a-secret-2"]
```

`method = "api_key"` with an empty `keys` list is rejected at startup. Clients must then send the
key in the **`x-api-key`** metadata header; a missing or wrong key is rejected with
`Unauthenticated` before the RPC runs. The comparison against the configured keys does not
early-exit — every key of the same length as the supplied one is checked — so _which_ key matched is
not leaked by timing. The supplied key's length is not blinded; keys of a different length are
rejected without a byte-wise compare, on the assumption that length is not secret.

`RemoteClient::connect` does not attach auth metadata, so against an authenticated server use the
generated client with an interceptor that injects the header:

```rust
use infrastore_proto::pb::catalog_store_client::CatalogStoreClient;
use infrastore_proto::pb::CountsReq;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

let channel = Channel::from_shared("http://127.0.0.1:50051")?.connect().await?;
let key: MetadataValue<_> = "replace-me-with-a-secret-1".parse()?;
let mut client = CatalogStoreClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
    req.metadata_mut().insert("x-api-key", key.clone());
    Ok(req)
});

let counts = client.get_counts(CountsReq {}).await?.into_inner();
```

## Clients in Other Languages

The proto file at `proto/infrastore/v1/store.proto` is a standard proto3 definition. Generate a
client for any gRPC-supported language from it, sending the `x-api-key` metadata header when the
server requires authentication.
