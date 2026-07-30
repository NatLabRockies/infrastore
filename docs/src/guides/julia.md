# Julia Developer Guide

This guide covers building on `InfraStore.jl`, the Julia package that wraps the
[C ABI](../reference/c-abi.md). For exact signatures see the
[Julia API reference](../reference/julia-api.md); to set up the package and the native library, see
[Integrate with Julia](../how-to/integrate-julia.md).

## Load the Package

`InfraStore.jl` resolves the native library from the `INFRASTORE_LIB` environment variable (or the
`InfraStore_jll` package when installed). Build the cdylib and point at it before
`using InfraStore`:

```sh
cargo build -p infrastore-ffi --release
export INFRASTORE_LIB=$PWD/target/release/libinfrastore_ffi.dylib  # .so on Linux
```

```julia
using Dates, InfraStore
```

Exported names include `Store`, `SingleTimeSeries`, `NonSequentialTimeSeries`, the forecast structs
(`Deterministic`, `Probabilistic`, `Scenarios`), `OwnerCategory` (`Component`,
`SupplementalAttribute`), the `add_time_series!` / `get_time_series` / `get_metadata` family, and
`transform_single_time_series!`. The store type is named **`Store`**.

## Open or Create a Store

```julia
# In-memory.
store = Store(in_memory=true)

# On disk: writes system.h5 and system.h5.sqlite.
store = Store(in_memory=false, path="system.h5")

# Reopen read-only.
store = open_store("system.h5"; read_only=true)
```

The store is finalized automatically, but you can release it eagerly with `close!(store)`.

## Add a Series

```julia
# `name` ("load") is a required field on the struct.
ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(100.0:123.0), "load")

key = add_time_series!(
    store,
    42,
    "Generator",
    Component,
    ts;                                   # name comes from ts
    features = Dict("model_year" => 2030),
    units = "MW",
)
```

Notes:

- **`owner_id` is an integer** (`Int64`) — the component identifier, e.g. `42`.
- **`resolution` is a `Period`** such as `Hour(1)` or `Minute(5)`.
- **`features`** is a `Dict` serialized to JSON, so values must be JSON scalars (`Int`, `Float64`,
  `Bool`, `String`). String features are supported and round-trip unchanged.
- Adding a duplicate [key](../explanation/data-model.md#keys) throws `DuplicateTimeSeriesError`.

`add_time_series!` returns a `TimeSeriesKey` holding an opaque handle into the store.

## Read a Series

```julia
got = get_time_series(store, key)
@assert got.data == ts.data
println(got.initial_timestamp, " ", got.resolution)   # resolution comes back as Millisecond
```

To read **many whole series at once** — e.g. loading everything for a plot — `bulk_read` takes a
vector of keys and returns the `SingleTimeSeries` in the same order, reading each packed dataset's
column span once instead of re-reading every chunk per series:

```julia
series = bulk_read(store, keys)   # keys :: Vector{TimeSeriesKey}, all SingleTimeSeries
```

## Attribute-Based Lookups

Beyond key handles, `InfraStore.jl` can resolve a series directly from its attributes — convenient
when a caller keeps its own identifiers (as an InfrastructureSystems.jl-side store does):

```julia
meta = get_metadata(
    store,
    42,
    Component,        # owner_category; the owner is the (owner_id, owner_category) pair
    "load";
    resolution = Hour(1),
    features = Dict("model_year" => 2030),
)
# meta :: TimeSeriesMetadata — the whole record: owner_id/owner_type/owner_category,
#          name, time_series_type, data_hash, initial_timestamp, resolution, length,
#          horizon/interval/count, percentiles, dtype, element_shape, features, units, ext

# Any other stored type: pass it first, exactly as get_time_series does. Omitting
# the type reads a SingleTimeSeries.
scen = get_metadata(Scenarios, store, 42, Component, "wind"; resolution = Hour(1))

values = get_array_by_hash(store, meta.data_hash)     # Vector{Float64}; pass ::Type{T} for other dtypes

# get_time_series itself resolves by attributes too (pass the type as the first argument):
got = get_time_series(SingleTimeSeries, store, 42, Component, "load"; resolution = Hour(1))

present = has_time_series(store, 42, Component, "load"; resolution = Hour(1))
remove_time_series!(store, 42, Component, "load"; resolution = Hour(1))
```

`get_time_series`, `has_time_series`, and `remove_time_series!` all accept either a `TimeSeriesKey`
or `(owner_id, owner_category, name; resolution, features)` attributes — the conventions are
interchangeable for every time series type, static or forecast.

## Forecasts

`InfraStore.jl` exposes `Deterministic`, `Probabilistic`, and `Scenarios` structs that wrap a native
`AbstractArray` in the type's logical shape (the wrapper derives the dtype and dims and serializes
the buffer row-major). Construct one and add it through the generic `add_time_series!`:

```julia
data = zeros(Float64, 24, 7)   # (horizon_count, count)
fc = Deterministic(DateTime(2024, 1, 1), Hour(1), Hour(24), Hour(24), 7, data, "load_fc")
key = add_time_series!(
    store,
    42,
    "Generator",
    Component,
    fc;                         # name comes from fc
    units = "MW",
)

got = get_time_series(Deterministic, store, 42, Component, "load_fc"; resolution = Hour(1))
values = got.data   # Float64 matrix, shape (24, 7)

# Same forecast, read by the key returned from add_time_series! — forecasts and
# static series both support the key-based and attribute-based conventions.
got_by_key = get_time_series(Deterministic, store, key)
```

`Probabilistic(initial_timestamp, resolution, horizon, interval, count, percentiles, data, name)`
carries the percentile vector, and
`Scenarios(initial_timestamp, resolution, horizon, interval, count, data, name)` takes
`scenario_count` from `data`'s leading axis. Every forecast constructor also accepts a `ext=`
keyword. Read the corresponding type back with `get_time_series(Probabilistic, …)` /
`get_time_series(Scenarios, …)`.

If two forecasts of one owner/name/type differ only by `interval` (say day-ahead and intra-day),
pass `interval=` to pin the one you want; without it the read is ambiguous and throws
`InvalidParameterError`:

```julia
got = get_time_series(Deterministic, store, 42, Component, "load_fc";
                      resolution = Hour(1), interval = Hour(6))
```

A `DeterministicSingleTimeSeries` is not added directly — derive one from the stored
`SingleTimeSeries` with `transform_single_time_series!`, which returns the number transformed. It
optionally restricts the transform to one `owner_category` and/or one `resolution`:

```julia
n = transform_single_time_series!(store, Hour(24), Hour(24))
n = transform_single_time_series!(store, Hour(24), Hour(24);
                                  owner_category = Component, resolution = Hour(1))
```

Requesting `Deterministic` also matches a transformed `DeterministicSingleTimeSeries`, so you read a
forecast the same way whether it was added densely or derived (either returns a `Deterministic`,
since a DST has no materialized struct):

```julia
fc = get_time_series(Deterministic, store, 42, Component, "load")
```

A genuine miss throws `NotFoundError`. To check which form you actually have, inspect the
`time_series_type` on the key or metadata — or pass `DeterministicSingleTimeSeries` to select only
the derived ones.

`transform_single_time_series!` returns no keys, so to read a derived forecast by key, enumerate the
owner's keys with `get_time_series_keys(store, owner_id, owner_category)` (the owner is the
`(owner_id, owner_category)` pair) and use `key_info`, whose `time_series_type` is the actual Julia
type — pass it straight to `get_time_series` (a `DeterministicSingleTimeSeries` reads back as a
`Deterministic`):

```julia
for k in get_time_series_keys(store, 42, Component)
    info = key_info(k)
    series = get_time_series(info.time_series_type, store, k)
end
```

To address one series rather than enumerate an owner, `get_time_series_key` resolves attributes to a
key — for any stored type, and validated against the catalog (a miss or an ambiguous match throws):

```julia
k = get_time_series_key(Scenarios, store, 42, Component, "wind"; resolution = Hour(1))
window = get_time_series(Scenarios, store, k)
```

(`list_keys` returns `KeyRow` description structs, not handles; use these two when you need a key to
pass to a reader or `bulk_read`.)

`has_time_series`, `remove_time_series!`, and `copy_time_series!` take the time series type as their
first argument to address anything other than a `SingleTimeSeries` (and take the same `resolution` /
`interval` / `features` keywords). `copy_time_series!` re-points a stored series at another owner
without duplicating data — it writes one association row against the same content-addressed array,
preserving the stored type (a DST stays a DST):

```julia
has_time_series(Scenarios, store, 42, Component, "wind"; resolution = Hour(1))
copy_time_series!(SingleTimeSeries, store, 42, Component, "load", 43, "Generator")
```

Every `time_series_type` filter keyword takes the Julia type as well:

```julia
list_keys(store; time_series_type = Deterministic)
get_resolutions(store; time_series_type = SingleTimeSeries)
```

The low-level `get_metadata` + `get_array_by_hash` path is still available for raw access. See the
[Julia API reference](../reference/julia-api.md#forecasts).

## Per-Timestamp Reads (Simulation Loop)

`get_time_series` hands back a whole series or forecast. Simulations instead walk the timeline and,
at each timestamp, want the value of _every_ series at that instant. For that, build a **reader**
once and drive it in a loop — it pins one resolution and reuses its output buffers, so the loop
allocates almost nothing. `StaticReader` serves `SingleTimeSeries`; `ForecastReader` serves
forecasts. (Full signatures:
[Julia API reference](../reference/julia-api.md#readers-per-timestamp-iteration).)

### Static series

```julia
reader = build_static_reader(store; resolution = Hour(1))
grid = static_grid(reader)                 # StaticGrid: initial_timestamp, resolution, length
for k in 0:(grid.length - 1)
    static_read!(reader, grid.initial_timestamp + grid.resolution * k)
    for (gi, g) in enumerate(static_groups(reader))
        vals = static_values(reader, gi)   # (num_columns, element_dims...); column j ↔ g.keys[j]
    end
end
```

Series are grouped by `(dtype, element_shape)`; each group's `static_values` is one dense array
whose columns line up with the group's `keys`. All matched series must share one grid
(`initial_timestamp` + `length`), validated at build.

### Forecasts

```julia
reader = build_forecast_reader(store, Deterministic; resolution = Hour(1))
tl = forecast_timeline(reader)             # ForecastTimeline: initial_timestamp, resolution, interval, count
for k in 0:(tl.count - 1)
    forecast_read!(reader, tl.initial_timestamp + tl.interval * k)
    for (i, e) in enumerate(forecast_entries(reader))
        window = forecast_values(reader, i)   # shape e.window_shape, for e.key
    end
end
```

A `Deterministic` reader is abstract — it also includes any `DeterministicSingleTimeSeries` (read
into identical windows).

### Shared forecasts are read once

Forecasts that share a backing array (deduplicated identical data, or several
`DeterministicSingleTimeSeries` over one `SingleTimeSeries`) collapse to a single **window slot**.
`forecast_read!` reads each slot from the `.h5` file once per timestamp, so a forecast shared by 10
components costs one read, not ten. `forecast_num_slots(reader)` is the physical read count, and
each `ForecastEntry.slot` says which slot an entry uses — group by `slot` to materialize each unique
window only once:

```julia
forecast_read!(reader, t)
windows = Dict{Int, Any}()
for (i, e) in enumerate(forecast_entries(reader))
    w = get!(() -> forecast_values(reader, i), windows, e.slot)
    # apply w to e.key's owner
end
```

## Store-Wide Operations

```julia
counts = get_counts(store)        # TimeSeriesCounts: components_with_time_series, static_time_series, forecasts
nerr   = verify_integrity(store)  # 0 == arrays intact (catalog not checked)
compact!(store)
```

## Associations

Two catalog tables record relationships between entities the store does not otherwise model, wholly
independently of time series: which supplemental attributes are attached to which components, and
directed parent/child edges between components. Removing a time series never touches either, and
vice versa — see
[Associations Between Entities](../explanation/data-model.md#associations-between-entities).

Filter keywords are all optional and ANDed; passing none matches everything.

```julia
add_supplemental_attribute_association!(
    store, SupplementalAttributeAssociation(42, "Generator", 100, "GeographicInfo"))

# Bulk add is one all-or-nothing transaction.
add_supplemental_attribute_associations!(store, [
    SupplementalAttributeAssociation(43, "Generator", 100, "GeographicInfo"),
    SupplementalAttributeAssociation(43, "Generator", 101, "Outage"),
])

# Queries run in both directions, returning distinct ids in ascending order.
list_supplemental_attribute_ids(store; component_id=43)      # [100, 101]
list_components_with_attributes(store; attribute_id=100)     # [42, 43]
has_supplemental_attribute_association(store; component_id=42, attribute_id=100)  # true

# `*_types` filters take CONCRETE type names, so expand an abstract type yourself —
# `get_all_subtype_names` in InfrastructureSystems.jl is the usual source. An empty
# vector is a deliberate "none of these" and matches nothing.
list_supplemental_attribute_ids(store; component_id=43, attribute_types=["Outage"])  # [101]

count_supplemental_attributes(store)         # 2, distinct attributes
count_components_with_attributes(store)      # 2, distinct components
supplemental_attribute_counts_by_type(store)
# [SupplementalAttributeTypeCount("GeographicInfo", 2), SupplementalAttributeTypeCount("Outage", 1)]
supplemental_attribute_summary(store)
# [SupplementalAttributeSummaryRow("Generator", "GeographicInfo", 2), ...]
```

Identity is the `(component_id, attribute_id)` pair. The type names ride along for filtering and are
not part of it, so re-attaching the same pair under different type names is still a duplicate:

```julia
try
    add_supplemental_attribute_association!(
        store, SupplementalAttributeAssociation(42, "Load", 100, "Outage"))
catch e
    e isa InfraStore.DuplicateAssociationError || rethrow()
    @info e.msg   # attribute 100 is already attached to component 42
end

# Removal returns a count. Matching nothing returns 0 rather than throwing, so assert on
# the count yourself if you expected a hit.
remove_supplemental_attribute_associations!(store; component_id=43)   # 2
```

Parent/child edges work the same way, except that identity is the **ordered** pair — the reverse of
an edge is a different edge — and both endpoints are always components:

```julia
add_parent_child_association!(store, ParentChildAssociation(42, "Generator", 7, "Bus"))
add_parent_child_associations!(store, [ParentChildAssociation(43, "Generator", 7, "Bus")])

list_children(store; parent_id=42)      # [7]
list_parents(store; child_id=7)         # [42, 43]
count_parent_child_associations(store)  # 2

# Renumbering a component rewrites both ends of every edge.
replace_parent_child_component_id!(store, 42, 99)   # 1
list_parents(store; child_id=7)                     # [43, 99]
```

Neither table is reachable over gRPC or the `infrastore` CLI.

## Persist to Disk

```julia
flush!(store)   # sync HDF5 + SQLite; afterwards system.h5 + system.h5.sqlite can be copied
```

Keep the `.h5` and `.h5.sqlite` files together.

## Error Handling

Errors subtype `InfraStore.TimeSeriesException`. The exception types are not exported, so reference
them module-qualified. Catch broadly or narrowly:

```julia
try
    add_time_series!(store, 42, "Generator", Component, ts)
catch e
    if e isa InfraStore.DuplicateTimeSeriesError
        @warn "already present"
    else
        rethrow()
    end
end
```

The available types are `InfraStore.NotFoundError`, `InfraStore.DuplicateTimeSeriesError`,
`InfraStore.InvalidParameterError`, `InfraStore.IntegrityError`, `InfraStore.ReadOnlyStoreError`,
`InfraStore.IncompatibleFormatError` (the on-disk store was written by an incompatible data format
version), and `InfraStore.GenericError` (which carries the raw FFI status `code`).

## InfrastructureSystems.jl Integration Notes

The model is designed to back an InfrastructureSystems.jl time-series store:

- Owners are integer component identifiers (`Int64`), matching InfrastructureSystems.jl
  component/attribute IDs.
- `OwnerCategory` distinguishes `Component` from `SupplementalAttribute` and is part of the owner
  identity: the owner is the `(owner_id, owner_category)` pair, so a component and a supplemental
  attribute may share a numeric id and stay distinct. Owner-scoped calls take the category alongside
  the id.
- The attribute-based accessors (`get_metadata`, `has_time_series`, `remove_time_series!`) plus
  `get_array_by_hash` let an InfrastructureSystems.jl-side store keep its own key objects and reach
  the array layer without holding a `TimeSeriesKey`.
- For the simulation read pattern — iterate every component's value at each timestamp, reading a
  forecast shared across components only once — use the readers
  ([Per-Timestamp Reads](#per-timestamp-reads-simulation-loop)). The `ForecastEntry.slot` /
  `forecast_num_slots` surface lets the wrapping store dedup its own per-component work, mirroring
  the store's one-read-per-shared-array behavior.

See [Language Bindings](../explanation/bindings.md#infrastructuresystemsjl-integration) for how this
maps onto the FFI.

## Diagnostics and tracing

The store emits structured tracing spans for every significant operation. To see them, initialize a
subscriber before your first store call.

**Via environment variable** — set `RUST_LOG` before loading the package. The module's `__init__`
hook calls `init_logging("")` automatically, which reads `RUST_LOG` if set:

```sh
# shell
export RUST_LOG=infrastore_core=debug
julia --project=. myscript.jl
```

**Programmatically** — call `init_logging` with a filter directive string:

```julia
using InfraStore

init_logging("infrastore_core=debug")

store = Store(in_memory=true)
add_time_series!(store, ...)   # spans appear on stderr
```

`init_logging` is a no-op if a subscriber is already registered (including the automatic one from
`RUST_LOG`). The filter syntax is the same as `RUST_LOG`: comma-separated `target=level` pairs, or a
bare level such as `"debug"` to match everything. Useful targets:

| Target            | What it covers                                             |
| ----------------- | ---------------------------------------------------------- |
| `infrastore_core` | All store operations — `add`, `get`, `remove` and HDF5 I/O |
