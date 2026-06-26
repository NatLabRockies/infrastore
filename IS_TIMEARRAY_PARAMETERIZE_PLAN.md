# Plan: Replace TimeArray with `Array` in IS; parameterize TS structs on `{T,N}`

## Goal

Stop storing `SingleTimeSeries` values as a `TimeSeries.TimeArray` in `IS3.jl`; store a plain
`Array` instead. Parameterize the time-series structs in **both** `TimeSeriesStore.jl` and `IS3.jl`
on `{T,N}` — `T` = element type of the value array, `N` = number of array dimensions. Make the
structs immutable, give them an explicit `initial_timestamp`, and drop the
`InfrastructureSystemsInternal` field.

This is step (1) of the broader IS → Rust migration (see `IS_MIGRATION_BACKLOG.md`).

## Decisions (locked)

1. **Parameter scheme `{T,N}`, field `data::Array{T,N}`.** Concrete `Array{T,N}` (not
   `AbstractArray`) so the field is type-stable. Constructors normalize views/ranges with
   `collect`/`Array`. `N=1` is the scalar-per-step case; `N≥2` is multidimensional per-timestep
   values (time is dim 1).
2. **`T` is package-specific (preserves the existing `{T}` vs `logical_type` duality):**
   - **IS**: `T` is the _in-memory_ value type — `Float64`, or a domain type like
     `LinearFunctionData`. `N` is the rank of that in-memory array (function-data series stay `N=1`,
     a `Vector{LinearFunctionData}`).
   - **TSS**: `T` is always a _stored numeric dtype_ (`Float64/Float32/Int64/Int32/UInt64/Bool`);
     `N` is the stored array rank (function data → `Array{Float64,2}`, `N=2`).
   - The bridge (`rust_time_series_store.jl`) already translates both `T` and `N` across this
     boundary via `_storage_array` (encode) and `logical_type` + decode (read). No new concept; we
     only make the parameters explicit on the types.
3. **Explicit `initial_timestamp::DateTime` field on the IS struct.** Timestamps are derived from
   `(initial_timestamp, resolution, size(data,1))` — no stored timestamp vector. Valid because
   `SingleTimeSeries` is regular by contract (`check_resolution`); irregular series are already a
   different type (`NonSequentialTimeSeries`).
4. **Immutable structs.** Affects IS only (TSS.jl structs are already immutable). Delete the
   `set_*!` setters; all slicing/copy ops already construct new instances.
5. **Drop `internal::InfrastructureSystemsInternal`.** Under the key-centric model, time-series
   identity is the array content hash (`rust_time_series_store.jl:439`), not a UUID. The store
   round-trip already discards `internal`. No `get_uuid` is ever called on a time-series value in
   IS3.
6. **Accessor API:**

   | Method                | Returns                                                                | Status                         |
   | --------------------- | ---------------------------------------------------------------------- | ------------------------------ |
   | `get_time_array(sts)` | freshly built `TimeArray` from `(initial_timestamp, resolution, data)` | **permanent**                  |
   | `get_array(sts)`      | the raw `data::Array{T,N}`                                             | **permanent**, new             |
   | `get_data(sts)`       | alias → `get_time_array(sts)`                                          | **temporary** back-compat shim |

   `get_time_array` is defined for `N ∈ {1,2}` (TimeArray is vector- or matrix-valued); for `N>2` it
   throws and points callers to `get_array`. `get_array` always works. Internal code prefers
   `get_array` (e.g. the bridge avoids building a TimeArray on every write).

## Target IS struct

```julia
struct SingleTimeSeries{T,N} <: StaticTimeSeries
    name::String
    initial_timestamp::DateTime
    resolution::Dates.Period
    data::Array{T,N}
end
```

## TimeSeriesStore.jl changes (`julia/TimeSeriesStore.jl/src/TimeSeriesStore.jl`)

1. **Parameterize the structs** (≈lines 169–290):
   - `struct SingleTimeSeries{T,N}` with `data::Array{T,N}`.
   - `struct NonSequentialTimeSeries{T}` with `data::Vector{T}` (always `N=1`).
   - `Deterministic{T,N}`, `Probabilistic{T,N}`, `Scenarios{T,N}` with `data::Array{T,N}` (canonical
     shapes already documented on each type).
   - `DeterministicSingleTimeSeries`: marker with no materialized data — leave unparameterized
     unless a shared `get_time_series(Type, …)` signature forces it.
2. **Inferring outer constructors.** Keep positional/keyword constructors; infer `{T,N}` from the
   passed array:
   `SingleTimeSeries(initial, res, data, name; …) =
   SingleTimeSeries{eltype(data),ndims(data)}(…)`.
3. **Wire the read path** (≈lines 787–815, 823+, 1110): the materialized array already has concrete
   `T`/`N` (`T = _julia_dtype(out_dtype)`, `nd = length(dims)`); pass it straight into the
   parametric constructor. No reshape/`permutedims` logic changes — only the field type tightens;
   ensure outputs are `Array{T,N}`.
4. `_dtype_code` / `_row_major_bytes` are unaffected (dispatch on `eltype`/`ndims`).
5. Audit `show` / `string(typeof(...))` for assumptions about a non-parametric name.

## IS3.jl changes (`src/single_time_series.jl` and siblings)

1. **Struct** (`:21`): `struct SingleTimeSeries{T,N} <: StaticTimeSeries` per the target above —
   immutable, add `initial_timestamp`, `data::Array{T,N}`, remove `internal`.
2. **Constructors** (`:34–208`): rework every constructor that builds a `TimeArray`:
   - Extract `TimeSeries.values(ta)` → `Array`; capture `TimeSeries.timestamp(ta)[1]` →
     `initial_timestamp`; validate regularity (`check_resolution`).
   - `normalization_factor` (`handle_normalization_factor`) operates on the `Array`.
   - Remove the `internal = InfrastructureSystemsInternal()` argument everywhere (`:53, :135, :84`).
   - Copy constructor `SingleTimeSeries(src, name)` (`:79`) drops the UUID-sharing; just rebuilds
     with the new `name`.
3. **Accessors:**
   - Add `get_time_array`, `get_array`; make `get_data` a deprecated alias of `get_time_array`.
   - `get_data_type(::SingleTimeSeries{T,N}) where {T,N} = string(T)` (`:71`) — `N` ignored.
   - `eltype_data` (`:260`) → `eltype(get_array(ts))`.
   - `get_initial_timestamp` → read the new field (override the `StaticTimeSeries` generic at
     `static_time_series.jl:12`, which currently uses `TimeSeries.timestamp(get_data(ts))[1]`).
   - `get_array_for_hdf` (`:265`) → `transform_array_for_hdf(get_array(ts))`.
   - Delete `get_internal`, `set_internal!`, `set_name!`, `set_data!` (`:245–258`).
4. **Reimplement TimeArray-backed behavior** against `(initial_timestamp, resolution, data)`
   (`:269–400`) — the bulk of the work:
   - `getindex`, `iterate`, `firstindex`/`lastindex`/`eachindex`
   - `when`, `from`, `to`, `head`, `tail`, `first`, `last`
   - `make_time_array`, the slice constructor `SingleTimeSeries(::SingleTimeSeries, ::TimeArray)`
     (`:338`) — rewrite explicitly (both its `TimeArray` and `InfrastructureSystemsInternal`
     branches are gone); no `fieldtypes` reflection.
   - `SingleTimeSeries(::Vector{SingleTimeSeries})` concatenation (`:186`).
   - `check_time_series_data` (`:210`) → validate against
     `range(initial_timestamp; step=resolution, length=size(data,1))`.
   - Hot ops (`getindex`, slicing) operate on indices/`data` directly; rarely-used ops may
     round-trip through `get_time_array`.
   - Remove the deserialize branch that constructs `InfrastructureSystemsInternal()` (`:344`).
5. **Sibling forecast structs** (`deterministic.jl`, `probabilistic.jl`, `scenarios.jl`,
   `deterministic_single_time_series.jl`, `static_time_series.jl`, `forecasts.jl`): parameterize on
   `{T,N}`, make immutable, drop `internal`/setters. `DeterministicSingleTimeSeries` is a view over
   a `SingleTimeSeries` — propagate its `{T,N}` from the underlying series.

## Bridge changes (`src/rust_time_series_store.jl`)

- `serialize_single!` (`:258`): read values via `get_array(sts)` (drops a TimeArray allocation per
  write); `_storage_array` continues to encode `T`/`N` → numeric storage form.
- `get_single` / manager-routed `get_time_series` (`:322, :558`): construct the IS struct directly
  from `(initial_timestamp, resolution, decoded Array)` — no `TimeSeries.TimeArray(...)` hop.
  Recover IS `{T,N}` from the decoded array's `eltype`/`ndims` plus `logical_type` (for domain `T`).

## Serialization / compatibility (highest-risk area)

- **Serialized type name changes.** `SingleTimeSeries{HydroDispatch}` becomes
  `SingleTimeSeries{HydroDispatch, 1}` in any string built from the parametric type
  (`utils/utils.jl:112` `_get_all_concrete_subtypes`; `strip_module_name`; HDF5/JSON metadata).
  Decide: normalize the serialized tag to ignore `N` (and possibly render only `T`) for back-compat,
  or bump the IS serialization version with a read shim. Audit `strip_module_name`, `nameof`, and
  the (de)serialization registry for assumptions about the number of type parameters.
- **Removed `internal`/`uuid` field.** The serialized struct shape loses `internal`. Confirm IS's
  generic serializer (over `InfrastructureSystemsType`) tolerates a TS struct with no `internal`.
  Likely fine since the on-disk TS format already changes with the Rust migration — verify.
- **`get_data_type` string stability.** Consumers expect just `T` (`"Float64"` /
  `"...LinearFunctionData"`); the `where {T,N}` definition preserves this.

## Testing

- **TSS.jl** (`test/runtests.jl`): assert
  `typeof(get_time_series(...)) == SingleTimeSeries{Float64,1}` plus an `N≥2` multidim case and each
  dtype; verify parametric constructors infer `{T,N}`.
- **IS3.jl**: run the time-series suite; assert `get_array` returns `Array{T,N}` with expected
  `T`/`N`; assert `get_time_array` reproduces correct timestamps for `N=1` and `N=2` and errors for
  `N>2`; keep a `get_data == get_time_array` equality test until the alias is removed; cover the
  function-data (`LinearFunctionData`, non-numeric `T`, `N=1`) round-trip, and the reimplemented
  `head/tail/from/to/when/getindex/concat` + `get_initial_timestamp`.
- **Bridge**: `serialize_single!` → `get_single` preserves `T`/`N`/values/timestamps; add
  function-data and multidim cases.
- **Serialization compat**: a pre-change serialized system (or recorded type string) still loads.

## Sequencing

1. **TSS.jl**: parameterize structs + wire read path + tests (self-contained, no IS dependency).
2. **IS3.jl `SingleTimeSeries`**: add `initial_timestamp`, make immutable, drop `internal`, swap
   `data` to `Array{T,N}`, add `get_time_array`/`get_array`, alias `get_data`, delete setters, port
   the TimeArray-backed methods.
3. **Bridge**: drop the TimeArray hops in `serialize_single!` / `get_single`; use `get_array`.
4. **Forecast sibling structs** in both packages.
5. **Serialization-name + removed-`internal` compatibility** shim + compat test.
6. **Downstream Sienna scan** (PowerSystems/PowerSimulations) for: `SingleTimeSeries{...}` dispatch;
   `set_data!`/`set_name!`/`set_internal!` call sites; direct `get_data(...)`-as-TimeArray
   assumptions; `get_uuid`/`get_internal` on time-series values (public-API break — identity is now
   content-hash via the store).

## Notes / residual risks

- No `get_uuid`/`get_internal` is called on a time-series _value_ anywhere in IS3 (the
  `get_internal(owner)` calls in `time_series_interface.jl:7,1027` are on the component/owner). The
  public API previously exposed `get_uuid`/`get_internal` on time series; downstream that relied on
  a stable TS UUID is the one breaking change — surface it in the downstream scan.
- `get_time_array` for `N>2` is intentionally unsupported (TimeArray is at most matrix-valued);
  `get_array` is the path for higher-rank values.
