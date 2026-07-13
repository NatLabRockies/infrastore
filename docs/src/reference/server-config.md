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

Validation runs at startup, so a bad `[authentication]` section fails loudly rather than at the
first request:

- `method = "api_key"` with an empty `keys` list is rejected.
- An unknown `method` value is rejected.

That validation covers the values, not the key names. `ServerConfig` does not reject unknown fields,
so a **misspelled TOML key is silently ignored** and the default (or a missing-field parse error,
for a required key) applies instead. A server that starts with `[authenticaton]` (sic) is running
with `method = "none"` — check the startup logs and confirm the effective settings rather than
assuming a typo would have been caught.

When `method = "api_key"`, each request must carry a matching value in the `x-api-key` metadata
header. Keys are compared without early-exit — every configured key of the same length as the
supplied one is checked, so timing does not reveal which key matched or how far a wrong key got. The
supplied key's length is not blinded: keys of a different length are rejected before the byte-wise
compare, treating length as non-secret.

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
