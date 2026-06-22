# gRPC API

The proto contract lives at `proto/time_series_store/v1/store.proto` and is compiled into
`time-series-store-proto` with `tonic`. The service is **read-only** — every write operation (add,
remove, clear, compact) requires local filesystem access and is intentionally absent.

- **Package:** `time_series_store.v1`
- **Service:** `TimeSeriesStore`

## Methods

| RPC                     | Request             | Response             | Purpose                              |
| ----------------------- | ------------------- | -------------------- | ------------------------------------ |
| `ListTimeSeries`        | `ListReq`           | `ListResp`           | List metadata matching a filter      |
| `GetTimeSeries`         | `GetReq`            | `GetResp`            | Fetch one series' values             |
| `GetTimeSeriesKeys`     | `KeysReq`           | `KeysResp`           | List keys for an owner               |
| `GetResolutions`        | `ResolutionsReq`    | `ResolutionsResp`    | Distinct resolutions present         |
| `GetCounts`             | `CountsReq`         | `CountsResp`         | Aggregate counts                     |
| `GetForecastParameters` | `ForecastParamsReq` | `ForecastParamsResp` | Horizon, interval, count, resolution |
| `HasTimeSeries`         | `HasReq`            | `HasResp`            | Existence check                      |
| `VerifyIntegrity`       | `VerifyReq`         | `VerifyResp`         | Recompute and compare stored hashes  |

## Common Messages

```proto
enum TimeSeriesType {
  SINGLE_TIME_SERIES               = 0;
  NON_SEQUENTIAL_TIME_SERIES       = 1;
  DETERMINISTIC                    = 2;
  DETERMINISTIC_SINGLE_TIME_SERIES = 3;
  PROBABILISTIC                    = 4;
  SCENARIOS                        = 5;
}

enum OwnerCategory { COMPONENT = 0; SUPPLEMENTAL_ATTRIBUTE = 1; }

message FeatureValue {
  oneof value {
    int64  int_value   = 1;
    double float_value = 2;
    bool   bool_value  = 3;
    string str_value   = 4;
  }
}

message Features { map<string, FeatureValue> entries = 1; }

message TimeSeriesKey {
  int64          owner_id         = 1;
  TimeSeriesType time_series_type = 2;
  string         name             = 3;
  int64          resolution_ms    = 4;   // 0 = unset
  Features       features         = 5;
}

message TimeSeriesMetadata {
  int64           owner_id                  = 1;
  string          owner_type                = 2;
  OwnerCategory   owner_category            = 3;
  TimeSeriesType  time_series_type          = 4;
  string          name                      = 5;
  bytes           data_hash                 = 6;   // 32 bytes
  string          initial_timestamp_rfc3339 = 7;
  int64           resolution_ms             = 8;
  uint64          length                    = 9;
  int64           horizon_ms                = 10;
  int64           interval_ms               = 11;
  uint64          count                     = 12;
  repeated string timestamps_rfc3339        = 13;
  Features        features                  = 14;
  string          units                     = 16;
}
```

## Request / Response Messages

```proto
message ListReq {
  optional int64          owner_id         = 1;
  optional string         owner_type       = 2;
  optional TimeSeriesType time_series_type = 3;
  optional string         name             = 4;
  optional int64          resolution_ms    = 5;
  Features                features         = 6;   // subset match
}
message ListResp { repeated TimeSeriesMetadata metadata = 1; }

message GetReq {
  TimeSeriesKey   key           = 1;
  optional string start_rfc3339 = 2;   // optional time-axis slice
  optional string end_rfc3339   = 3;
}
message GetResp {
  string          initial_timestamp_rfc3339 = 1;
  int64           resolution_ms             = 2;
  uint64          length                    = 3;
  repeated uint64 shape                     = 4;   // array dimensions (multi-dim supported)
  repeated double values                    = 5;   // row-major f64
  TimeSeriesType  time_series_type          = 6;
  repeated string timestamps_rfc3339        = 7;   // set for NonSequentialTimeSeries
}

message KeysReq  { int64 owner_id = 1; }
message KeysResp { repeated TimeSeriesKey keys = 1; }

message ResolutionsReq  { optional TimeSeriesType time_series_type = 1; }
message ResolutionsResp { repeated int64 resolution_ms = 1; }

message CountsReq  {}
message CountsResp {
  int64 components_with_time_series = 1;
  int64 static_time_series          = 2;
  int64 forecasts                   = 3;
}

message ForecastParamsReq  {}
message ForecastParamsResp {
  optional int64  horizon_ms    = 1;
  optional int64  interval_ms   = 2;
  optional uint64 count         = 3;
  optional int64  resolution_ms = 4;
}

message HasReq  { TimeSeriesKey key = 1; }
message HasResp { bool present = 1; }

message VerifyReq  {}
message VerifyResp { repeated string errors = 1; }
```

## Forecasts Over gRPC

The service is read-only and was not extended for forecast _values_. Forecast associations created
through the [Rust core](./rust-api.md#forecasts) or [C ABI](./c-abi.md#forecasts) do appear in
`ListTimeSeries` — `TimeSeriesMetadata` already carries `horizon_ms`, `interval_ms`, and `count`,
and `GetCounts` includes them in `forecasts`. Two caveats:

- **`percentiles` is not on the wire.** `Probabilistic` percentiles are dropped in the gRPC
  conversion, so they are not returned to clients.
- **`GetTimeSeries` does not fetch forecast values.** It reconstructs `SingleTimeSeries` or
  `NonSequentialTimeSeries` and returns `InvalidArgument` for forecast types. Non-sequential
  responses set `time_series_type` and `timestamps_rfc3339`. Read forecast arrays through a local
  store or the C ABI instead.
- **Array typing is `f64` over the wire.** The `dtype`, `element_shape`, and `logical_type` fields
  are not carried in the proto contract — `GetResp.values` are always `f64` (with `shape` giving the
  dimensions), and `TimeSeriesMetadata` reconstructed from gRPC defaults to `f64` / empty element
  shape / no `logical_type`.

## Authentication

When the server is configured with `method = "api_key"`, clients must send the key in the
**`x-api-key`** request metadata (header). The server compares it in constant time against the
configured keys; a missing or wrong key is rejected before the RPC runs. With `method = "none"` no
metadata is required. See [Server Configuration](./server-config.md).

## Rust Client

`time-series-store-server` ships an async `RemoteClient` that mirrors the read methods and returns
core types, mapping gRPC `Status` codes to `TimeSeriesError::ConnectionError`:

```rust
use time_series_store_server::client::RemoteClient;

let client = RemoteClient::connect("http://127.0.0.1:50051".into()).await?;
let counts = client.get_counts().await?;
let keys = client.get_time_series_keys("42".into()).await?;
let data = client.get_time_series(&keys[0], None).await?;
```

`RemoteClient` methods: `connect`, `from_channel`, `list_time_series`, `get_time_series`,
`get_time_series_keys`, `get_resolutions`, `get_counts`, `get_forecast_parameters`,
`has_time_series`, `verify_integrity`. See the [gRPC Server guide](../guides/server.md) for
end-to-end usage and adding an API key to client requests.
