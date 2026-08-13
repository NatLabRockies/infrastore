# gRPC API

The proto contract lives at `proto/infrastore/v1/store.proto` and is compiled into
`infrastore-proto` with `tonic`. The service is **read-only** — every write operation (add, remove,
clear, compact) requires local filesystem access and is intentionally absent.

The [association catalogs](../explanation/data-model.md#associations-between-entities) are absent
too, reads included: no message or RPC covers `supplemental_attribute_associations` or
`parent_child_associations`. Consumers of those tables work against a local `Store`.

- **Package:** `infrastore.v1`
- **Service:** `CatalogStore`

## Methods

| RPC                      | Request                 | Response                 | Purpose                                 |
| ------------------------ | ----------------------- | ------------------------ | --------------------------------------- |
| `ListTimeSeries`         | `ListReq`               | `ListResp`               | List metadata matching a filter         |
| `GetTimeSeries`          | `GetReq`                | `GetResp`                | Fetch one series' values                |
| `GetTimeSeriesKeys`      | `KeysReq`               | `KeysResp`               | List keys for an owner                  |
| `GetResolutions`         | `ResolutionsReq`        | `ResolutionsResp`        | Distinct resolutions present            |
| `GetCounts`              | `CountsReq`             | `CountsResp`             | Aggregate counts                        |
| `GetForecastParameters`  | `ForecastParamsReq`     | `ForecastParamsResp`     | Horizon, interval, count, resolution    |
| `HasTimeSeries`          | `HasReq`                | `HasResp`                | Existence check                         |
| `VerifyIntegrity`        | `VerifyReq`             | `VerifyResp`             | Recompute and compare stored hashes     |
| `ListKeys`               | `ListKeysReq`           | `ListKeysResp`           | Full keys (opt. content hash) by filter |
| `GetMetadata`            | `KeyReq`                | `TimeSeriesMetadata`     | Metadata for one key                    |
| `BulkRead`               | `BulkReadReq`           | `BulkReadResp`           | Fetch many series at once (opt. range)  |
| `GetDetailedCounts`      | `EmptyReq`              | `DetailedCountsResp`     | Distinct owners/arrays per kind         |
| `GetCountsByType`        | `EmptyReq`              | `CountsByTypeResp`       | Association count per type              |
| `ListOwnerIds`           | `ListOwnerIdsReq`       | `ListOwnerIdsResp`       | Distinct owner ids in a category        |
| `GetIntervals`           | `IntervalsReq`          | `IntervalsResp`          | Distinct forecast intervals             |
| `GetStaticSummary`       | `EmptyReq`              | `StaticSummaryResp`      | Grouped static-series summary           |
| `GetForecastSummary`     | `EmptyReq`              | `ForecastSummaryResp`    | Grouped forecast summary                |
| `CheckStaticConsistency` | `ConsistencyReq`        | `ConsistencyResp`        | Per-resolution static-grid check        |
| `ResolveForecastKey`     | `ResolveForecastKeyReq` | `ResolveForecastKeyResp` | Attributes + type → concrete key        |

`TimeSeriesKey` messages returned by `GetTimeSeriesKeys`, `ListKeys`, and `ResolveForecastKey` now
carry the per-variant descriptive snapshot (`initial_timestamp_rfc3339`, `length`, `horizon`,
`count`) in addition to the identity tuple, so `RemoteClient` reconstructs the full core
`TimeSeriesKey` enum.

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
  string         resolution       = 4;   // ISO-8601 duration; empty = unset
  Features       features         = 5;
  OwnerCategory  owner_category   = 6;   // part of the owner identity / key
  string         interval         = 7;   // ISO-8601 duration; empty = unset
  // Descriptive snapshot (NOT part of the identity); variant implied by type.
  optional string initial_timestamp_rfc3339 = 8;
  optional uint64 length                     = 9;
  optional string horizon                    = 10;  // ISO-8601 duration
  optional uint64 count                      = 11;
}

message TimeSeriesMetadata {
  int64           owner_id                  = 1;
  string          owner_type                = 2;
  OwnerCategory   owner_category            = 3;
  TimeSeriesType  time_series_type          = 4;
  string          name                      = 5;
  bytes           data_hash                 = 6;   // 32 bytes
  // Temporal fields are `optional` so genuine values (e.g. length == 0) decode
  // correctly rather than colliding with a zero/empty sentinel.
  optional string initial_timestamp_rfc3339 = 7;
  optional string resolution                = 8;   // ISO-8601 duration
  optional uint64 length                    = 9;
  optional string horizon                   = 10;  // ISO-8601 duration
  optional string interval                  = 11;  // ISO-8601 duration
  optional uint64 count                     = 12;
  repeated string timestamps_rfc3339        = 13;
  Features        features                  = 14;
  optional string units                     = 16;
  string          element_type              = 21;  // canonical element-type string
  // (17 is reserved: the former int32 dtype code)
  repeated uint64 element_shape             = 18;  // per-step trailing dims
  optional string application_data          = 19;  // opaque package-owned payload
  repeated double percentiles               = 20;  // Probabilistic only
  optional string quantity_kind             = 22;  // QUDT QuantityKind local name
  optional string unit_system               = 23;  // "natural_units" | "component_base"
}
```

## Request / Response Messages

```proto
message ListReq {
  optional int64          owner_id         = 1;
  optional string         owner_type       = 2;
  optional TimeSeriesType time_series_type = 3;
  optional string         name             = 4;
  optional string         resolution       = 5;   // ISO-8601 duration
  Features                features         = 6;   // subset match
  optional OwnerCategory  owner_category   = 7;
  optional string         interval         = 8;   // ISO-8601 duration
}
message ListResp { repeated TimeSeriesMetadata metadata = 1; }

message GetReq {
  TimeSeriesKey   key           = 1;
  optional string start_rfc3339 = 2;   // optional time-axis slice; all-or-nothing with end
  optional string end_rfc3339   = 3;
}
message GetResp {
  string          initial_timestamp_rfc3339 = 1;
  string          resolution                = 2;   // ISO-8601 duration
  uint64          length                    = 3;
  repeated uint64 shape                     = 4;   // array dimensions (multi-dim supported)
  reserved 5;                                      // was: repeated double values
  TimeSeriesType  time_series_type          = 6;
  repeated string timestamps_rfc3339        = 7;   // set for NonSequentialTimeSeries
  string          element_type              = 16;  // canonical element-type string
  // (8 is reserved: the former int32 dtype code)
  bytes           value_bytes               = 9;   // raw little-endian, row-major
  string          application_data              = 10;
  // Forecast-specific fields (populated for Deterministic / Probabilistic / Scenarios).
  string          horizon                   = 11;  // ISO-8601 duration
  string          interval                  = 12;  // ISO-8601 duration
  uint64          count                     = 13;
  repeated double percentiles               = 14;  // Probabilistic only
  uint64          scenario_count            = 15;  // Scenarios only
}

message KeysReq  { int64 owner_id = 1; OwnerCategory owner_category = 2; }
message KeysResp { repeated TimeSeriesKey keys = 1; }

message ResolutionsReq  { optional TimeSeriesType time_series_type = 1; }
message ResolutionsResp { repeated string resolution = 1; }   // ISO-8601 durations

message CountsReq  {}
message CountsResp {
  int64 components_with_time_series = 1;
  int64 static_time_series          = 2;
  int64 forecasts                   = 3;
}

message ForecastParamsReq  {
  optional string resolution = 1;   // ISO-8601 duration filter
  optional string interval   = 2;   // ISO-8601 duration filter
}
message ForecastParamsResp {
  optional string horizon                   = 1;   // ISO-8601 duration
  optional string interval                  = 2;   // ISO-8601 duration
  optional uint64 count                     = 3;
  optional string resolution                = 4;   // ISO-8601 duration
  optional string initial_timestamp_rfc3339 = 5;
}

message HasReq  { TimeSeriesKey key = 1; }
message HasResp { bool present = 1; }

message VerifyReq  {}
message VerifyResp { repeated string errors = 1; }
```

### `GetReq` Time Slice

`start_rfc3339` and `end_rfc3339` are **all-or-nothing**: supply both to request a time-axis slice,
or neither to fetch the whole series. Setting exactly one is rejected with `InvalidArgument`
(`"start_rfc3339 and end_rfc3339 must be supplied together"`). Each value must parse as RFC 3339; a
malformed timestamp is also `InvalidArgument`.

## Forecasts Over gRPC

The service is read-only, but its read surface covers dense forecasts. Forecast associations created
through the [Rust core](./rust-api.md#forecasts) or [C ABI](./c-abi.md#forecasts) appear in
`ListTimeSeries` — `TimeSeriesMetadata` carries `horizon`, `interval` (ISO-8601 durations), `count`,
and (for `Probabilistic`) `percentiles` — and `GetCounts` includes them in `forecasts`.

`GetTimeSeries` returns forecast values too. For a `Deterministic`, `DeterministicSingleTimeSeries`
(synthesized into `Deterministic`), `Probabilistic`, or `Scenarios` key it fills the `GetResp` array
fields (`value_bytes` + `element_type`), the window parameters (`horizon`, `interval`, `count`), and
the `percentiles` (`Probabilistic`) or `scenario_count` (`Scenarios`); the client reconstructs the
matching type. Arrays are dtype-generic on the wire — `value_bytes` is the raw little-endian buffer
and `element_type` says both what the elements mean and, through it, their physical dtype
(`f64`/`f32`/`i64`/…, or a composite kind like `piecewise_linear`), so non-`f64` arrays survive the
round trip without coercion. One caveat:

- **`application_data` is not carried in `GetResp`.** The opaque package-owned payload is returned
  by `ListTimeSeries` (on `TimeSeriesMetadata`) but left empty by `GetTimeSeries`, so a value
  fetched directly by key comes back without it.

## Authentication

When the server is configured with `method = "api_key"`, clients must send the key in the
**`x-api-key`** request metadata (header). The server checks the supplied key against every
configured key of the same length without early-exit, so a match is not leaked by timing; the
comparison is not blinded against the supplied key's _length_, which is treated as non-secret. A
missing or wrong key is rejected before the RPC runs. With `method = "none"` no metadata is
required. See [Server Configuration](./server-config.md).

## Rust Client

`infrastore-server` ships an async `RemoteClient` that mirrors the read methods and returns core
types, mapping gRPC `Status` codes back onto the `TimeSeriesError` taxonomy:

| gRPC `Code`          | `TimeSeriesError`                |
| -------------------- | -------------------------------- |
| `NotFound`           | `NotFound`                       |
| `AlreadyExists`      | `DuplicateTimeSeries`            |
| `InvalidArgument`    | `InvalidParameter(message)`      |
| `FailedPrecondition` | `InvalidParameter(message)`      |
| `DataLoss`           | `IntegrityError(message)`        |
| anything else        | `ConnectionError(code: message)` |

`ConnectionError` is only the fallback arm, so a remote `NotFound` or a rejected argument surfaces
with the same variant a local `Store` would return.

```rust
use infrastore_core::OwnerCategory;
use infrastore_server::client::RemoteClient;

let client = RemoteClient::connect("http://127.0.0.1:50051".into()).await?;
let counts = client.get_counts().await?;
let keys = client.get_time_series_keys(42, OwnerCategory::Component).await?;
let data = client.get_time_series(&keys[0], None).await?;
```

`RemoteClient` methods: `connect`, `from_channel`, `list_time_series`, `list_keys`, `get_metadata`,
`get_time_series`, `bulk_read`, `get_time_series_keys`, `get_resolutions`, `get_intervals`,
`get_counts`, `counts_by_type`, `time_series_counts_detailed`, `get_forecast_parameters`,
`static_summary`, `forecast_summary`, `list_owner_ids`, `has_time_series`,
`check_static_consistency`, `resolve_forecast_key`, `verify_integrity` — the full read surface
described above. See the [gRPC Server guide](../guides/server.md) for end-to-end usage and adding an
API key to client requests.
