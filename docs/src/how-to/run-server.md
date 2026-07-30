# Run the gRPC Server

Serve an existing store read-only over gRPC. For background and the Rust client, see the
[gRPC Server guide](../guides/server.md); for every config key, see
[Server Configuration](../reference/server-config.md).

## 1. Produce a Store on Disk

Create the `.h5` + `.h5.sqlite` pair with any binding, then `flush()` it. For example, in Rust:

```rust
let mut store = create_store(Some(Path::new("system.h5")), false)?;
// ... add_time_series ...
store.flush()?;
```

## 2. Write a Config

```toml
# my_server.toml
[server]
host = "0.0.0.0"
port = 50051

[data]
files = ["./system.h5"]   # the ./system.h5.sqlite catalog must sit beside it

[authentication]
method = "none"
```

## 3. Launch

```sh
cargo run -p infrastore-server -- --config my_server.toml
# or a release build:
./target/release/infrastore-server --config my_server.toml
```

Add `RUST_LOG=debug` for verbose logging.

## Require an API Key

```toml
[authentication]
method = "api_key"
keys = ["replace-me-with-a-secret"]
```

Clients must then send the key in the `x-api-key` metadata header; a missing or wrong key is
rejected with `Unauthenticated`. An empty `keys` list with `method = "api_key"` is rejected at
startup.

## Quick Connectivity Check

From Rust:

```rust
use infrastore_server::client::RemoteClient;

let client = RemoteClient::connect("http://127.0.0.1:50051".into()).await?;
println!("{:?}", client.get_counts().await?);
```

Or with [`grpcurl`](https://github.com/fullstorydev/grpcurl) using the proto file:

```sh
grpcurl -plaintext -proto proto/infrastore/v1/store.proto \
  127.0.0.1:50051 infrastore.v1.CatalogStore/GetCounts
```

(Add `-H 'x-api-key: replace-me-with-a-secret'` when authentication is enabled.)

## Notes

- The server is **read-only**; it never writes the files. Run one writer locally, then serve the
  result.
- v0 serves the **first** `[data].files` entry; multi-file serving is reserved for later.
