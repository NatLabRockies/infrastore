# gRPC Server & Client Guide

The gRPC server exposes a store for remote, **read-only** access. Writes always require local
filesystem access, so the service offers only list/get/keys/resolutions/counts/exists/verify. This
guide covers running the server and talking to it from Rust. For the wire contract see the
[gRPC API reference](../reference/grpc-api.md); for the config file see
[Server Configuration](../reference/server-config.md).

## When to Use It

Use the server to fan out reads of an existing store to many clients or across the network — for
example, serving a published dataset to analysis jobs. A single writer produces the `.nc` +
`.nc.sqlite` pair locally; the server then reads it and answers queries. It never modifies the
files.

## Run the Server

1. Produce a store with any binding and `flush()` it to disk.
2. Write a config (start from `examples/server.toml`):

   ```toml
   [server]
   host = "0.0.0.0"
   port = 50051

   [data]
   files = ["./system.nc"]   # the .nc.sqlite catalog must sit beside it

   [authentication]
   method = "none"
   ```

3. Launch:

   ```sh
   cargo run -p castore-server -- --config my_server.toml
   # or, from a release build:
   ./target/release/castore-server --config my_server.toml
   ```

On startup the server validates the auth section, opens the first `[data].files` entry read-only,
and serves the `CatalogStore` service on `host:port`. Set `RUST_LOG=debug` for verbose logs.

## The Rust Client

`RemoteClient` mirrors the read methods of `Store` and returns the same core types. gRPC status
codes are mapped back onto `TimeSeriesError`, so remote and local calls surface the same error
taxonomy.

```rust
use castore_core::OwnerCategory;
use castore_server::client::RemoteClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = RemoteClient::connect("http://127.0.0.1:50051".into()).await?;

    let counts = client.get_counts().await?;
    println!("{} static series", counts.static_time_series);

    // The owner is the (owner_id, owner_category) pair.
    let keys = client.get_time_series_keys(42, OwnerCategory::Component).await?;
    if let Some(key) = keys.first() {
        let data = client.get_time_series(key, None).await?;
        println!("read {} values", data.as_single().unwrap().length);
    }
    Ok(())
}
```

Available methods: `connect`, `from_channel`, `list_time_series`, `get_time_series`,
`get_time_series_keys`, `get_resolutions`, `get_counts`, `get_forecast_parameters`,
`has_time_series`, `verify_integrity`.

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
use castore_proto::pb::catalog_store_client::CatalogStoreClient;
use castore_proto::pb::CountsReq;
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

The proto file at `proto/castore/v1/store.proto` is a standard proto3 definition. Generate a client
for any gRPC-supported language from it, sending the `x-api-key` metadata header when the server
requires authentication.
