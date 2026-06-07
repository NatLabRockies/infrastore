# Data Model

The data model mirrors the time-series concepts in
[InfrastructureSystems.jl](https://github.com/NREL-Sienna/InfrastructureSystems.jl) (IS.jl): a
**component** (or supplemental attribute) owns one or more named time series, and each time series
may exist in several variants distinguished by **features**.

## Owners

Every time series belongs to an owner, identified by three fields:

| Field            | Type            | Meaning                                                           |
| ---------------- | --------------- | ----------------------------------------------------------------- |
| `owner_uuid`     | string          | Stable identity of the owning object (an IS.jl UUID, for example) |
| `owner_type`     | string          | The owner's concrete type, e.g. `"Generator"`                     |
| `owner_category` | `OwnerCategory` | `Component` or `SupplementalAttribute`                            |

`owner_uuid` is a free-form string, so it interoperates with IS.jl UUIDs, integer IDs rendered as
text, or any other stable identifier scheme. Only `owner_uuid` participates in the association's
uniqueness constraint; `owner_type` and `owner_category` are descriptive.

## Time-Series Types

The data model defines six time-series types, all present in the `TimeSeriesType` enum and the
metadata schema. Both static series types are implemented across every interface; the four forecast
types are implemented in the Rust core and the C ABI:

| Type                            | Where implemented | Description                                         |
| ------------------------------- | ----------------- | --------------------------------------------------- |
| `SingleTimeSeries`              | All interfaces    | One array sampled at a fixed resolution             |
| `NonSequentialTimeSeries`       | All interfaces    | Values at explicit, irregular timestamps            |
| `Deterministic`                 | Core + C ABI      | Forecast: a `(horizon × count)` window matrix       |
| `DeterministicSingleTimeSeries` | Core + C ABI      | Forecast view over an underlying `SingleTimeSeries` |
| `Probabilistic`                 | Core + C ABI      | Forecast with percentile bands                      |
| `Scenarios`                     | Core + C ABI      | Forecast with discrete scenarios                    |

The Python and Julia bindings and the gRPC server currently surface the forecast types only as
`TimeSeriesType` values and aggregate counts — creating and reading forecast _values_ is a
Rust-core and C-ABI capability today. See [Forecasts](#forecasts) below.

### `NonSequentialTimeSeries`

A `NonSequentialTimeSeries` pairs each value with an explicit UTC timestamp. Timestamps must be
strictly increasing and their count must match the data length. Its values are stored as a
standalone NetCDF array; timestamps are stored with the association metadata.

### `SingleTimeSeries`

A `SingleTimeSeries` is an `initial_timestamp`, a `resolution` (a fixed step), and an array of
values:

```text
value
  ^
  |        *
  |     *     *
  |  *           *
  +--+--+--+--+--+--> time
   t0 t0+r  ...   t0+(n-1)r
```

The timestamps are implied — sample `i` is at `initial_timestamp + i * resolution` — so only the
values are stored. v0 stores **1-D values only** (one scalar per timestep). The in-memory type
(`ArrayD<f64>`) can carry trailing axes for per-step vectors such as cost-curve coefficients, but
the NetCDF backend rejects anything other than rank-1 with an `InvalidParameter` error. That
dimension is reserved for a later milestone.

### Forecasts

The four forecast types share one storage strategy: the forecast values are **flattened to a 1-D,
column-major array** and stored content-addressed like any other array, while the windowing
parameters live in metadata. A forecast association records `horizon` (the span each window covers),
`interval` (the spacing between successive window start times), `count` (the number of windows), and
— for `Probabilistic` — a `percentiles` vector.

| Type                            | Conventional array shape                   | Extra metadata |
| ------------------------------- | ------------------------------------------ | -------------- |
| `Deterministic`                 | `(horizon_count, count)`                   | —              |
| `DeterministicSingleTimeSeries` | the backing `SingleTimeSeries` array       | —              |
| `Probabilistic`                 | `(percentile_count, horizon_count, count)` | `percentiles`  |
| `Scenarios`                     | `(scenario_count, horizon_count, count)`   | —              |

The store does not interpret the layout — the caller flattens on write and reshapes on read, and a
`DeterministicSingleTimeSeries` deduplicates against the static series it forecasts. Forecasts are
read through the low-level metadata + array path rather than `get_time_series`; see the
[Rust API](../reference/rust-api.md#forecasts) and [C ABI](../reference/c-abi.md#forecasts).

## Features

Two series can share an owner and a name yet differ — for example a load profile for model year 2030
versus 2050. **Features** disambiguate them. A feature map is a set of typed key/value pairs:

```python
features = {"model_year": 2030, "scenario": "high", "calibrated": True}
```

Feature values are one of four kinds: `int`, `float`, `bool`, or `str`. Internally the map is sorted
by key (a `BTreeMap`), which gives a stable order for hashing and for the uniqueness constraint.

## Keys

A **`TimeSeriesKey`** is the logical handle that re-finds a series. It is exactly the tuple that
must be unique:

```text
TimeSeriesKey = (owner_uuid, time_series_type, name, resolution, features)
```

`add_time_series` returns a key; `get_time_series`, `has_time_series`, and `remove_time_series` take
one. Two series with the same key cannot coexist — attempting to add a duplicate raises
`DuplicateTimeSeries`. Change any element of the tuple (a different `name`, a different `model_year`
feature, a different `resolution`) and you have a distinct series.

```mermaid
flowchart LR
    OWNER["Owner<br/>uuid=42, type=Generator"]
    OWNER --> K1["name=load<br/>year=2030"]
    OWNER --> K2["name=load<br/>year=2050"]
    OWNER --> K3["name=max_active_power"]
    K1 --> A1[("array A")]
    K2 --> A2[("array B")]
    K3 --> A1

    style OWNER fill:#4a9eff,color:#fff
    style K1 fill:#17a2b8,color:#fff
    style K2 fill:#17a2b8,color:#fff
    style K3 fill:#17a2b8,color:#fff
    style A1 fill:#28a745,color:#fff
    style A2 fill:#28a745,color:#fff
```

Note that two different keys (`K1` and `K3` above) can point at the _same_ underlying array. The key
is a metadata concept; the array is shared by [content addressing](./content-addressing.md).

## Optional Descriptors

Each association can also carry:

- **`units`** — a free-form label such as `"MW"`. No dimensional analysis is performed.
- **`scaling_factor_multiplier`** — an opaque expression string such as `"x * 1.05"`. v0 stores it
  verbatim and never evaluates it.

These are recorded in metadata and returned on read, but they do not affect identity or storage.
