module TimeSeriesStore

using Dates
import JSON

export Store, SingleTimeSeries, NonSequentialTimeSeries,
       Deterministic, DeterministicSingleTimeSeries, AbstractDeterministic,
       Probabilistic, Scenarios, TimeSeriesKey,
       OwnerCategory, Component, SupplementalAttribute,
       add_time_series!, AddBatch, add_time_series_bulk!,
       get_time_series, get_time_series_keys, key_info, list_keys,
       remove_time_series!,
       has_time_series, get_counts, counts_by_type, num_distinct_arrays,
       time_series_counts, list_owner_ids, static_summary, forecast_summary,
       get_forecast_parameters, check_static_consistency, get_resolutions, get_compression,
       verify_integrity, compact!,
       get_metadata, get_forecast_metadata, get_array_by_hash, count_array_references,
       open_store, flush!, clear!, replace_owner!,
       transform_single_time_series!, has_typed, remove_typed!,
       close!,
       init_logging

# ---- libtime_series_store_ffi resolution ---------------------------------
#
# Resolution order:
#   1. `TIME_SERIES_STORE_LIB` environment variable (development override).
#   2. `TimeSeriesStore_jll` (the BinaryBuilder/Yggdrasil binary) when installed.
# The JLL is looked up without a hard dependency so this package still loads and
# works via the env var before the JLL is published to the registry.

const _LIB_REF = Ref{String}("")

function _jll_library_path()
    pkgid = Base.identify_package("TimeSeriesStore_jll")
    pkgid === nothing && return ""
    mod = try
        Base.require(pkgid)
    catch
        return ""
    end
    return isdefined(mod, :libtime_series_store_ffi) ?
           String(getproperty(mod, :libtime_series_store_ffi)) : ""
end

"""
Path to the `libtime_series_store_ffi` cdylib. Override with the
`TIME_SERIES_STORE_LIB` environment variable (development builds); otherwise the
`TimeSeriesStore_jll` binary is used.
"""
function lib_path()
    if !isempty(_LIB_REF[])
        return _LIB_REF[]
    end
    p = get(ENV, "TIME_SERIES_STORE_LIB", "")
    if isempty(p)
        p = _jll_library_path()
    end
    isempty(p) && error(
        "Could not locate libtime_series_store_ffi. Set the TIME_SERIES_STORE_LIB " *
        "environment variable to a built cdylib, or install TimeSeriesStore_jll.",
    )
    _LIB_REF[] = p
    return p
end

# ---- Status codes (must match crates/time-series-store-ffi/src/lib.rs) ----

const TS_OK                    = Int32(0)
const TS_ERR_NULL_POINTER      = Int32(1)
const TS_ERR_INVALID_UTF8      = Int32(2)
const TS_ERR_INVALID_PARAMETER = Int32(3)
const TS_ERR_NOT_FOUND         = Int32(4)
const TS_ERR_DUPLICATE         = Int32(5)
const TS_ERR_INTEGRITY         = Int32(6)
const TS_ERR_READ_ONLY         = Int32(7)
const TS_ERR_IO                = Int32(8)
const TS_ERR_INTERNAL          = Int32(99)

# ---- Owner category --------------------------------------------------------

@enum OwnerCategory begin
    Component              = 0
    SupplementalAttribute  = 1
end

# ---- Errors ---------------------------------------------------------------

abstract type TimeSeriesException <: Exception end

struct NotFoundError           <: TimeSeriesException; msg::String; end
struct DuplicateTimeSeriesError <: TimeSeriesException; msg::String; end
struct InvalidParameterError    <: TimeSeriesException; msg::String; end
struct IntegrityError           <: TimeSeriesException; msg::String; end
struct ReadOnlyStoreError       <: TimeSeriesException; msg::String; end
struct GenericError             <: TimeSeriesException; msg::String; code::Int32; end

Base.showerror(io::IO, e::TimeSeriesException) = print(io, "TimeSeriesStore.", typeof(e).name.name, ": ", e.msg)

function _last_error_message()
    needed = Ref{UInt64}(0)
    ccall((:ts_last_error_message, lib_path()), Int32,
          (Ptr{UInt8}, UInt64, Ptr{UInt64}),
          C_NULL, UInt64(0), needed)
    n = Int(needed[])
    n == 0 && return ""
    buf = Vector{UInt8}(undef, n + 1)
    ccall((:ts_last_error_message, lib_path()), Int32,
          (Ptr{UInt8}, UInt64, Ptr{UInt64}),
          buf, UInt64(n + 1), C_NULL)
    return String(buf[1:n])
end

function _check(code::Int32)
    code == TS_OK && return
    msg = _last_error_message()
    if code == TS_ERR_NOT_FOUND
        throw(NotFoundError(msg))
    elseif code == TS_ERR_DUPLICATE
        throw(DuplicateTimeSeriesError(msg))
    elseif code == TS_ERR_INVALID_PARAMETER || code == TS_ERR_INVALID_UTF8 || code == TS_ERR_NULL_POINTER
        throw(InvalidParameterError(msg))
    elseif code == TS_ERR_INTEGRITY
        throw(IntegrityError(msg))
    elseif code == TS_ERR_READ_ONLY
        throw(ReadOnlyStoreError(msg))
    else
        throw(GenericError(msg, code))
    end
end

# ---- Element dtypes -------------------------------------------------------
# Codes must match `Dtype` in the Rust core / FFI.

_dtype_code(::Type{Float64}) = Int32(0)
_dtype_code(::Type{Float32}) = Int32(1)
_dtype_code(::Type{Int64})   = Int32(2)
_dtype_code(::Type{Int32})   = Int32(3)
_dtype_code(::Type{UInt64})  = Int32(4)
_dtype_code(::Type{Bool})    = Int32(5)
_dtype_code(::Type{T}) where {T} =
    throw(InvalidParameterError("unsupported element dtype $T"))

const _DTYPE_JULIA = (Float64, Float32, Int64, Int32, UInt64, Bool)
_julia_dtype(code::Integer) = _DTYPE_JULIA[Int(code) + 1]

# Row-major little-endian bytes for a (possibly multi-dimensional) array. Julia
# is column-major, so transpose the axis order before flattening.
function _row_major_bytes(arr::AbstractArray)
    flat = if ndims(arr) <= 1
        Vector(vec(arr))
    else
        Vector(vec(permutedims(arr, reverse(ntuple(identity, ndims(arr))))))
    end
    return collect(reinterpret(UInt8, flat))
end

# `name` is a per-association attribute carried on the binding structs (matching
# InfrastructureSystems.jl); it is not part of the deduplicated core data type.
# `name` is required.
_maybe_string(::Nothing) = nothing
_maybe_string(s::AbstractString) = String(s)

# ---- Single time series ---------------------------------------------------

struct SingleTimeSeries
    initial_timestamp :: DateTime
    resolution        :: Period
    "Values: a 1-D vector (scalar per step) or N-D array (dim 1 = time)."
    data              :: AbstractArray
    "Association name (required; the same array may be stored under different names)."
    name              :: String
    "Opaque logical-type tag for the binding to reconstruct domain objects."
    logical_type      :: Union{Nothing,String}
end

SingleTimeSeries(initial, resolution, data::AbstractArray, name::AbstractString;
                 logical_type::Union{Nothing,AbstractString}=nothing) =
    SingleTimeSeries(initial, resolution, data, String(name),
                     _maybe_string(logical_type))

# ---- Non-sequential time series -------------------------------------------

struct NonSequentialTimeSeries
    timestamps  :: Vector{DateTime}
    "Values: a 1-D vector with one value per timestamp."
    data        :: AbstractVector
    "Association name (required)."
    name        :: String
    "Opaque logical-type tag for the binding to reconstruct domain objects."
    logical_type :: Union{Nothing,String}

    function NonSequentialTimeSeries(timestamps, data, name::AbstractString;
            logical_type::Union{Nothing,AbstractString}=nothing)
        length(timestamps) == length(data) ||
            throw(InvalidParameterError("timestamp count must match data length"))
        all(timestamps[i] < timestamps[i + 1] for i in 1:(length(timestamps) - 1)) ||
            throw(InvalidParameterError("timestamps must be strictly increasing"))
        new(Vector{DateTime}(timestamps), data, String(name),
            _maybe_string(logical_type))
    end
end

# ---- Forecast types -------------------------------------------------------
#
# Dense forecasts mirror the InfrastructureSystems.jl objects. `data` is a Julia
# (column-major) array in the canonical shape noted on each type; it round-trips
# through `add_time_series!` / `get_time_series`. `DeterministicSingleTimeSeries`
# is a marker type with no materialized form: it is derived from a stored
# `SingleTimeSeries` via `transform_single_time_series!` and read back as a
# `Deterministic` (see the type below).

"""
    AbstractDeterministic

Supertype of [`Deterministic`] and [`DeterministicSingleTimeSeries`], mirroring
InfrastructureSystems.jl. Use it as the requested type to read whichever of the
two concrete forecasts is stored under an identity:
`get_time_series(AbstractDeterministic, store, owner_id, owner_category, name)`.
The concrete types match only themselves; the family is resolved authoritatively
by the Rust core (no guess-and-retry), which errors if both concrete types share
the identity.
"""
abstract type AbstractDeterministic end

struct Deterministic <: AbstractDeterministic
    initial_timestamp :: DateTime
    resolution        :: Period
    horizon           :: Period
    interval          :: Period
    count             :: Int
    "Values with canonical shape `(H, count, element_dims...)`."
    data              :: AbstractArray
    "Association name (required)."
    name              :: String
    "Opaque logical-type tag for the binding to reconstruct domain objects."
    logical_type      :: Union{Nothing,String}
end

Deterministic(initial, resolution, horizon, interval, count, data::AbstractArray,
              name::AbstractString;
              logical_type::Union{Nothing,AbstractString}=nothing) =
    Deterministic(initial, resolution, horizon, interval, Int(count), data, String(name),
                  _maybe_string(logical_type))

struct Probabilistic
    initial_timestamp :: DateTime
    resolution        :: Period
    horizon           :: Period
    interval          :: Period
    count             :: Int
    percentiles       :: Vector{Float64}
    "Values with canonical shape `(num_percentiles, H, count, element_dims...)`."
    data              :: AbstractArray
    "Association name (required)."
    name              :: String
    "Opaque logical-type tag for the binding to reconstruct domain objects."
    logical_type      :: Union{Nothing,String}
end

Probabilistic(initial, resolution, horizon, interval, count, percentiles, data::AbstractArray,
              name::AbstractString;
              logical_type::Union{Nothing,AbstractString}=nothing) =
    Probabilistic(initial, resolution, horizon, interval, Int(count),
                  Vector{Float64}(percentiles), data, String(name),
                  _maybe_string(logical_type))

struct Scenarios
    initial_timestamp :: DateTime
    resolution        :: Period
    horizon           :: Period
    interval          :: Period
    count             :: Int
    scenario_count    :: Int
    "Values with canonical shape `(scenario_count, H, count, element_dims...)`."
    data              :: AbstractArray
    "Association name (required)."
    name              :: String
    "Opaque logical-type tag for the binding to reconstruct domain objects."
    logical_type      :: Union{Nothing,String}
end

# `scenario_count` defaults to the leading axis of `data`.
Scenarios(initial, resolution, horizon, interval, count, data::AbstractArray,
          name::AbstractString;
          logical_type::Union{Nothing,AbstractString}=nothing) =
    Scenarios(initial, resolution, horizon, interval, Int(count), size(data, 1), data,
              String(name), _maybe_string(logical_type))

"""
    DeterministicSingleTimeSeries

Marker type naming a forecast derived from a `SingleTimeSeries` via
`transform_single_time_series!` (mirrors the InfrastructureSystems.jl type). It
is never constructed or added directly and has no materialized struct: reading
one — e.g. `get_time_series(DeterministicSingleTimeSeries, store, key)` — returns
a [`Deterministic`]. It surfaces as the `time_series_type` of keys returned by
`get_time_series_keys` / `key_info`.
"""
abstract type DeterministicSingleTimeSeries <: AbstractDeterministic end

# ---- Keys -----------------------------------------------------------------

mutable struct TimeSeriesKey
    handle :: Ptr{Cvoid}
    function TimeSeriesKey(handle::Ptr{Cvoid})
        k = new(handle)
        finalizer(_finalize_key, k)
        k
    end
end

function _finalize_key(k::TimeSeriesKey)
    if k.handle != C_NULL
        ccall((:ts_key_free, lib_path()), Cvoid, (Ptr{Cvoid},), k.handle)
        k.handle = C_NULL
    end
end

# ---- Store ----------------------------------------------------------------

mutable struct Store
    handle :: Ptr{Cvoid}
    function Store(handle::Ptr{Cvoid})
        s = new(handle)
        finalizer(close!, s)
        s
    end
end

function close!(s::Store)
    if s.handle != C_NULL
        ccall((:ts_store_free, lib_path()), Cvoid, (Ptr{Cvoid},), s.handle)
        s.handle = C_NULL
    end
end

"""
    Store(; in_memory=true, path=nothing,
            compression=:deflate, compression_level=3, shuffle=true)

Construct a new store. Pass `path` (and `in_memory=false`) to persist to a
NetCDF file on disk.

`compression` selects the on-disk filter for NetCDF data variables:
`:deflate` (default) applies DEFLATE at `compression_level` (0–9) with optional
byte `shuffle`; `:none` disables compression. The setting is ignored for
in-memory stores and is persisted so later appends reuse it.
"""
function Store(; in_memory::Bool=true, path::Union{Nothing,AbstractString}=nothing,
               compression::Union{Symbol,AbstractString}=:deflate,
               compression_level::Integer=3, shuffle::Bool=true)
    kind = Symbol(compression)
    compression_kind = if kind === :none
        UInt8(0)
    elseif kind === :deflate
        UInt8(1)
    else
        throw(ArgumentError("unknown compression $(repr(compression)), expected :deflate or :none"))
    end
    out = Ref{Ptr{Cvoid}}(C_NULL)
    cpath = path === nothing ? C_NULL : pointer(String(path))
    code = ccall((:ts_store_create_with_compression, lib_path()), Int32,
                 (Cstring, Bool, UInt8, UInt8, Bool, Ref{Ptr{Cvoid}}),
                 cpath, in_memory, compression_kind, UInt8(compression_level), shuffle, out)
    _check(code)
    return Store(out[])
end

"""
    open_store(path; read_only=false)

Open an existing on-disk store.
"""
function open_store(path::AbstractString; read_only::Bool=false)
    out = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall((:ts_store_open, lib_path()), Int32,
                 (Cstring, Bool, Ref{Ptr{Cvoid}}),
                 path, read_only, out)
    _check(code)
    return Store(out[])
end

# ---- Operations -----------------------------------------------------------

# Convert a DateTime to Unix milliseconds.
function _to_unix_ms(dt::DateTime)
    return Int64(Dates.datetime2unix(dt) * 1000)
end

# Convert milliseconds since epoch back into a DateTime.
function _from_unix_ms(ms::Int64)
    return Dates.unix2datetime(ms / 1000)
end

function _resolution_to_ms(p::Period)
    Dates.toms(p)
end

"""
    add_time_series!(store, owner_id, owner_type, owner_category, ts;
                     features=Dict(), units=nothing)

`owner_id` identifies the owning component / supplemental attribute (a signed
64-bit integer). The association `name` comes from the time series object
(`ts.name`).
"""
function add_time_series!(
    store::Store,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::SingleTimeSeries;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=ts.logical_type,
)
    name = ts.name
    initial_ms = _to_unix_ms(ts.initial_timestamp)
    resolution_ms = _resolution_to_ms(ts.resolution)
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    units_ptr = units === nothing ? C_NULL : pointer(String(units))
    logical_ptr = logical_type === nothing ? C_NULL : pointer(String(logical_type))

    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:ts_store_add_single, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Cstring, Int32, Cstring, Int64, Int64,
         Int32, UInt64, Ptr{UInt64}, Ptr{UInt8}, UInt64, Cstring,
         Cstring, Cstring, Ref{Ptr{Cvoid}}),
        store.handle,
        Int64(owner_id),
        owner_type,
        Int32(Int(owner_category)),
        name,
        initial_ms,
        resolution_ms,
        dtype,
        UInt64(length(dims)),
        dims,
        bytes,
        UInt64(length(bytes)),
        logical_ptr,
        features_json,
        units_ptr,
        out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

function add_time_series!(
    store::Store,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::NonSequentialTimeSeries;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=ts.logical_type,
)
    name = ts.name
    timestamps = Int64[_to_unix_ms(timestamp) for timestamp in ts.timestamps]
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[length(ts.data)]
    bytes = _row_major_bytes(ts.data)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    units_ptr = units === nothing ? C_NULL : pointer(String(units))
    logical_ptr = logical_type === nothing ? C_NULL : pointer(String(logical_type))
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:ts_store_add_non_sequential, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Cstring, Int32, Cstring, Ptr{Int64}, UInt64,
         Int32, UInt64, Ptr{UInt64}, Ptr{UInt8}, UInt64, Cstring,
         Cstring, Cstring, Ref{Ptr{Cvoid}}),
        store.handle, Int64(owner_id), owner_type, Int32(Int(owner_category)), name,
        timestamps, UInt64(length(timestamps)), dtype, UInt64(1), dims, bytes,
        UInt64(length(bytes)), logical_ptr, features_json, units_ptr,
        out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

"""
    get_metadata(store, owner_id, owner_category, name; resolution, features=Dict())

Look up a SingleTimeSeries by attributes and return a named tuple of
`(initial_timestamp, resolution, length, data_hash)`. `owner_category` is the
owner's `OwnerCategory` (`Component` or `SupplementalAttribute`). `data_hash` is
the 32-byte content hash as a `Vector{UInt8}`. Throws `NotFoundError` if absent.
"""
function get_metadata(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    out_initial = Ref{Int64}(0)
    out_resolution = Ref{Int64}(0)
    out_length = Ref{UInt64}(0)
    out_hash = Vector{UInt8}(undef, 32)
    out_dtype = Ref{Int32}(0)
    lt_buf = Vector{UInt8}(undef, 256)
    out_lt_len = Ref{UInt64}(0)
    code = ccall(
        (:ts_store_get_metadata, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Int32, Cstring, Int64, Cstring,
         Ref{Int64}, Ref{Int64}, Ref{UInt64}, Ptr{UInt8}, Ref{Int32},
         Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle, Int64(owner_id), _category_int(owner_category), name, resolution_ms, features_json,
        out_initial, out_resolution, out_length, out_hash, out_dtype,
        lt_buf, UInt64(length(lt_buf)), out_lt_len,
    )
    _check(code)
    res_ms = out_resolution[]
    n = min(Int(out_lt_len[]), length(lt_buf))
    logical_type = n == 0 ? nothing : String(lt_buf[1:n])
    return (
        initial_timestamp=_from_unix_ms(out_initial[]),
        resolution=Millisecond(res_ms),
        length=Int(out_length[]),
        data_hash=out_hash,
        dtype=_julia_dtype(out_dtype[]),
        logical_type=logical_type,
    )
end

"""
    get_forecast_metadata(store, owner_id, owner_category, name, ts_type; resolution, features=Dict())

Return `(initial_timestamp, resolution, horizon, interval, count, length, data_hash)`
for a stored forecast of integer `ts_type` (see the `TS_TYPE_*` constants).
`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`).
"""
function get_forecast_metadata(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts_type::Integer;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    out_initial = Ref{Int64}(0); out_resolution = Ref{Int64}(0)
    out_horizon = Ref{Int64}(0); out_interval = Ref{Int64}(0)
    out_count = Ref{UInt64}(0); out_length = Ref{UInt64}(0)
    out_hash = Vector{UInt8}(undef, 32)
    lt_buf = Vector{UInt8}(undef, 256); out_lt_len = Ref{UInt64}(0)
    code = ccall(
        (:ts_store_get_forecast_metadata, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Int32, Cstring, Int32, Int64, Cstring,
         Ref{Int64}, Ref{Int64}, Ref{Int64}, Ref{Int64}, Ref{UInt64}, Ref{UInt64}, Ptr{UInt8},
         Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle, Int64(owner_id), _category_int(owner_category), name, Int32(ts_type), resolution_ms, features_json,
        out_initial, out_resolution, out_horizon, out_interval, out_count, out_length, out_hash,
        lt_buf, UInt64(length(lt_buf)), out_lt_len,
    )
    _check(code)
    n = min(Int(out_lt_len[]), length(lt_buf))
    logical_type = n == 0 ? nothing : String(lt_buf[1:n])
    return (
        initial_timestamp=_from_unix_ms(out_initial[]),
        resolution=Millisecond(out_resolution[]),
        horizon=Millisecond(out_horizon[]),
        interval=Millisecond(out_interval[]),
        count=Int(out_count[]),
        length=Int(out_length[]),
        data_hash=out_hash,
        logical_type=logical_type,
    )
end

"""
    get_array_by_hash(store, data_hash, ::Type{T}=Float64) -> Vector{T}

Fetch the full stored array for a 32-byte content hash, interpreting the raw
element bytes as `T`. For multi-dimensional element shapes the result is the
flat row-major vector; the caller reshapes using the known element shape.
"""
function get_array_by_hash(store::Store, data_hash::Vector{UInt8}, ::Type{T}=Float64) where {T}
    length(data_hash) == 32 || throw(InvalidParameterError("data_hash must be 32 bytes"))
    out_dtype = Ref{Int32}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:ts_store_get_array_by_hash, lib_path()), Int32,
        (Ptr{Cvoid}, Ptr{UInt8}, Ref{Int32}, Ref{Ptr{UInt8}}, Ref{UInt64}),
        store.handle, data_hash, out_dtype, out_data, out_len,
    )
    _check(code)
    nbytes = Int(out_len[])
    raw = unsafe_wrap(Array, out_data[], nbytes; own=false)
    bytes = copy(raw)
    ccall((:ts_buffer_free_u8, lib_path()), Cvoid, (Ptr{UInt8}, UInt64), out_data[], out_len[])
    return collect(reinterpret(T, bytes))
end

"""
    count_array_references(store, data_hash) -> (; sts, dst)

Count the `SingleTimeSeries` and `DeterministicSingleTimeSeries` associations
referencing the 32-byte content hash `data_hash`, across all owners. A
`DeterministicSingleTimeSeries` shares the underlying array of the
`SingleTimeSeries` it was derived from, so a caller uses these counts to decide
whether removing a `SingleTimeSeries` would orphan a DST. Resolved by a single
catalog query in the Rust core.
"""
function count_array_references(store::Store, data_hash::Vector{UInt8})
    length(data_hash) == 32 || throw(InvalidParameterError("data_hash must be 32 bytes"))
    out_sts = Ref{UInt64}(0)
    out_dst = Ref{UInt64}(0)
    code = ccall(
        (:ts_store_count_array_references, lib_path()), Int32,
        (Ptr{Cvoid}, Ptr{UInt8}, Ref{UInt64}, Ref{UInt64}),
        store.handle, data_hash, out_sts, out_dst,
    )
    _check(code)
    return (sts = Int(out_sts[]), dst = Int(out_dst[]))
end

"""
    get_array_nd(store, data_hash, T, dims) -> Array{T}

Fetch a stored array and reshape it to `dims` as a column-major Julia array. The
store hands back row-major bytes, so this is the inverse of the row-major encoding
used on write (handles the column-major ↔ row-major transpose for rank ≥ 2).
"""
function get_array_nd(store::Store, data_hash::Vector{UInt8}, ::Type{T}, dims) where {T}
    flat = get_array_by_hash(store, data_hash, T)
    n = length(dims)
    n <= 1 && return reshape(flat, dims...)
    return permutedims(reshape(flat, reverse(dims)...), reverse(ntuple(identity, n)))
end

"""
    has_time_series(store, owner_id, owner_category, name; resolution, features=Dict()) -> Bool

`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`).
"""
function has_time_series(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    out = Ref{Bool}(false)
    code = ccall(
        (:ts_store_has_by_attrs, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Int32, Cstring, Int64, Cstring, Ref{Bool}),
        store.handle, Int64(owner_id), _category_int(owner_category), name, resolution_ms, features_json, out,
    )
    _check(code)
    return out[]
end

"""
    has_for_owner(store, owner_id, owner_category; time_series_type=nothing) -> Bool

True if `(owner_id, owner_category)` has any time series, optionally restricted to
a single `time_series_type` code (the name-less existence query).
`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`).
"""
function has_for_owner(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory;
    time_series_type::Union{Nothing,Integer}=nothing,
)
    out = Ref{Bool}(false)
    use_type = time_series_type !== nothing
    code = ccall(
        (:ts_store_has_for_owner, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Int32, Int32, Bool, Ref{Bool}),
        store.handle, Int64(owner_id), _category_int(owner_category),
        Int32(use_type ? time_series_type : 0), use_type, out,
    )
    _check(code)
    return out[]
end

"""
    remove_time_series!(store, owner_id, owner_category, name; resolution, features=Dict())

`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`).
"""
function remove_time_series!(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    code = ccall(
        (:ts_store_remove_by_attrs, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Int32, Cstring, Int64, Cstring),
        store.handle, Int64(owner_id), _category_int(owner_category), name, resolution_ms, features_json,
    )
    _check(code)
    return nothing
end

function get_time_series(store::Store, key::TimeSeriesKey)
    out_initial = Ref{Int64}(0)
    out_resolution = Ref{Int64}(0)
    out_data = Ref{Ptr{Float64}}(C_NULL)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:ts_store_get_single, lib_path()), Int32,
        (Ptr{Cvoid}, Ptr{Cvoid}, Ref{Int64}, Ref{Int64}, Ref{Ptr{Float64}}, Ref{UInt64}),
        store.handle, key.handle, out_initial, out_resolution, out_data, out_len,
    )
    _check(code)

    n = Int(out_len[])
    # Copy into a Julia-managed array, then free the FFI buffer.
    raw = unsafe_wrap(Array, out_data[], n; own=false)
    data = copy(raw)
    ccall((:ts_buffer_free_f64, lib_path()), Cvoid, (Ptr{Float64}, UInt64), out_data[], out_len[])

    initial = _from_unix_ms(out_initial[])
    # resolution_ms is integer milliseconds.
    resolution = Millisecond(out_resolution[])
    assoc = _get_association(store, key)
    return SingleTimeSeries(initial, resolution, data, assoc.name)
end

function get_time_series(
    ::Type{NonSequentialTimeSeries},
    store::Store,
    key::TimeSeriesKey,
)
    out_timestamps = Ref{Ptr{Int64}}(C_NULL)
    out_timestamps_len = Ref{UInt64}(0)
    out_dtype = Ref{Int32}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL)
    out_data_len = Ref{UInt64}(0)
    code = ccall(
        (:ts_store_get_non_sequential, lib_path()), Int32,
        (Ptr{Cvoid}, Ptr{Cvoid}, Ref{Ptr{Int64}}, Ref{UInt64}, Ref{Int32},
         Ref{Ptr{UInt8}}, Ref{UInt64}),
        store.handle, key.handle, out_timestamps, out_timestamps_len, out_dtype,
        out_data, out_data_len,
    )
    _check(code)

    timestamp_ms = copy(unsafe_wrap(
        Array, out_timestamps[], Int(out_timestamps_len[]); own=false,
    ))
    ccall(
        (:ts_buffer_free_i64, lib_path()), Cvoid, (Ptr{Int64}, UInt64),
        out_timestamps[], out_timestamps_len[],
    )
    bytes = copy(unsafe_wrap(Array, out_data[], Int(out_data_len[]); own=false))
    ccall(
        (:ts_buffer_free_u8, lib_path()), Cvoid, (Ptr{UInt8}, UInt64),
        out_data[], out_data_len[],
    )
    dtype = _julia_dtype(out_dtype[])
    values = collect(reinterpret(dtype, bytes))
    assoc = _get_association(store, key)
    return NonSequentialTimeSeries(_from_unix_ms.(timestamp_ms), values, assoc.name)
end

# ---- Attribute-addressed static reads --------------------------------------
#
# Every type supports both calling conventions. `get_time_series(T, store, key)`
# is keyed by a `TimeSeriesKey` handle (returned by `add_time_series!`);
# `get_time_series(T, store, owner_id, name; ...)` builds a key from attributes
# (the same `(owner_id, name, resolution, features)` addressing used by
# `has_time_series` / `remove_time_series!` / `get_metadata`) and routes through
# the key-based reader.

# Build a `TimeSeriesKey` from attributes via the FFI key constructor.
function _make_key(
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts_type::Integer;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = _features_arg(features)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:ts_make_key_from_attrs, lib_path()), Int32,
        (Int64, Int32, Cstring, Int32, Int64, Cstring, Ref{Ptr{Cvoid}}),
        Int64(owner_id), _category_int(owner_category), name, Int32(ts_type), resolution_ms, features_json, out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

# Fetch the per-association `name` for a key (the attribute the read FFIs don't
# return), to populate the struct on read.
function _get_association(store::Store, key::TimeSeriesKey)
    name_len = Ref{UInt64}(0)
    code = ccall(
        (:ts_store_get_association, lib_path()), Int32,
        (Ptr{Cvoid}, Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle, key.handle, C_NULL, UInt64(0), name_len,
    )
    _check(code)
    name_buf = Vector{UInt8}(undef, Int(name_len[]) + 1)
    code = ccall(
        (:ts_store_get_association, lib_path()), Int32,
        (Ptr{Cvoid}, Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle, key.handle,
        name_buf, UInt64(length(name_buf)), name_len,
    )
    _check(code)
    name = String(name_buf[1:Int(name_len[])])
    return (name=name,)
end

# Association attributes for an attribute-addressed read: build the matching key,
# then look up `name`.
_assoc_attrs(store::Store, owner_id::Integer, owner_category::OwnerCategory, name::AbstractString, ts_type::Integer;
             resolution::Union{Nothing,Period}=nothing, features::AbstractDict=Dict{String,Any}()) =
    _get_association(store, _make_key(owner_id, owner_category, name, ts_type; resolution=resolution, features=features))

"""
    get_time_series_keys(store, owner_id, owner_category) -> Vector{TimeSeriesKey}

Every key associated with `(owner_id, owner_category)`, one per stored association
(including `DeterministicSingleTimeSeries` rows derived by
`transform_single_time_series!`). `owner_category` is the owner's `OwnerCategory`
(`Component` or `SupplementalAttribute`). Each key can be passed to the key-based
`get_time_series(Type, store, key)` readers — the way to read a transform-derived
forecast by key.
"""
function get_time_series_keys(store::Store, owner_id::Integer, owner_category::OwnerCategory)
    out_keys = Ref{Ptr{Ptr{Cvoid}}}(C_NULL)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:ts_store_get_time_series_keys, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Int32, Ref{Ptr{Ptr{Cvoid}}}, Ref{UInt64}),
        store.handle, Int64(owner_id), _category_int(owner_category), out_keys, out_len,
    )
    _check(code)
    n = Int(out_len[])
    keys = Vector{TimeSeriesKey}(undef, n)
    if n > 0
        # Copy each owned handle into a finalized wrapper, then free the array
        # buffer (the wrappers own the handles and free them via ts_key_free).
        raw = unsafe_wrap(Array, out_keys[], n; own=false)
        for i in 1:n
            keys[i] = TimeSeriesKey(raw[i])
        end
        ccall(
            (:ts_keys_buffer_free, lib_path()), Cvoid, (Ptr{Ptr{Cvoid}}, UInt64),
            out_keys[], out_len[],
        )
    end
    return keys
end

# The Julia time series type for a key's integer type code.
_type_for_code(code::Integer) =
    code == TS_TYPE_SINGLE                  ? SingleTimeSeries :
    code == TS_TYPE_NON_SEQUENTIAL          ? NonSequentialTimeSeries :
    code == TS_TYPE_DETERMINISTIC           ? Deterministic :
    code == TS_TYPE_DETERMINISTIC_SINGLE    ? DeterministicSingleTimeSeries :
    code == TS_TYPE_PROBABILISTIC           ? Probabilistic :
    code == TS_TYPE_SCENARIOS               ? Scenarios :
    throw(InvalidParameterError("unknown time series type code $code"))

# The Julia time series type for a metadata row's type name (the `as_str` form).
_type_for_name(name::AbstractString) =
    name == "SingleTimeSeries"               ? SingleTimeSeries :
    name == "NonSequentialTimeSeries"        ? NonSequentialTimeSeries :
    name == "Deterministic"                  ? Deterministic :
    name == "DeterministicSingleTimeSeries"  ? DeterministicSingleTimeSeries :
    name == "Probabilistic"                  ? Probabilistic :
    name == "Scenarios"                      ? Scenarios :
    throw(InvalidParameterError("unknown time series type name $name"))

_row_ms(x) = x === nothing ? nothing : Millisecond(Int64(x))
_row_int(x) = x === nothing ? nothing : Int(x)

function _decode_key_row(r::AbstractDict)
    its = r["initial_timestamp_ms"]
    return (
        owner_id = Int64(r["owner_id"]),
        owner_category = String(r["owner_category"]),
        time_series_type = _type_for_name(r["time_series_type"]),
        name = String(r["name"]),
        initial_timestamp = its === nothing ? nothing : _from_unix_ms(Int64(its)),
        resolution = _row_ms(r["resolution_ms"]),
        length = _row_int(r["length"]),
        horizon = _row_ms(r["horizon_ms"]),
        interval = _row_ms(r["interval_ms"]),
        count = _row_int(r["count"]),
        features = Dict{String, Any}(r["features"]),
    )
end

"""
    list_keys(store; owner_id=nothing, owner_category=nothing, time_series_type=nothing,
              name=nothing, resolution=nothing, features=Dict()) -> Vector{NamedTuple}

List the key of every stored time series matching the (all-optional, independent)
filters. With no filter set the whole store is listed.

- `owner_id`, `owner_category` (an `OwnerCategory`) — scope to one owner.
- `time_series_type` — a `TS_TYPE_*` integer code.
- `name` — exact association name.
- `resolution` — a `Period`.
- `features` — match keys whose features include all the given entries (subset).

Each key is a `NamedTuple` with `owner_id`, `owner_category`, `time_series_type`
(the Julia type), `name`, `initial_timestamp`, `resolution`, `length`, `horizon`,
`interval`, `count`, and `features`; fields that do not apply to a key's type are
`nothing`. Physical storage detail (`data_hash`, `logical_type`, `percentiles`) is
not on the key — read it via [`get_metadata`](@ref) / [`get_forecast_metadata`](@ref).
"""
function list_keys(store::Store; owner_id::Union{Nothing, Integer} = nothing,
                   owner_category::Union{Nothing, OwnerCategory} = nothing,
                   time_series_type::Union{Nothing, Integer} = nothing,
                   name::Union{Nothing, AbstractString} = nothing,
                   resolution::Union{Nothing, Period} = nothing,
                   features::AbstractDict = Dict{String, Any}())
    has_owner = owner_id !== nothing
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    has_category = owner_category !== nothing
    category_arg = has_category ? _category_int(owner_category) : Int32(0)
    has_type = time_series_type !== nothing
    type_arg = has_type ? Int32(time_series_type) : Int32(0)
    name_arg = name === nothing ? C_NULL : String(name)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    out_len = Ref{UInt64}(0)
    code = ccall((:ts_store_list_keys, lib_path()), Int32,
                 (Ptr{Cvoid}, Bool, Int64, Bool, Int32, Bool, Int32, Cstring, Int64,
                  Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
                 store.handle, has_owner, owner_arg, has_category, category_arg,
                 has_type, type_arg, name_arg, resolution_ms, features_json,
                 C_NULL, UInt64(0), out_len)
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall((:ts_store_list_keys, lib_path()), Int32,
                 (Ptr{Cvoid}, Bool, Int64, Bool, Int32, Bool, Int32, Cstring, Int64,
                  Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
                 store.handle, has_owner, owner_arg, has_category, category_arg,
                 has_type, type_arg, name_arg, resolution_ms, features_json,
                 buf, UInt64(length(buf)), out_len)
    _check(code)
    rows = JSON.parse(String(buf[1:Int(out_len[])]))
    return [_decode_key_row(r) for r in rows]
end

"""
    key_info(key) -> NamedTuple

Inspect an opaque `TimeSeriesKey` (e.g. one returned by `get_time_series_keys`):
returns `(owner_id, owner_category, name, time_series_type, resolution, features)`.
`owner_category` is an `OwnerCategory` (`Component` or `SupplementalAttribute`).
`time_series_type` is the Julia type (one of `SingleTimeSeries`,
`NonSequentialTimeSeries`, `Deterministic`, `DeterministicSingleTimeSeries`,
`Probabilistic`, `Scenarios`) — pass it straight to
`get_time_series(time_series_type, store, key)`. `features` is a `Dict` (empty
when none).
"""
function key_info(key::TimeSeriesKey)
    out_type = Ref{Int32}(0)
    out_res = Ref{Int64}(0)
    out_owner = Ref{Int64}(0)
    out_category = Ref{Int32}(0)
    name_len = Ref{UInt64}(0)
    feat_len = Ref{UInt64}(0)
    # Probe the string lengths (type, resolution, owner id, and owner category are
    # filled on this call too).
    code = ccall(
        (:ts_key_attributes, lib_path()), Int32,
        (Ptr{Cvoid}, Ref{Int32}, Ref{Int64}, Ref{Int64}, Ref{Int32},
         Ptr{UInt8}, UInt64, Ref{UInt64},
         Ptr{UInt8}, UInt64, Ref{UInt64}),
        key.handle, out_type, out_res, out_owner, out_category,
        C_NULL, UInt64(0), name_len,
        C_NULL, UInt64(0), feat_len,
    )
    _check(code)
    name_buf = Vector{UInt8}(undef, Int(name_len[]) + 1)
    feat_buf = Vector{UInt8}(undef, Int(feat_len[]) + 1)
    code = ccall(
        (:ts_key_attributes, lib_path()), Int32,
        (Ptr{Cvoid}, Ref{Int32}, Ref{Int64}, Ref{Int64}, Ref{Int32},
         Ptr{UInt8}, UInt64, Ref{UInt64},
         Ptr{UInt8}, UInt64, Ref{UInt64}),
        key.handle, out_type, out_res, out_owner, out_category,
        name_buf, UInt64(length(name_buf)), name_len,
        feat_buf, UInt64(length(feat_buf)), feat_len,
    )
    _check(code)
    name = String(name_buf[1:Int(name_len[])])
    features = JSON.parse(String(feat_buf[1:Int(feat_len[])]))
    resolution = out_res[] == 0 ? nothing : Millisecond(out_res[])
    return (
        owner_id         = out_owner[],
        owner_category   = OwnerCategory(Int(out_category[])),
        name             = name,
        time_series_type = _type_for_code(out_type[]),
        resolution       = resolution,
        features         = features,
    )
end

# Key-based alias so `SingleTimeSeries` matches the `get_time_series(T, store, key)`
# shape the other types use (the bare `get_time_series(store, key)` form is kept).
get_time_series(::Type{SingleTimeSeries}, store::Store, key::TimeSeriesKey) =
    get_time_series(store, key)

"""
    get_time_series(SingleTimeSeries, store, owner_id, owner_category, name; resolution, features)

Attribute-addressed counterpart to `get_time_series(store, key)`. `owner_category`
is the owner's `OwnerCategory` (`Component` or `SupplementalAttribute`).
"""
function get_time_series(
    ::Type{SingleTimeSeries},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    key = _make_key(owner_id, owner_category, name, TS_TYPE_SINGLE; resolution=resolution, features=features)
    return get_time_series(store, key)
end

"""
    get_time_series(NonSequentialTimeSeries, store, owner_id, owner_category, name; resolution, features)

Attribute-addressed counterpart to `get_time_series(NonSequentialTimeSeries, store, key)`.
`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`).
"""
function get_time_series(
    ::Type{NonSequentialTimeSeries},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    key = _make_key(owner_id, owner_category, name, TS_TYPE_NON_SEQUENTIAL; resolution=resolution, features=features)
    return get_time_series(NonSequentialTimeSeries, store, key)
end

function remove_time_series!(store::Store, key::TimeSeriesKey)
    code = ccall((:ts_store_remove, lib_path()), Int32,
                 (Ptr{Cvoid}, Ptr{Cvoid}), store.handle, key.handle)
    _check(code)
    return nothing
end

function has_time_series(store::Store, key::TimeSeriesKey)
    out = Ref{Bool}(false)
    code = ccall((:ts_store_has, lib_path()), Int32,
                 (Ptr{Cvoid}, Ptr{Cvoid}, Ref{Bool}),
                 store.handle, key.handle, out)
    _check(code)
    return out[]
end

function get_counts(store::Store)
    a = Ref{Int64}(0); b = Ref{Int64}(0); c = Ref{Int64}(0)
    code = ccall((:ts_store_counts, lib_path()), Int32,
                 (Ptr{Cvoid}, Ref{Int64}, Ref{Int64}, Ref{Int64}),
                 store.handle, a, b, c)
    _check(code)
    return (components_with_time_series=a[], static_time_series=b[], forecasts=c[])
end

"""
    counts_by_type(store) -> Vector{NamedTuple}

Association count grouped by time series type, as `(time_series_type, count)`
NamedTuples (`time_series_type` is the Julia type). One catalog query in the core.
"""
function counts_by_type(store::Store)
    out_len = Ref{UInt64}(0)
    code = ccall((:ts_store_counts_by_type, lib_path()), Int32,
                 (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
                 store.handle, C_NULL, UInt64(0), out_len)
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall((:ts_store_counts_by_type, lib_path()), Int32,
                 (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
                 store.handle, buf, UInt64(length(buf)), out_len)
    _check(code)
    rows = JSON.parse(String(buf[1:Int(out_len[])]))
    return [(time_series_type=_type_for_name(r["time_series_type"]), count=Int(r["count"]))
            for r in rows]
end

"""
    num_distinct_arrays(store) -> Int

Number of distinct stored arrays (content hashes); series that share an array
(de-duplicated by content) count once.
"""
function num_distinct_arrays(store::Store)
    out = Ref{Int64}(0)
    code = ccall((:ts_store_num_distinct_arrays, lib_path()), Int32,
                 (Ptr{Cvoid}, Ref{Int64}), store.handle, out)
    _check(code)
    return Int(out[])
end

"""
    time_series_counts(store) -> NamedTuple

Distinct owners per category and distinct stored arrays per kind:
`(components_with_time_series, supplemental_attributes_with_time_series,
static_time_series_count, forecast_count)`. Arrays shared by content count once.
"""
function time_series_counts(store::Store)
    a = Ref{Int64}(0); b = Ref{Int64}(0); c = Ref{Int64}(0); d = Ref{Int64}(0)
    code = ccall((:ts_store_counts_detailed, lib_path()), Int32,
                 (Ptr{Cvoid}, Ref{Int64}, Ref{Int64}, Ref{Int64}, Ref{Int64}),
                 store.handle, a, b, c, d)
    _check(code)
    return (components_with_time_series=a[],
            supplemental_attributes_with_time_series=b[],
            static_time_series_count=c[], forecast_count=d[])
end

"""
    list_owner_ids(store, owner_category; time_series_type=nothing, resolution=nothing) -> Vector{Int}

Distinct owner ids of `owner_category` (an `OwnerCategory`) that have a time
series, optionally restricted by `time_series_type` (a `TS_TYPE_*` integer code)
and/or `resolution` (a `Period`).
"""
function list_owner_ids(store::Store, owner_category::OwnerCategory;
                        time_series_type::Union{Nothing, Integer} = nothing,
                        resolution::Union{Nothing, Period} = nothing)
    has_type = time_series_type !== nothing
    type_arg = has_type ? Int32(time_series_type) : Int32(0)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    cat = _category_int(owner_category)
    out_len = Ref{UInt64}(0)
    code = ccall((:ts_store_list_owner_ids, lib_path()), Int32,
                 (Ptr{Cvoid}, Int32, Bool, Int32, Int64, Ptr{UInt8}, UInt64, Ref{UInt64}),
                 store.handle, cat, has_type, type_arg, resolution_ms, C_NULL, UInt64(0), out_len)
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall((:ts_store_list_owner_ids, lib_path()), Int32,
                 (Ptr{Cvoid}, Int32, Bool, Int32, Int64, Ptr{UInt8}, UInt64, Ref{UInt64}),
                 store.handle, cat, has_type, type_arg, resolution_ms, buf, UInt64(length(buf)), out_len)
    _check(code)
    ids = JSON.parse(String(buf[1:Int(out_len[])]))
    return Int[Int(i) for i in ids]
end

function _decode_static_summary_row(r::AbstractDict)
    its = r["initial_timestamp_ms"]
    return (
        owner_type = String(r["owner_type"]),
        owner_category = String(r["owner_category"]),
        time_series_type = _type_for_name(r["time_series_type"]),
        name = String(r["name"]),
        initial_timestamp = its === nothing ? nothing : _from_unix_ms(Int64(its)),
        resolution = _row_ms(r["resolution_ms"]),
        time_step_count = _row_int(r["time_step_count"]),
        count = Int(r["count"]),
    )
end

function _decode_forecast_summary_row(r::AbstractDict)
    its = r["initial_timestamp_ms"]
    return (
        owner_type = String(r["owner_type"]),
        owner_category = String(r["owner_category"]),
        time_series_type = _type_for_name(r["time_series_type"]),
        name = String(r["name"]),
        initial_timestamp = its === nothing ? nothing : _from_unix_ms(Int64(its)),
        resolution = _row_ms(r["resolution_ms"]),
        horizon = _row_ms(r["horizon_ms"]),
        interval = _row_ms(r["interval_ms"]),
        window_count = _row_int(r["window_count"]),
        count = Int(r["count"]),
    )
end

"""
    static_summary(store) -> Vector{NamedTuple}

Grouped static-series (SingleTimeSeries + NonSequentialTimeSeries) summary: one
row per distinct `(owner_type, owner_category, time_series_type, name,
initial_timestamp, resolution, time_step_count)` with `count` = the number of
associations in the group. The core does the GROUP BY; callers build any
presentation table (e.g. a DataFrame).
"""
function static_summary(store::Store)
    out_len = Ref{UInt64}(0)
    code = ccall((:ts_store_static_summary, lib_path()), Int32,
                 (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
                 store.handle, C_NULL, UInt64(0), out_len)
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall((:ts_store_static_summary, lib_path()), Int32,
                 (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
                 store.handle, buf, UInt64(length(buf)), out_len)
    _check(code)
    rows = JSON.parse(String(buf[1:Int(out_len[])]))
    return [_decode_static_summary_row(r) for r in rows]
end

"""
    forecast_summary(store) -> Vector{NamedTuple}

Grouped forecast summary: one row per distinct `(owner_type, owner_category,
time_series_type, name, initial_timestamp, resolution, horizon, interval,
window_count)` with `count` = the number of associations in the group.
"""
function forecast_summary(store::Store)
    out_len = Ref{UInt64}(0)
    code = ccall((:ts_store_forecast_summary, lib_path()), Int32,
                 (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
                 store.handle, C_NULL, UInt64(0), out_len)
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall((:ts_store_forecast_summary, lib_path()), Int32,
                 (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
                 store.handle, buf, UInt64(length(buf)), out_len)
    _check(code)
    rows = JSON.parse(String(buf[1:Int(out_len[])]))
    return [_decode_forecast_summary_row(r) for r in rows]
end

"""
    get_forecast_parameters(store; resolution=nothing, interval=nothing)

Return the store's forecast parameters as a NamedTuple
`(horizon, interval, count, resolution, initial_timestamp)`, optionally restricted
to forecasts with the given `resolution` and/or `interval` (`Period`s).
`horizon`, `interval`, and `resolution` are `Millisecond` periods (or `nothing`);
`count` is an `Int` (or `nothing`); `initial_timestamp` is a `DateTime` (or
`nothing`). Every field is `nothing` when no forecast matches.
"""
function get_forecast_parameters(store::Store; resolution::Union{Nothing, Period} = nothing,
                                 interval::Union{Nothing, Period} = nothing)
    fres = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    fivl = interval === nothing ? Int64(0) : _resolution_to_ms(interval)
    present = Ref{Bool}(false)
    horizon_out = Ref{Int64}(-1); interval_out = Ref{Int64}(-1)
    count = Ref{Int64}(-1); resolution_out = Ref{Int64}(-1); initial_out = Ref{Int64}(-1)
    code = ccall((:ts_store_get_forecast_parameters, lib_path()), Int32,
                 (Ptr{Cvoid}, Int64, Int64, Ref{Bool}, Ref{Int64}, Ref{Int64}, Ref{Int64},
                  Ref{Int64}, Ref{Int64}),
                 store.handle, fres, fivl, present, horizon_out, interval_out, count,
                 resolution_out, initial_out)
    _check(code)
    _ms(x) = x < 0 ? nothing : Millisecond(x)
    return (
        horizon=_ms(horizon_out[]),
        interval=_ms(interval_out[]),
        count=(count[] < 0 ? nothing : Int(count[])),
        resolution=_ms(resolution_out[]),
        initial_timestamp=(initial_out[] < 0 ? nothing : _from_unix_ms(initial_out[])),
    )
end

"""
    check_static_consistency(store) -> Union{Nothing, NamedTuple}

Return `(initial_timestamp, length)` shared by every `SingleTimeSeries`, or
`nothing` when there are none. Throws if the stored `SingleTimeSeries` disagree on
their `(initial_timestamp, length)`. One catalog query.
"""
function check_static_consistency(store::Store)
    present = Ref{Bool}(false); initial_ms = Ref{Int64}(0); len = Ref{Int64}(0)
    code = ccall((:ts_store_check_static_consistency, lib_path()), Int32,
                 (Ptr{Cvoid}, Ref{Bool}, Ref{Int64}, Ref{Int64}),
                 store.handle, present, initial_ms, len)
    _check(code)
    return present[] ? (initial_timestamp=_from_unix_ms(initial_ms[]), length=Int(len[])) : nothing
end

"""
    get_resolutions(store; time_series_type=nothing) -> Vector{Millisecond}

Return the distinct resolutions stored, ascending. When `time_series_type` (a
`TS_TYPE_*` integer code) is given the result is restricted to that type. This is
a single catalog query in the core rather than a scan of every association.
"""
function get_resolutions(store::Store; time_series_type::Union{Nothing, Integer} = nothing)
    has_type = time_series_type !== nothing
    type_arg = has_type ? Int32(time_series_type) : Int32(0)
    out_len = Ref{UInt64}(0)
    code = ccall((:ts_store_get_resolutions, lib_path()), Int32,
                 (Ptr{Cvoid}, Bool, Int32, Ptr{UInt8}, UInt64, Ref{UInt64}),
                 store.handle, has_type, type_arg, C_NULL, UInt64(0), out_len)
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall((:ts_store_get_resolutions, lib_path()), Int32,
                 (Ptr{Cvoid}, Bool, Int32, Ptr{UInt8}, UInt64, Ref{UInt64}),
                 store.handle, has_type, type_arg, buf, UInt64(length(buf)), out_len)
    _check(code)
    ms = JSON.parse(String(buf[1:Int(out_len[])]))
    return Millisecond[Millisecond(Int64(m)) for m in ms]
end

"""
    get_compression(store)

Return the store's compression policy as a NamedTuple `(compression, level,
shuffle)`. `compression` is `:deflate` or `:none`; `level` (0-9) and `shuffle`
apply to DEFLATE. For a store opened from disk this reflects the policy it was
created with; in-memory stores report `:none`.
"""
function get_compression(store::Store)
    kind = Ref{UInt8}(0); level = Ref{UInt8}(0); shuffle = Ref{Bool}(false)
    code = ccall((:ts_store_get_compression, lib_path()), Int32,
                 (Ptr{Cvoid}, Ref{UInt8}, Ref{UInt8}, Ref{Bool}),
                 store.handle, kind, level, shuffle)
    _check(code)
    return kind[] == 0 ? (compression=:none, level=Int(level[]), shuffle=shuffle[]) :
           (compression=:deflate, level=Int(level[]), shuffle=shuffle[])
end

function verify_integrity(store::Store)
    out = Ref{UInt64}(0)
    code = ccall((:ts_store_verify, lib_path()), Int32,
                 (Ptr{Cvoid}, Ref{UInt64}), store.handle, out)
    _check(code)
    return Int(out[])
end

function compact!(store::Store)
    code = ccall((:ts_store_compact, lib_path()), Int32,
                 (Ptr{Cvoid},), store.handle)
    _check(code)
    return nothing
end

"""
    flush!(store)

Flush pending writes (NetCDF arrays + SQLite metadata) to disk. After this the
on-disk `<path>.nc` and `<path>.sqlite` artifacts can be copied for persistence.
"""
function flush!(store::Store)
    code = ccall((:ts_store_flush, lib_path()), Int32,
                 (Ptr{Cvoid},), store.handle)
    _check(code)
    return nothing
end

"""
    clear!(store; owner_id=nothing, owner_category=nothing)

Remove all time series (data + metadata) from the store, or only those belonging
to a single owner. An owner is the pair `(owner_id, owner_category)`, so to scope
the clear to one owner pass both `owner_id` and `owner_category` (an
`OwnerCategory`). With neither given the whole store is cleared.
"""
function clear!(store::Store; owner_id::Union{Nothing, Integer} = nothing,
                owner_category::Union{Nothing, OwnerCategory} = nothing)
    has_owner = owner_id !== nothing
    if has_owner && owner_category === nothing
        throw(ArgumentError("clear! with owner_id also requires owner_category"))
    end
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    category_arg = has_owner ? _category_int(owner_category) : Int32(0)
    code = ccall((:ts_store_clear, lib_path()), Int32,
                 (Ptr{Cvoid}, Bool, Int64, Int32),
                 store.handle, has_owner, owner_arg, category_arg)
    _check(code)
    return nothing
end

"""
    replace_owner!(store, old_owner_id, new_owner_id, owner_category) -> Int

Reassign every time series owned by `(old_owner_id, owner_category)` to
`(new_owner_id, owner_category)`. `owner_category` is the owner's `OwnerCategory`
(`Component` or `SupplementalAttribute`). The underlying arrays are
content-addressed and shared, so only the association records change. Returns the
number of associations updated.
"""
function replace_owner!(store::Store, old_owner_id::Integer,
                        new_owner_id::Integer, owner_category::OwnerCategory)
    out = Ref{UInt64}(0)
    code = ccall((:ts_store_replace_owner, lib_path()), Int32,
                 (Ptr{Cvoid}, Int64, Int64, Int32, Ref{UInt64}),
                 store.handle, Int64(old_owner_id), Int64(new_owner_id),
                 _category_int(owner_category), out)
    _check(code)
    return Int(out[])
end

# ---- Forecasts -------------------------------------------------------------
#
# TimeSeriesType integer codes (must match the Rust `TimeSeriesType` enum):
const TS_TYPE_SINGLE                       = 0
const TS_TYPE_NON_SEQUENTIAL               = 1
const TS_TYPE_DETERMINISTIC                = 2
const TS_TYPE_DETERMINISTIC_SINGLE         = 3
const TS_TYPE_PROBABILISTIC                = 4
const TS_TYPE_SCENARIOS                    = 5
# Request-only family sentinel (never a stored type): matches a stored
# `Deterministic` or `DeterministicSingleTimeSeries`. The Rust core resolves it
# and reports the concrete type that matched. Must match `TS_TYPE_ABSTRACT_DETERMINISTIC`
# in the C ABI.
const TS_TYPE_ABSTRACT_DETERMINISTIC       = 100

_features_arg(features) = isempty(features) ? C_NULL : JSON.json(features)
_category_int(c::OwnerCategory) = Int32(Int(c))

"""
    add_time_series!(store, owner_id, owner_type, owner_category, ts::Deterministic; ...)
    add_time_series!(store, owner_id, owner_type, owner_category, ts::Scenarios; ...)

Add a dense forecast. `ts.data` is the canonical-shape Julia array and is stored
as a standalone array. The association `name` comes from the time series object.
"""
function add_time_series!(
    store::Store,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::Deterministic;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=ts.logical_type,
)
    return _add_dense_forecast!(
        store, owner_id, owner_type, owner_category, ts.name, TS_TYPE_DETERMINISTIC,
        ts.initial_timestamp, ts.resolution, ts.horizon, ts.interval, ts.count, ts.data;
        features=features, units=units, logical_type=logical_type,
    )
end

function add_time_series!(
    store::Store,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::Scenarios;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=ts.logical_type,
)
    return _add_dense_forecast!(
        store, owner_id, owner_type, owner_category, ts.name, TS_TYPE_SCENARIOS,
        ts.initial_timestamp, ts.resolution, ts.horizon, ts.interval, ts.count, ts.data;
        features=features, units=units, logical_type=logical_type,
    )
end

# Shared implementation: ccall the per-type C transport `ts_store_add_forecast`
# (Deterministic / Scenarios).
function _add_dense_forecast!(
    store::Store,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts_type::Integer,
    initial_timestamp::DateTime,
    resolution::Period,
    horizon::Period,
    interval::Period,
    count::Integer,
    data::AbstractArray;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=nothing,
)
    features_json = _features_arg(features)
    units_ptr = units === nothing ? C_NULL : String(units)
    logical_ptr = logical_type === nothing ? C_NULL : pointer(String(logical_type))
    dtype = _dtype_code(eltype(data))
    dims = UInt64[size(data)...]
    bytes = _row_major_bytes(data)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:ts_store_add_forecast, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Cstring, Int32, Cstring, Int32, Int64, Int64, Int64, Int64,
         UInt64, Int32, UInt64, Ptr{UInt64}, Ptr{UInt8}, UInt64, Cstring, Cstring, Cstring,
         Ref{Ptr{Cvoid}}),
        store.handle, Int64(owner_id), owner_type, _category_int(owner_category), name,
        Int32(ts_type), _to_unix_ms(initial_timestamp), _resolution_to_ms(resolution),
        _resolution_to_ms(horizon), _resolution_to_ms(interval), UInt64(count),
        dtype, UInt64(length(dims)), dims, bytes, UInt64(length(bytes)),
        logical_ptr, features_json, units_ptr, out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

"""
    transform_single_time_series!(store, horizon, interval; owner_category=nothing) -> Int

Derive `DeterministicSingleTimeSeries` forecasts from the stored `SingleTimeSeries`
associations (mirrors InfrastructureSystems.jl's `transform_single_time_series!`):
each is re-described as a DST sharing the same underlying array; `count` is derived
from each series' length. When `owner_category` is given (`Component` or
`SupplementalAttribute`) only series of that owner category are transformed;
otherwise every category is. Returns the number of series transformed.
"""
function transform_single_time_series!(
    store::Store, horizon::Period, interval::Period;
    owner_category::Union{Nothing, OwnerCategory} = nothing,
    resolution::Union{Nothing, Period} = nothing,
)
    cat = owner_category === nothing ? Int32(-1) : Int32(Int(owner_category))
    res_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    out_count = Ref{UInt64}(0)
    code = ccall(
        (:ts_store_transform_single_time_series, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Int64, Int32, Int64, Ref{UInt64}),
        store.handle, _resolution_to_ms(horizon), _resolution_to_ms(interval), cat, res_ms,
        out_count,
    )
    _check(code)
    return Int(out_count[])
end

"""True iff a time series of `ts_type` with the given attributes exists.
`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`)."""
function has_typed(
    store::Store, owner_id::Integer, owner_category::OwnerCategory, name::AbstractString, ts_type::Integer;
    resolution::Union{Nothing,Period}=nothing, features::AbstractDict=Dict{String,Any}(),
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = _features_arg(features)
    out = Ref{Bool}(false)
    code = ccall(
        (:ts_store_has_typed, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Int32, Cstring, Int32, Int64, Cstring, Ref{Bool}),
        store.handle, Int64(owner_id), _category_int(owner_category), name, Int32(ts_type), resolution_ms, features_json, out,
    )
    _check(code)
    return out[]
end

"""Remove a time series of `ts_type` by attributes. `owner_category` is the
owner's `OwnerCategory` (`Component` or `SupplementalAttribute`)."""
function remove_typed!(
    store::Store, owner_id::Integer, owner_category::OwnerCategory, name::AbstractString, ts_type::Integer;
    resolution::Union{Nothing,Period}=nothing, features::AbstractDict=Dict{String,Any}(),
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = _features_arg(features)
    code = ccall(
        (:ts_store_remove_typed, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Int32, Cstring, Int32, Int64, Cstring),
        store.handle, Int64(owner_id), _category_int(owner_category), name, Int32(ts_type), resolution_ms, features_json,
    )
    _check(code)
    return nothing
end

"""
    add_time_series!(store, owner_id, owner_type, owner_category, ts::Probabilistic; ...)

Add a `Probabilistic` forecast (carries a `percentiles` vector). The association
`name` comes from the time series object.
"""
function add_time_series!(
    store::Store,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::Probabilistic;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=ts.logical_type,
)
    name = ts.name
    initial_timestamp = ts.initial_timestamp
    resolution = ts.resolution
    horizon = ts.horizon
    interval = ts.interval
    count = ts.count
    percentiles = ts.percentiles
    data = ts.data
    features_json = _features_arg(features)
    units_ptr = units === nothing ? C_NULL : String(units)
    logical_ptr = logical_type === nothing ? C_NULL : pointer(String(logical_type))
    dtype = _dtype_code(eltype(data))
    dims = UInt64[size(data)...]
    bytes = _row_major_bytes(data)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:ts_store_add_probabilistic, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Cstring, Int32, Cstring, Int64, Int64, Int64, Int64, UInt64,
         Ptr{Float64}, UInt64, Int32, UInt64, Ptr{UInt64}, Ptr{UInt8}, UInt64, Cstring,
         Cstring, Cstring, Ref{Ptr{Cvoid}}),
        store.handle, Int64(owner_id), owner_type, _category_int(owner_category), name,
        _to_unix_ms(initial_timestamp), _resolution_to_ms(resolution),
        _resolution_to_ms(horizon), _resolution_to_ms(interval), UInt64(count),
        percentiles, UInt64(length(percentiles)),
        dtype, UInt64(length(dims)), dims, bytes, UInt64(length(bytes)),
        logical_ptr, features_json, units_ptr, out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

# ---- Batched adds ----------------------------------------------------------

"""
    AddBatch()

Accumulates pending add requests client-side; submit them with
[`add_time_series_bulk!`](@ref), which commits the whole batch in one metadata
transaction. This is the fast path for ingesting many time series: per-item
`add_time_series!` calls pay one SQLite commit each, while a batch pays a
single commit for all items.

Use the same `add_time_series!` methods with an `AddBatch` first argument in
place of the `Store`. The batch is drained by `add_time_series_bulk!` and may
be reused afterwards.
"""
mutable struct AddBatch
    handle :: Ptr{Cvoid}
    count :: Int
    function AddBatch()
        handle = ccall((:ts_batch_new, lib_path()), Ptr{Cvoid}, ())
        batch = new(handle, 0)
        finalizer(_finalize_batch, batch)
        batch
    end
end

function _finalize_batch(b::AddBatch)
    if b.handle != C_NULL
        ccall((:ts_batch_free, lib_path()), Cvoid, (Ptr{Cvoid},), b.handle)
        b.handle = C_NULL
    end
    nothing
end

Base.length(b::AddBatch) = b.count

_opt_string_arg(s) = s === nothing ? C_NULL : String(s)

function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::SingleTimeSeries;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=ts.logical_type,
)
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
    code = ccall(
        (:ts_batch_add_single, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Cstring, Int32, Cstring, Int64, Int64,
         Int32, UInt64, Ptr{UInt64}, Ptr{UInt8}, UInt64, Cstring, Cstring, Cstring),
        batch.handle, Int64(owner_id), owner_type, _category_int(owner_category), ts.name,
        _to_unix_ms(ts.initial_timestamp), _resolution_to_ms(ts.resolution),
        dtype, UInt64(length(dims)), dims, bytes, UInt64(length(bytes)),
        _opt_string_arg(logical_type), _features_arg(features), _opt_string_arg(units),
    )
    _check(code)
    batch.count += 1
    return batch
end

function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::NonSequentialTimeSeries;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=ts.logical_type,
)
    timestamps = Int64[_to_unix_ms(timestamp) for timestamp in ts.timestamps]
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[length(ts.data)]
    bytes = _row_major_bytes(ts.data)
    code = ccall(
        (:ts_batch_add_non_sequential, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Cstring, Int32, Cstring, Ptr{Int64}, UInt64,
         Int32, UInt64, Ptr{UInt64}, Ptr{UInt8}, UInt64, Cstring, Cstring, Cstring),
        batch.handle, Int64(owner_id), owner_type, _category_int(owner_category), ts.name,
        timestamps, UInt64(length(timestamps)), dtype, UInt64(1), dims, bytes,
        UInt64(length(bytes)),
        _opt_string_arg(logical_type), _features_arg(features), _opt_string_arg(units),
    )
    _check(code)
    batch.count += 1
    return batch
end

function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::Deterministic;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=ts.logical_type,
)
    return _batch_add_dense_forecast!(
        batch, owner_id, owner_type, owner_category, ts.name, TS_TYPE_DETERMINISTIC,
        ts.initial_timestamp, ts.resolution, ts.horizon, ts.interval, ts.count, ts.data;
        features=features, units=units, logical_type=logical_type,
    )
end

function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::Scenarios;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=ts.logical_type,
)
    return _batch_add_dense_forecast!(
        batch, owner_id, owner_type, owner_category, ts.name, TS_TYPE_SCENARIOS,
        ts.initial_timestamp, ts.resolution, ts.horizon, ts.interval, ts.count, ts.data;
        features=features, units=units, logical_type=logical_type,
    )
end

function _batch_add_dense_forecast!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts_type::Integer,
    initial_timestamp::DateTime,
    resolution::Period,
    horizon::Period,
    interval::Period,
    count::Integer,
    data::AbstractArray;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=nothing,
)
    dtype = _dtype_code(eltype(data))
    dims = UInt64[size(data)...]
    bytes = _row_major_bytes(data)
    code = ccall(
        (:ts_batch_add_forecast, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Cstring, Int32, Cstring, Int32, Int64, Int64, Int64, Int64,
         UInt64, Int32, UInt64, Ptr{UInt64}, Ptr{UInt8}, UInt64, Cstring, Cstring, Cstring),
        batch.handle, Int64(owner_id), owner_type, _category_int(owner_category), name,
        Int32(ts_type), _to_unix_ms(initial_timestamp), _resolution_to_ms(resolution),
        _resolution_to_ms(horizon), _resolution_to_ms(interval), UInt64(count),
        dtype, UInt64(length(dims)), dims, bytes, UInt64(length(bytes)),
        _opt_string_arg(logical_type), _features_arg(features), _opt_string_arg(units),
    )
    _check(code)
    batch.count += 1
    return batch
end

function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::Probabilistic;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=ts.logical_type,
)
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
    code = ccall(
        (:ts_batch_add_probabilistic, lib_path()), Int32,
        (Ptr{Cvoid}, Int64, Cstring, Int32, Cstring, Int64, Int64, Int64, Int64, UInt64,
         Ptr{Float64}, UInt64, Int32, UInt64, Ptr{UInt64}, Ptr{UInt8}, UInt64,
         Cstring, Cstring, Cstring),
        batch.handle, Int64(owner_id), owner_type, _category_int(owner_category), ts.name,
        _to_unix_ms(ts.initial_timestamp), _resolution_to_ms(ts.resolution),
        _resolution_to_ms(ts.horizon), _resolution_to_ms(ts.interval), UInt64(ts.count),
        ts.percentiles, UInt64(length(ts.percentiles)),
        dtype, UInt64(length(dims)), dims, bytes, UInt64(length(bytes)),
        _opt_string_arg(logical_type), _features_arg(features), _opt_string_arg(units),
    )
    _check(code)
    batch.count += 1
    return batch
end

"""
    add_time_series_bulk!(store, batch::AddBatch) -> Vector{TimeSeriesKey}

Submit every request in `batch` through one all-or-nothing bulk add and return
the new keys in insertion order. The batch is drained in all cases — on error
nothing was committed and the batch is left empty.
"""
function add_time_series_bulk!(store::Store, batch::AddBatch)
    out_keys = Ref{Ptr{Ptr{Cvoid}}}(C_NULL)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:ts_store_add_batch, lib_path()), Int32,
        (Ptr{Cvoid}, Ptr{Cvoid}, Ref{Ptr{Ptr{Cvoid}}}, Ref{UInt64}),
        store.handle, batch.handle, out_keys, out_len,
    )
    batch.count = 0
    _check(code)
    n = Int(out_len[])
    keys = Vector{TimeSeriesKey}(undef, n)
    if n > 0
        # Copy each owned handle into a finalized wrapper, then free the array
        # buffer (the wrappers own the handles and free them via ts_key_free).
        raw = unsafe_wrap(Array, out_keys[], n; own=false)
        for i in 1:n
            keys[i] = TimeSeriesKey(raw[i])
        end
        ccall(
            (:ts_keys_buffer_free, lib_path()), Cvoid, (Ptr{Ptr{Cvoid}}, UInt64),
            out_keys[], out_len[],
        )
    end
    return keys
end

# ---- Forecast data reads ---------------------------------------------------
#
# All three functions call `ts_store_get_forecast` and return named tuples
# with the data array reshaped to the canonical Julia (column-major) layout
# that is the inverse of `_row_major_bytes`.
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
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = _features_arg(features)

    time_range_present = time_range !== nothing
    range_start_ms = time_range_present ? _to_unix_ms(time_range[1]) : Int64(0)
    range_end_ms   = time_range_present ? _to_unix_ms(time_range[2]) : Int64(0)

    out_initial   = Ref{Int64}(0)
    out_res       = Ref{Int64}(0)
    out_horizon   = Ref{Int64}(0)
    out_interval  = Ref{Int64}(0)
    out_count     = Ref{UInt64}(0)
    out_scen      = Ref{UInt64}(0)
    out_ndims     = Ref{UInt64}(0)
    out_dims      = Ref{Ptr{UInt64}}(C_NULL)
    out_dtype     = Ref{Int32}(0)
    out_data      = Ref{Ptr{UInt8}}(C_NULL)
    out_byte_len  = Ref{UInt64}(0)
    out_pct       = Ref{Ptr{Float64}}(C_NULL)
    out_pct_len   = Ref{UInt64}(0)
    out_matched   = Ref{Int32}(0)

    code = ccall(
        (:ts_store_get_forecast, lib_path()), Int32,
        (Ptr{Cvoid},   # handle
         Int64,        # owner_id
         Int32,        # owner_category
         Cstring,      # name
         Int32,        # ts_type
         Int64,        # resolution_ms
         Cstring,      # features_json
         Bool,         # time_range_present
         Int64,        # time_range_start_ms
         Int64,        # time_range_end_ms
         Ref{Int64},   # out_initial_ts_unix_ms
         Ref{Int64},   # out_resolution_ms
         Ref{Int64},   # out_horizon_ms
         Ref{Int64},   # out_interval_ms
         Ref{UInt64},  # out_count
         Ref{UInt64},  # out_scenario_count
         Ref{UInt64},  # out_ndims
         Ref{Ptr{UInt64}},  # out_dims
         Ref{Int32},   # out_dtype
         Ref{Ptr{UInt8}},   # out_data
         Ref{UInt64},  # out_data_byte_len
         Ref{Ptr{Float64}}, # out_percentiles
         Ref{UInt64},  # out_percentiles_len
         Ref{Int32}),  # out_matched_type
        store.handle,
        Int64(owner_id),
        _category_int(owner_category),
        name,
        Int32(ts_type),
        resolution_ms,
        features_json,
        time_range_present,
        range_start_ms,
        range_end_ms,
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
    )
    _check(code)

    return _decode_forecast_outputs(
        out_initial, out_res, out_horizon, out_interval, out_count, out_scen,
        out_ndims, out_dims, out_dtype, out_data, out_byte_len, out_pct, out_pct_len,
        out_matched,
    )
end

# Decode the out-params populated by `ts_store_get_forecast` /
# `ts_store_get_forecast_by_key` into the common named tuple, copying then
# freeing every FFI-owned buffer.
function _decode_forecast_outputs(
    out_initial, out_res, out_horizon, out_interval, out_count, out_scen,
    out_ndims, out_dims, out_dtype, out_data, out_byte_len, out_pct, out_pct_len,
    out_matched,
)
    # Copy dims and free FFI buffer.
    nd = Int(out_ndims[])
    dims_raw = unsafe_wrap(Array, out_dims[], nd; own=false)
    dims = Int.(copy(dims_raw))
    ccall((:ts_buffer_free_u64, lib_path()), Cvoid, (Ptr{UInt64}, UInt64), out_dims[], out_ndims[])

    # Copy data bytes and free FFI buffer.
    n_bytes = Int(out_byte_len[])
    bytes_raw = unsafe_wrap(Array, out_data[], n_bytes; own=false)
    bytes = copy(bytes_raw)
    ccall((:ts_buffer_free_u8, lib_path()), Cvoid, (Ptr{UInt8}, UInt64), out_data[], out_byte_len[])

    # Percentiles (Probabilistic only; null for others).
    np = Int(out_pct_len[])
    percentiles = if np > 0 && out_pct[] != C_NULL
        p = copy(unsafe_wrap(Array, out_pct[], np; own=false))
        ccall((:ts_buffer_free_f64, lib_path()), Cvoid, (Ptr{Float64}, UInt64), out_pct[], out_pct_len[])
        p
    else
        Float64[]
    end

    return (
        initial_timestamp = _from_unix_ms(out_initial[]),
        resolution        = Millisecond(out_res[]),
        horizon           = Millisecond(out_horizon[]),
        interval          = Millisecond(out_interval[]),
        count             = Int(out_count[]),
        scenario_count    = Int(out_scen[]),
        dims              = dims,
        bytes             = bytes,
        dtype_code        = out_dtype[],
        percentiles       = percentiles,
        matched_type      = Int(out_matched[]),
    )
end

# Key-based counterpart of `_get_forecast_raw`: reads via the key handle
# (`ts_store_get_forecast_by_key`), so the time series type comes from the key.
function _get_forecast_raw(
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    time_range_present = time_range !== nothing
    range_start_ms = time_range_present ? _to_unix_ms(time_range[1]) : Int64(0)
    range_end_ms   = time_range_present ? _to_unix_ms(time_range[2]) : Int64(0)

    out_initial   = Ref{Int64}(0)
    out_res       = Ref{Int64}(0)
    out_horizon   = Ref{Int64}(0)
    out_interval  = Ref{Int64}(0)
    out_count     = Ref{UInt64}(0)
    out_scen      = Ref{UInt64}(0)
    out_ndims     = Ref{UInt64}(0)
    out_dims      = Ref{Ptr{UInt64}}(C_NULL)
    out_dtype     = Ref{Int32}(0)
    out_data      = Ref{Ptr{UInt8}}(C_NULL)
    out_byte_len  = Ref{UInt64}(0)
    out_pct       = Ref{Ptr{Float64}}(C_NULL)
    out_pct_len   = Ref{UInt64}(0)
    out_matched   = Ref{Int32}(0)

    code = ccall(
        (:ts_store_get_forecast_by_key, lib_path()), Int32,
        (Ptr{Cvoid},   # handle
         Ptr{Cvoid},   # key
         Bool,         # time_range_present
         Int64,        # time_range_start_ms
         Int64,        # time_range_end_ms
         Ref{Int64},   # out_initial_ts_unix_ms
         Ref{Int64},   # out_resolution_ms
         Ref{Int64},   # out_horizon_ms
         Ref{Int64},   # out_interval_ms
         Ref{UInt64},  # out_count
         Ref{UInt64},  # out_scenario_count
         Ref{UInt64},  # out_ndims
         Ref{Ptr{UInt64}},  # out_dims
         Ref{Int32},   # out_dtype
         Ref{Ptr{UInt8}},   # out_data
         Ref{UInt64},  # out_data_byte_len
         Ref{Ptr{Float64}}, # out_percentiles
         Ref{UInt64},  # out_percentiles_len
         Ref{Int32}),  # out_matched_type
        store.handle,
        key.handle,
        time_range_present,
        range_start_ms,
        range_end_ms,
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
    )
    _check(code)

    return _decode_forecast_outputs(
        out_initial, out_res, out_horizon, out_interval, out_count, out_scen,
        out_ndims, out_dims, out_dtype, out_data, out_byte_len, out_pct, out_pct_len,
        out_matched,
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

"""
    get_time_series(AbstractDeterministic, store, owner_id, owner_category, name; resolution, features, time_range)

Fetch whichever of `Deterministic` / `DeterministicSingleTimeSeries` is stored
under the identity, returned as a [`Deterministic`]. `owner_category` is the
owner's `OwnerCategory` (`Component` or `SupplementalAttribute`).

The Rust core resolves the family in a single call — no guess-and-retry. A
genuine miss raises `NotFoundError`; an identity that matches *both* a
`Deterministic` and a `DeterministicSingleTimeSeries` is ambiguous and raises an
error (request a concrete type instead). `data` has canonical shape
`(H, count, element_dims...)`; pass `time_range = (start, end)` (exclusive end)
to select a window sub-range.
"""
function get_time_series(
    ::Type{AbstractDeterministic},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(
        store, owner_id, owner_category, name, TS_TYPE_ABSTRACT_DETERMINISTIC;
        resolution=resolution, features=features, time_range=time_range,
    )
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    a = _assoc_attrs(store, owner_id, owner_category, name, r.matched_type; resolution=resolution, features=features)
    return Deterministic(r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, data,
                         a.name)
end

"""
    get_time_series(Deterministic, store, owner_id, owner_category, name; resolution, features, time_range)

Fetch a stored `Deterministic` forecast by its concrete type (a
`DeterministicSingleTimeSeries` is *not* matched — use `AbstractDeterministic`
for the family, or [`get_time_series(DeterministicSingleTimeSeries, ...)`] for a
DST). `owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`). `data` has canonical shape `(H, count, element_dims...)`
where `H = horizon / resolution`. Pass `time_range = (start, end)` (exclusive end)
to select a window sub-range per the InfrastructureSystems.jl convention.
"""
function get_time_series(
    ::Type{Deterministic},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(
        store, owner_id, owner_category, name, TS_TYPE_DETERMINISTIC;
        resolution=resolution, features=features, time_range=time_range,
    )
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    a = _assoc_attrs(store, owner_id, owner_category, name, TS_TYPE_DETERMINISTIC;
                     resolution=resolution, features=features)
    return Deterministic(r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, data,
                         a.name)
end

"""
    get_time_series(DeterministicSingleTimeSeries, store, owner_id, owner_category, name; resolution, features, time_range)

Fetch a `DeterministicSingleTimeSeries` (derived via `transform_single_time_series!`)
explicitly by its stored type. `owner_category` is the owner's `OwnerCategory`
(`Component` or `SupplementalAttribute`). It has no materialized form, so the
result is a [`Deterministic`].
"""
function get_time_series(
    ::Type{DeterministicSingleTimeSeries},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(
        store, owner_id, owner_category, name, TS_TYPE_DETERMINISTIC_SINGLE;
        resolution=resolution, features=features, time_range=time_range,
    )
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    a = _assoc_attrs(store, owner_id, owner_category, name, TS_TYPE_DETERMINISTIC_SINGLE;
                     resolution=resolution, features=features)
    return Deterministic(r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, data,
                         a.name)
end

"""
    get_time_series(Probabilistic, store, owner_id, owner_category, name; resolution, features, time_range)

Fetch a `Probabilistic` forecast. `owner_category` is the owner's `OwnerCategory`
(`Component` or `SupplementalAttribute`). `data` has canonical shape
`(num_percentiles, H, count, element_dims...)`.
"""
function get_time_series(
    ::Type{Probabilistic},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(
        store, owner_id, owner_category, name, TS_TYPE_PROBABILISTIC;
        resolution=resolution, features=features, time_range=time_range,
    )
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    a = _assoc_attrs(store, owner_id, owner_category, name, TS_TYPE_PROBABILISTIC;
                     resolution=resolution, features=features)
    return Probabilistic(
        r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, r.percentiles, data,
        a.name,
    )
end

"""
    get_time_series(Scenarios, store, owner_id, owner_category, name; resolution, features, time_range)

Fetch a `Scenarios` forecast. `owner_category` is the owner's `OwnerCategory`
(`Component` or `SupplementalAttribute`). `data` has canonical shape
`(scenario_count, H, count, element_dims...)`.
"""
function get_time_series(
    ::Type{Scenarios},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(
        store, owner_id, owner_category, name, TS_TYPE_SCENARIOS;
        resolution=resolution, features=features, time_range=time_range,
    )
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    a = _assoc_attrs(store, owner_id, owner_category, name, TS_TYPE_SCENARIOS;
                     resolution=resolution, features=features)
    # `scenario_count` is the leading axis of the decoded data.
    return Scenarios(r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, data,
                     a.name)
end

# ---- Key-based forecast reads ----------------------------------------------
#
# Counterparts to the attribute-addressed forecast readers above, keyed by a
# `TimeSeriesKey` handle (returned by `add_time_series!`). The time series type
# comes from the key; the `::Type{...}` argument selects how the result is
# decoded and which struct is returned. Unlike the attribute-based `Deterministic`
# reader there is no `DeterministicSingleTimeSeries` fallback — the key already
# names the exact stored type (a DST key reads back as a `Deterministic`).

"""
    get_time_series(Deterministic, store, key; time_range)

Key-based counterpart to `get_time_series(Deterministic, store, owner_id, name; ...)`.
"""
function get_time_series(
    ::Type{Deterministic},
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(store, key; time_range=time_range)
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    a = _get_association(store, key)
    return Deterministic(r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, data,
                         a.name)
end

"""
    get_time_series(DeterministicSingleTimeSeries, store, key; time_range)

Key-based read of a `DeterministicSingleTimeSeries` key (as returned by
`get_time_series_keys`). It has no materialized form, so the result is a
[`Deterministic`] — identical decoding to the `Deterministic` key reader.
"""
function get_time_series(
    ::Type{DeterministicSingleTimeSeries},
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    return get_time_series(Deterministic, store, key; time_range=time_range)
end

"""
    get_time_series(Probabilistic, store, key; time_range)

Key-based counterpart to `get_time_series(Probabilistic, store, owner_id, name; ...)`.
"""
function get_time_series(
    ::Type{Probabilistic},
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(store, key; time_range=time_range)
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    a = _get_association(store, key)
    return Probabilistic(
        r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, r.percentiles, data,
        a.name,
    )
end

"""
    get_time_series(Scenarios, store, key; time_range)

Key-based counterpart to `get_time_series(Scenarios, store, owner_id, name; ...)`.
"""
function get_time_series(
    ::Type{Scenarios},
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(store, key; time_range=time_range)
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    a = _get_association(store, key)
    return Scenarios(r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, data,
                     a.name)
end


# ---- Tracing ---------------------------------------------------------------

"""
    init_logging(level::AbstractString = "")

Initialize the Rust tracing subscriber.

`level` is a [`tracing_subscriber::EnvFilter`] directive string such as
`"debug"`, `"time_series_store_core=debug"`, or
`"warn,time_series_store_core=trace"`. Pass an empty string (the default)
to read the `RUST_LOG` environment variable; if that variable is also unset,
no output is produced.

The subscriber is initialized at most once per process — subsequent calls are
no-ops. `TimeSeriesStore.__init__` reads `RUST_LOG` on module load, so setting
`ENV["RUST_LOG"]` before `using TimeSeriesStore` is sufficient for the common
case.

Returns the FFI status code (`TS_OK = 0`, `TS_ERR_INVALID_PARAMETER = 3` for
an invalid directive string).
"""
function init_logging(level::AbstractString="")
    filter_ptr = isempty(level) ? C_NULL : level
    ret = ccall((:ts_store_init_logging, lib_path()), Int32, (Cstring,), filter_ptr)
    if ret != 0
        @warn "TimeSeriesStore.init_logging: ts_store_init_logging returned error code $ret"
    end
    return ret
end

# Read RUST_LOG at module-load time so that `using TimeSeriesStore` with RUST_LOG
# set in the environment automatically enables tracing without extra user code.
function __init__()
    rust_log = get(ENV, "RUST_LOG", "")
    if !isempty(rust_log)
        try
            init_logging(rust_log)
        catch e
            @warn "TimeSeriesStore.__init__: failed to initialize tracing" exception=e
        end
    end
end

end # module TimeSeriesStore
