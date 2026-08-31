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

| RPC                      | Request                     | Response                     | Purpose                                |
| ------------------------ | --------------------------- | ---------------------------- | -------------------------------------- |
| `ListMetadata`           | `ListMetadataReq`           | `ListMetadataResp`           | Catalog rows matching a filter         |
| `ListMetadataByIds`      | `ListMetadataByIdsReq`      | `ListMetadataByIdsResp`      | Catalog rows for a set of ids          |
| `GetMetadataById`        | `GetMetadataByIdReq`        | `TimeSeriesMetadata`         | One catalog row by id                  |
| `AssociationExists`      | `AssociationExistsReq`      | `AssociationExistsResp`      | Is an id still filed? (fetches no row) |
| `HasAnyTimeSeries`       | `HasAnyTimeSeriesReq`       | `HasAnyTimeSeriesResp`       | Attribute-addressed existence probe    |
| `ReadById`               | `ReadByIdReq`               | `ReadByIdResp`               | One series' values (opt. range)        |
| `ReadByIds`              | `ReadByIdsReq`              | `ReadByIdsResp`              | Many series at once (opt. range)       |
| `GetResolutions`         | `GetResolutionsReq`         | `GetResolutionsResp`         | Distinct resolutions present           |
| `GetIntervals`           | `GetIntervalsReq`           | `GetIntervalsResp`           | Distinct forecast intervals            |
| `GetCounts`              | `GetCountsReq`              | `GetCountsResp`              | Aggregate counts                       |
| `GetDetailedCounts`      | `GetDetailedCountsReq`      | `GetDetailedCountsResp`      | Distinct owners/arrays per kind        |
| `GetCountsByType`        | `GetCountsByTypeReq`        | `GetCountsByTypeResp`        | Association count per type             |
| `GetForecastParameters`  | `GetForecastParametersReq`  | `GetForecastParametersResp`  | Horizon, interval, count, resolution   |
| `ListOwnerIds`           | `ListOwnerIdsReq`           | `ListOwnerIdsResp`           | Distinct owner ids in a category       |
| `GetStaticSummary`       | `GetStaticSummaryReq`       | `GetStaticSummaryResp`       | Grouped static-series summary          |
| `GetForecastSummary`     | `GetForecastSummaryReq`     | `GetForecastSummaryResp`     | Grouped forecast summary               |
| `CheckStaticConsistency` | `CheckStaticConsistencyReq` | `CheckStaticConsistencyResp` | Per-resolution static-grid check       |
| `VerifyIntegrity`        | `VerifyIntegrityReq`        | `VerifyIntegrityResp`        | Recompute and compare stored hashes    |

Every RPC is named for the `Store` method it exposes, and its request and response are `<Rpc>Req` /
`<Rpc>Resp` — including the ones that carry no field today, so a later filter lands as an added
field rather than a new message and a second RPC.

**There is no key on this wire.** A series is addressed by its catalog **association id**, an
`int64` that `ListMetadata` and `ListMetadataByIds` hand back on every row and that `ReadById`,
`ReadByIds` and `GetMetadataById` take. The split is _identify_ then _act_: `ListMetadata` is the
flexible half (the filter names attributes), and everything that reads or resolves a single series
takes the id it returned. There is deliberately no attribute-to-id resolver RPC — a caller that
wants exactly one row poses the filter and checks that it got one.

`GetMetadataById` and `ListMetadataByIds` return `NOT_FOUND` for an id that names no row, because a
call already committed to fetching treats a stale reference as a failure. `AssociationExists` is the
call that treats it as an answer, and it is a primary-key probe that hydrates nothing — the right
one for validating a whole model's stored references on load.

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
  optional string component_field           = 24;  // owning component's field, free-form
  //   `ListMetadataReq.component_field` filters on this. A row that declares none
  //   matches no value, so it cannot select the rows that left it unset.
  optional string time_reference            = 25;  // "utc" | "zoneless" | "-07:00" | IANA name
  //   How this series' timestamps were spelled. Absent means unspecified,
  //   which is NOT a claim they were written as UTC.
  optional int64  id                        = 26;  // the catalog association id
  //   The handle a consumer stores in its own model to reference this series
  //   later, and what every read RPC takes. Transmitted rather than derived:
  //   nothing computes it from the attributes, so the serving store is the
  //   only thing that knows it. Every row a server returns carries one.
}
```

## Request / Response Messages

```proto
message ListMetadataReq {
  optional int64          owner_id         = 1;
  optional string         owner_type       = 2;
  optional TimeSeriesType time_series_type = 3;
  optional string         name             = 4;
  optional string         resolution       = 5;   // ISO-8601 duration
  Features                features         = 6;   // subset match
  optional OwnerCategory  owner_category   = 7;
  optional string         interval         = 8;   // ISO-8601 duration
  optional string         component_field  = 9;   // exact, case-sensitive
  optional bool           zoneless         = 10;  // coherence predicate; see below
}
message ListMetadataResp { repeated TimeSeriesMetadata metadata = 1; }

message ListMetadataByIdsReq  { repeated int64 ids = 1; }        // NOT_FOUND if any is stale
message ListMetadataByIdsResp { repeated TimeSeriesMetadata metadata = 1; }

message GetMetadataByIdReq   { int64 id = 1; }                   // NOT_FOUND if stale
message AssociationExistsReq { int64 id = 1; }                   // never NOT_FOUND
message AssociationExistsResp { bool present = 1; }

message ReadByIdReq {
  int64           id              = 1;   // catalog association id, from a ListMetadata row
  optional string start_rfc3339   = 2;   // optional time-axis slice; all-or-nothing with end
  optional string end_rfc3339     = 3;
  optional bool   bounds_zoneless = 4;   // how the client spelled those bounds; see below
}
message ReadByIdResp {
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
  optional string time_reference            = 21;  // how the timestamps were spelled
  string          name                      = 22;  // the series' name
  //   A read names an id, and an id carries no name, so this is the only place
  //   a client can get one without a second call.
}

message ReadByIdsReq {
  repeated int64  ids             = 1;   // results align with these, repeats in place
  optional string start_rfc3339   = 2;
  optional string end_rfc3339     = 3;
  optional bool   bounds_zoneless = 4;
}
message ReadByIdsResp { repeated ReadByIdResp items = 1; }

message GetResolutionsReq  { optional TimeSeriesType time_series_type = 1; }
message GetResolutionsResp { repeated string resolution = 1; }   // ISO-8601 durations

message GetIntervalsReq  { optional TimeSeriesType time_series_type = 1; }
message GetIntervalsResp { repeated string interval = 1; }       // ISO-8601 durations

message GetCountsReq  {}
message GetCountsResp {
  int64 components_with_time_series = 1;
  int64 static_time_series          = 2;
  int64 forecasts                   = 3;
}

message GetForecastParametersReq  {
  optional string resolution = 1;   // ISO-8601 duration filter
  optional string interval   = 2;   // ISO-8601 duration filter
}
message GetForecastParametersResp {
  optional string horizon                   = 1;   // ISO-8601 duration
  optional string interval                  = 2;   // ISO-8601 duration
  optional uint64 count                     = 3;
  optional string resolution                = 4;   // ISO-8601 duration
  optional string initial_timestamp_rfc3339 = 5;
}

// An existence probe stays attribute-addressed: it is answered off the catalog
// indexes without hydrating a row, so posing it through an id lookup would cost
// more than the question. `features` matches the whole set, not a subset.
message HasAnyTimeSeriesReq {
  int64          owner_id       = 1;
  OwnerCategory  owner_category = 2;
  string         name           = 3;
  optional TimeSeriesType time_series_type = 4;
  optional string resolution    = 5;   // ISO-8601 duration
  optional string interval      = 6;   // ISO-8601 duration
  map<string, FeatureValue> features = 7;
}
message HasAnyTimeSeriesResp { bool present = 1; }

message VerifyIntegrityReq  {}
message VerifyIntegrityResp { repeated string errors = 1; }
```

### `ReadByIdReq` Time Slice

`start_rfc3339` and `end_rfc3339` are **all-or-nothing**: supply both to request a time-axis slice,
or neither to fetch the whole series. Setting exactly one is rejected with `InvalidArgument`
(`"start_rfc3339 and end_rfc3339 must be supplied together"`). Each value must parse as RFC 3339; a
malformed timestamp is also `InvalidArgument`.

## Time References

`TimeSeriesMetadata` carries an optional `time_reference` recording how a series' timestamps were
**spelled**: `"utc"`, `"zoneless"`, a fixed offset (`"-07:00"`), or an IANA zone name
(`"America/Denver"`). Absent means _unspecified_, which is not a claim they were written as UTC. It
is descriptive, so it is outside the identity the catalog files a row under. An unparseable value is
a convert error rather than a silent absence: "unspecified" and "a spelling this build cannot read"
must not look alike.

Timestamps stay RFC 3339 UTC on the wire whatever the reference says — the reference is the label,
applied by the client. `ReadByIdReq.bounds_zoneless` and `ReadByIdsReq.bounds_zoneless` carry how
the client spelled its slice bounds, because the wire form is identical either way: a zoneless
client sends the wall clock read as if UTC, exactly as the store holds one. The server refuses a
bound whose spelling the series cannot answer (`InvalidArgument`) rather than coercing it, and
refuses a ranged bulk read whose selection mixes zoneless series with instant-bearing ones.
`ListMetadataReq.zoneless` is the constructive half — `true` selects the wall-clock series, `false`
selects everything that accepts an instant bound, including the rows that recorded no reference.

See [Time references](../explanation/data-model.md#time-references) for the full rules.

## Forecasts Over gRPC

The service is read-only, but its read surface covers dense forecasts. Forecast associations created
through the [Rust core](./rust-api.md#forecasts) or [C ABI](./c-abi.md#forecasts) appear in
`ListMetadata` — `TimeSeriesMetadata` carries `horizon`, `interval` (ISO-8601 durations), `count`,
and (for `Probabilistic`) `percentiles` — and `GetCounts` includes them in `forecasts`.

`ReadById` returns forecast values too. For a `Deterministic`, `DeterministicSingleTimeSeries`
(synthesized into `Deterministic`), `Probabilistic`, or `Scenarios` row it fills the `ReadByIdResp`
array fields (`value_bytes` + `element_type`), the window parameters (`horizon`, `interval`,
`count`), and the `percentiles` (`Probabilistic`) or `scenario_count` (`Scenarios`); the client
reconstructs the matching type. Arrays are dtype-generic on the wire — `value_bytes` is the raw
little-endian buffer and `element_type` says both what the elements mean and, through it, their
physical dtype (`f64`/`f32`/`i64`/…, or a composite kind like `piecewise_linear`), so non-`f64`
arrays survive the round trip without coercion. One caveat:

- **`application_data` is not carried in `ReadByIdResp`.** The opaque package-owned payload is
  returned by `ListMetadata` (on `TimeSeriesMetadata`) but left empty by `ReadById`, so values
  fetched directly by id come back without it. Every other descriptor — `name`, `units`,
  `quantity_kind`, `unit_system`, `component_field`, `time_reference` — is on the response, so a
  read by id returns the same described series a local read does.

## Catalog Revision and Read-Only Opens

The server opens its store **read-only**, which means it cannot upgrade a catalog. A store written
by an older build — one whose catalog is at an older `CATALOG_SCHEMA_REVISION` — is refused on open
with `CatalogMigrationRequired`, surfaced as a gRPC status whose message names the remedy.

**The store must be opened once for writing before the server can serve it.** The CLI command for
exactly that is `infrastore --store <path> upgrade`, which does nothing but the writable open and is
a no-op on a store that is already current. Every _read_ command, `store-info` included, opens the
store read-only and so cannot upgrade it.

`infrastore store-info` reports `catalog_schema_revision` beside `data_format_version` once the
store is readable, which is how to confirm the upgrade landed.

A catalog written by a _newer_ build is `CatalogTooNew` and is refused outright; there is no
downgrade. See
[Upgrade a store in place](../explanation/design-choices.md#upgrade-a-store-in-place-rather-than-bricking-it).

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
use infrastore_core::{ListFilter, OwnerCategory};
use infrastore_server::client::RemoteClient;

let client = RemoteClient::connect("http://127.0.0.1:50051".into()).await?;
let counts = client.get_counts().await?;

// Identify, then act: the rows carry the ids every read takes.
let rows = client
    .list_metadata(Some(42), Some(OwnerCategory::Component), None, None, None, None, None, None, None, None)
    .await?;
let id = rows[0].id.expect("a served row always carries its id");
let data = client.read_by_id(id, None).await?;

// A model holding ids from an earlier session hydrates them in one round trip,
// after sifting the ones that no longer resolve.
let live: Vec<_> = /* ids the model recorded */ vec![id];
let hydrated = client.list_metadata_by_ids(&live).await?;
```

`RemoteClient` methods mirror the RPC table one for one: `connect`, `from_channel`, `list_metadata`,
`list_metadata_by_ids`, `get_metadata_by_id`, `association_exists`, `has_any_time_series`,
`read_by_id`, `read_by_ids`, `get_resolutions`, `get_intervals`, `get_counts`, `counts_by_type`,
`time_series_counts_detailed`, `get_forecast_parameters`, `list_owner_ids`, `static_summary`,
`forecast_summary`, `check_static_consistency`, `verify_integrity`. The id-taking ones accept
`infrastore_core::TimeSeriesId`, the same newtype the local `Store` uses, so an `owner_id` cannot be
passed where a series id belongs. See the [gRPC Server guide](../guides/server.md) for end-to-end
usage and adding an API key to client requests.
