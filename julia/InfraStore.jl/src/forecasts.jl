# ---- Forecasts -------------------------------------------------------------
#
# TimeSeriesType integer codes (must match the Rust `TimeSeriesType` enum).
# These are the C ABI's wire encoding, not part of the Julia API: every public
# function names a time series type with the Julia type itself, and `_type_code`
# does the conversion at the boundary.
const INFRASTORE_TYPE_SINGLE = 0
const INFRASTORE_TYPE_NON_SEQUENTIAL = 1
const INFRASTORE_TYPE_DETERMINISTIC = 2
const INFRASTORE_TYPE_DETERMINISTIC_SINGLE = 3
const INFRASTORE_TYPE_PROBABILISTIC = 4
const INFRASTORE_TYPE_SCENARIOS = 5

function _features_arg(features)
    return (features === nothing || isempty(features)) ? C_NULL : JSON.json(features)
end
_category_int(c::OwnerCategory) = Int32(Int(c))

"""
Everything [`transform_single_time_series!`](@ref) resolved, beyond the number of
series it wrote.

- `transformed` — DST rows written.
- `sources` — `SingleTimeSeries` matched before idempotent skips; `0` means
  there was nothing to transform, which is distinct from a transform that
  skipped everything as already derived.
- `interval` — the interval actually stored, which differs from the requested
  one when `normalize_single_window` collapsed a single-window request.
- `interval_normalized` — whether the request described a single window.
"""
struct TransformOutcome
    transformed::Int
    sources::Int
    interval::Period
    interval_normalized::Bool
end

"""
    transform_single_time_series!(store, horizon, interval; owner_category=nothing,
                                  resolution=nothing, normalize_single_window=false,
                                  require_uniform_forecast_grid=false,
                                  dry_run=false) -> TransformOutcome

Derive `DeterministicSingleTimeSeries` forecasts from the stored `SingleTimeSeries`
associations (mirrors InfrastructureSystems.jl's `transform_single_time_series!`):
each is re-described as a DST sharing the same underlying array; `count` is derived
from each series' length. When `owner_category` is given (`Component` or
`SupplementalAttribute`) only series of that owner category are transformed;
otherwise every category is. When `resolution` is given only series at that
resolution are transformed.

The store performs the whole of the eligibility validation — horizon divisibility
and fit, interval divisibility, per-resolution grid uniformity, and conflicts with
existing forecasts — so callers do not pre-check per series.

The two policy flags select rules that are a *client's* contract rather than a
storage invariant, and both default to the permissive behavior:

- `normalize_single_window` — store a single-window request (an interval equal to
  a horizon that spans the whole series) as the zero interval rather than
  verbatim. The interval is part of the association identity, so this decides
  which form later lookups must use.
- `require_uniform_forecast_grid` — require every resolution in scope, and any
  forecast already stored at the same `(resolution, interval)`, to agree on the
  derived window `count` and `initial_timestamp`.

InfrastructureSystems.jl passes both as `true`.

`dry_run` runs every check and reports what would happen without writing, so
`transformed` is the count a committing call would produce. It is the way to
answer "would this transform succeed?", and is legal against a read-only store.
"""
function transform_single_time_series!(
    store::Store,
    horizon::Period,
    interval::Period;
    owner_category::Union{Nothing, OwnerCategory}=nothing,
    resolution::Union{Nothing, Period}=nothing,
    normalize_single_window::Bool=false,
    require_uniform_forecast_grid::Bool=false,
    dry_run::Bool=false,
)
    cat = owner_category === nothing ? Int32(-1) : Int32(Int(owner_category))
    res_iso = _period_to_cstr(resolution)
    out_count = Ref{UInt64}(0)
    out_sources = Ref{UInt64}(0)
    # An ISO-8601 period is bounded, so the interval comes back through a fixed
    # buffer (INTERVAL_BUF_LEN in the C header) rather than a probe-then-fetch.
    out_interval = Vector{UInt8}(undef, 64)
    out_normalized = Ref{Bool}(false)
    code = @ccall lib_path().infrastore_store_transform_single_time_series(
        store::Ptr{Cvoid},
        _period_to_iso(horizon)::Cstring,
        _period_to_iso(interval)::Cstring,
        cat::Int32,
        res_iso::Cstring,
        normalize_single_window::Bool,
        require_uniform_forecast_grid::Bool,
        dry_run::Bool,
        out_count::Ref{UInt64},
        out_sources::Ref{UInt64},
        out_interval::Ptr{UInt8},
        out_normalized::Ref{Bool},
        # The ids of the views just written; not surfaced yet.
        C_NULL::Ptr{Ptr{Int64}},
    )::Int32
    _check(code)
    return TransformOutcome(
        Int(out_count[]),
        Int(out_sources[]),
        _iso_to_period(unsafe_string(pointer(out_interval))),
        out_normalized[],
    )
end

"""
    has_time_series(T, store, owner_id, owner_category, name; resolution, interval, features=nothing) -> Bool

True iff a time series of type `T` with the given attributes exists. `T` is any
stored time series type; the type-less form is the `SingleTimeSeries` shorthand.
`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`).
"""
function has_time_series(
    ::Type{T},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing, Period}=nothing,
    interval::Union{Nothing, Period}=nothing,
    features::Union{Nothing, AbstractDict}=nothing,
) where {T}
    resolution_iso = _period_to_cstr(resolution)
    interval_iso = _period_to_cstr(interval)
    features_json = _features_arg(features)
    out = Ref{Bool}(false)
    code = @ccall lib_path().infrastore_store_has_typed(
        store::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        _category_int(owner_category)::Int32,
        name::Cstring,
        Int32(_type_code(T))::Int32,
        resolution_iso::Cstring,
        interval_iso::Cstring,
        features_json::Cstring,
        out::Ref{Bool},
    )::Int32
    _check(code)
    return out[]
end

"""
    copy_time_series!(store, src_id, dst_owner_id, dst_owner_type; new_name=nothing) -> Int64

Copy the association filed under `src_id` onto `dst_owner_id`, optionally
renaming it to `new_name`, and return the catalog `id` of the new row.

Arrays are content-addressed, so this writes only a new association row against
the same underlying array: no data is duplicated, and the stored time series type
is preserved. In particular a `DeterministicSingleTimeSeries` stays one, whereas a
read-then-write copy through [`read_by_id`](@ref) / [`add_time_series!`](@ref)
would materialize it into a dense `Deterministic`.

A copy is its own row with its own id — the source's id is untouched, and both
resolve afterwards. The copy keeps the source's `owner_category`. Throws if the
destination already holds a matching series.

Identify the source first when you know it by attributes:

```julia
src = only(list_metadata(store; owner_id = 42, name = "load")).id
copy_time_series!(store, src, 43, "Generator")
```
"""
function copy_time_series!(
    store::Store,
    src_id::Integer,
    dst_owner_id::Integer,
    dst_owner_type::AbstractString;
    new_name::Union{Nothing, AbstractString}=nothing,
)
    renamed = new_name === nothing ? C_NULL : new_name
    out_id = Ref{Int64}(0)
    code = @ccall lib_path().infrastore_store_copy_time_series(
        store::Ptr{Cvoid},
        Int64(src_id)::Int64,
        Int64(dst_owner_id)::Int64,
        dst_owner_type::Cstring,
        renamed::Cstring,
        out_id::Ref{Int64},
    )::Int32
    _check(code)
    return out_id[]
end
