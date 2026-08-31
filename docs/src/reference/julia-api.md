# Julia API

The Julia package is **`InfraStore.jl`** (module `InfraStore`); it wraps the [C ABI](./c-abi.md)
cdylib. The library is resolved from the `INFRASTORE_LIB` environment variable (development builds),
or else from the `libinfrastore_ffi` artifact that `Pkg` downloads at install time (see
[Integrate with Julia](../how-to/integrate-julia.md)).

```julia
using InfraStore
```

Exported names (types first, then functions):

`AddBatch`, `ArrayReferenceCounts`, `CompactionReport`, `Component`, `CompressionSettings`,
`Deterministic`, `DeterministicSingleTimeSeries`, `FixedOffsetReference`, `ForecastEntry`,
`ForecastParameters`, `ForecastReader`, `ForecastSummaryRow`, `ForecastTimeline`,
`NonSequentialTimeSeries`, `OwnerCategory`, `ParentChildAssociation`, `Probabilistic`, `Scenarios`,
`SingleTimeSeries`, `StaticGrid`, `StaticGroup`, `StaticReader`, `StaticSummaryRow`, `Store`,
`SupplementalAttribute`, `SupplementalAttributeAssociation`, `SupplementalAttributeSummaryRow`,
`SupplementalAttributeTypeCount`, `TimeReference`, `TimeSeriesCounts`, `TimeSeriesCountsDetailed`,
`TimeSeriesMetadata`, `TimeSeriesTypeCount`, `TransformOutcome`, `UTCReference`, `UnitSystem`
(`NaturalUnits`, `ComponentBase`), `ZoneReference`, `ZonelessReference`,
`add_parent_child_association!`, `add_parent_child_associations!`,
`add_supplemental_attribute_association!`, `add_supplemental_attribute_associations!`,
`add_time_series!`, `add_time_series_bulk!`, `association_exists`, `begin_transaction!`,
`build_forecast_reader`, `build_static_reader`, `catalog_mode`, `check_static_consistency`,
`clear!`, `close!`, `commit_transaction!`, `compact!`, `copy_time_series!`,
`count_array_references`, `count_components_with_attributes`, `count_parent_child_associations`,
`count_supplemental_attribute_associations`, `count_supplemental_attributes`, `counts_by_type`,
`export_supplemental_attribute_associations_openapi`, `export_time_series_associations_openapi`,
`flush!`, `forecast_entries`, `forecast_num_slots`, `forecast_read!`, `forecast_summary`,
`forecast_timeline`, `forecast_values`, `get_array_by_hash`, `get_compression`, `get_counts`,
`get_forecast_parameters`, `get_intervals`, `get_metadata_by_id`, `get_path`, `get_resolutions`,
`has_any_time_series`, `has_for_owner`, `has_parent_child_association`,
`has_supplemental_attribute_association`, `has_time_series`,
`import_supplemental_attribute_associations_openapi!`, `import_time_series_associations_openapi!`,
`in_transaction`, `init_logging`, `is_empty`, `is_zoneless`, `list_children`,
`list_components_with_attributes`, `list_metadata`, `list_metadata_by_ids`, `list_names`,
`list_owner_ids`, `list_owner_types`, `list_parent_child_associations`, `list_parents`,
`list_supplemental_attribute_associations`, `list_supplemental_attribute_ids`,
`num_distinct_arrays`, `open_copy`, `open_store`, `persist!`, `persist_catalog!`, `read_by_id`,
`read_by_ids`, `read_only`, `remove_by_filter!`, `remove_by_ids!`,
`remove_parent_child_associations!`, `remove_supplemental_attribute_associations!`,
`rename_time_series!`, `replace_owner!`, `replace_parent_child_component_id!`,
`replace_supplemental_attribute_component_id!`, `rollback_transaction!`, `static_grid`,
`static_groups`, `static_read!`, `static_summary`, `static_timestamps`, `static_values`,
`supplemental_attribute_counts_by_type`, `supplemental_attribute_summary`, `time_series_counts`,
`transaction`, `transform_single_time_series!`, `verify_integrity`, `zoned_timestamp`,
`zoned_timestamps`.

## Constructors

```julia
Store(; in_memory::Union{Nothing,Bool}=nothing, path::Union{Nothing,AbstractString}=nothing,
        compression::Union{Symbol,AbstractString}=:deflate,
        compression_level::Integer=3, shuffle::Bool=true,
        catalog::Union{Nothing,Symbol,AbstractString}=nothing,
        overwrite::Bool=false) -> Store
open_store(path::AbstractString; read_only::Bool=false,
           catalog::Union{Symbol,AbstractString}=:attached) -> Store
open_copy(src::AbstractString, dest::AbstractString;
          catalog::Union{Symbol,AbstractString}=:attached) -> Store
catalog_mode(store::Store) -> Symbol
```

`in_memory` defaults to whatever `path` implies — in-memory without one, file-backed with one — and
rarely needs setting; `path` together with `in_memory=true` throws `ArgumentError` (it used to be
accepted and silently discarded everything written).

A `Store` (and any reader built from it) is **not thread-safe**: the Rust core mutates the handle
without synchronization, so concurrent calls from two tasks or threads are undefined behavior, not
merely a race on results. Confine a store to one task, or guard every call with your own lock.

- `Store()` — in-memory store.
- `Store(in_memory=false, path="system.h5")` — persists to `system.h5` plus `system.h5.sqlite`.
- `compression=:none` stores arrays uncompressed; `:deflate` (default) applies DEFLATE at
  `compression_level` (0–9) with optional byte `shuffle`. The policy is persisted with the store and
  reused on later appends; it is ignored for in-memory stores. An unknown `compression` throws
  `ArgumentError`.
- `catalog=:attached` makes the catalog the `.sqlite` file, where every commit is durable;
  `catalog=:memory` holds it in RAM so it reaches disk only through `persist!` or
  `persist_catalog!`. Arrays stream to the HDF5 file either way. The default (`nothing`) matches the
  backend — `:memory` when `in_memory` is true, else `:attached` — so existing call sites are
  unchanged. An unknown `catalog` throws `ArgumentError`. See
  [Where the Catalog Lives](../explanation/storage-model.md#where-the-catalog-lives).
- `Store(in_memory=false, path=...)` throws `StoreExistsError` if `path` or `$path.sqlite` already
  holds a store. Creating there would discard the arrays while keeping the catalog, leaving a store
  that reopens cleanly with every array missing — see
  [protecting a saved artifact](../explanation/storage-model.md#protecting-a-saved-artifact).
  `overwrite=true` discards both halves on purpose; it throws `ArgumentError` for an in-memory
  store, which has no artifact to replace.
- `open_store(path; read_only=true)` — opens an existing on-disk pair.
- `open_copy(src, dest)` — copies both halves to `dest` and opens the copy read-write, leaving `src`
  untouched. **This is the safe way to load a store you intend to change.** `open_store` defaults to
  read-write, and mutations then land in that file directly; HDF5 has no journal and no repair tool,
  so an interrupted write is unrecoverable. Change the copy and `persist!(store, src)` — one atomic
  rename replaces the original. Throws `StoreExistsError` if `dest` already holds a store. Has a
  do-block form.
- `catalog_mode(store)` returns `:attached` or `:memory`.

The store registers a finalizer; close it eagerly with `close!(store)`.

## Types

Each struct carries the association `name` (required) and an optional `application_data`. Every
constructor takes `name` as the positional after `data` and `application_data=` as a keyword — e.g.
`SingleTimeSeries(initial, resolution, data, name; application_data=nothing)`.

Every data-carrying struct is parameterized `{T,N}` on the element type and dimensionality of its
value array; `{T,N}` is inferred from `data` by the constructor (an `AbstractArray` argument — a
view or a range — is normalized to a concrete `Array{T,N}`).

```julia
struct SingleTimeSeries{T,N}
    initial_timestamp :: DateTime
    resolution        :: Period          # e.g. Hour(1), Millisecond(500)
    data              :: Array{T,N}      # any element type; dim 1 = time
    name              :: String          # required association name
    application_data  :: Union{Nothing,String}
    element_type      :: Union{Nothing,String}   # canonical element_type, or nothing for plain scalars
    units             :: Union{Nothing,String}
    quantity_kind     :: Union{Nothing,String}
    unit_system       :: Union{Nothing,UnitSystem}   # nothing = unspecified, not NaturalUnits
    component_field   :: Union{Nothing,String}
    time_reference    :: Union{Nothing,TimeReference}  # inferred from the timestamp; see below
end
SingleTimeSeries(initial_timestamp, resolution, data, name; application_data=nothing, element_type=nothing, units=nothing,
    quantity_kind=nothing, unit_system=nothing, component_field=nothing, time_reference=<inferred>)

struct NonSequentialTimeSeries{T,N}
    timestamps   :: Vector{DateTime}     # strictly increasing; one per row of dim 1
    data         :: Array{T,N}
    name         :: String
    application_data  :: Union{Nothing,String}
    element_type      :: Union{Nothing,String}   # canonical element_type, or nothing for plain scalars
    units             :: Union{Nothing,String}
    quantity_kind     :: Union{Nothing,String}
    unit_system       :: Union{Nothing,UnitSystem}   # nothing = unspecified, not NaturalUnits
    component_field   :: Union{Nothing,String}
    time_reference    :: Union{Nothing,TimeReference}  # inferred from the timestamp; see below
end
NonSequentialTimeSeries(timestamps, data, name; application_data=nothing, element_type=nothing, units=nothing,
    quantity_kind=nothing, unit_system=nothing, component_field=nothing, time_reference=<inferred>)

struct Deterministic{T,N}
    initial_timestamp :: DateTime
    resolution        :: Period
    horizon           :: Period
    interval          :: Period
    count             :: Int
    data              :: Array{T,N}      # (H, count, element_dims...)
    name              :: String
    application_data  :: Union{Nothing,String}
    element_type      :: Union{Nothing,String}   # canonical element_type, or nothing for plain scalars
    units             :: Union{Nothing,String}
    quantity_kind     :: Union{Nothing,String}
    unit_system       :: Union{Nothing,UnitSystem}   # nothing = unspecified, not NaturalUnits
    component_field   :: Union{Nothing,String}
    time_reference    :: Union{Nothing,TimeReference}  # inferred from the timestamp; see below
end
Deterministic(initial_timestamp, resolution, horizon, interval, count, data, name; application_data=nothing, element_type=nothing, units=nothing,
    quantity_kind=nothing, unit_system=nothing, component_field=nothing, time_reference=<inferred>)

struct Probabilistic{T,N}
    initial_timestamp :: DateTime
    resolution        :: Period
    horizon           :: Period
    interval          :: Period
    count             :: Int
    percentiles       :: Vector{Float64}
    data              :: Array{T,N}      # (num_percentiles, H, count, element_dims...)
    name              :: String
    application_data  :: Union{Nothing,String}
    element_type      :: Union{Nothing,String}   # canonical element_type, or nothing for plain scalars
    units             :: Union{Nothing,String}
    quantity_kind     :: Union{Nothing,String}
    unit_system       :: Union{Nothing,UnitSystem}   # nothing = unspecified, not NaturalUnits
    component_field   :: Union{Nothing,String}
    time_reference    :: Union{Nothing,TimeReference}  # inferred from the timestamp; see below
end
Probabilistic(initial_timestamp, resolution, horizon, interval, count, percentiles, data, name; application_data=nothing, element_type=nothing, units=nothing,
    quantity_kind=nothing, unit_system=nothing, component_field=nothing, time_reference=<inferred>)

struct Scenarios{T,N}
    initial_timestamp :: DateTime
    resolution        :: Period
    horizon           :: Period
    interval          :: Period
    count             :: Int
    scenario_count    :: Int             # set from size(data, 1) by the constructor
    data              :: Array{T,N}      # (scenario_count, H, count, element_dims...)
    name              :: String
    application_data  :: Union{Nothing,String}
    element_type      :: Union{Nothing,String}   # canonical element_type, or nothing for plain scalars
    units             :: Union{Nothing,String}
    quantity_kind     :: Union{Nothing,String}
    unit_system       :: Union{Nothing,UnitSystem}   # nothing = unspecified, not NaturalUnits
    component_field   :: Union{Nothing,String}
    time_reference    :: Union{Nothing,TimeReference}  # inferred from the timestamp; see below
end
Scenarios(initial_timestamp, resolution, horizon, interval, count, data, name; application_data=nothing, element_type=nothing, units=nothing,
    quantity_kind=nothing, unit_system=nothing, component_field=nothing, time_reference=<inferred>)
# note: scenario_count is NOT a constructor argument

# The seven descriptors after `name` are carried on the struct and become the
# add_time_series! defaults, so a series built with units="MW" keeps them on add.
# `unit_system` is a `UnitSystem`: `NaturalUnits` (the units named by `units`)
# or `ComponentBase` (per-unit against the owning component's own base). The
# store records the declaration only — it holds no base and rescales nothing —
# and `nothing` means unspecified, which is deliberately not `NaturalUnits`.
# `time_reference` is normally left to the constructor, which infers it from the
# timestamp it was handed — see "Time references" below.

# Marker type; never constructed and with no materialized struct. Derived via
# transform_single_time_series! and read back as a Deterministic. You normally
# do not request it: a Deterministic request matches it too. It surfaces as a
# key's / row's time_series_type, so which forecasts are synthetic stays
# inspectable, and passing it narrows a query to the derived ones. {T,N} exists
# only so a row's time_series_type is parameterized for every stored type; it
# describes the Deterministic the row reads back as. Write it bare as a request.
abstract type DeterministicSingleTimeSeries{T,N} end

mutable struct Store
    handle :: Ptr{Cvoid}
end

@enum OwnerCategory begin
    Component             = 0
    SupplementalAttribute = 1
end
```

`application_data` is an opaque, package-owned payload (typically JSON) the binding can use to
reconstruct a domain object on read; the store stores it verbatim and never interprets it.
`add_time_series!` reads `name` off the object (it is not a call argument), so the same array can be
stored under different names; its `application_data=` keyword defaults to the object's
`application_data`. `data` keeps its Julia element type: the binding maps `T` to a stored dtype
(`Float64`, `Float32`, the signed and unsigned integer widths, `Bool`) and converts to row-major
bytes on the way down. An `element_type=` keyword declares what the elements _mean_ when they are
not plain numbers (`"tuple(3,f64)"`, `"piecewise_linear"`, … — see
[Element types](./element-types.md)); it defaults to the object's own `element_type`, which is
`nothing` for plain scalars.

## Element values

A composite `element_type` describes a layout, not a number: `"piecewise_linear"` is a curve per
timestep, packed across the array's trailing axis. **The write and read paths do that packing for
you** — hand a series its values and get the same values back:

```julia
curves = [PiecewiseLinear([(x = 0.0, y = 1.0), (x = 1.0, y = 3.0)]),
          PiecewiseLinear([(x = 0.0, y = 2.0)])]

ts = SingleTimeSeries(t0, Hour(1), curves, "cost")   # element_type: "piecewise_linear"
id = add_time_series!(store, 1, "Generator", Component, ts)

read_by_id(store, id).data == curves                 # true
get_metadata_by_id(store, id).time_series_type       # SingleTimeSeries{PiecewiseLinear, 1}
```

The constructor names the `element_type` from the values, so `element_type=` is only for the numeric
case where the numbers alone cannot say what they mean; declaring one that contradicts the values is
an error rather than an override.

`raw = true` on a read hands back the packing instead — one axis more, held as the physical dtype —
for a caller that wants the bytes as stored:

```julia
read_by_id(store, id; raw = true).data               # 2×5 Matrix{Float64}
```

The **readers are deliberately not decoded**. `StaticReader` and `ForecastReader` are the
per-timestamp simulation path, and `StaticGroup.dtype` is physical by definition; they hand back the
packed numbers, which `decode_element_values` turns into values if you want them.

The two codec functions are public in their own right, and neither takes a `Store` — they work on
any array you already have:

```julia
array, element_type = encode_element_values(curves)      # (2, 5), "piecewise_linear"
values = decode_element_values(array, element_type)
```

`decode_element_values` returns the array's shape **without** its trailing element axis — a vector
for a static series, an `(H, count)` matrix for a `Deterministic`, an `(P, H, count)` array for a
`Probabilistic` or `Scenarios` — so a forecast comes back windowed rather than flattened. A scalar
`element_type` is returned unchanged, because there the stored numbers already are the values;
`is_composite_element_type` tells the cases apart, and an unrecognized spelling reads back as raw
numbers rather than throwing.

| Value type          | `element_type`       | Constructor                           |
| ------------------- | -------------------- | ------------------------------------- |
| `LinearFunction`    | `linear_function`    | `(proportional, constant)`            |
| `QuadraticFunction` | `quadratic_function` | `(quadratic, proportional, constant)` |
| `PiecewiseLinear`   | `piecewise_linear`   | `(points)`, a vector of `XYCoords`    |
| `PiecewiseStep`     | `piecewise_step`     | `(x_coords, y_values)`                |
| `NTuple{N,Float64}` | `tuple(N,f64)`       | —                                     |

These types are **permissive on purpose**: they accept everything the store accepts, including the
zero- and one-point piecewise curves that a domain type such as InfrastructureSystems.jl's
`PiecewiseLinearData` rejects. A codec that could not represent a stored row could not read a store
back. They are named for the wire vocabulary for a second reason: so that
`using InfraStore, InfrastructureSystems` is not an ambiguity error.

A consumer with its own domain types never materializes them. Decode takes a `types` keyword whose
entries have exactly the constructor signatures in the table above:

```julia
decode_element_values(array, "piecewise_linear";
    types = merge(DEFAULT_ELEMENT_TYPES, (piecewise_linear = MyCurve,)))
```

and encode is open dispatch — add methods to `element_type_tag`, `element_row_width` and
`write_element_row!` for your own type and it encodes without a conversion step.

The encodings themselves are the store's, specified in [Element types](./element-types.md) and
pinned across every binding by `conformance/element_type_vectors.json`, which this package's tests
read. One consequence worth knowing: the ragged kinds are padded to the widest entry **in the series
being written**, so equal curves in differently-shaped series encode to different bytes and do not
share a stored array.

## Result Types

The catalog, metadata, and summary queries return **structs**, not `NamedTuple`s or `Dict`s. Each is
immutable, compares and hashes by value (so results can go straight into a `Set` or `Dict`), and
`show`s with its field names. Read a field with `x.field`; fields that do not apply to a row's time
series type are `nothing`.

Two conventions hold across every one of them: a `time_series_type` field holds the **Julia type**
(`SingleTimeSeries`, `Deterministic`, …), ready to pass to a `time_series_type` filter, and a reader
group's `dtype` field holds the **Julia element type** (`Float64`, `Bool`, …). Metadata instead
carries the store's canonical `element_type` **string**, which names both the meaning and (through
it) the dtype. An `owner_category` field is an `OwnerCategory`, never a string.

`TimeSeriesMetadata.time_series_type` is the **full** type, parameterized `{T,N}` like the value
structs — `SingleTimeSeries{Float64,1}`, `Deterministic{Float32,3}` — so a row names what a read of
it hands back, not merely which of the six kinds it is:

```julia
md = get_metadata_by_id(store, id)
md.time_series_type == typeof(read_by_id(store, id))   # every stored type but DST
```

The one exception is a derived `DeterministicSingleTimeSeries`: its row keeps the DST tag, since
that is where the derivation stays visible, while a read of it hands back the `Deterministic` it
becomes. The `{T,N}` agree; the outer types do not, and they are unrelated, so neither `==` nor `<:`
holds between them. Dispatch on the read's type when the two have to agree, and on the row's when
you mean "was this derived?".

Both parameters come off the row itself. For a plain numeric series `T` is the dtype and `N` is one
more than the rank of `element_shape`. For a **composite** `element_type` — one a read decodes — `T`
is the domain type and `N` is one _lower_, because the axis the values were packed across is the one
decoding consumes:

| `element_type`     | `element_shape` | `time_series_type`                      | with `raw = true`             |
| ------------------ | --------------- | --------------------------------------- | ----------------------------- |
| `f64`              | `()`            | `SingleTimeSeries{Float64,1}`           | same                          |
| `piecewise_linear` | `(7,)`          | `SingleTimeSeries{PiecewiseLinear,1}`   | `SingleTimeSeries{Float64,2}` |
| `tuple(3,f64)`     | `(3,)`          | `SingleTimeSeries{NTuple{3,Float64},1}` | `SingleTimeSeries{Float64,2}` |

A `DeterministicSingleTimeSeries` is parameterized by the `Deterministic` it reads back as, not by
the `SingleTimeSeries` whose array it shares — the parameters follow the read, the outer type does
not. An `element_type` written by a newer core than the wrapper knows leaves the row describing the
stored numbers, which is what a read of it hands back.

Test it with `<:`, not `==`, when you mean "which kind is this row":

```julia
md.time_series_type <: SingleTimeSeries        # kind
md.time_series_type == SingleTimeSeries        # false — it is SingleTimeSeries{Float64,1}
```

A **request** — a `time_series_type=` filter, `has_time_series`, a reader — takes either spelling.
Parameters on a request are **ignored**, never matched: a series is addressed by its identity
(owner, category, type, name, resolution, interval, features), which carries no element type, so
`{T,N}` has nothing to select on. They are accepted so that a row's `time_series_type` round-trips
straight back into any of those calls;
`list_metadata(store; time_series_type = SingleTimeSeries{Int32,1})` still matches every stored
`SingleTimeSeries`. A type that is no kind of time series raises `InvalidParameterError`.

`TimeSeriesTypeCount`, `StaticSummaryRow`, and `ForecastSummaryRow` group by stored type alone — the
grouping carries no dtype — so their `time_series_type` is always the bare one.

Every write returns the catalog row's `id` as a plain `Int64` — assigned, never reissued, and what
every read, removal, rename and copy takes.

```julia
struct TimeSeriesMetadata                    # get_metadata_by_id / list_metadata
    owner_id          :: Int64
    owner_type        :: String
    owner_category    :: OwnerCategory
    time_series_type  :: Type                # parameterized, e.g. SingleTimeSeries{Float64,1}
    name              :: String
    data_hash         :: Vector{UInt8}       # 32-byte content hash
    initial_timestamp :: Union{Nothing,DateTime}
    resolution        :: Union{Nothing,Period}
    horizon           :: Union{Nothing,Period}     # forecasts
    interval          :: Union{Nothing,Period}     # forecasts
    count             :: Union{Nothing,Int}        # forecasts
    length            :: Union{Nothing,Int}        # static series
    percentiles       :: Union{Nothing,Vector{Float64}}   # Probabilistic
    element_type      :: String              # "f64", "tuple(3,f64)", "piecewise_linear", …
    element_shape     :: Tuple{Vararg{Int}}  # per-timestep shape; () for scalars
    features          :: Dict{String,Any}
    units             :: Union{Nothing,String}
    quantity_kind     :: Union{Nothing,String}
    unit_system       :: Union{Nothing,UnitSystem}   # NaturalUnits | ComponentBase
    time_reference    :: Union{Nothing,TimeReference}   # how the timestamps were spelled
    component_field   :: Union{Nothing,String}       # e.g. "max_active_power"
    application_data  :: Union{Nothing,String}
    id                :: Union{Nothing,Int64}    # the catalog row's id; nothing off-catalog
end
```

`TimeSeriesMetadata` is the Julia mirror of the Rust core's type of the same name, and the package's
**only** metadata type: one struct for every time series type, reached either one at a time by
[`get_metadata_by_id`](#store-wide-operations) by id, or in bulk by
[`list_metadata`](#store-wide-operations). The fields a type does not use are `nothing` rather than
absent, so no field is silently dropped by the addressing path taken.

| Struct                            | Returned by                               | Fields                                                                                                                                        |
| --------------------------------- | ----------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `TimeSeriesCounts`                | `get_counts`                              | `components_with_time_series`, `static_time_series`, `forecasts`                                                                              |
| `TimeSeriesCountsDetailed`        | `time_series_counts`                      | `components_with_time_series`, `supplemental_attributes_with_time_series`, `static_time_series_count`, `forecast_count`                       |
| `TimeSeriesTypeCount`             | `counts_by_type`                          | `time_series_type`, `count`                                                                                                                   |
| `ArrayReferenceCounts`            | `count_array_references`                  | `sts`, `dst`                                                                                                                                  |
| `StaticSummaryRow`                | `static_summary`                          | `owner_type`, `owner_category`, `time_series_type`, `name`, `initial_timestamp`, `resolution`, `time_step_count`, `count`                     |
| `ForecastSummaryRow`              | `forecast_summary`                        | `owner_type`, `owner_category`, `time_series_type`, `name`, `initial_timestamp`, `resolution`, `horizon`, `interval`, `window_count`, `count` |
| `SupplementalAttributeTypeCount`  | `supplemental_attribute_counts_by_type`   | `attribute_type`, `count`                                                                                                                     |
| `SupplementalAttributeSummaryRow` | `supplemental_attribute_summary`          | `component_type`, `attribute_type`, `count`                                                                                                   |
| `ForecastParameters`              | `get_forecast_parameters`                 | `horizon`, `interval`, `count`, `resolution`, `initial_timestamp` (all `nothing` when nothing matches)                                        |
| `StaticGrid`                      | `static_grid`, `check_static_consistency` | `initial_timestamp`, `resolution` (`nothing` for an irregular reader), `length`, `time_reference`                                             |
| `ForecastTimeline`                | `forecast_timeline`                       | `initial_timestamp`, `resolution`, `interval`, `count`, `time_reference`                                                                      |
| `CompressionSettings`             | `get_compression`                         | `compression` (`:deflate` / `:none`), `level`, `shuffle`                                                                                      |
| `CompactionReport`                | `compact!`                                | `slots_reclaimed`, `datasets_dropped`, `feature_sets_reclaimed`, `timestamp_sets_reclaimed`, `bytes_reclaimed`                                |

Every struct in the table compares and hashes by value over **all** of its fields, `id` included: a
`TimeSeriesMetadata` describing the same series in two different stores is not equal to its
counterpart, because the id is the catalog's record of that row rather than a property of the data.
The association row types are the exception — `SupplementalAttributeAssociation` and
`ParentChildAssociation` compare on their endpoints alone, since a caller constructs those as plain
values and a row read back has to equal the one that wrote it.

`StaticGrid` is shared by `static_grid` (a reader's timeline) and `check_static_consistency` (one
per resolution present) — the same concept, so the same type. Its `resolution` is `nothing` only for
a `NonSequentialTimeSeries` reader, whose timeline is an explicit list of instants rather than a
grid; enumerate it with `static_timestamps`.

`time_reference` is the one spelling the axis carries — a reader spans one timeline, so a cohort
whose columns agree reports their reference, one whose columns merely agree on naming instants
reports `UTCReference()`, and a cohort mixing zoneless with the rest never builds at all. It is
`nothing` when the cohort records no spelling, and from `check_static_consistency`, which reports
grids rather than readers. `nothing` is not `ZonelessReference()`: the second is the positive claim
that the timestamps are wall clocks. Three- and four-argument constructors (`StaticGrid` and
`ForecastTimeline` respectively) leave it unset.

## Static Series

```julia
add_time_series!(
    store::Store, owner_id, owner_type, owner_category::OwnerCategory,
    ts;   # SingleTimeSeries, NonSequentialTimeSeries, or any dense forecast struct
    features::AbstractDict = Dict(), element_type = ts.element_type, units = ts.units,
    quantity_kind = ts.quantity_kind, unit_system = ts.unit_system,
    component_field = ts.component_field, application_data = ts.application_data,
    id = nothing,   # file under this catalog id (imports); `nothing` lets the catalog assign
) -> Int64   # the catalog row's id -- what every read, removal and rename takes

read_by_id(store::Store, id::Integer;
          start_time=nothing, len=nothing, count=nothing,
          owner=nothing) -> SingleTimeSeries | ...
read_by_ids(store::Store, ids::AbstractVector{<:Integer};
           time_range=nothing) -> Vector
```

A read names only an id, so the row's own stored type decides what comes back. Every read populates
the returned struct's `application_data` field from the stored association, so a binding's
reconstruction tag comes back with the data — no separate `get_metadata_by_id` call is needed.

`owner_id` is an integer identifier (`Int64`) and `owner_category` (`Component` /
`SupplementalAttribute`) completes the owner identity — the owner is the pair
`(owner_id, owner_category)`. `features` is serialized to JSON and must contain only JSON-scalar
values (`Int`, `Float64`, `Bool`, `String`); a feature name that shadows a time-series or identity
field (`name`, `resolution`, `owner_id`, …) is rejected on add — see
[reserved feature names](../explanation/data-model.md#reserved-feature-names).

A series known by its attributes is found with [`list_metadata`](#store-wide-operations), whose rows
carry the `id` every read takes. That split — identify, then act — is deliberate: a caller that
records ids in its own model does the first half once.

To read every series' value at one timestamp in a loop (the simulation pattern), use a
[`StaticReader`](#staticreader) rather than calling `read_by_id` per series.

`owner = (owner_id, category)` holds the row to that owner, throwing `OwnerMismatchError` when it
belongs to another — see [the owner guard](#the-owner-guard).

### Bulk reads

```julia
read_by_ids(store::Store, ids::AbstractVector{<:Integer};
          time_range::Union{Nothing,Tuple{Any,Any}}=nothing) -> Vector
# time_range clips every series to that window (default: each series in full);
# the bounds are DateTime or, with TimeZones loaded, ZonedDateTime
```

Reads many whole series in one call, returning one per id **in the order the ids are given**,
repeats included, each as the struct matching its stored type (`SingleTimeSeries`,
`NonSequentialTimeSeries`, `Deterministic`, `Probabilistic`, or `Scenarios`) — the result is a
`Vector{Any}`, so narrow it yourself when every id is one type. Packed `SingleTimeSeries` are read
and decompressed once per dataset instead of per series, so this is the efficient way to load many
complete series (exploration, plotting). An empty id vector returns an empty vector without touching
the store.

An id naming no row throws `NotFoundError` (the whole call fails; the error does not say which id
dangled — sift them with `association_exists` when that matters).

`time_range` **clips** to whatever falls between the two instants, where `read_by_id`'s window is
**checked**. Both bounds must be spelled the way the series are, and a selection spanning both
coherence groups (zoneless and instant-bearing) is refused rather than resolved per series; narrow
it with `list_metadata`'s `zoneless` filter.

```julia
series = read_by_ids(store, ids)
window = read_by_ids(store, ids; time_range = (t0, t1))
```

```julia
read_by_id(store::Store, id::Integer; start_time=nothing, len=nothing, count=nothing)
```

The single-id read, which also takes the slice. Both halves happen in one call: the id is a
primary-key lookup and the row it lands on carries the grid the window resolves against, so a caller
holding an id spends nothing to learn a series' `resolution` or `count` before asking for the second
day of it. With no keywords this is `read_by_ids` for one id.

`start_time` is the first timestamp to read — a window boundary (`initial_timestamp + k·interval`)
for a forecast — and may be a `DateTime` or, with TimeZones loaded, a `ZonedDateTime`. `len` counts
timesteps and applies to `SingleTimeSeries` / `NonSequentialTimeSeries`; `count` counts windows and
applies to the forecasts; passing the one that does not apply throws `InvalidParameterError`. So
does a `start_time` off the series' own grid, or a `len`/`count` running past its end — a window is
checked where the `time_range` on `read_by_ids` is clamped. `NotFoundError` if the id names no row.

```julia
day_two = read_by_id(store, id; start_time = t0 + Day(1), len = 24)
```

## Bulk Adds

```julia
batch = AddBatch()
add_time_series!(batch, owner_id, owner_type, owner_category, ts; ...)  # any series type
add_time_series_bulk!(store::Store, batch::AddBatch) -> Vector{Int64}   # ids, in input order
```

`AddBatch` accepts the same `add_time_series!` methods as `Store` (every series and forecast type)
but only accumulates the requests; `add_time_series_bulk!` commits the whole batch in **one**
metadata transaction, which is much faster than per-item adds when ingesting many series. The submit
is all-or-nothing: on error nothing is committed. The batch is drained by the call in either case
and may be reused. `length(batch)` returns the number of pending requests.

### Lookups

```julia
get_metadata_by_id(store, id::Integer) -> Union{TimeSeriesMetadata, Nothing}
list_metadata_by_ids(store, ids::AbstractVector{<:Integer}) -> Vector{TimeSeriesMetadata}
association_exists(store, id::Integer) -> Bool

has_time_series(store, owner_id, owner_category::OwnerCategory, name;
                resolution=nothing, features=Dict()) -> Bool
has_time_series(T::Type, store, owner_id, owner_category::OwnerCategory, name;
                resolution=nothing, interval=nothing, features=Dict()) -> Bool
```

`get_metadata_by_id` returns the whole [`TimeSeriesMetadata`](#result-types) record — every stored
type through the one function — or `nothing` when the catalog holds no such row. `nothing` rather
than a throw because a consumer validating references it persisted earlier is asking whether one
still resolves, and a stale reference is an answer; `association_exists` asks the same question
without building the row, cheap enough to check every reference in a model on load.
`list_metadata_by_ids` is the bulk form and _does_ throw `NotFoundError` on a stale id, since a
caller naming ids is asserting they exist.

`has_time_series` stays attribute-addressed: it is answered off the catalog indexes without
hydrating a row, so routing it through an id lookup would cost more than the question. It takes the
type as its first argument to address anything other than a `SingleTimeSeries`, and it matches the
feature map exactly. `owner_category` (`Component` / `SupplementalAttribute`) is required
throughout: the owner identity is the pair `(owner_id, owner_category)`, so a component and a
supplemental attribute may share a numeric `owner_id` and remain distinct.

A series known by its attributes is found with [`list_metadata`](#store-wide-operations), whose rows
carry the `id` every read and removal takes. There is deliberately no separate attribute-to-id
resolver — a caller that wants exactly one row poses the filter and checks that it got one:

```julia
row = only(list_metadata(store; owner_id = 42, name = "wind",
                         time_series_type = Scenarios, resolution = Hour(1)))
series = read_by_id(store, row.id)
```

Filtering for `Deterministic` also selects a stored `DeterministicSingleTimeSeries`, and each row
reports the concrete form it is; `interval` disambiguates forecasts that differ solely by interval.

```julia
get_array_by_hash(store, data_hash::Vector{UInt8}, ::Type{T}=Float64) -> Vector{T}
```

Fetches the flattened array for a 32-byte content hash, decoded as element type `T`. Combine with
`get_metadata_by_id` (for the element type and shape) to read values without reconstructing a
series. Every `data_hash` in this API is these same 32 bytes, so any metadata record feeds it
directly; `bytes2hex` gives the display form.

### Removal

```julia
remove_by_ids!(store, ids::AbstractVector{<:Integer}; owner=nothing) -> Int
remove_by_filter!(store; owner_id=nothing, name=nothing, ...) -> Int
```

`remove_by_ids!` removes every id in one all-or-nothing transaction and returns the count: an id
naming no row throws `NotFoundError` and nothing is removed (sift the set with `association_exists`
first when some references are expected to have gone), and a repeated id is removed, and counted,
once. An empty id vector returns `0` without touching the store.

It refuses to remove a `SingleTimeSeries` whose array still backs a `DeterministicSingleTimeSeries`
when it is the last backing series (the DST is a view of that array), raising
`InvalidParameterError` — remove the derived forecast first, or use an owner-scoped `clear!`, which
is exempt.

#### The owner guard

```julia
remove_by_ids!(store, ids; owner = (7, Component))       # only rows owned by component 7
read_by_id(store, id; owner = (7, Component))            # only if component 7 owns it
```

Both id-addressed calls take an optional `owner = (owner_id, category)`. The addressed row is held
to that owner and one belonging to anyone else throws `OwnerMismatchError`; for the removal the
check and the delete are one transaction, so a refused batch removes nothing.

A caller whose model says "this component's series" must pass the owner rather than confirm it in a
call of its own. An id is the whole address and it survives `replace_owner!`, so a
`get_metadata_by_id` that confirms the owner and a `remove_by_ids!` that then deletes are two calls
with a window between them — and a reassignment landing in that window makes the removal retire the
_new_ owner's series, the very thing the check was for. The category is half the owner: a component
and a supplemental attribute can carry the same integer id.

On the read side there is no window either way, but the guard is still the cheaper spelling: the
owner comes off the same row the values are materialized from, so it costs nothing, where a separate
check is a second round trip.

`remove_by_filter!` is the one removal that does not take ids, because enumerating them first is the
wrong shape for "remove everything matching": it takes the same filter as `list_metadata`, resolves
it to ids internally, and removes those in one transaction.

Reading a `DeterministicSingleTimeSeries` returns a `Deterministic`, since the type has no
materialized form. Its row still reports `DeterministicSingleTimeSeries` as its `time_series_type`,
parameterized by that `Deterministic`.

## Forecasts

Dense forecasts are constructed as `Deterministic`, `Probabilistic`, or `Scenarios` structs (see
[Types](#types)) and added through the generic `add_time_series!`. Each struct wraps a native
`AbstractArray` of any supported element type and dimensionality — the binding derives the stored
dtype and dims and converts to row-major bytes, just like the static `add_time_series!` (see the
[data model](../explanation/data-model.md#forecasts) for the conventional shapes).

The forecast `name` comes from the struct, e.g.
`Deterministic(initial, resolution, horizon, interval, count, data, name)`.

```julia
add_time_series!(
    store, owner_id, owner_type, owner_category::OwnerCategory,
    ts::Union{Deterministic,Probabilistic,Scenarios};
    features=Dict(), element_type=ts.element_type, units=ts.units,
    quantity_kind=ts.quantity_kind, unit_system=ts.unit_system,
    component_field=ts.component_field, application_data=ts.application_data,
) -> Int64
```

The descriptor keywords default to the struct's own fields, so a label set at construction survives
the add; pass a keyword to override it for one association.

A `DeterministicSingleTimeSeries` is not added directly. Derive one from every stored
`SingleTimeSeries` (sharing the backing array) with:

```julia
transform_single_time_series!(store, horizon::Period, interval::Period;
                              owner_category::Union{Nothing,OwnerCategory}=nothing,
                              resolution::Union{Nothing,Period}=nothing,
                              normalize_single_window::Bool=false,
                              require_uniform_forecast_grid::Bool=false,
                              dry_run::Bool=false) -> TransformOutcome

struct TransformOutcome
    transformed         :: Int      # DSTs derived (or that would be, under dry_run)
    sources             :: Int      # SingleTimeSeries in scope
    interval            :: Period   # the interval actually stored
    interval_normalized :: Bool     # true when a single-window request was stored as zero interval
end
```

`count` is derived from each series' length. `owner_category` restricts the transform to one owner
category (both are transformed when it is `nothing`); `resolution` restricts it to the
`SingleTimeSeries` at that resolution. The store performs the whole eligibility check — horizon fit
and divisibility, interval divisibility, per-resolution grid uniformity, conflicts with existing
forecasts — so callers need not pre-check per series.

The two policy flags encode a _client's_ contract rather than a storage invariant, and both default
to permissive. `normalize_single_window` stores a single-window request (interval equal to a horizon
spanning the whole series) as the zero interval rather than verbatim — the interval is part of the
key, so this decides which form later lookups must use. `require_uniform_forecast_grid` demands that
every resolution in scope, and any forecast already stored at the same `(resolution, interval)`,
agree on the derived `count` and `initial_timestamp`. **InfrastructureSystems.jl passes both as
`true`.** `dry_run` runs every check and reports the outcome without writing; it is legal against a
read-only store.

`has_time_series` takes the time series type as its first argument to ask about a type other than
`SingleTimeSeries`:

```julia
has_time_series(T::Type, store, owner_id, owner_category, name;
                resolution=nothing, interval=nothing, features=nothing) -> Bool
```

`interval` (a `Period`) pins the forecast interval — the only way to distinguish two forecasts of
one owner/name/type that differ solely by interval (e.g. day-ahead vs intra-day). Without it such a
question is ambiguous and errors.

This is an existence question, not an address: it answers `Bool` and hands back nothing to act on.
To read or remove a forecast, identify it with `list_metadata` and use the row's `id` — see
[Lookups](#lookups).

### Copying an association

```julia
copy_time_series!(store, src_id::Integer, dst_owner_id, dst_owner_type::AbstractString;
                  new_name=nothing) -> Int64
```

Copies the association filed under `src_id` onto `dst_owner_id` (of Julia/domain type
`dst_owner_type`), optionally renaming it to `new_name`, and returns the catalog `id` of the new
row. Arrays are content-addressed, so this writes only a new association row against the same
underlying array: no data is duplicated and the stored time series type is preserved — a
`DeterministicSingleTimeSeries` stays a DST, whereas a read-then-write copy through `read_by_id` /
`add_time_series!` would materialize it into a dense `Deterministic`. A copy is its own row with its
own id: the source's id is untouched and both resolve afterwards. The copy keeps the source's
`owner_category`. Throws if the destination already holds a matching series.

```julia
src = only(list_metadata(store; owner_id=42, name="load")).id
copy_time_series!(store, src, 43, "Generator")   # → a new id, under owner 43
```

### Reading forecast values

Forecasts are read the way everything else is: identify the row with `list_metadata`, then read it
by `id`. `read_by_id` dispatches on the row's stored type, so it returns the corresponding struct —
`Deterministic`, `Probabilistic`, or `Scenarios` — whose `data` field is a decoded N-dimensional
Julia array (reshaped to the type's logical shape, with native Julia indexing).

```julia
id = only(list_metadata(store; owner_id=400, owner_category=Component, name="load",
                        time_series_type=Deterministic)).id

read_by_id(store, id)                                  -> Deterministic
                # data shape: (H, count, element_dims...)
read_by_id(store, id; start_time=t, count=3)           -> Deterministic   # three windows from t
```

For a `Probabilistic` the data shape is `(num_percentiles, H, count, element_dims...)`, and for
`Scenarios` it is `(scenario_count, H, count, element_dims...)`.

`read_by_id`'s window is _checked_: `start_time` must be a window boundary
(`initial_timestamp + k·interval`) and `count` must not run past the end, or it throws
`InvalidParameterError`. The `time_range` on `read_by_ids` _clips_ instead — see
[Bulk reads](#bulk-reads).

The `interval` filter on `list_metadata` (a `Period`) is what distinguishes two forecasts under the
same owner/name/type that differ solely by interval; without it such a listing returns both rows and
`only` throws.

#### Reading a transformed forecast

An id names the exact stored row, so how a forecast came to exist never changes how it is read: a
`DeterministicSingleTimeSeries` row reads back as a `Deterministic`, since the type has no
materialized struct.

```julia
transform_single_time_series!(store, Hour(4), Hour(2))
id = only(list_metadata(store; owner_id=400, owner_category=Component, name="dst")).id
fc = read_by_id(store, id)                             # a Deterministic
```

Where the distinction matters — auditing which forecasts are synthetic rather than reading values —
it is in the catalog, not in the read:

```julia
get_metadata_by_id(store, id).time_series_type
# DeterministicSingleTimeSeries{Float64,2}     -- test kinds with <:, not ==
```

Filtering with `time_series_type=Deterministic` spans both: it matches a directly-stored
`Deterministic` _and_ a DST derived by `transform_single_time_series!`. Narrow to
`time_series_type=DeterministicSingleTimeSeries` to select only the derived rows.

Alternatively, use `get_metadata_by_id` to obtain the `data_hash`, then `get_array_by_hash` for the
raw flattened array.

For the per-timestamp simulation access pattern (walk the timeline, read every series at each
instant) prefer a reader — see [Readers](#readers-per-timestamp-iteration) below.

## Readers (per-timestamp iteration)

`read_by_id` returns a whole series or forecast struct. For the simulation access pattern — _walk
every timestamp and, at each, read the value of every series_ — use a **reader** instead. A reader
is built once over a filter, pins one timeline, and reuses output buffers that each read overwrites
in place, so a tight loop allocates almost nothing. There are two: `StaticReader` for the static
types, and `ForecastReader` for forecasts. Both follow the same lifecycle: build → inspect the
layout once → `*_read!(t)` in a loop → pull values per group/entry.

### StaticReader

Reads the value of every matching static series at one timestamp. Results are **columnar**: series
are partitioned into `(dtype, element_shape)` groups, and each group's values come back as one dense
`(num_columns, element_dims...)` array.

```julia
build_static_reader(store; resolution::Union{Nothing,Period}=nothing,
                    time_series_type::Type=SingleTimeSeries, owner_id=nothing,
                    owner_category=nothing, name=nothing, name_glob=nothing,
                    features=Dict(), component_field=nothing) -> StaticReader

static_grid(reader)       -> StaticGrid  # .initial_timestamp, .resolution (or nothing), .length
static_timestamps(reader) -> Vector{DateTime}  # every instant on the timeline, in order
static_groups(reader)     -> Vector{StaticGroup}  # each: .dtype, .element_shape, .keys
static_read!(reader, t::DateTime) -> reader  # fills buffers; errors if t is off the timeline
static_values(reader, group_index::Integer) -> Array
       # (num_columns, element_dims...); column j is static_groups(reader)[group_index].keys[j]
```

All matched series must share one timeline — one grid (`initial_timestamp` + `length`) for
`SingleTimeSeries`, one timestamp vector for `NonSequentialTimeSeries`. The build validates this and
errors on divergence, so there is no presence mask — every column has a value at every valid
timestamp.

`resolution` is required for `SingleTimeSeries` (one resolution per reader) and must be omitted for
`time_series_type=NonSequentialTimeSeries`, which has none; `static_grid(reader).resolution` is then
`nothing`. Iterating `static_timestamps` covers either kind, so one loop serves both:

```julia
reader = build_static_reader(store; resolution = Hour(1))
# ...or, for irregular series:
# reader = build_static_reader(store; time_series_type = NonSequentialTimeSeries)
for t in static_timestamps(reader)
    static_read!(reader, t)
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

`time_series_type` must be one of the four forecast types — `Deterministic`,
`DeterministicSingleTimeSeries`, `Probabilistic`, or `Scenarios`. Any other type raises
`InvalidParameterError`.

```julia
build_forecast_reader(store, time_series_type::Type; resolution::Period,
                      owner_id=nothing, owner_category=nothing, name=nothing,
                      name_glob=nothing, features=Dict(),
                      component_field=nothing) -> ForecastReader

forecast_timeline(reader)  -> ForecastTimeline
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
slot_. `forecast_read!` performs one backend (`.h5`) read per slot, not per entry, so a forecast
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
get_counts(store) -> TimeSeriesCounts   # components_with_time_series, static_time_series, forecasts
counts_by_type(store) -> Vector{TimeSeriesTypeCount}   # (time_series_type, count) per stored type
num_distinct_arrays(store) -> Int   # distinct content hashes; shared arrays count once
time_series_counts(store) -> TimeSeriesCountsDetailed   # distinct owners per category + distinct arrays per kind
list_owner_ids(store, owner_category; time_series_type=nothing, resolution=nothing) -> Vector{Int}
count_array_references(store, data_hash::Vector{UInt8}) -> ArrayReferenceCounts  # (sts, dst) refs to a 32-byte hash
static_summary(store) -> Vector{StaticSummaryRow}   # grouped static rows with a `count`; build your own table
forecast_summary(store) -> Vector{ForecastSummaryRow}   # grouped forecast rows with a `count`
get_forecast_parameters(store; resolution=nothing, interval=nothing) -> ForecastParameters  # horizon, interval, count, resolution, initial_timestamp; fields `nothing` when none match
check_static_consistency(store; resolution=nothing) -> Vector{StaticGrid}  # one grid per resolution present (empty when none); throws if the series at one resolution disagree
get_resolutions(store; time_series_type=nothing) -> Vector{Period}  # distinct resolutions, in the core's stored (lexical-by-ISO) order
get_intervals(store; time_series_type=nothing) -> Vector{Period}    # distinct forecast intervals, same order; empty for static types
get_path(store) -> Union{Nothing,String}   # the .h5 path, or nothing for an in-memory store
read_only(store) -> Bool
has_for_owner(store, owner_id, owner_category; time_series_type=nothing) -> Bool
                                  # does this owner have any series (of that type)? One index probe.
list_names(store; <list_metadata filters>) -> Vector{String}        # distinct names, sorted
list_owner_types(store; <list_metadata filters>) -> Vector{String}  # distinct owner types, sorted
remove_by_filter!(store; <list_metadata filters>) -> Int
                                  # remove every match in one all-or-nothing transaction; count removed
rename_time_series!(store, id::Integer, new_name) -> Int64   # same row and id, new name
get_compression(store) -> CompressionSettings  # compression=:deflate|:none, level, shuffle; restored from file on open
verify_integrity(store) -> Int    # number of integrity errors; 0 == intact
compact!(store) -> CompactionReport   # reclaims both halves; on an on-disk store this rewrites the
                                      # .h5 file from the live set and replaces it (single writer)
flush!(store) -> Nothing          # sync to disk; afterwards .h5 and .sqlite can be copied
persist!(store, path) -> Nothing  # write both halves to `path` + `$path.sqlite`, replacing them
persist_catalog!(store) -> Nothing  # write an in-memory catalog to this store's own $path.sqlite,
                                    # stamped to match the .h5 already beside it. Copies no arrays:
                                    # they are already in place. A checkpoint, not a mode switch;
                                    # for catalog=:attached this is flush!.

transaction(f, store)             # do-block: commit if `f` returns, roll back if it throws.
                                  # Spans any number of operations; removals are reversible only
                                  # inside one. Nests. Holds the SQLite write lock until it ends.
begin_transaction!(store) -> Nothing
commit_transaction!(store) -> Nothing    # errors if no transaction is open
rollback_transaction!(store) -> Nothing  # errors if no transaction is open
in_transaction(store) -> Bool
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
list_metadata(store; owner_id=nothing, owner_category=nothing, time_series_type=nothing,
              name=nothing, name_glob=nothing, resolution=nothing, interval=nothing,
              features=nothing, component_field=nothing,
              zoneless=nothing) -> Vector{TimeSeriesMetadata}
```

`list_metadata` is the package's one **identify** entry point: it returns a full
[`TimeSeriesMetadata`](#result-types) per matching row — identity, the per-type descriptive
snapshot, the physical detail (`data_hash`, `element_type`, `percentiles`, `application_data`), and
the row's `id`, which is what every read, removal, rename, and copy then takes. Fields that do not
apply to a row's type are `nothing`. All the filters are optional and independent, and combine as a
conjunction; with none set the whole store is listed:

- `owner_id`, `owner_category` — scope to one owner.
- `time_series_type` — the Julia type (`SingleTimeSeries`, `Deterministic`, …), the same value the
  `time_series_type` field of a returned row carries. `Deterministic` additionally matches
  `DeterministicSingleTimeSeries` rows; each row still reports its own stored type, and passing
  `DeterministicSingleTimeSeries` selects only those.
- `name` — exact association name.
- `name_glob` — a SQLite `GLOB` pattern over the name (`*` and `?`, case-sensitive), e.g.
  `"wind_*"`. ANDed with `name` rather than replacing it: set both and a row must satisfy both.
- `resolution` — a `Period`.
- `interval` — a `Period`; forecasts only (static rows carry no interval and never match an interval
  filter).
- `features` — match keys whose features include all the given entries (subset match).
- `component_field` — exact, case-sensitive match on the owning component's field (e.g.
  `"max_active_power"`): every series that varies that field, alone or scoped to one owner. A row
  that declares no `component_field` matches no value, so this cannot select the rows that left it
  unset.
- `zoneless` — the coherence group: `true` selects the wall-clock series, `false` the ones that name
  instants. The constructive remedy when a bulk read or a reader refuses a selection spanning both.

```julia
has_any_time_series(store; owner_id=nothing, owner_category=nothing, time_series_type=nothing,
                    name=nothing, name_glob=nothing, resolution=nothing, interval=nothing,
                    features=Dict(), component_field=nothing) -> Bool
```

`has_any_time_series` is the existence probe over the same filters: true iff `list_metadata` with
that filter would return at least one row, answered off the catalog indexes without hydrating or
marshaling any rows, so it is safe for hot per-component loops. `features` is a **subset** match
here, unlike the exact-key `has_time_series` forms, which compare the whole feature set by content
hash. A `features` filter still stays on indexes: the requested set is probed as an exact set by
hash first (one covering seek when the caller passes the complete feature set), with an indexed
per-feature fallback for genuinely partial lists.

The two matching rules are the thing to keep straight when a parent package resolves user queries:
the exact-identity `has_time_series` forms must be given the **complete** feature map or they miss,
while the list/filter forms accept a partial one and may return several rows — deciding what more
than one match means is the caller's job.

```julia
is_empty(store) -> Bool
```

`is_empty` is the store-wide predicate: true iff the store holds nothing at all — no time series,
and no associations in either catalog. It is one short-circuited existence probe per catalog table,
so its cost does not grow with the store, and it is the store's own answer: as the catalog gains
tables it stays correct, where a caller-side conjunction over `get_counts` and the
`count_*_associations` functions both costs a full aggregation and silently goes stale.

Every row `list_metadata` returns carries `data_hash` — the 32-byte content hash of the array the
row resolves to (a `Vector{UInt8}` hashes and compares by content, so it groups directly as a `Dict`
key). Rows that share a stored array share their `data_hash`: both deduplicated identical arrays and
a `SingleTimeSeries` together with any `DeterministicSingleTimeSeries` derived from it. **Group rows
by `data_hash` to discover which time series share their underlying data** — the foundation for
reading a shared series once (see [Window-read deduplication](#window-read-deduplication)). It is
one catalog query; there are no per-row `get_metadata_by_id` round-trips.

`count_array_references(store, data_hash)` returns an `ArrayReferenceCounts` (`sts`, `dst`) — how
many `SingleTimeSeries` and `DeterministicSingleTimeSeries` associations reference the given 32-byte
hash, across all owners. Because a DST shares its backing `SingleTimeSeries` array, a caller uses
these counts to decide whether removing a `SingleTimeSeries` would orphan a derived DST.

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
add_supplemental_attribute_association!(store, association::SupplementalAttributeAssociation) -> Int64
                                  # the catalog id it was filed under
add_supplemental_attribute_associations!(store, associations::AbstractVector{SupplementalAttributeAssociation}) -> Vector{Int64}
                                  # one all-or-nothing transaction; one id per
                                  # input row, in order (count is `length`)
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
supplemental_attribute_counts_by_type(store) -> Vector{SupplementalAttributeTypeCount}   # (attribute_type, count)
supplemental_attribute_summary(store) -> Vector{SupplementalAttributeSummaryRow}
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
add_parent_child_association!(store, association::ParentChildAssociation) -> Int64
                                  # the catalog id it was filed under
add_parent_child_associations!(store, associations::AbstractVector{ParentChildAssociation}) -> Vector{Int64}
                                  # one all-or-nothing transaction; one id per
                                  # input row, in order (count is `length`)
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
[`infrastore` CLI](./cli.md).

### OpenAPI-row association serde

Direct JSON serde of the two association catalogs, in the wire spelling
[SiennaSchemas](https://github.com/Sienna-Platform/SiennaSchemas) defines (`TimeSeries/*.json`,
`Core/Associations/SupplementalAttributeAssociation.json`). Unlike `list_metadata` /
`list_supplemental_attribute_associations`, which return Julia structs, these four functions
exchange the wire JSON verbatim — the format a document author (e.g. PowerTableDataParser) reads and
writes directly.

```julia
export_time_series_associations_openapi(store; filters...) -> String
import_time_series_associations_openapi!(store, json::AbstractString) -> Int
export_supplemental_attribute_associations_openapi(store) -> String
import_supplemental_attribute_associations_openapi!(store, json::AbstractString) -> Int
```

`export_time_series_associations_openapi` takes the same filter keywords as `list_metadata`. Every
row's `uri` and `data_hash` are the hex-encoded content hash the store already has for that row —
never a caller-supplied locator. With no filter this exports the whole catalog, sorted by identity.

`export_supplemental_attribute_associations_openapi` exports the whole
`supplemental_attribute_associations` table, sorted by `(component_id, attribute_id)`;
`import_supplemental_attribute_associations_openapi!` is its import half — a bulk, all-or-nothing
insert (a duplicate anywhere in the batch throws `DuplicateAssociationError` and rolls the batch
back), returning the number of rows inserted.

`import_time_series_associations_openapi!` is the time-series import half, and it writes **rows
only**: the document carries locators, never values, so every row must name an array this store
already holds — the arrays arrive with the artifact. Each row keeps the `association_id` it carries,
which is the point: an import that assigned fresh ids would leave every reference the document
records pointing at the wrong series. A row whose array is absent, or a `NonSequentialTimeSeries`
row (whose `timestamps_hash` is store-internal and so not on the wire, leaving the document with no
way to say which stored time axis the row sits on), throws `InvalidParameterError` and rolls the
whole batch back.

Infrastore never modifies the data to make an incoming document agree with what it already holds. A
geometry disagreement between an added series and its own association row is likewise rejected at
the add boundary (`InvalidParameterError`), loudly and without writing anything.

## Errors

All subtype `TimeSeriesException`:

| Type                        | Mapped from FFI code                                                                               |
| --------------------------- | -------------------------------------------------------------------------------------------------- |
| `NotFoundError`             | `INFRASTORE_ERR_NOT_FOUND`                                                                         |
| `DuplicateTimeSeriesError`  | `INFRASTORE_ERR_DUPLICATE`                                                                         |
| `DuplicateAssociationError` | `INFRASTORE_ERR_DUPLICATE_ASSOCIATION`                                                             |
| `InvalidParameterError`     | `INFRASTORE_ERR_INVALID_PARAMETER` / `INFRASTORE_ERR_INVALID_UTF8` / `INFRASTORE_ERR_NULL_POINTER` |
| `IntegrityError`            | `INFRASTORE_ERR_INTEGRITY`                                                                         |
| `ReadOnlyStoreError`        | `INFRASTORE_ERR_READ_ONLY`                                                                         |
| `IncompatibleFormatError`   | `INFRASTORE_ERR_INCOMPATIBLE_FORMAT`                                                               |
| `IOError`                   | `INFRASTORE_ERR_IO`                                                                                |
| `StoreExistsError`          | `INFRASTORE_ERR_STORE_EXISTS`                                                                      |
| `MismatchedArtifactError`   | `INFRASTORE_ERR_MISMATCHED_ARTIFACT`                                                               |
| `OwnerMismatchError`        | `INFRASTORE_ERR_OWNER_MISMATCH`                                                                    |
| `GenericError`              | Any other non-zero code (carries the numeric `code`)                                               |

The message text comes from the FFI layer's thread-local error buffer.

## Base Interface

The package overloads `Base` so the wrapped types behave like native Julia values:

- `show` renders compact one-liners for `Store` and the five value types; every result struct
  (`TimeSeriesMetadata`, `StaticSummaryRow`, …) gets generated `==`/`hash`/`show`, so results work
  as `Dict`/`Set` members, and `AddBatch` defines `length`.
- `length`, `eltype`, `getindex`, and `iterate` on `SingleTimeSeries` / `NonSequentialTimeSeries`
  delegate to the wrapped `data` array (element count, not time steps, for multi-dimensional
  values). Forecast types define `length` = window count.
- Do-block forms guarantee `close!` even on throw:

```julia
Store(in_memory=true) do store
    add_time_series!(store, 1, "Generator", Component, ts)
end

open_store(path; read_only=true) do store
    only(list_metadata(store; owner_id=1, owner_category=Component, name="load"))
end
```

## Time and Resolution Conversions

- `DateTime` is converted to/from Unix milliseconds at the boundary. A bare `DateTime` carries no
  zone, so it names a **wall clock**, not an instant: it is stored as its own fields and recorded as
  `ZonelessReference()`. The stored instant is unchanged from the old UTC-by-convention reading —
  what is new is that the store now records that it _was_ a convention.
- A **`TimeZones.ZonedDateTime` is accepted wherever a `DateTime` is** — an initial timestamp, a
  timestamp vector, a `time_range` bound, a reader's `t` — and is converted to the instant it names,
  recording the spelling its zone names. TimeZones is a **weak dependency**: the conversion lives in
  the `InfraStoreTimeZonesExt` extension, which loads when you `using TimeZones`, so nobody else
  pays for the tz database. Passing one without loading TimeZones raises an `InvalidParameterError`
  saying so.
- **Reads always return a `DateTime`** holding the instant, whichever kind went in, with the
  spelling beside it as a `time_reference`. Widening the return type was rejected: it would make the
  type depend on package load order, and `zdt == dt` raises in Julia, so it would turn working
  comparisons against a `DateTime` literal into runtime errors.
- A vector of `ZonedDateTime`s is ordered by the **instants** it names, not by its local wall
  clocks, so the strictly-increasing rule is checked after conversion. It must also agree on one
  spelling — one series records one reference.
- Milliseconds are lossless in both directions: the store records every instant to the millisecond
  and refuses a finer one on write, so this boundary cannot truncate a series written under that
  rule. See [timestamp precision](../explanation/data-model.md#timestamp-precision). (An artifact
  written before the rule may hold finer instants; those still truncate here.)
- `resolution` is passed as a `Period` and converted to an ISO-8601 duration string; reads return
  resolution as a `Period` (`Millisecond` for fixed durations).

### Time references

```julia
abstract type TimeReference end
struct UTCReference         <: TimeReference end          # an instant, written as UTC
struct FixedOffsetReference <: TimeReference; minutes::Int end   # minutes east
struct ZoneReference        <: TimeReference; name::String end   # an IANA zone name
struct ZonelessReference    <: TimeReference end          # a wall clock, naming no instant

is_zoneless(reference) -> Bool     # false for `nothing`: unset groups with the zoned ones
```

An abstract type with subtypes rather than an `@enum` like `UnitSystem`, because two of the four
carry a payload. The constructors infer one for you:

| Input                                    | `time_reference`                  |
| ---------------------------------------- | --------------------------------- |
| `ZonedDateTime(..., tz"UTC")`            | `UTCReference()`                  |
| `ZonedDateTime(..., tz"-07:00")`         | `FixedOffsetReference(-420)`      |
| `ZonedDateTime(..., tz"America/Denver")` | `ZoneReference("America/Denver")` |
| a bare `DateTime` or `Date`              | `ZonelessReference()`             |

The constructor signatures above write this default as `time_reference=<inferred>` rather than
naming a value, because there is no Julia literal for it: omitting the keyword infers the spelling
from the timestamp handed in, per the table above. Copying a literal `nothing` out of a signature
would _suppress_ that inference.

Passing `time_reference=nothing` explicitly is a different claim from omitting the keyword: it
records _unspecified_, which is also what a read hands back for a series that declared no spelling
(one written by a native Rust caller, say). The two are never collapsed — a read that invented
`ZonelessReference()` for an unspecified series would have `add_time_series!` write that invention
back, since its default is the series' own reference.

The two `FixedTimeZone` cases split on the zone's _name_, not its offset: `tz"UTC"` and `tz"+00:00"`
place every instant identically forever, and telling them apart is the point of recording a spelling
at all.

```julia
zoned_timestamp(instant::DateTime, reference::TimeReference) -> ZonedDateTime
zoned_timestamp(series) -> ZonedDateTime          # SingleTimeSeries / the three forecasts
zoned_timestamp(metadata::TimeSeriesMetadata) -> ZonedDateTime
zoned_timestamps(series::NonSequentialTimeSeries) -> Vector{ZonedDateTime}
```

Fuses a read instant back together with the spelling it was written in. Requires `using TimeZones`
(the methods live in the extension), and it is **lossless** — the instant plus the zone name
reconstructs the exact value written, including which side of a fall-back hour it was on:

```julia
using TimeZones
series = read_by_id(store, id)
zoned_timestamp(series)   # 2024-01-01T00:00:00-07:00
```

Throws for a `ZonelessReference()` series, whose timestamps name no instant, and for one that
recorded no reference at all.

A **query bound must be spelled the way the series is**: a bare `DateTime` bound against a series
that records instants, or a `ZonedDateTime` bound against a zoneless one, raises
`InvalidParameterError` rather than being coerced, and so does a `time_range` whose two ends
disagree. `list_metadata`, `build_static_reader`, and the other filter-taking functions accept
`zoneless=true|false` for building a coherent selection. See
[Time references](../explanation/data-model.md#time-references) for the full rules, including why a
calendar `Month`/`Year` resolution still steps on the UTC calendar.

Because a read always hands back a bare `DateTime`, the obvious round trip does **not** close on a
series that records instants — the returned timestamp holds the instant, but its Julia type says
wall clock:

```julia
t = series.initial_timestamp                                # a DateTime: a wall clock
read_by_ids(store, [id]; time_range=(t, t + Hour(3)))       # InvalidParameterError
```

Fuse the instant back together with the spelling that came with it, and the bound matches the
series:

```julia
using TimeZones
t = zoned_timestamp(series)                                 # or zoned_timestamp(metadata)
read_by_ids(store, [id]; time_range=(t, t + Hour(3)))       # reads
```

A Julia-only workflow never meets this, because a bare `DateTime` writes a zoneless series and a
`DateTime` bound then matches it. It is a store written by Python, the CLI, or a native Rust caller
— which record instants — that needs the zoned bound, and therefore `using TimeZones`.

## Tracing

```julia
init_logging(level::AbstractString = "") -> Int32  # the FFI status code
```

Initialize the Rust tracing subscriber. `level` is an
[`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
directive string such as `"debug"` or `"infrastore_core=debug"`. Pass an empty string (the default)
to read `RUST_LOG`; if that variable is also unset, no output is produced.

The subscriber is initialized at most once per process — subsequent calls are no-ops. The module's
`__init__` hook calls `init_logging("")` automatically when `RUST_LOG` is set, so the common case
requires no code change:

```sh
export RUST_LOG=infrastore_core=debug
julia --project=. myscript.jl
```

For programmatic control without environment variables:

```julia
using InfraStore
init_logging("infrastore_core=debug")
```

See [Julia developer guide](../guides/julia.md#diagnostics-and-tracing) for usage examples and a
table of available span targets.
