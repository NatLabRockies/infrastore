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
# Request-only family sentinel (never a stored type): matches a stored
# `Deterministic` or `DeterministicSingleTimeSeries`. The Rust core resolves it
# and reports the concrete type that matched. Must match `INFRASTORE_TYPE_ABSTRACT_DETERMINISTIC`
# in the C ABI.
const INFRASTORE_TYPE_ABSTRACT_DETERMINISTIC = 100

_features_arg(features) = isempty(features) ? C_NULL : JSON.json(features)
_category_int(c::OwnerCategory) = Int32(Int(c))

"""
    transform_single_time_series!(store, horizon, interval; owner_category=nothing,
                                  resolution=nothing) -> Int

Derive `DeterministicSingleTimeSeries` forecasts from the stored `SingleTimeSeries`
associations (mirrors InfrastructureSystems.jl's `transform_single_time_series!`):
each is re-described as a DST sharing the same underlying array; `count` is derived
from each series' length. When `owner_category` is given (`Component` or
`SupplementalAttribute`) only series of that owner category are transformed;
otherwise every category is. When `resolution` is given only series at that
resolution are transformed. Returns the number of series transformed.
"""
function transform_single_time_series!(
    store::Store,
    horizon::Period,
    interval::Period;
    owner_category::Union{Nothing, OwnerCategory}=nothing,
    resolution::Union{Nothing, Period}=nothing,
)
    cat = owner_category === nothing ? Int32(-1) : Int32(Int(owner_category))
    res_iso = _period_to_cstr(resolution)
    out_count = Ref{UInt64}(0)
    code = @ccall lib_path().infrastore_store_transform_single_time_series(
        store.handle::Ptr{Cvoid},
        _period_to_iso(horizon)::Cstring,
        _period_to_iso(interval)::Cstring,
        cat::Int32,
        res_iso::Cstring,
        out_count::Ref{UInt64},
    )::Int32
    _check(code)
    return Int(out_count[])
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
        store.handle::Ptr{Cvoid},
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
        store.handle::Ptr{Cvoid},
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
        store.handle::Ptr{Cvoid},
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
    time_range::Union{Nothing, Tuple{DateTime, DateTime}}=nothing,
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
    out_ext = Ref{Ptr{Cchar}}(C_NULL)

    _check(
        @ccall lib_path().infrastore_store_get_forecast(
            store.handle::Ptr{Cvoid},
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
            out_ext::Ref{Ptr{Cchar}},
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
        out_ext,
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
    out_ext,
)
    # Copy dims and free FFI buffer.
    nd = Int(out_ndims[])
    dims_raw = unsafe_wrap(Array, out_dims[], nd; own=false)
    dims = Int.(copy(dims_raw))
    @ccall lib_path().infrastore_buffer_free_u64(
        out_dims[]::Ptr{UInt64}, out_ndims[]::UInt64
    )::Cvoid

    # Copy data bytes and free FFI buffer.
    n_bytes = Int(out_byte_len[])
    bytes_raw = unsafe_wrap(Array, out_data[], n_bytes; own=false)
    bytes = copy(bytes_raw)
    @ccall lib_path().infrastore_buffer_free_u8(
        out_data[]::Ptr{UInt8}, out_byte_len[]::UInt64
    )::Cvoid

    # Percentiles (Probabilistic only; null for others).
    np = Int(out_pct_len[])
    percentiles = if np > 0 && out_pct[] != C_NULL
        p = copy(unsafe_wrap(Array, out_pct[], np; own=false))
        @ccall lib_path().infrastore_buffer_free_f64(
            out_pct[]::Ptr{Float64}, out_pct_len[]::UInt64
        )::Cvoid
        p
    else
        Float64[]
    end

    return (
        initial_timestamp=_from_unix_ms(out_initial[]),
        resolution=_take_period(out_res[]),
        horizon=_take_period(out_horizon[]),
        interval=_take_period(out_interval[]),
        count=Int(out_count[]),
        scenario_count=Int(out_scen[]),
        dims=dims,
        bytes=bytes,
        dtype_code=out_dtype[],
        percentiles=percentiles,
        matched_type=Int(out_matched[]),
        ext=_take_cstr(out_ext[]),
    )
end

# Key-based counterpart of `_get_forecast_raw`: reads via the key handle
# (`infrastore_store_get_forecast_by_key`), so the time series type comes from the key.
function _get_forecast_raw(
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing, Tuple{DateTime, DateTime}}=nothing,
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
    out_ext = Ref{Ptr{Cchar}}(C_NULL)

    _check(
        @ccall lib_path().infrastore_store_get_forecast_by_key(
            store.handle::Ptr{Cvoid},
            key.handle::Ptr{Cvoid},
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
            out_ext::Ref{Ptr{Cchar}},
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
        out_ext,
    )
end

# Helper: decode raw forecast bytes into a properly-shaped Julia array.
# `dims` is in row-major order [d0, d1, ...]; we reconstruct the column-major
# Julia array as the inverse of `_row_major_bytes`.
function _decode_forecast_array(bytes::Vector{UInt8}, dtype_code::Int32, dims::Vector{Int})
    T = _julia_dtype(dtype_code)
    flat = collect(reinterpret(T, bytes))
    n = length(dims)
    n <= 1 && return reshape(flat, dims...)
    # Row-major → column-major: reshape with reversed dims, then permute axes.
    return permutedims(reshape(flat, reverse(dims)...), reverse(ntuple(identity, n)))
end

# Result struct for a requested forecast type: the deterministic family
# (including a stored `DeterministicSingleTimeSeries`, which has no materialized
# form) reads back as `Deterministic`.
_forecast_result_type(::Type{<:AbstractDeterministic}) = Deterministic
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
        _decode_forecast_array(r.bytes, r.dtype_code, r.dims),
        name;
        ext=r.ext,
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
        _decode_forecast_array(r.bytes, r.dtype_code, r.dims),
        name;
        ext=r.ext,
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
        _decode_forecast_array(r.bytes, r.dtype_code, r.dims),
        name;
        ext=r.ext,
    )
end

"""
    get_time_series(T, store, owner_id, owner_category, name;
                    resolution, interval, features=Dict(), time_range)

Fetch a stored forecast of type `T`: `Deterministic`, `DeterministicSingleTimeSeries`,
`Probabilistic`, `Scenarios`, or `AbstractDeterministic` to match whichever of
the deterministic pair is stored. `owner_category` is the owner's
`OwnerCategory` (`Component` or `SupplementalAttribute`).

The Rust core resolves the identity (and, for `AbstractDeterministic`, the
family) in a single call — no guess-and-retry. A genuine miss raises
`NotFoundError`; an ambiguous request raises an error naming the candidates
(narrow it with a concrete type, `resolution`, and/or `interval`).

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
    time_range::Union{Nothing, Tuple{DateTime, DateTime}}=nothing,
) where {T <: Union{AbstractDeterministic, Probabilistic, Scenarios}}
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

"""
    get_time_series(T, store, key; time_range)

Key-based counterpart to the attribute-addressed forecast reader: the stored
type comes from `key` (as returned by `add_time_series!` or
`get_time_series_keys`); `T` selects how the result is decoded. A
`DeterministicSingleTimeSeries` key reads back as a [`Deterministic`].
"""
function get_time_series(
    ::Type{T},
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing, Tuple{DateTime, DateTime}}=nothing,
) where {T <: Union{AbstractDeterministic, Probabilistic, Scenarios}}
    r = _get_forecast_raw(store, key; time_range=time_range)
    return _forecast_from_raw(_forecast_result_type(T), r, _key_name(key))
end
