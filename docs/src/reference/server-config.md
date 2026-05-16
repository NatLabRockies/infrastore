# Server Configuration

The gRPC server is configured by a single TOML file passed with `--config`. The starting point is
`examples/server.toml`.

```sh
time-series-store-server --config my_server.toml
```

## File Structure

```toml
[server]
host = "0.0.0.0"
port = 50051

[data]
files = ["./store.nc"]

[authentication]
# "none" or "api_key". For api_key, populate `keys`; clients must send the
# value in the `x-api-key` request header.
method = "none"
# keys = ["replace-me-with-a-secret-1", "replace-me-with-a-secret-2"]
```

## Sections

### `[server]`

| Key    | Type    | Required | Description                                |
| ------ | ------- | -------- | ------------------------------------------ |
| `host` | string  | yes      | Bind address (e.g. `0.0.0.0`, `127.0.0.1`) |
| `port` | integer | yes      | TCP port                                   |

### `[data]`

| Key     | Type            | Required | Description                          |
| ------- | --------------- | -------- | ------------------------------------ |
| `files` | array of string | yes      | NetCDF file paths to serve read-only |

v0 serves a **single** file (the first entry). Multiple entries are reserved for a later milestone.
The matching `<path>.sqlite` catalog must sit beside each NetCDF file. The server opens the store
read-only.

### `[authentication]`

The whole section is optional; omitting it defaults to `method = "none"`.

| Key      | Type            | Default  | Description                                           |
| -------- | --------------- | -------- | ----------------------------------------------------- |
| `method` | string          | `"none"` | `"none"` or `"api_key"` (`oauth` reserved)            |
| `keys`   | array of string | `[]`     | Accepted API keys; required when `method = "api_key"` |

Validation runs at startup, so misconfiguration fails loudly rather than at the first request:

- `method = "api_key"` with an empty `keys` list is rejected.
- An unknown `method` value is rejected.

When `method = "api_key"`, each request must carry a matching value in the `x-api-key` metadata
header; keys are compared in constant time.

## Startup Behavior

On launch the server:

1. Loads and parses the TOML file.
2. Validates the `[authentication]` section.
3. Opens the first `[data].files` entry as a read-only store (errors if the list is empty).
4. Binds `host:port` and serves the `TimeSeriesStore` gRPC service.

Logging honors the `RUST_LOG` environment variable (default `info`):

```sh
RUST_LOG=debug time-series-store-server --config my_server.toml
```

See the [gRPC Server guide](../guides/server.md) for the end-to-end workflow and the
[gRPC API reference](./grpc-api.md) for the served methods.
