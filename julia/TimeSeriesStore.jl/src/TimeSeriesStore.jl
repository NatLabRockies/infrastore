module TimeSeriesStore

using Dates
import JSON

export Store, SingleTimeSeries, NonSequentialTimeSeries, TimeSeriesKey,
       OwnerCategory, Component, SupplementalAttribute,
       add_time_series!, get_time_series, remove_time_series!,
       has_time_series, get_counts, verify_integrity, compact!,
       get_metadata, get_array_by_hash, open_store, flush!, clear!,
       add_forecast!, get_forecast_metadata, has_typed, remove_typed!,
       add_probabilistic!, get_probabilistic_metadata,
       get_deterministic, get_probabilistic, get_scenarios,
       close!

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

# ---- Single time series ---------------------------------------------------

struct SingleTimeSeries
    initial_timestamp :: DateTime
    resolution        :: Period
    "Values: a 1-D vector (scalar per step) or N-D array (dim 1 = time)."
    data              :: AbstractArray
    "Opaque logical-type tag for the binding to reconstruct domain objects."
    logical_type      :: Union{Nothing,String}
end

SingleTimeSeries(initial, resolution, data::AbstractArray) =
    SingleTimeSeries(initial, resolution, data, nothing)

# ---- Non-sequential time series -------------------------------------------

struct NonSequentialTimeSeries
    timestamps  :: Vector{DateTime}
    "Values: a 1-D vector with one value per timestamp."
    data        :: AbstractVector
    "Opaque logical-type tag for the binding to reconstruct domain objects."
    logical_type :: Union{Nothing,String}

    function NonSequentialTimeSeries(timestamps, data, logical_type=nothing)
        length(timestamps) == length(data) ||
            throw(InvalidParameterError("timestamp count must match data length"))
        all(timestamps[i] < timestamps[i + 1] for i in 1:(length(timestamps) - 1)) ||
            throw(InvalidParameterError("timestamps must be strictly increasing"))
        new(Vector{DateTime}(timestamps), data, logical_type)
    end
end

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
    Store(; in_memory=true, path=nothing)

Construct a new store. Pass `path` (and `in_memory=false`) to persist to a
NetCDF file on disk.
"""
function Store(; in_memory::Bool=true, path::Union{Nothing,AbstractString}=nothing)
    out = Ref{Ptr{Cvoid}}(C_NULL)
    cpath = path === nothing ? C_NULL : pointer(String(path))
    code = ccall((:ts_store_create, lib_path()), Int32,
                 (Cstring, Bool, Ref{Ptr{Cvoid}}),
                 cpath, in_memory, out)
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
    add_time_series!(store, owner_uuid, owner_type, owner_category, name, ts;
                     features=Dict(), units=nothing, scaling_factor_multiplier=nothing)

`owner_uuid` identifies the owning component / supplemental attribute (a string,
typically the stringified UUID).
"""
function add_time_series!(
    store::Store,
    owner_uuid::AbstractString,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts::SingleTimeSeries;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    scaling_factor_multiplier::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=ts.logical_type,
)
    initial_ms = _to_unix_ms(ts.initial_timestamp)
    resolution_ms = _resolution_to_ms(ts.resolution)
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    units_ptr = units === nothing ? C_NULL : pointer(String(units))
    scaling_ptr = scaling_factor_multiplier === nothing ? C_NULL : pointer(String(scaling_factor_multiplier))
    logical_ptr = logical_type === nothing ? C_NULL : pointer(String(logical_type))

    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:ts_store_add_single, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int32, Cstring, Int64, Int64,
         Int32, UInt64, Ptr{UInt64}, Ptr{UInt8}, UInt64, Cstring,
         Cstring, Cstring, Cstring, Ref{Ptr{Cvoid}}),
        store.handle,
        owner_uuid,
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
        scaling_ptr,
        out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

function add_time_series!(
    store::Store,
    owner_uuid::AbstractString,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts::NonSequentialTimeSeries;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    scaling_factor_multiplier::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=ts.logical_type,
)
    timestamps = Int64[_to_unix_ms(timestamp) for timestamp in ts.timestamps]
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[length(ts.data)]
    bytes = _row_major_bytes(ts.data)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    units_ptr = units === nothing ? C_NULL : pointer(String(units))
    scaling_ptr = scaling_factor_multiplier === nothing ? C_NULL :
                  pointer(String(scaling_factor_multiplier))
    logical_ptr = logical_type === nothing ? C_NULL : pointer(String(logical_type))
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:ts_store_add_non_sequential, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int32, Cstring, Ptr{Int64}, UInt64,
         Int32, UInt64, Ptr{UInt64}, Ptr{UInt8}, UInt64, Cstring,
         Cstring, Cstring, Cstring, Ref{Ptr{Cvoid}}),
        store.handle, owner_uuid, owner_type, Int32(Int(owner_category)), name,
        timestamps, UInt64(length(timestamps)), dtype, UInt64(1), dims, bytes,
        UInt64(length(bytes)), logical_ptr, features_json, units_ptr, scaling_ptr,
        out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

"""
    get_metadata(store, owner_uuid, name; resolution, features=Dict())

Look up a SingleTimeSeries by attributes and return a named tuple of
`(initial_timestamp, resolution, length, data_hash)`. `data_hash` is the 32-byte
content hash as a `Vector{UInt8}`. Throws `NotFoundError` if absent.
"""
function get_metadata(
    store::Store,
    owner_uuid::AbstractString,
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
        (Ptr{Cvoid}, Cstring, Cstring, Int64, Cstring,
         Ref{Int64}, Ref{Int64}, Ref{UInt64}, Ptr{UInt8}, Ref{Int32},
         Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle, owner_uuid, name, resolution_ms, features_json,
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
    has_time_series(store, owner_uuid, name; resolution, features=Dict()) -> Bool
"""
function has_time_series(
    store::Store,
    owner_uuid::AbstractString,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    out = Ref{Bool}(false)
    code = ccall(
        (:ts_store_has_by_attrs, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int64, Cstring, Ref{Bool}),
        store.handle, owner_uuid, name, resolution_ms, features_json, out,
    )
    _check(code)
    return out[]
end

"""
    has_for_owner(store, owner_uuid; time_series_type=nothing) -> Bool

True if `owner_uuid` has any time series, optionally restricted to a single
`time_series_type` code (the name-less existence query).
"""
function has_for_owner(
    store::Store,
    owner_uuid::AbstractString;
    time_series_type::Union{Nothing,Integer}=nothing,
)
    out = Ref{Bool}(false)
    use_type = time_series_type !== nothing
    code = ccall(
        (:ts_store_has_for_owner, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Int32, Bool, Ref{Bool}),
        store.handle, owner_uuid,
        Int32(use_type ? time_series_type : 0), use_type, out,
    )
    _check(code)
    return out[]
end

"""
    remove_time_series!(store, owner_uuid, name; resolution, features=Dict())
"""
function remove_time_series!(
    store::Store,
    owner_uuid::AbstractString,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    code = ccall(
        (:ts_store_remove_by_attrs, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int64, Cstring),
        store.handle, owner_uuid, name, resolution_ms, features_json,
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
    return SingleTimeSeries(initial, resolution, data)
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
    return NonSequentialTimeSeries(_from_unix_ms.(timestamp_ms), values)
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

"""Remove all time series (data + metadata) from the store."""
function clear!(store::Store)
    code = ccall((:ts_store_clear, lib_path()), Int32,
                 (Ptr{Cvoid}, Cstring), store.handle, C_NULL)
    _check(code)
    return nothing
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

_features_arg(features) = isempty(features) ? C_NULL : JSON.json(features)
_category_int(c::OwnerCategory) = Int32(Int(c))

"""
    add_forecast!(store, owner_uuid, owner_type, owner_category, name, ts_type,
                  initial_timestamp, resolution, horizon, interval, count, flat_values;
                  features=Dict(), units=nothing, scaling_factor_multiplier=nothing)

Add a forecast. `flat_values` is the flattened storage array (the caller owns the
window layout); `ts_type` is one of the `TS_TYPE_*` codes.
"""
function add_forecast!(
    store::Store,
    owner_uuid::AbstractString,
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
    scaling_factor_multiplier::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=nothing,
)
    features_json = _features_arg(features)
    units_ptr = units === nothing ? C_NULL : String(units)
    scaling_ptr = scaling_factor_multiplier === nothing ? C_NULL : String(scaling_factor_multiplier)
    logical_ptr = logical_type === nothing ? C_NULL : pointer(String(logical_type))
    dtype = _dtype_code(eltype(data))
    dims = UInt64[size(data)...]
    bytes = _row_major_bytes(data)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:ts_store_add_forecast, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int32, Cstring, Int32, Int64, Int64, Int64, Int64,
         UInt64, Int32, UInt64, Ptr{UInt64}, Ptr{UInt8}, UInt64, Cstring, Cstring, Cstring,
         Cstring, Ref{Ptr{Cvoid}}),
        store.handle, owner_uuid, owner_type, _category_int(owner_category), name,
        Int32(ts_type), _to_unix_ms(initial_timestamp), _resolution_to_ms(resolution),
        _resolution_to_ms(horizon), _resolution_to_ms(interval), UInt64(count),
        dtype, UInt64(length(dims)), dims, bytes, UInt64(length(bytes)),
        logical_ptr, features_json, units_ptr, scaling_ptr, out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

"""
    get_forecast_metadata(store, owner_uuid, name, ts_type; resolution, features=Dict())

Return `(; initial_timestamp, resolution, horizon, interval, count, length, data_hash)`.
"""
function get_forecast_metadata(
    store::Store,
    owner_uuid::AbstractString,
    name::AbstractString,
    ts_type::Integer;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = _features_arg(features)
    oi = Ref{Int64}(0); orr = Ref{Int64}(0); oh = Ref{Int64}(0); ov = Ref{Int64}(0)
    oc = Ref{UInt64}(0); ol = Ref{UInt64}(0); ohash = Vector{UInt8}(undef, 32)
    code = ccall(
        (:ts_store_get_forecast_metadata, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int32, Int64, Cstring,
         Ref{Int64}, Ref{Int64}, Ref{Int64}, Ref{Int64}, Ref{UInt64}, Ref{UInt64}, Ptr{UInt8}),
        store.handle, owner_uuid, name, Int32(ts_type), resolution_ms, features_json,
        oi, orr, oh, ov, oc, ol, ohash,
    )
    _check(code)
    return (
        initial_timestamp=_from_unix_ms(oi[]),
        resolution=Millisecond(orr[]),
        horizon=Millisecond(oh[]),
        interval=Millisecond(ov[]),
        count=Int(oc[]), length=Int(ol[]), data_hash=ohash,
    )
end

"""True iff a time series of `ts_type` with the given attributes exists."""
function has_typed(
    store::Store, owner_uuid::AbstractString, name::AbstractString, ts_type::Integer;
    resolution::Union{Nothing,Period}=nothing, features::AbstractDict=Dict{String,Any}(),
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = _features_arg(features)
    out = Ref{Bool}(false)
    code = ccall(
        (:ts_store_has_typed, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int32, Int64, Cstring, Ref{Bool}),
        store.handle, owner_uuid, name, Int32(ts_type), resolution_ms, features_json, out,
    )
    _check(code)
    return out[]
end

"""Remove a time series of `ts_type` by attributes."""
function remove_typed!(
    store::Store, owner_uuid::AbstractString, name::AbstractString, ts_type::Integer;
    resolution::Union{Nothing,Period}=nothing, features::AbstractDict=Dict{String,Any}(),
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = _features_arg(features)
    code = ccall(
        (:ts_store_remove_typed, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int32, Int64, Cstring),
        store.handle, owner_uuid, name, Int32(ts_type), resolution_ms, features_json,
    )
    _check(code)
    return nothing
end

"""Add a Probabilistic forecast (carries a `percentiles` vector)."""
function add_probabilistic!(
    store::Store,
    owner_uuid::AbstractString,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    name::AbstractString,
    initial_timestamp::DateTime,
    resolution::Period,
    horizon::Period,
    interval::Period,
    count::Integer,
    percentiles::Vector{Float64},
    data::AbstractArray;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    scaling_factor_multiplier::Union{Nothing,AbstractString}=nothing,
    logical_type::Union{Nothing,AbstractString}=nothing,
)
    features_json = _features_arg(features)
    units_ptr = units === nothing ? C_NULL : String(units)
    scaling_ptr = scaling_factor_multiplier === nothing ? C_NULL : String(scaling_factor_multiplier)
    logical_ptr = logical_type === nothing ? C_NULL : pointer(String(logical_type))
    dtype = _dtype_code(eltype(data))
    dims = UInt64[size(data)...]
    bytes = _row_major_bytes(data)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:ts_store_add_probabilistic, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int32, Cstring, Int64, Int64, Int64, Int64, UInt64,
         Ptr{Float64}, UInt64, Int32, UInt64, Ptr{UInt64}, Ptr{UInt8}, UInt64, Cstring,
         Cstring, Cstring, Cstring, Ref{Ptr{Cvoid}}),
        store.handle, owner_uuid, owner_type, _category_int(owner_category), name,
        _to_unix_ms(initial_timestamp), _resolution_to_ms(resolution),
        _resolution_to_ms(horizon), _resolution_to_ms(interval), UInt64(count),
        percentiles, UInt64(length(percentiles)),
        dtype, UInt64(length(dims)), dims, bytes, UInt64(length(bytes)),
        logical_ptr, features_json, units_ptr, scaling_ptr, out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

"""Read Probabilistic metadata; the named tuple also includes `percentiles`."""
function get_probabilistic_metadata(
    store::Store, owner_uuid::AbstractString, name::AbstractString;
    resolution::Union{Nothing,Period}=nothing, features::AbstractDict=Dict{String,Any}(),
)
    resolution_ms = resolution === nothing ? Int64(0) : _resolution_to_ms(resolution)
    features_json = _features_arg(features)
    oi = Ref{Int64}(0); orr = Ref{Int64}(0); oh = Ref{Int64}(0); ov = Ref{Int64}(0)
    oc = Ref{UInt64}(0); ol = Ref{UInt64}(0); ohash = Vector{UInt8}(undef, 32)
    op = Ref{Ptr{Float64}}(C_NULL); opl = Ref{UInt64}(0)
    code = ccall(
        (:ts_store_get_probabilistic_metadata, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int64, Cstring, Ref{Int64}, Ref{Int64}, Ref{Int64},
         Ref{Int64}, Ref{UInt64}, Ref{UInt64}, Ptr{UInt8}, Ref{Ptr{Float64}}, Ref{UInt64}),
        store.handle, owner_uuid, name, resolution_ms, features_json,
        oi, orr, oh, ov, oc, ol, ohash, op, opl,
    )
    _check(code)
    np = Int(opl[])
    percentiles = copy(unsafe_wrap(Array, op[], np; own=false))
    ccall((:ts_buffer_free_f64, lib_path()), Cvoid, (Ptr{Float64}, UInt64), op[], opl[])
    return (
        initial_timestamp=_from_unix_ms(oi[]),
        resolution=Millisecond(orr[]),
        horizon=Millisecond(oh[]),
        interval=Millisecond(ov[]),
        count=Int(oc[]), length=Int(ol[]), data_hash=ohash, percentiles=percentiles,
    )
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
    owner_uuid::AbstractString,
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

    code = ccall(
        (:ts_store_get_forecast, lib_path()), Int32,
        (Ptr{Cvoid},   # handle
         Cstring,      # owner_uuid
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
         Ref{UInt64}), # out_percentiles_len
        store.handle,
        owner_uuid,
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
    )
    _check(code)

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

    # Decode scalars.
    initial_timestamp = _from_unix_ms(out_initial[])
    resolution_out    = Millisecond(out_res[])
    horizon_out       = Millisecond(out_horizon[])
    interval_out      = Millisecond(out_interval[])
    count_out         = Int(out_count[])
    scenario_count    = Int(out_scen[])

    return (
        initial_timestamp = initial_timestamp,
        resolution        = resolution_out,
        horizon           = horizon_out,
        interval          = interval_out,
        count             = count_out,
        scenario_count    = scenario_count,
        dims              = dims,
        bytes             = bytes,
        dtype_code        = out_dtype[],
        percentiles       = percentiles,
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
    get_deterministic(store, owner_uuid, name; resolution, features, time_range) -> NamedTuple

Fetch a `Deterministic` forecast and return a named tuple:
`(; initial_timestamp, resolution, horizon, interval, count, data)`.

`data` is an N-dimensional Julia array with canonical shape
`(H, count, element_dims...)` where `H = horizon / resolution`.

Pass `time_range = (start::DateTime, end::DateTime)` (exclusive end) to select
a window sub-range per the InfrastructureSystems.jl convention.
"""
function get_deterministic(
    store::Store,
    owner_uuid::AbstractString,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(
        store, owner_uuid, name, TS_TYPE_DETERMINISTIC;
        resolution=resolution, features=features, time_range=time_range,
    )
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    return (
        initial_timestamp = r.initial_timestamp,
        resolution        = r.resolution,
        horizon           = r.horizon,
        interval          = r.interval,
        count             = r.count,
        data              = data,
    )
end

"""
    get_probabilistic(store, owner_uuid, name; resolution, features, time_range) -> NamedTuple

Fetch a `Probabilistic` forecast and return a named tuple:
`(; initial_timestamp, resolution, horizon, interval, count, percentiles, data)`.

`data` is an N-dimensional Julia array with canonical shape
`(num_percentiles, H, count, element_dims...)`.
"""
function get_probabilistic(
    store::Store,
    owner_uuid::AbstractString,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(
        store, owner_uuid, name, TS_TYPE_PROBABILISTIC;
        resolution=resolution, features=features, time_range=time_range,
    )
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    return (
        initial_timestamp = r.initial_timestamp,
        resolution        = r.resolution,
        horizon           = r.horizon,
        interval          = r.interval,
        count             = r.count,
        percentiles       = r.percentiles,
        data              = data,
    )
end

"""
    get_scenarios(store, owner_uuid, name; resolution, features, time_range) -> NamedTuple

Fetch a `Scenarios` forecast and return a named tuple:
`(; initial_timestamp, resolution, horizon, interval, count, scenario_count, data)`.

`data` is an N-dimensional Julia array with canonical shape
`(scenario_count, H, count, element_dims...)`.
"""
function get_scenarios(
    store::Store,
    owner_uuid::AbstractString,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(
        store, owner_uuid, name, TS_TYPE_SCENARIOS;
        resolution=resolution, features=features, time_range=time_range,
    )
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    return (
        initial_timestamp = r.initial_timestamp,
        resolution        = r.resolution,
        horizon           = r.horizon,
        interval          = r.interval,
        count             = r.count,
        scenario_count    = r.scenario_count,
        data              = data,
    )
end

end # module TimeSeriesStore
