module TimeSeries

using Dates
import JSON

export TimeSeriesStore, SingleTimeSeries, TimeSeriesKey,
       OwnerCategory, Component, SupplementalAttribute,
       add_time_series!, get_time_series, remove_time_series!,
       has_time_series, get_counts, verify_integrity, compact!,
       get_metadata, get_array_by_hash, open_store, flush!,
       close!

# ---- libtime_series_store_ffi resolution ---------------------------------

const _LIB_REF = Ref{String}("")

"""
Path to the cdylib. Set via the `TIME_SERIES_STORE_LIB` environment variable
when running outside an installed JLL.
"""
function lib_path()
    if !isempty(_LIB_REF[])
        return _LIB_REF[]
    end
    p = get(ENV, "TIME_SERIES_STORE_LIB", "")
    isempty(p) && error(
        "TIME_SERIES_STORE_LIB env var must point to libtime_series_store_ffi.{dylib,so,dll}"
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

Base.showerror(io::IO, e::TimeSeriesException) = print(io, "TimeSeries.", typeof(e).name.name, ": ", e.msg)

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

# ---- Single time series ---------------------------------------------------

struct SingleTimeSeries
    initial_timestamp :: DateTime
    resolution        :: Period
    data              :: Vector{Float64}
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

mutable struct TimeSeriesStore
    handle :: Ptr{Cvoid}
    function TimeSeriesStore(handle::Ptr{Cvoid})
        s = new(handle)
        finalizer(close!, s)
        s
    end
end

function close!(s::TimeSeriesStore)
    if s.handle != C_NULL
        ccall((:ts_store_free, lib_path()), Cvoid, (Ptr{Cvoid},), s.handle)
        s.handle = C_NULL
    end
end

"""
    TimeSeriesStore(; in_memory=true, path=nothing)

Construct a new store. Pass `path` (and `in_memory=false`) to persist to a
NetCDF file on disk.
"""
function TimeSeriesStore(; in_memory::Bool=true, path::Union{Nothing,AbstractString}=nothing)
    out = Ref{Ptr{Cvoid}}(C_NULL)
    cpath = path === nothing ? C_NULL : pointer(String(path))
    code = ccall((:ts_store_create, lib_path()), Int32,
                 (Cstring, Bool, Ref{Ptr{Cvoid}}),
                 cpath, in_memory, out)
    _check(code)
    return TimeSeriesStore(out[])
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
    return TimeSeriesStore(out[])
end

# ---- Operations -----------------------------------------------------------

# Convert a DateTime to Unix nanoseconds.
function _to_unix_ns(dt::DateTime)
    ms = Int64(Dates.datetime2unix(dt) * 1000)
    return ms * 1_000_000
end

# Convert nanoseconds since epoch back into a DateTime.
function _from_unix_ns(ns::Int64)
    ms_total = div(ns, 1_000_000)
    return Dates.unix2datetime(ms_total / 1000)
end

function _resolution_to_ns(p::Period)
    Dates.toms(p) * 1_000_000
end

"""
    add_time_series!(store, owner_uuid, owner_type, owner_category, name, ts;
                     features=Dict(), units=nothing, scaling_factor_multiplier=nothing)

`owner_uuid` identifies the owning component / supplemental attribute (a string,
typically the stringified UUID).
"""
function add_time_series!(
    store::TimeSeriesStore,
    owner_uuid::AbstractString,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts::SingleTimeSeries;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    scaling_factor_multiplier::Union{Nothing,AbstractString}=nothing,
)
    initial_ns = _to_unix_ns(ts.initial_timestamp)
    resolution_ns = _resolution_to_ns(ts.resolution)
    data = Vector{Float64}(ts.data)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    units_ptr = units === nothing ? C_NULL : pointer(String(units))
    scaling_ptr = scaling_factor_multiplier === nothing ? C_NULL : pointer(String(scaling_factor_multiplier))

    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:ts_store_add_single, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int32, Cstring, Int64, Int64,
         Ptr{Float64}, UInt64, Cstring, Cstring, Cstring, Ref{Ptr{Cvoid}}),
        store.handle,
        owner_uuid,
        owner_type,
        Int32(Int(owner_category)),
        name,
        initial_ns,
        resolution_ns,
        data,
        UInt64(length(data)),
        features_json,
        units_ptr,
        scaling_ptr,
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
    store::TimeSeriesStore,
    owner_uuid::AbstractString,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_ns = resolution === nothing ? Int64(0) : _resolution_to_ns(resolution)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    out_initial = Ref{Int64}(0)
    out_resolution = Ref{Int64}(0)
    out_length = Ref{UInt64}(0)
    out_hash = Vector{UInt8}(undef, 32)
    code = ccall(
        (:ts_store_get_metadata, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int64, Cstring,
         Ref{Int64}, Ref{Int64}, Ref{UInt64}, Ptr{UInt8}),
        store.handle, owner_uuid, name, resolution_ns, features_json,
        out_initial, out_resolution, out_length, out_hash,
    )
    _check(code)
    res_ms = div(out_resolution[], 1_000_000)
    return (
        initial_timestamp=_from_unix_ns(out_initial[]),
        resolution=Millisecond(res_ms),
        length=Int(out_length[]),
        data_hash=out_hash,
    )
end

"""
    get_array_by_hash(store, data_hash) -> Vector{Float64}

Fetch the full stored array for a 32-byte content hash.
"""
function get_array_by_hash(store::TimeSeriesStore, data_hash::Vector{UInt8})
    length(data_hash) == 32 || throw(InvalidParameterError("data_hash must be 32 bytes"))
    out_data = Ref{Ptr{Float64}}(C_NULL)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:ts_store_get_array_by_hash, lib_path()), Int32,
        (Ptr{Cvoid}, Ptr{UInt8}, Ref{Ptr{Float64}}, Ref{UInt64}),
        store.handle, data_hash, out_data, out_len,
    )
    _check(code)
    n = Int(out_len[])
    raw = unsafe_wrap(Array, out_data[], n; own=false)
    result = copy(raw)
    ccall((:ts_buffer_free_f64, lib_path()), Cvoid, (Ptr{Float64}, UInt64), out_data[], out_len[])
    return result
end

"""
    has_time_series(store, owner_uuid, name; resolution, features=Dict()) -> Bool
"""
function has_time_series(
    store::TimeSeriesStore,
    owner_uuid::AbstractString,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_ns = resolution === nothing ? Int64(0) : _resolution_to_ns(resolution)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    out = Ref{Bool}(false)
    code = ccall(
        (:ts_store_has_by_attrs, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int64, Cstring, Ref{Bool}),
        store.handle, owner_uuid, name, resolution_ns, features_json, out,
    )
    _check(code)
    return out[]
end

"""
    remove_time_series!(store, owner_uuid, name; resolution, features=Dict())
"""
function remove_time_series!(
    store::TimeSeriesStore,
    owner_uuid::AbstractString,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_ns = resolution === nothing ? Int64(0) : _resolution_to_ns(resolution)
    features_json = isempty(features) ? C_NULL : pointer(JSON.json(features))
    code = ccall(
        (:ts_store_remove_by_attrs, lib_path()), Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int64, Cstring),
        store.handle, owner_uuid, name, resolution_ns, features_json,
    )
    _check(code)
    return nothing
end

function get_time_series(store::TimeSeriesStore, key::TimeSeriesKey)
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

    initial = _from_unix_ns(out_initial[])
    # resolution_ns is integer nanoseconds. Convert to Period: prefer Millisecond.
    res_ns = out_resolution[]
    res_ms = div(res_ns, 1_000_000)
    resolution = Millisecond(res_ms)
    return SingleTimeSeries(initial, resolution, data)
end

function remove_time_series!(store::TimeSeriesStore, key::TimeSeriesKey)
    code = ccall((:ts_store_remove, lib_path()), Int32,
                 (Ptr{Cvoid}, Ptr{Cvoid}), store.handle, key.handle)
    _check(code)
    return nothing
end

function has_time_series(store::TimeSeriesStore, key::TimeSeriesKey)
    out = Ref{Bool}(false)
    code = ccall((:ts_store_has, lib_path()), Int32,
                 (Ptr{Cvoid}, Ptr{Cvoid}, Ref{Bool}),
                 store.handle, key.handle, out)
    _check(code)
    return out[]
end

function get_counts(store::TimeSeriesStore)
    a = Ref{Int64}(0); b = Ref{Int64}(0); c = Ref{Int64}(0)
    code = ccall((:ts_store_counts, lib_path()), Int32,
                 (Ptr{Cvoid}, Ref{Int64}, Ref{Int64}, Ref{Int64}),
                 store.handle, a, b, c)
    _check(code)
    return (components_with_time_series=a[], static_time_series=b[], forecasts=c[])
end

function verify_integrity(store::TimeSeriesStore)
    out = Ref{UInt64}(0)
    code = ccall((:ts_store_verify, lib_path()), Int32,
                 (Ptr{Cvoid}, Ref{UInt64}), store.handle, out)
    _check(code)
    return Int(out[])
end

function compact!(store::TimeSeriesStore)
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
function flush!(store::TimeSeriesStore)
    code = ccall((:ts_store_flush, lib_path()), Int32,
                 (Ptr{Cvoid},), store.handle)
    _check(code)
    return nothing
end

end # module TimeSeries
