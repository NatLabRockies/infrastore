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

_features_arg(features) = isempty(features) ? C_NULL : JSON.json(features)
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
    has_time_series(T, store, owner_id, owner_category, name; resolution, interval, features=Dict()) -> Bool

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
    features::AbstractDict=Dict{String, Any}(),
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
    remove_time_series!(T, store, owner_id, owner_category, name; resolution, interval, features=Dict())

Remove the time series of type `T` with the given attributes. `T` is any stored
time series type; the type-less form is the `SingleTimeSeries` shorthand.
`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`).
"""
function remove_time_series!(
    ::Type{T},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing, Period}=nothing,
    interval::Union{Nothing, Period}=nothing,
    features::AbstractDict=Dict{String, Any}(),
) where {T}
    resolution_iso = _period_to_cstr(resolution)
    interval_iso = _period_to_cstr(interval)
    features_json = _features_arg(features)
    code = @ccall lib_path().infrastore_store_remove_typed(
        store::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        _category_int(owner_category)::Int32,
        name::Cstring,
        Int32(_type_code(T))::Int32,
        resolution_iso::Cstring,
        interval_iso::Cstring,
        features_json::Cstring,
    )::Int32
    _check(code)
    return nothing
end

"""
    copy_time_series!(T, store, owner_id, owner_category, name,
                      dst_owner_id, dst_owner_type; new_name=nothing,
                      resolution=nothing, interval=nothing, features=Dict())

Copy the time series of type `T` identified by the source attributes onto
`dst_owner_id`, optionally renaming it to `new_name`.

Arrays are content-addressed, so this writes only a new association row against
the same underlying array: no data is duplicated, and the stored time series type
is preserved. In particular a `DeterministicSingleTimeSeries` stays one, whereas a
read-then-write copy through `get_time_series` / `add_time_series!` would
materialize it into a dense `Deterministic`.

The copy keeps the source's `owner_category`. Throws if the destination already
holds a matching series.
"""
function copy_time_series!(
    ::Type{T},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString,
    dst_owner_id::Integer,
    dst_owner_type::AbstractString;
    new_name::Union{Nothing, AbstractString}=nothing,
    resolution::Union{Nothing, Period}=nothing,
    interval::Union{Nothing, Period}=nothing,
    features::AbstractDict=Dict{String, Any}(),
) where {T}
    resolution_iso = _period_to_cstr(resolution)
    interval_iso = _period_to_cstr(interval)
    features_json = _features_arg(features)
    renamed = new_name === nothing ? C_NULL : new_name
    code = @ccall lib_path().infrastore_store_copy_time_series(
        store::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        _category_int(owner_category)::Int32,
        name::Cstring,
        Int32(_type_code(T))::Int32,
        resolution_iso::Cstring,
        interval_iso::Cstring,
        features_json::Cstring,
        Int64(dst_owner_id)::Int64,
        dst_owner_type::Cstring,
        renamed::Cstring,
    )::Int32
    _check(code)
    return nothing
end

# ---- Forecast data reads ---------------------------------------------------
#
# All three functions call `infrastore_store_get_forecast` and return the
# matching forecast struct, with the data array reshaped to the canonical Julia
# (column-major) layout that is the inverse of `_row_major_bytes`.
#
# Canonical row-major shapes from FORECAST_READ_SPEC.md:
#   Deterministic  [H, count, *E]
#   Probabilistic  [P, H, count, *E]
#   Scenarios      [scenario_count, H, count, *E]
#
# Since Rust stores row-major bytes and Julia is column-major, we need to
# reverse the dim order when reinterpreting and then permute axes back:
# the same transform used in `get_array_nd`.

# Internal helper: issue the ccall and return raw out-param values.
function _get_forecast_raw(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts_type::Integer;
    resolution::Union{Nothing, Period}=nothing,
    interval::Union{Nothing, Period}=nothing,
    features::AbstractDict=Dict{String, Any}(),
    time_range::TimeRangeArg=nothing,
)
    resolution_iso = _period_to_cstr(resolution)
    interval_iso = _period_to_cstr(interval)
    features_json = _features_arg(features)

    time_range_present = time_range !== nothing
    range_start_ms = time_range_present ? _to_unix_ms(time_range[1]) : Int64(0)
    range_end_ms = time_range_present ? _to_unix_ms(time_range[2]) : Int64(0)

    out_initial = Ref{Int64}(0)
    out_res = Ref{Ptr{Cchar}}(C_NULL)
    out_horizon = Ref{Ptr{Cchar}}(C_NULL)
    out_interval = Ref{Ptr{Cchar}}(C_NULL)
    out_count = Ref{UInt64}(0)
    out_scen = Ref{UInt64}(0)
    out_ndims = Ref{UInt64}(0)
    out_dims = Ref{Ptr{UInt64}}(C_NULL)
    out_dtype = Ref{Int32}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL)
    out_byte_len = Ref{UInt64}(0)
    out_pct = Ref{Ptr{Float64}}(C_NULL)
    out_pct_len = Ref{UInt64}(0)
    out_matched = Ref{Int32}(0)
    out_application_data = Ref{Ptr{Cchar}}(C_NULL)
    out_element_type = Ref{Ptr{Cchar}}(C_NULL)
    out_units = Ref{Ptr{Cchar}}(C_NULL)
    out_quantity_kind = Ref{Ptr{Cchar}}(C_NULL)
    out_unit_system = Ref{Ptr{Cchar}}(C_NULL)
    out_component_field = Ref{Ptr{Cchar}}(C_NULL)

    _check(
        @ccall lib_path().infrastore_store_get_forecast(
            store::Ptr{Cvoid},
            Int64(owner_id)::Int64,
            _category_int(owner_category)::Int32,
            name::Cstring,
            Int32(ts_type)::Int32,
            resolution_iso::Cstring,
            interval_iso::Cstring,
            features_json::Cstring,
            time_range_present::Bool,
            range_start_ms::Int64,
            range_end_ms::Int64,
            out_initial::Ref{Int64},
            out_res::Ref{Ptr{Cchar}},
            out_horizon::Ref{Ptr{Cchar}},
            out_interval::Ref{Ptr{Cchar}},
            out_count::Ref{UInt64},
            out_scen::Ref{UInt64},
            out_ndims::Ref{UInt64},
            out_dims::Ref{Ptr{UInt64}},
            out_dtype::Ref{Int32},
            out_data::Ref{Ptr{UInt8}},
            out_byte_len::Ref{UInt64},
            out_pct::Ref{Ptr{Float64}},
            out_pct_len::Ref{UInt64},
            out_matched::Ref{Int32},
            out_application_data::Ref{Ptr{Cchar}},
            out_element_type::Ref{Ptr{Cchar}},
            out_units::Ref{Ptr{Cchar}},
            out_quantity_kind::Ref{Ptr{Cchar}},
            out_unit_system::Ref{Ptr{Cchar}},
            out_component_field::Ref{Ptr{Cchar}},
        )::Int32
    )

    return _decode_forecast_outputs(
        out_initial,
        out_res,
        out_horizon,
        out_interval,
        out_count,
        out_scen,
        out_ndims,
        out_dims,
        out_dtype,
        out_data,
        out_byte_len,
        out_pct,
        out_pct_len,
        out_matched,
        out_application_data,
        out_element_type,
        out_units,
        out_quantity_kind,
        out_unit_system,
        out_component_field,
    )
end

# Decode the out-params populated by `infrastore_store_get_forecast` /
# `infrastore_store_get_forecast_by_key` into the common named tuple, copying then
# freeing every FFI-owned buffer.
function _decode_forecast_outputs(
    out_initial,
    out_res,
    out_horizon,
    out_interval,
    out_count,
    out_scen,
    out_ndims,
    out_dims,
    out_dtype,
    out_data,
    out_byte_len,
    out_pct,
    out_pct_len,
    out_matched,
    out_application_data,
    out_element_type,
    out_units,
    out_quantity_kind,
    out_unit_system,
    out_component_field,
)
    # Copy everything inside try/finally: every FFI allocation is released
    # exactly once in the `finally`, so an exception mid-decode cannot leak the
    # rest.
    try
        dims = Int.(unsafe_wrap(Array, out_dims[], Int(out_ndims[]); own=false))
        bytes = copy(unsafe_wrap(Array, out_data[], Int(out_byte_len[]); own=false))
        # Percentiles (Probabilistic only; null for others).
        percentiles = if Int(out_pct_len[]) > 0 && out_pct[] != C_NULL
            copy(unsafe_wrap(Array, out_pct[], Int(out_pct_len[]); own=false))
        else
            Float64[]
        end
        return (
            initial_timestamp=_from_unix_ms(out_initial[]),
            resolution=_peek_period(out_res[]),
            horizon=_peek_period(out_horizon[]),
            interval=_peek_period(out_interval[]),
            count=Int(out_count[]),
            scenario_count=Int(out_scen[]),
            dims=dims,
            bytes=bytes,
            dtype_code=out_dtype[],
            percentiles=percentiles,
            matched_type=Int(out_matched[]),
            application_data=_peek_cstr(out_application_data[]),
            element_type=_peek_cstr(out_element_type[]),
            units=_peek_cstr(out_units[]),
            quantity_kind=_peek_cstr(out_quantity_kind[]),
            unit_system=_unit_system(_peek_cstr(out_unit_system[])),
            component_field=_peek_cstr(out_component_field[]),
        )
    finally
        _free_u64(out_dims[], out_ndims[])
        _free_u8(out_data[], out_byte_len[])
        _free_f64(out_pct[], out_pct_len[])
        _free_cstr(out_res[])
        _free_cstr(out_horizon[])
        _free_cstr(out_interval[])
        _free_cstr(out_application_data[])
        _free_cstr(out_element_type[])
        _free_cstr(out_units[])
        _free_cstr(out_quantity_kind[])
        _free_cstr(out_unit_system[])
        _free_cstr(out_component_field[])
    end
end

# Key-based counterpart of `_get_forecast_raw`: reads via the key handle
# (`infrastore_store_get_forecast_by_key`), so the time series type comes from the key.
function _get_forecast_raw(
    store::Store,
    key::TimeSeriesKey;
    time_range::TimeRangeArg=nothing,
)
    time_range_present = time_range !== nothing
    range_start_ms = time_range_present ? _to_unix_ms(time_range[1]) : Int64(0)
    range_end_ms = time_range_present ? _to_unix_ms(time_range[2]) : Int64(0)

    out_initial = Ref{Int64}(0)
    out_res = Ref{Ptr{Cchar}}(C_NULL)
    out_horizon = Ref{Ptr{Cchar}}(C_NULL)
    out_interval = Ref{Ptr{Cchar}}(C_NULL)
    out_count = Ref{UInt64}(0)
    out_scen = Ref{UInt64}(0)
    out_ndims = Ref{UInt64}(0)
    out_dims = Ref{Ptr{UInt64}}(C_NULL)
    out_dtype = Ref{Int32}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL)
    out_byte_len = Ref{UInt64}(0)
    out_pct = Ref{Ptr{Float64}}(C_NULL)
    out_pct_len = Ref{UInt64}(0)
    out_matched = Ref{Int32}(0)
    out_application_data = Ref{Ptr{Cchar}}(C_NULL)
    out_element_type = Ref{Ptr{Cchar}}(C_NULL)
    out_units = Ref{Ptr{Cchar}}(C_NULL)
    out_quantity_kind = Ref{Ptr{Cchar}}(C_NULL)
    out_unit_system = Ref{Ptr{Cchar}}(C_NULL)
    out_component_field = Ref{Ptr{Cchar}}(C_NULL)

    _check(
        @ccall lib_path().infrastore_store_get_forecast_by_key(
            store::Ptr{Cvoid},
            key::Ptr{Cvoid},
            time_range_present::Bool,
            range_start_ms::Int64,
            range_end_ms::Int64,
            out_initial::Ref{Int64},
            out_res::Ref{Ptr{Cchar}},
            out_horizon::Ref{Ptr{Cchar}},
            out_interval::Ref{Ptr{Cchar}},
            out_count::Ref{UInt64},
            out_scen::Ref{UInt64},
            out_ndims::Ref{UInt64},
            out_dims::Ref{Ptr{UInt64}},
            out_dtype::Ref{Int32},
            out_data::Ref{Ptr{UInt8}},
            out_byte_len::Ref{UInt64},
            out_pct::Ref{Ptr{Float64}},
            out_pct_len::Ref{UInt64},
            out_matched::Ref{Int32},
            out_application_data::Ref{Ptr{Cchar}},
            out_element_type::Ref{Ptr{Cchar}},
            out_units::Ref{Ptr{Cchar}},
            out_quantity_kind::Ref{Ptr{Cchar}},
            out_unit_system::Ref{Ptr{Cchar}},
            out_component_field::Ref{Ptr{Cchar}},
        )::Int32
    )

    return _decode_forecast_outputs(
        out_initial,
        out_res,
        out_horizon,
        out_interval,
        out_count,
        out_scen,
        out_ndims,
        out_dims,
        out_dtype,
        out_data,
        out_byte_len,
        out_pct,
        out_pct_len,
        out_matched,
        out_application_data,
        out_element_type,
        out_units,
        out_quantity_kind,
        out_unit_system,
        out_component_field,
    )
end

# Result struct for a requested forecast type: both deterministic forms read
# back as `Deterministic` — a `DeterministicSingleTimeSeries` has no
# materialized form, so requesting it still yields the synthesized windows.
_forecast_result_type(::Type{Deterministic}) = Deterministic
_forecast_result_type(::Type{DeterministicSingleTimeSeries}) = Deterministic
_forecast_result_type(::Type{Probabilistic}) = Probabilistic
_forecast_result_type(::Type{Scenarios}) = Scenarios

# Build the result struct from a `_get_forecast_raw` named tuple plus the
# association name.
function _forecast_from_raw(::Type{Deterministic}, r, name::AbstractString)
    return Deterministic(
        r.initial_timestamp,
        r.resolution,
        r.horizon,
        r.interval,
        r.count,
        _decode_array(r.bytes, r.dtype_code, r.dims),
        name;
        application_data=r.application_data,
        element_type=r.element_type,
        units=r.units,
        quantity_kind=r.quantity_kind,
        unit_system=r.unit_system,
        component_field=r.component_field,
    )
end

function _forecast_from_raw(::Type{Probabilistic}, r, name::AbstractString)
    return Probabilistic(
        r.initial_timestamp,
        r.resolution,
        r.horizon,
        r.interval,
        r.count,
        r.percentiles,
        _decode_array(r.bytes, r.dtype_code, r.dims),
        name;
        application_data=r.application_data,
        element_type=r.element_type,
        units=r.units,
        quantity_kind=r.quantity_kind,
        unit_system=r.unit_system,
        component_field=r.component_field,
    )
end

function _forecast_from_raw(::Type{Scenarios}, r, name::AbstractString)
    # `scenario_count` is the leading axis of the decoded data.
    return Scenarios(
        r.initial_timestamp,
        r.resolution,
        r.horizon,
        r.interval,
        r.count,
        _decode_array(r.bytes, r.dtype_code, r.dims),
        name;
        application_data=r.application_data,
        element_type=r.element_type,
        units=r.units,
        quantity_kind=r.quantity_kind,
        unit_system=r.unit_system,
        component_field=r.component_field,
    )
end

"""
    get_time_series(T, store, owner_id, owner_category, name;
                    resolution, interval, features=Dict(), time_range)

Fetch a stored forecast of type `T`: `Deterministic`, `Probabilistic`, or
`Scenarios`. `owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`).

Ask for `Deterministic` whether the forecast was added densely or derived by
`transform_single_time_series!` — it matches both, so how the store holds it
stays an internal detail. (`DeterministicSingleTimeSeries` is also accepted, and
narrows to the derived form; you need it only when auditing which forecasts are
synthetic.)

The Rust core resolves the identity in a single call — no guess-and-retry. A
genuine miss raises `NotFoundError`; an ambiguous request raises an error naming
the candidates (narrow it with `resolution` and/or `interval`).

A stored `DeterministicSingleTimeSeries` has no materialized form and is
returned as a [`Deterministic`]. `data` has the canonical shape
`(H, count, element_dims...)` for the deterministic family,
`(num_percentiles, H, count, element_dims...)` for `Probabilistic`, and
`(scenario_count, H, count, element_dims...)` for `Scenarios`, where
`H = horizon / resolution`. Pass `time_range = (start, end)` (exclusive end) to
select a window sub-range per the InfrastructureSystems.jl convention.
"""
function get_time_series(
    ::Type{T},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing, Period}=nothing,
    interval::Union{Nothing, Period}=nothing,
    features::AbstractDict=Dict{String, Any}(),
    time_range::TimeRangeArg=nothing,
) where {T <: _ForecastRequest}
    r = _get_forecast_raw(
        store,
        owner_id,
        owner_category,
        name,
        _type_code(T);
        resolution=resolution,
        interval=interval,
        features=features,
        time_range=time_range,
    )
    return _forecast_from_raw(_forecast_result_type(T), r, String(name))
end

# Whether a `T`-shaped read may decode a forecast the store matched as
# `matched`. Only the two deterministic forms are interchangeable, and that is
# by design: a `DeterministicSingleTimeSeries` is a synthetic view of a
# `SingleTimeSeries` and always reads back as a `Deterministic`.
function _forecast_request_matches(::Type{T}, matched::Type) where {T}
    return _forecast_result_type(T) === _forecast_result_type(matched)
end

"""
    get_time_series(T, store, key; time_range)

Key-based counterpart to the attribute-addressed forecast reader. The stored
type comes from `key` (as returned by `add_time_series!` or
`get_time_series_keys`), and `T` must agree with it; a
`DeterministicSingleTimeSeries` key reads back as a [`Deterministic`].

Throws [`InvalidParameterError`](@ref) when `T` names a different forecast type
than the key does. The axes of the three forecast types mean different things —
a `Probabilistic` carries a leading percentile axis a `Deterministic` does not —
so decoding one as another does not merely mislabel the result, it misreads it.
"""
function get_time_series(
    ::Type{T},
    store::Store,
    key::TimeSeriesKey;
    time_range::TimeRangeArg=nothing,
) where {T <: _ForecastRequest}
    r = _get_forecast_raw(store, key; time_range=time_range)
    # The FFI reports the type it actually matched, and this used to decode as
    # `T` regardless. Asking for a `Deterministic` with a `Probabilistic` key
    # returned a `Deterministic{Float64,3}` whose `count` disagreed with its own
    # second axis, the percentile axis silently absorbed as a leading dimension
    # and the percentiles themselves dropped — wrong numbers, no error.
    matched = _type_for_code(r.matched_type)
    if !_forecast_request_matches(T, matched)
        throw(
            InvalidParameterError(
                "key names a $matched, not a $T; " *
                "call get_time_series($matched, store, key)",
            ),
        )
    end
    return _forecast_from_raw(_forecast_result_type(T), r, _key_name(key))
end
