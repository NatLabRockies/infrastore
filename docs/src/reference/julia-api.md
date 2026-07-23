# Julia API

The Julia package is **`Castore.jl`** (module `Castore`); it wraps the [C ABI](./c-abi.md) cdylib.
The library is resolved from the `CASTORE_LIB` environment variable (development builds), or from
the `Castore_jll` binary package when installed.

```julia
using Castore
```

Exported names: `Store`, `SingleTimeSeries`, `NonSequentialTimeSeries`, `Deterministic`,
`DeterministicSingleTimeSeries`, `AbstractDeterministic`, `Probabilistic`, `Scenarios`,
`TimeSeriesKey`, `OwnerCategory`, `Component`, `SupplementalAttribute`, `add_time_series!`,
`AddBatch`, `add_time_series_bulk!`, `get_time_series`, `bulk_read`, `get_time_series_keys`,
`key_info`, `list_keys`, `list_array_groups`, `remove_time_series!`, `has_time_series`,
`get_counts`, `counts_by_type`, `num_distinct_arrays`, `time_series_counts`, `list_owner_ids`,
`static_summary`, `forecast_summary`, `SupplementalAttributeAssociation`,
`add_supplemental_attribute_association!`, `add_supplemental_attribute_associations!`,
`has_supplemental_attribute_association`, `list_supplemental_attribute_associations`,
`list_supplemental_attribute_ids`, `list_components_with_attributes`,
`remove_supplemental_attribute_associations!`, `replace_supplemental_attribute_component_id!`,
`count_supplemental_attribute_associations`, `count_supplemental_attributes`,
`count_components_with_attributes`, `supplemental_attribute_counts_by_type`,
`supplemental_attribute_summary`, `ParentChildAssociation`, `add_parent_child_association!`,
`add_parent_child_associations!`, `has_parent_child_association`, `list_parent_child_associations`,
`list_children`, `list_parents`, `remove_parent_child_associations!`,
`replace_parent_child_component_id!`, `count_parent_child_associations`, `get_forecast_parameters`,
`check_static_consistency`, `get_resolutions`, `get_compression`, `verify_integrity`, `compact!`,
`get_metadata`, `get_forecast_metadata`, `get_array_by_hash`, `count_array_references`,
`open_store`, `flush!`, `clear!`, `replace_owner!`, `transform_single_time_series!`, `has_typed`,
`remove_typed!`, `copy_time_series!`, `close!`, `persist!`, `StaticReader`, `build_static_reader`,
`static_grid`, `static_groups`, `static_read!`, `static_values`, `ForecastReader`,
`build_forecast_reader`, `forecast_timeline`, `forecast_entries`, `forecast_num_slots`,
`forecast_read!`, `forecast_values`, `init_logging`.

## Constructors

```julia
Store(; in_memory::Bool=true, path::Union{Nothing,AbstractString}=nothing,
        compression::Union{Symbol,AbstractString}=:deflate,
        compression_level::Integer=3, shuffle::Bool=true) -> Store
open_store(path::AbstractString; read_only::Bool=false) -> Store
```

- `Store()` — in-memory store.
- `Store(in_memory=false, path="system.nc")` — persists to `system.nc` plus `system.nc.sqlite`.
- `compression=:none` stores arrays uncompressed; `:deflate` (default) applies DEFLATE at
  `compression_level` (0–9) with optional byte `shuffle`. The policy is persisted with the store and
  reused on later appends; it is ignored for in-memory stores. An unknown `compression` throws
  `ArgumentError`.
- `open_store(path; read_only=true)` — opens an existing on-disk pair.

The store registers a finalizer; close it eagerly with `close!(store)`.

## Types

Each struct carries the association `name` (required) and an optional `ext`. Every constructor takes
`name` as the positional after `data` and `ext=` as a keyword — e.g.
`SingleTimeSeries(initial, resolution, data, name; ext=nothing)`.

Every data-carrying struct is parameterized `{T,N}` on the element type and dimensionality of its
value array; `{T,N}` is inferred from `data` by the constructor (an `AbstractArray` argument — a
view or a range — is normalized to a concrete `Array{T,N}`).

```julia
struct SingleTimeSeries{T,N}
    initial_timestamp :: DateTime
    resolution        :: Period          # e.g. Hour(1), Millisecond(500)
    data              :: Array{T,N}      # any element type; dim 1 = time
    name              :: String          # required association name
    ext      :: Union{Nothing,String}
end
SingleTimeSeries(initial_timestamp, resolution, data, name; ext=nothing)

struct NonSequentialTimeSeries{T,N}
    timestamps   :: Vector{DateTime}     # strictly increasing; one per row of dim 1
    data         :: Array{T,N}
    name         :: String
    ext :: Union{Nothing,String}
end
NonSequentialTimeSeries(timestamps, data, name; ext=nothing)

struct Deterministic{T,N} <: AbstractDeterministic
    initial_timestamp :: DateTime
    resolution        :: Period
    horizon           :: Period
    interval          :: Period
    count             :: Int
    data              :: Array{T,N}      # (H, count, element_dims...)
    name              :: String
    ext      :: Union{Nothing,String}
end
Deterministic(initial_timestamp, resolution, horizon, interval, count, data, name;
              ext=nothing)

struct Probabilistic{T,N}
    initial_timestamp :: DateTime
    resolution        :: Period
    horizon           :: Period
    interval          :: Period
    count             :: Int
    percentiles       :: Vector{Float64}
    data              :: Array{T,N}      # (num_percentiles, H, count, element_dims...)
    name              :: String
    ext      :: Union{Nothing,String}
end
Probabilistic(initial_timestamp, resolution, horizon, interval, count, percentiles, data, name;
              ext=nothing)

struct Scenarios{T,N}
    initial_timestamp :: DateTime
    resolution        :: Period
    horizon           :: Period
    interval          :: Period
    count             :: Int
    scenario_count    :: Int             # set from size(data, 1) by the constructor
    data              :: Array{T,N}      # (scenario_count, H, count, element_dims...)
    name              :: String
    ext      :: Union{Nothing,String}
end
Scenarios(initial_timestamp, resolution, horizon, interval, count, data, name;
          ext=nothing)         # note: scenario_count is NOT a constructor argument

# Marker type; never constructed and with no materialized struct. Derived via
# transform_single_time_series! and read back as a Deterministic. Surfaces as a
# key's time_series_type.
abstract type DeterministicSingleTimeSeries <: AbstractDeterministic end

# Request-only supertype of Deterministic and DeterministicSingleTimeSeries.
# Pass it as the requested type to get_time_series to read whichever of the two
# is stored (see Reading forecast values). It is NOT a valid build_forecast_reader
# type — that call takes a concrete forecast type.
abstract type AbstractDeterministic end

mutable struct Store
    handle :: Ptr{Cvoid}
end

mutable struct TimeSeriesKey
    handle :: Ptr{Cvoid}                 # opaque; finalized automatically
end

@enum OwnerCategory begin
    Component             = 0
    SupplementalAttribute = 1
end
```

`ext` is an opaque, package-owned payload (typically JSON) the binding can use to reconstruct a
domain object on read; the store stores it verbatim and never interprets it. `add_time_series!`
reads `name` off the object (it is not a call argument), so the same array can be stored under
different names; its `ext=` keyword defaults to the object's `ext`. `data` keeps its Julia element
type: the binding maps `T` to a stored dtype (`Float64`, `Float32`, `Int64`, `Int32`, `UInt64`,
`Bool`) and converts to row-major bytes on the way down.

## Static Series

```julia
add_time_series!(
    store::Store, owner_id, owner_type, owner_category::OwnerCategory,
    ts::SingleTimeSeries;
    features::AbstractDict = Dict(), units = nothing,
    ext = ts.ext,
) -> TimeSeriesKey

add_time_series!(
    store::Store, owner_id, owner_type, owner_category::OwnerCategory,
    ts::NonSequentialTimeSeries;
    features = Dict(), units = nothing,
    ext = ts.ext,
) -> TimeSeriesKey

get_time_series(store::Store, key::TimeSeriesKey; time_range=nothing) -> SingleTimeSeries
get_time_series(SingleTimeSeries, store::Store, key::TimeSeriesKey;
               time_range=nothing) -> SingleTimeSeries
get_time_series(NonSequentialTimeSeries, store::Store, key::TimeSeriesKey;
               time_range=nothing) -> NonSequentialTimeSeries

get_time_series(SingleTimeSeries, store, owner_id, owner_category, name;
               resolution=nothing, features=Dict(), time_range=nothing) -> SingleTimeSeries
get_time_series(NonSequentialTimeSeries, store, owner_id, owner_category, name;
               resolution=nothing, features=Dict(), time_range=nothing) -> NonSequentialTimeSeries
```

`time_range = (start::DateTime, stop::DateTime)` (exclusive end) slices the read on every form.

`owner_id` is an integer identifier (`Int64`) and `owner_category` (`Component` /
`SupplementalAttribute`) completes the owner identity — the owner is the pair
`(owner_id, owner_category)`. `features` is serialized to JSON and must contain only JSON-scalar
values (`Int`, `Float64`, `Bool`, `String`). Pass the type as the first argument to
`get_time_series` to read a non-sequential series back.

`get_time_series` supports two unified calling conventions for **every** type: pass the
`TimeSeriesKey` returned by `add_time_series!` (key-based), or pass `owner_id, owner_category, name`
plus optional `resolution` / `features` keywords (attribute-based, the same addressing used by
`get_metadata` / `has_time_series` / `remove_time_series!`). Both forms return the same struct. The
bare `get_time_series(store, key)` remains a convenience alias for `SingleTimeSeries`.

To read every series' value at one timestamp in a loop (the simulation pattern), use a
[`StaticReader`](#staticreader) rather than calling `get_time_series` per series.

### Bulk reads

```julia
bulk_read(store::Store, keys::AbstractVector{TimeSeriesKey};
          time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing) -> Vector{SingleTimeSeries}
# time_range slices every series to that window (default: each series in full)
```

Reads many whole `SingleTimeSeries` in one call, returning one per key **in the same order**. Each
packed dataset is read and decompressed once instead of per series, so this is the efficient way to
load many complete series (exploration, plotting). Every key must identify a `SingleTimeSeries`; an
empty key vector returns an empty vector without touching the store.

```julia
series = bulk_read(store, keys)   # keys :: Vector{TimeSeriesKey}
```

## Bulk Adds

```julia
batch = AddBatch()
add_time_series!(batch, owner_id, owner_type, owner_category, ts; ...)  # any series type
add_time_series_bulk!(store::Store, batch::AddBatch) -> Vector{TimeSeriesKey}
```

`AddBatch` accepts the same `add_time_series!` methods as `Store` (every series and forecast type)
but only accumulates the requests; `add_time_series_bulk!` commits the whole batch in **one**
metadata transaction, which is much faster than per-item adds when ingesting many series. The submit
is all-or-nothing: on error nothing is committed. The batch is drained by the call in either case
and may be reused. `length(batch)` returns the number of pending requests.

### Attribute-based lookups

```julia
get_metadata(store, owner_id, owner_category::OwnerCategory, name;
             resolution=nothing, features=Dict()) -> NamedTuple
has_time_series(store, owner_id, owner_category::OwnerCategory, name;
                resolution=nothing, features=Dict()) -> Bool
remove_time_series!(store, owner_id, owner_category::OwnerCategory, name;
                    resolution=nothing, features=Dict()) -> Nothing
```

`owner_category` (`Component` / `SupplementalAttribute`) is required: the owner identity is the pair
`(owner_id, owner_category)`, so a component and a supplemental attribute may share a numeric
`owner_id` and remain distinct. `get_metadata` returns
`(initial_timestamp, resolution, length, data_hash, dtype, ext, units, element_shape,
features)`,
where `data_hash` is the 32-byte content hash, `element_shape` is the per-timestep shape tuple
(empty for scalar elements), and `features` is the feature dictionary. It throws `NotFoundError` if
absent. `get_forecast_metadata` and `get_probabilistic_metadata` carry the same
`element_shape`/`features` fields (there, `element_shape` is the stored array's trailing dims after
its first axis).

```julia
get_array_by_hash(store, data_hash::Vector{UInt8}, ::Type{T}=Float64) -> Vector{T}
```

Fetches the flattened array for a 32-byte content hash, decoded as element type `T`. Combine with
`get_metadata` (for the dtype and shape) to read values without holding a `TimeSeriesKey`.

### Key-based variants

```julia
has_time_series(store, key::TimeSeriesKey) -> Bool
remove_time_series!(store, key::TimeSeriesKey) -> Nothing
remove_time_series!(store, keys::Vector{TimeSeriesKey}) -> Int
```

The vector form removes every key in one all-or-nothing transaction and returns the count; a single
missing key aborts the whole batch.

### Enumerating keys

```julia
get_time_series_keys(store, owner_id, owner_category::OwnerCategory) -> Vector{TimeSeriesKey}
key_info(key::TimeSeriesKey) -> NamedTuple
# (owner_id, owner_category, name, time_series_type, resolution, features)
```

`get_time_series_keys` returns one key per stored association for the owner identified by the
`(owner_id, owner_category)` pair, including `DeterministicSingleTimeSeries` rows derived by
`transform_single_time_series!` — the way to read a transform-derived forecast by key (it returns no
key of its own). The keys are opaque; `key_info` inspects one. `time_series_type` is the **actual
Julia type** (`SingleTimeSeries`, `NonSequentialTimeSeries`, `Deterministic`,
`DeterministicSingleTimeSeries`, `Probabilistic`, or `Scenarios`), as in InfrastructureSystems.jl —
pass it straight to `get_time_series`. `features` is a `Dict` (empty when none) that round-trips the
JSON-scalar feature values. For example:

```julia
for k in get_time_series_keys(store, 12345, Component)
    info = key_info(k)
    series = get_time_series(info.time_series_type, store, k)
end
```

`key_info` also returns `owner_category` (the `Component` / `SupplementalAttribute` half of the
owner identity) alongside `owner_id`.

Reading a `DeterministicSingleTimeSeries` (by key or attributes) returns a `Deterministic`, since
the type has no materialized form.

## Forecasts

Dense forecasts are constructed as `Deterministic`, `Probabilistic`, or `Scenarios` structs (see
[Types](#types)) and added through the generic `add_time_series!`. Each struct wraps a native
`AbstractArray` of any element type and dimensionality — the binding derives the stored dtype and
dims and converts to row-major bytes, just like the static `add_time_series!` (see the
[data model](../explanation/data-model.md#forecasts) for the conventional shapes).

The forecast `name` comes from the struct, e.g.
`Deterministic(initial, resolution, horizon, interval, count, data, name)`.

```julia
add_time_series!(
    store, owner_id, owner_type, owner_category::OwnerCategory,
    ts::Deterministic;
    features=Dict(), units=nothing, ext=nothing,
) -> TimeSeriesKey

add_time_series!(
    store, owner_id, owner_type, owner_category::OwnerCategory,
    ts::Probabilistic;
    features=Dict(), units=nothing, ext=nothing,
) -> TimeSeriesKey

add_time_series!(
    store, owner_id, owner_type, owner_category::OwnerCategory,
    ts::Scenarios;
    features=Dict(), units=nothing, ext=nothing,
) -> TimeSeriesKey
```

A `DeterministicSingleTimeSeries` is not added directly. Derive one from every stored
`SingleTimeSeries` (sharing the backing array) with:

```julia
transform_single_time_series!(store, horizon::Period, interval::Period;
                              owner_category::Union{Nothing,OwnerCategory}=nothing,
                              resolution::Union{Nothing,Period}=nothing) -> Int  # number transformed
```

`count` is derived from each series' length. `owner_category` restricts the transform to one owner
category (both are transformed when it is `nothing`); `resolution` restricts it to the
`SingleTimeSeries` at that resolution.

`has_typed`, `remove_typed!`, and `copy_time_series!` address a series by its `ts_type` integer code
(`0 = SingleTimeSeries`, `1 = NonSequentialTimeSeries`, `2 = Deterministic`,
`3 = DeterministicSingleTimeSeries`, `4 = Probabilistic`, `5 = Scenarios`):

```julia
has_typed(store, owner_id, owner_category, name, ts_type::Integer;
          resolution=nothing, interval=nothing, features=Dict()) -> Bool
remove_typed!(store, owner_id, owner_category, name, ts_type::Integer;
              resolution=nothing, interval=nothing, features=Dict())
```

`interval` (a `Period`) pins the forecast interval — the only way to disambiguate two forecasts of
one owner/name/type that differ solely by interval (e.g. day-ahead vs intra-day). Without it such a
lookup is ambiguous and errors.

### Copying an association

```julia
copy_time_series!(store, owner_id, owner_category, name, ts_type::Integer,
                  dst_owner_id, dst_owner_type::AbstractString;
                  new_name=nothing, resolution=nothing, interval=nothing,
                  features=Dict()) -> Nothing
```

Copies the time series identified by the source attributes onto `dst_owner_id` (of Julia/domain type
`dst_owner_type`), optionally renaming it to `new_name`. Arrays are content-addressed, so this
writes only a new association row against the same underlying array: no data is duplicated and the
stored time series type is preserved — a `DeterministicSingleTimeSeries` stays a DST, whereas a
read-then-write copy through `get_time_series` / `add_time_series!` would materialize it into a
dense `Deterministic`. The copy keeps the source's `owner_category`. Throws if the destination
already holds a matching series.

```julia
copy_time_series!(store, 42, Component, "load", 0, 43, "Generator")   # SingleTimeSeries → owner 43
```

### Reading forecast values

The type-dispatched `get_time_series(Type, …)` functions return the corresponding struct, whose
`data` field is a decoded N-dimensional Julia array (reshaped to the type's logical shape, with
native Julia indexing). Pass `time_range = (start::DateTime, end::DateTime)` (exclusive end) to
select a window sub-range.

Like the static readers, forecasts support both calling conventions: attribute-based
(`owner_id, owner_category, name` plus optional `resolution` / `interval` / `features`) or key-based
(the `TimeSeriesKey` returned by `add_time_series!`).

```julia
get_time_series(Deterministic, store, owner_id, owner_category, name;
                resolution=nothing, interval=nothing, features=Dict(),
                time_range=nothing) -> Deterministic
get_time_series(Deterministic, store, key::TimeSeriesKey; time_range=nothing) -> Deterministic
                # data shape: (H, count, element_dims...)

get_time_series(DeterministicSingleTimeSeries, store, owner_id, owner_category, name;
                resolution=nothing, interval=nothing, features=Dict(),
                time_range=nothing) -> Deterministic
get_time_series(DeterministicSingleTimeSeries, store, key::TimeSeriesKey;
                time_range=nothing) -> Deterministic

get_time_series(AbstractDeterministic, store, owner_id, owner_category, name;
                resolution=nothing, interval=nothing, features=Dict(),
                time_range=nothing) -> Deterministic

get_time_series(Probabilistic, store, owner_id, owner_category, name;
                resolution=nothing, interval=nothing, features=Dict(),
                time_range=nothing) -> Probabilistic
get_time_series(Probabilistic, store, key::TimeSeriesKey; time_range=nothing) -> Probabilistic
                # data shape: (num_percentiles, H, count, element_dims...)

get_time_series(Scenarios, store, owner_id, owner_category, name;
                resolution=nothing, interval=nothing, features=Dict(),
                time_range=nothing) -> Scenarios
get_time_series(Scenarios, store, key::TimeSeriesKey; time_range=nothing) -> Scenarios
                # data shape: (scenario_count, H, count, element_dims...)
```

`interval` (a `Period`) pins the forecast interval; it is the only way to address one of two
forecasts under the same owner/name/type that differ solely by interval. A read that leaves it
`nothing` when two such forecasts exist raises `InvalidParameterError`.

#### Concrete types vs. the `AbstractDeterministic` family

The concrete request types match **only themselves**:

- `get_time_series(Deterministic, …)` matches a directly-stored `Deterministic` and does **not**
  match a `DeterministicSingleTimeSeries` — asking for `Deterministic` when only a transformed DST
  exists raises `NotFoundError`. There is no fallback.
- `get_time_series(DeterministicSingleTimeSeries, …)` matches only a transformed DST. Since the type
  has no materialized struct, it returns a `Deterministic`.

To read whichever of the two is stored under an identity, request the family type
`AbstractDeterministic` (attribute-based only — a key already names its exact stored type):

```julia
transform_single_time_series!(store, Hour(4), Hour(2))
fc = get_time_series(AbstractDeterministic, store, 400, Component, "dst")   # a Deterministic
```

The Rust core resolves the family in a single call — there is no guess-and-retry. A genuine miss
raises `NotFoundError`, and an identity matching **both** a `Deterministic` and a
`DeterministicSingleTimeSeries` is ambiguous and errors (request a concrete type instead).

The **key-based** readers carry the exact stored type in the key, so the question does not arise: a
`DeterministicSingleTimeSeries` key reads back as a `Deterministic`.

Alternatively, use `get_metadata` to obtain the `data_hash`, then `get_array_by_hash` for the raw
flattened array.

For the per-timestamp simulation access pattern (walk the timeline, read every series at each
instant) prefer a reader — see [Readers](#readers-per-timestamp-iteration) below.

## Readers (per-timestamp iteration)

`get_time_series` returns a whole series or forecast struct. For the simulation access pattern —
_walk every timestamp and, at each, read the value of every series_ — use a **reader** instead. A
reader is built once over a filter, pins one resolution, and reuses output buffers that each read
overwrites in place, so a tight loop allocates almost nothing. There are two: `StaticReader` for
`SingleTimeSeries`, and `ForecastReader` for forecasts. Both follow the same lifecycle: build →
inspect the layout once → `*_read!(t)` in a loop → pull values per group/entry.

### StaticReader

Reads the value of every matching `SingleTimeSeries` at one timestamp. Results are **columnar**:
series are partitioned into `(dtype, element_shape)` groups, and each group's values come back as
one dense `(num_columns, element_dims...)` array.

```julia
build_static_reader(store; resolution::Period, owner_id=nothing,
                    owner_category=nothing, name=nothing, features=Dict()) -> StaticReader

static_grid(reader)   -> NamedTuple  # (initial_timestamp::DateTime, resolution::Period, length::Int)
static_groups(reader) -> Vector{StaticGroup}  # each: .dtype, .element_shape, .keys
static_read!(reader, t::DateTime) -> reader   # fills buffers; errors if t is off the grid
static_values(reader, group_index::Integer) -> Array
       # (num_columns, element_dims...); column j is static_groups(reader)[group_index].keys[j]
```

All matched series must share one grid (`initial_timestamp` + `length`); the build validates this
and errors on divergence, so there is no presence mask — every column has a value at every valid
timestamp.

```julia
reader = build_static_reader(store; resolution = Hour(1))
grid = static_grid(reader)
for k in 0:(grid.length - 1)
    static_read!(reader, grid.initial_timestamp + grid.resolution * k)
    for (gi, g) in enumerate(static_groups(reader))
        vals = static_values(reader, gi)   # column j ↔ g.keys[j]
    end
end
```

### ForecastReader

Reads the forecast _window_ at one timestamp for every matching forecast of one type. The build
filter must name a forecast type and pin a resolution; a `Deterministic` reader is abstract and also
includes `DeterministicSingleTimeSeries` (read into identical `[H, *E]` windows). All matched
forecasts must share one window timeline (`initial_timestamp` + `interval` + `count`).

`time_series_type` must be one of the four concrete forecast types — `Deterministic`,
`DeterministicSingleTimeSeries`, `Probabilistic`, or `Scenarios`. Any other type (including
`AbstractDeterministic`, which is a `get_time_series` request type only) raises
`InvalidParameterError`; a `Deterministic` reader already covers the family.

```julia
build_forecast_reader(store, time_series_type::Type; resolution::Period,
                      owner_id=nothing, owner_category=nothing, name=nothing,
                      features=Dict()) -> ForecastReader

forecast_timeline(reader)  -> NamedTuple
       # (initial_timestamp::DateTime, resolution::Period, interval::Period, count::Int)
forecast_entries(reader)   -> Vector{ForecastEntry}  # each: .dtype, .window_shape, .key, .slot
forecast_num_slots(reader) -> Int                    # physical reads per timestamp (see below)
forecast_read!(reader, t::DateTime) -> reader        # fills buffers; errors if t is off the timeline
forecast_values(reader, entry_index::Integer) -> Array  # window of size .window_shape
```

Valid read timestamps are `initial_timestamp + k·interval` for `k in 0:count-1` (each names the
window forecast _from_ that instant). A window's shape is `[H, *E]` for `Deterministic` /
`DeterministicSingleTimeSeries`, `[num_percentiles, H, *E]` for `Probabilistic`, and
`[scenario_count, H, *E]` for `Scenarios`.

```julia
reader = build_forecast_reader(store, Deterministic; resolution = Hour(1))
tl = forecast_timeline(reader)
for k in 0:(tl.count - 1)
    forecast_read!(reader, tl.initial_timestamp + tl.interval * k)
    for (i, e) in enumerate(forecast_entries(reader))
        window = forecast_values(reader, i)   # shape e.window_shape, for e.key's owner
    end
end
```

#### Window-read deduplication

Forecasts that reference the **same backing array and read plan** — deduplicated identical data, or
several `DeterministicSingleTimeSeries` over one `SingleTimeSeries` — collapse to a single _window
slot_. `forecast_read!` performs one backend (`.nc`) read per slot, not per entry, so a forecast
shared by N owners is read once per timestamp. `forecast_num_slots(reader)` is that physical read
count (`≤ length(forecast_entries(reader))`), and every `ForecastEntry.slot` (0-based) identifies
the slot backing that entry; entries that share data report the same `slot`. Group entries by `slot`
to also materialize each unique window only once on the Julia side:

```julia
forecast_read!(reader, t)
windows = Dict{Int, Any}()
for (i, e) in enumerate(forecast_entries(reader))
    window = get!(() -> forecast_values(reader, i), windows, e.slot)   # materialize once per slot
    # apply `window` to e.key's owner
end
```

## Store-Wide Operations

```julia
get_counts(store) -> NamedTuple   # (components_with_time_series, static_time_series, forecasts)
counts_by_type(store) -> Vector{NamedTuple}   # (time_series_type, count) per stored type
num_distinct_arrays(store) -> Int   # distinct content hashes; shared arrays count once
time_series_counts(store) -> NamedTuple   # distinct owners per category + distinct arrays per kind
list_owner_ids(store, owner_category; time_series_type=nothing, resolution=nothing) -> Vector{Int}
list_array_groups(store; owner_id=nothing, owner_category=nothing, time_series_type=nothing,
                  name=nothing, resolution=nothing, features=Dict()) -> Vector{NamedTuple}
                                  # list_keys rows + `data_hash`; group by it to find shared arrays
count_array_references(store, data_hash::Vector{UInt8}) -> NamedTuple  # (sts, dst) refs to a 32-byte hash
static_summary(store) -> Vector{NamedTuple}   # grouped static rows with a `count`; build your own table
forecast_summary(store) -> Vector{NamedTuple}   # grouped forecast rows with a `count`
get_forecast_parameters(store; resolution=nothing, interval=nothing) -> NamedTuple  # (horizon, interval, count, resolution, initial_timestamp); fields `nothing` when none match
check_static_consistency(store; resolution=nothing) -> Vector{NamedTuple}  # one (resolution, initial_timestamp, length) per resolution present (empty when none); throws if the series at one resolution disagree
get_resolutions(store; time_series_type=nothing) -> Vector{Period}  # distinct resolutions, in the core's stored (lexical-by-ISO) order
get_compression(store) -> NamedTuple  # (compression=:deflate|:none, level, shuffle); restored from file on open
verify_integrity(store) -> Int    # number of integrity errors; 0 == intact
compact!(store) -> Nothing
flush!(store) -> Nothing          # sync to disk; afterwards .nc and .sqlite can be copied
clear!(store; owner_id=nothing, owner_category=nothing) -> Nothing
                                  # both `nothing`: remove every series in the store.
                                  # Scope to one owner by passing BOTH keywords — they identify the
                                  # (owner_id, owner_category) pair. `owner_id` without
                                  # `owner_category` throws ArgumentError.
replace_owner!(store, old_owner_id, new_owner_id, owner_category::OwnerCategory) -> Int
                                  # reassign one owner's series to a new id (same category); count moved
close!(store) -> Nothing
```

```julia
list_keys(store; owner_id=nothing, owner_category=nothing, time_series_type=nothing,
          name=nothing, resolution=nothing, features=Dict()) -> Vector{NamedTuple}
```

`list_keys` lists the key of every stored series as `NamedTuple`s (identity plus the per-type
descriptive snapshot: `initial_timestamp`, `resolution`, `length`, `horizon`, `interval`, `count`,
`features`; fields that do not apply to a key's type are `nothing`). All six filters are optional
and independent, and combine as a conjunction; with none set the whole store is listed:

- `owner_id`, `owner_category` — scope to one owner.
- `time_series_type` — a `CASTORE_TYPE_*` **integer code** (`0 = SingleTimeSeries` …
  `5 = Scenarios`), not a Julia type. (The `time_series_type` field on a returned row _is_ the Julia
  type.)
- `name` — exact association name.
- `resolution` — a `Period`.
- `features` — match keys whose features include all the given entries (subset match).

Physical storage detail (`data_hash`, `ext`, `percentiles`) is not on a key — read it via
`get_metadata` / `get_forecast_metadata`.

`list_array_groups` takes the same six filters and returns the same row fields as `list_keys`, but
each row additionally carries `data_hash` — the **64-character lowercase hex** content hash of the
array the row resolves to (note this is a hex `String`, whereas `get_metadata` returns the hash as a
32-byte `Vector{UInt8}`). Rows that share a stored array share their `data_hash`: both deduplicated
identical arrays and a `SingleTimeSeries` together with any `DeterministicSingleTimeSeries` derived
from it. **Group rows by `data_hash` to discover which time series share their underlying data** —
the foundation for reading a shared series once (see
[Window-read deduplication](#window-read-deduplication)). It is one catalog query (the hash is read
off each metadata row); there are no per-row `get_metadata` round-trips.

`count_array_references(store, data_hash)` returns `(; sts, dst)` — how many `SingleTimeSeries` and
`DeterministicSingleTimeSeries` associations reference the given 32-byte hash, across all owners.
Because a DST shares its backing `SingleTimeSeries` array, a caller uses these counts to decide
whether removing a `SingleTimeSeries` would orphan a derived DST.

## Associations

Two catalogs of relationships between entities the store does not otherwise model, replacing the
association tables IS3.jl used to keep itself. Both are **independent of time series**: there are no
foreign keys and no cascade (both endpoints live in the caller's object graph, so a cascade could
never fire), so removing a time series never removes an association and vice versa; a caller that
wants both makes both calls.

Every query in a family takes that family's four optional keyword filters, ANDed; with none set they
match every row, which is what makes a bare `list_*` call a whole-catalog export that the matching
`add_*!` re-imports unchanged. The `*_types` keywords take a vector of **concrete** type names,
matched as SQL `IN (…)`: expanding an abstract type into its subtypes stays on the Julia side, where
the type hierarchy lives, and an empty vector matches nothing, unlike omitting the keyword, which
matches everything. Every `remove_*!` returns the number of rows removed; removing nothing is `0`,
not an error.

### Supplemental-attribute associations

Which supplemental attributes are attached to which components. One attribute may be attached to
many components.

```julia
struct SupplementalAttributeAssociation
    component_id::Int64
    component_type::String
    attribute_id::Int64
    attribute_type::String
end
```

`SupplementalAttributeAssociation` overloads `==`, `hash`, and `show` (a compact
`SupplementalAttributeAssociation(Generator 1 <- GeographicInfo 100)`), so attachments work as
`Dict`/`Set` members. In the **catalog**, identity is only the `(component_id, attribute_id)` pair —
the type names are denormalized labels carried for filtering — so re-attaching the same pair under
different type names throws `DuplicateAssociationError`.

```julia
add_supplemental_attribute_association!(store, association::SupplementalAttributeAssociation) -> Nothing
add_supplemental_attribute_associations!(store, associations::AbstractVector{SupplementalAttributeAssociation}) -> Int
                                  # one all-or-nothing transaction; count inserted
has_supplemental_attribute_association(store; filters...) -> Bool
list_supplemental_attribute_associations(store; filters...) -> Vector{SupplementalAttributeAssociation}
                                  # insertion order
list_supplemental_attribute_ids(store; filters...) -> Vector{Int}
                                  # distinct attribute ids, ascending
list_components_with_attributes(store; filters...) -> Vector{Int}
                                  # distinct component ids, ascending
remove_supplemental_attribute_associations!(store; filters...) -> Int   # count removed
replace_supplemental_attribute_component_id!(store, old_id, new_id) -> Int   # rows updated
count_supplemental_attribute_associations(store; filters...) -> Int
count_supplemental_attributes(store; filters...) -> Int
count_components_with_attributes(store; filters...) -> Int
supplemental_attribute_counts_by_type(store) -> Vector{NamedTuple}   # (type, count), by type
supplemental_attribute_summary(store) -> Vector{NamedTuple}
                                  # (component_type, attribute_type, count), by attribute then component type
```

The four keyword filters are `component_id`, `component_types`, `attribute_id`, and
`attribute_types`.

`list_supplemental_attribute_ids` is "the attributes attached to this component" when `component_id`
is set; `list_components_with_attributes` is the other end, "the components carrying this attribute"
when `attribute_id` is set. `count_supplemental_attributes` and `count_components_with_attributes`
are those two queries counted, and `count_supplemental_attribute_associations` counts the matching
rows themselves.

`replace_supplemental_attribute_component_id!` moves every attachment from component `old_id` to
`new_id`, and throws `DuplicateAssociationError` if `new_id` already carries one of the attributes
being moved.

```julia
store = Store(in_memory=true)
add_supplemental_attribute_association!(
    store, SupplementalAttributeAssociation(1, "Generator", 100, "GeographicInfo"))
add_supplemental_attribute_association!(
    store, SupplementalAttributeAssociation(2, "Load", 100, "GeographicInfo"))

list_supplemental_attribute_ids(store; component_id=1)     # [100]
list_components_with_attributes(store; attribute_id=100)   # [1, 2]

remove_supplemental_attribute_associations!(store; component_id=1)
# 1; component 1's time series are untouched
```

### Parent/child associations

Directed edges between components — a generator (parent) wired to a bus (child), say. Both endpoints
are always components; an attribute cannot appear here.

```julia
struct ParentChildAssociation
    parent_id::Int64
    parent_type::String
    child_id::Int64
    child_type::String
end
```

`ParentChildAssociation` overloads `==`, `hash`, and `show` (a compact
`ParentChildAssociation(Generator 1 -> Bus 7)`) the same way. In the **catalog**, identity is the
_ordered_ `(parent_id, child_id)` pair, so the reversed pair is a different edge, while repeating
the same ordered pair under different type names throws `DuplicateAssociationError`. There is no
relationship-kind column, so one ordered pair may be related at most once.

This family is deliberately narrower than the supplemental one — no counts-by-type and no grouped
summary — because there is no consumer for them yet; both are additive if one appears.

```julia
add_parent_child_association!(store, association::ParentChildAssociation) -> Nothing
add_parent_child_associations!(store, associations::AbstractVector{ParentChildAssociation}) -> Int
                                  # one all-or-nothing transaction; count inserted
has_parent_child_association(store; filters...) -> Bool
list_parent_child_associations(store; filters...) -> Vector{ParentChildAssociation}
                                  # insertion order
list_children(store; filters...) -> Vector{Int}   # distinct child ids, ascending
list_parents(store; filters...) -> Vector{Int}    # distinct parent ids, ascending
remove_parent_child_associations!(store; filters...) -> Int   # count removed
replace_parent_child_component_id!(store, old_id, new_id) -> Int   # rows updated
count_parent_child_associations(store; filters...) -> Int
```

The four keyword filters are `parent_id`, `parent_types`, `child_id`, and `child_types`.

`replace_parent_child_component_id!` rewrites `old_id` to `new_id` on **both** ends of every edge,
and throws `DuplicateAssociationError` if the rewrite would duplicate an edge `new_id` already has.

```julia
store = Store(in_memory=true)
add_parent_child_association!(store, ParentChildAssociation(1, "Generator", 7, "Bus"))
# The reversed pair is a different edge, not a duplicate.
add_parent_child_association!(store, ParentChildAssociation(7, "Bus", 1, "Generator"))

list_children(store; parent_id=1)   # [7]
list_parents(store; child_id=7)     # [1]

remove_parent_child_associations!(store; parent_types=["Bus"])   # 1
```

Neither association catalog is exposed over the [gRPC server](./grpc-api.md) or the
[`cas` CLI](./cli.md).

## Errors

All subtype `TimeSeriesException`:

| Type                        | Mapped from FFI code                                                                      |
| --------------------------- | ----------------------------------------------------------------------------------------- |
| `NotFoundError`             | `CASTORE_ERR_NOT_FOUND`                                                                   |
| `DuplicateTimeSeriesError`  | `CASTORE_ERR_DUPLICATE`                                                                   |
| `DuplicateAssociationError` | `CASTORE_ERR_DUPLICATE_ASSOCIATION`                                                       |
| `InvalidParameterError`     | `CASTORE_ERR_INVALID_PARAMETER` / `CASTORE_ERR_INVALID_UTF8` / `CASTORE_ERR_NULL_POINTER` |
| `IntegrityError`            | `CASTORE_ERR_INTEGRITY`                                                                   |
| `ReadOnlyStoreError`        | `CASTORE_ERR_READ_ONLY`                                                                   |
| `IncompatibleFormatError`   | `CASTORE_ERR_INCOMPATIBLE_FORMAT`                                                         |
| `IOError`                   | `CASTORE_ERR_IO`                                                                          |
| `GenericError`              | Any other non-zero code (carries the numeric `code`)                                      |

The message text comes from the FFI layer's thread-local error buffer.

## Base Interface

The package overloads `Base` so the wrapped types behave like native Julia values:

- `==` and `hash` on `TimeSeriesKey` delegate to the Rust core's identity semantics (owner,
  category, type, name, resolution, interval, features), so keys work as `Dict`/`Set` members.
- `show` renders compact one-liners for `Store`, `TimeSeriesKey`, and the five value types.
- `length`, `eltype`, `getindex`, and `iterate` on `SingleTimeSeries` / `NonSequentialTimeSeries`
  delegate to the wrapped `data` array (element count, not time steps, for multi-dimensional
  values). Forecast types define `length` = window count.
- Do-block forms guarantee `close!` even on throw:

```julia
Store(in_memory=true) do store
    add_time_series!(store, 1, "Generator", Component, ts)
end

open_store(path; read_only=true) do store
    get_metadata(store, 1, Component, "load")
end
```

## Time and Resolution Conversions

- `DateTime` is converted to/from Unix milliseconds at the boundary.
- `resolution` is passed as a `Period` and converted to an ISO-8601 duration string; reads return
  resolution as a `Period` (`Millisecond` for fixed durations).

## Tracing

```julia
init_logging(level::AbstractString = "") -> Int32  # the FFI status code
```

Initialize the Rust tracing subscriber. `level` is an
[`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
directive string such as `"debug"` or `"castore_core=debug"`. Pass an empty string (the default) to
read `RUST_LOG`; if that variable is also unset, no output is produced.

The subscriber is initialized at most once per process — subsequent calls are no-ops. The module's
`__init__` hook calls `init_logging("")` automatically when `RUST_LOG` is set, so the common case
requires no code change:

```sh
export RUST_LOG=castore_core=debug
julia --project=. myscript.jl
```

For programmatic control without environment variables:

```julia
using Castore
init_logging("castore_core=debug")
```

See [Julia developer guide](../guides/julia.md#diagnostics-and-tracing) for usage examples and a
table of available span targets.
