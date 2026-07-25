module Castore

using Dates
using JSON: JSON

export Store,
    SingleTimeSeries,
    NonSequentialTimeSeries,
    Deterministic,
    DeterministicSingleTimeSeries,
    AbstractDeterministic,
    Probabilistic,
    Scenarios,
    TimeSeriesKey,
    OwnerCategory,
    Component,
    SupplementalAttribute,
    add_time_series!,
    AddBatch,
    add_time_series_bulk!,
    get_time_series,
    bulk_read,
    get_time_series_keys,
    key_info,
    list_keys,
    list_array_groups,
    remove_time_series!,
    has_time_series,
    get_counts,
    counts_by_type,
    num_distinct_arrays,
    time_series_counts,
    list_owner_ids,
    static_summary,
    forecast_summary,
    SupplementalAttributeAssociation,
    add_supplemental_attribute_association!,
    add_supplemental_attribute_associations!,
    has_supplemental_attribute_association,
    list_supplemental_attribute_associations,
    list_supplemental_attribute_ids,
    list_components_with_attributes,
    remove_supplemental_attribute_associations!,
    replace_supplemental_attribute_component_id!,
    count_supplemental_attribute_associations,
    count_supplemental_attributes,
    count_components_with_attributes,
    supplemental_attribute_counts_by_type,
    supplemental_attribute_summary,
    ParentChildAssociation,
    add_parent_child_association!,
    add_parent_child_associations!,
    has_parent_child_association,
    list_parent_child_associations,
    list_children,
    list_parents,
    remove_parent_child_associations!,
    replace_parent_child_component_id!,
    count_parent_child_associations,
    get_forecast_parameters,
    check_static_consistency,
    get_resolutions,
    get_intervals,
    get_compression,
    get_path,
    read_only,
    verify_integrity,
    compact!,
    get_metadata,
    get_forecast_metadata,
    get_probabilistic_metadata,
    get_array_by_hash,
    count_array_references,
    list_time_series,
    list_names,
    list_owner_types,
    remove_by_filter!,
    rename_time_series!,
    resolve_forecast_key,
    has_for_owner,
    open_store,
    flush!,
    clear!,
    replace_owner!,
    transform_single_time_series!,
    has_typed,
    remove_typed!,
    copy_time_series!,
    close!,
    persist!,
    StaticReader,
    build_static_reader,
    static_grid,
    static_groups,
    static_read!,
    static_values,
    ForecastReader,
    build_forecast_reader,
    forecast_timeline,
    forecast_entries,
    forecast_num_slots,
    forecast_read!,
    forecast_values,
    init_logging

# ---- libcastore_ffi resolution ---------------------------------
#
# Resolution order:
#   1. `CASTORE_LIB` environment variable (development override).
#   2. `Castore_jll` (the BinaryBuilder/Yggdrasil binary) when installed.
# The JLL is looked up without a hard dependency so this package still loads and
# works via the env var before the JLL is published to the registry.

const _LIB_REF = Ref{String}("")

function _jll_library_path()
    pkgid = Base.identify_package("Castore_jll")
    pkgid === nothing && return ""
    mod = try
        Base.require(pkgid)
    catch
        return ""
    end
    return if isdefined(mod, :libcastore_ffi)
        String(getproperty(mod, :libcastore_ffi))
    else
        ""
    end
end

"""
Path to the `libcastore_ffi` cdylib. Override with the
`CASTORE_LIB` environment variable (development builds); otherwise the
`Castore_jll` binary is used.
"""
function lib_path()
    if !isempty(_LIB_REF[])
        return _LIB_REF[]
    end
    p = get(ENV, "CASTORE_LIB", "")
    if isempty(p)
        p = _jll_library_path()
    end
    isempty(p) && error(
        "Could not locate libcastore_ffi. Set the CASTORE_LIB " *
        "environment variable to a built cdylib, or install Castore_jll.",
    )
    _LIB_REF[] = p
    return p
end

# ---- Status codes (must match crates/castore-ffi/src/lib.rs) ----

const CASTORE_OK = Int32(0)
const CASTORE_ERR_NULL_POINTER = Int32(1)
const CASTORE_ERR_INVALID_UTF8 = Int32(2)
const CASTORE_ERR_INVALID_PARAMETER = Int32(3)
const CASTORE_ERR_NOT_FOUND = Int32(4)
const CASTORE_ERR_DUPLICATE = Int32(5)
const CASTORE_ERR_INTEGRITY = Int32(6)
const CASTORE_ERR_READ_ONLY = Int32(7)
const CASTORE_ERR_IO = Int32(8)
const CASTORE_ERR_INCOMPATIBLE_FORMAT = Int32(9)
const CASTORE_ERR_DUPLICATE_ASSOCIATION = Int32(10)
const CASTORE_ERR_INTERNAL = Int32(99)

# ---- Owner category --------------------------------------------------------

@enum OwnerCategory begin
    Component = 0
    SupplementalAttribute = 1
end

# ---- Errors ---------------------------------------------------------------

abstract type TimeSeriesException <: Exception end

struct NotFoundError <: TimeSeriesException
    msg::String;
end

struct DuplicateTimeSeriesError <: TimeSeriesException
    msg::String;
end

struct DuplicateAssociationError <: TimeSeriesException
    msg::String;
end

struct InvalidParameterError <: TimeSeriesException
    msg::String;
end

struct IntegrityError <: TimeSeriesException
    msg::String;
end

struct ReadOnlyStoreError <: TimeSeriesException
    msg::String;
end

struct IncompatibleFormatError <: TimeSeriesException
    msg::String;
end

struct IOError <: TimeSeriesException
    msg::String;
end

struct GenericError <: TimeSeriesException
    msg::String;
    code::Int32;
end

function Base.showerror(io::IO, e::TimeSeriesException)
    return print(io, "Castore.", typeof(e).name.name, ": ", e.msg)
end

function _last_error_message()
    needed = Ref{UInt64}(0)
    ccall(
        (:castore_last_error_message, lib_path()),
        Int32,
        (Ptr{UInt8}, UInt64, Ptr{UInt64}),
        C_NULL,
        UInt64(0),
        needed,
    )
    n = Int(needed[])
    n == 0 && return ""
    buf = Vector{UInt8}(undef, n + 1)
    ccall(
        (:castore_last_error_message, lib_path()),
        Int32,
        (Ptr{UInt8}, UInt64, Ptr{UInt64}),
        buf,
        UInt64(n + 1),
        C_NULL,
    )
    return String(buf[1:n])
end

function _check(code::Int32)
    code == CASTORE_OK && return nothing
    msg = _last_error_message()
    if code == CASTORE_ERR_NOT_FOUND
        throw(NotFoundError(msg))
    elseif code == CASTORE_ERR_DUPLICATE
        throw(DuplicateTimeSeriesError(msg))
    elseif code == CASTORE_ERR_DUPLICATE_ASSOCIATION
        throw(DuplicateAssociationError(msg))
    elseif code == CASTORE_ERR_INVALID_PARAMETER ||
        code == CASTORE_ERR_INVALID_UTF8 ||
        code == CASTORE_ERR_NULL_POINTER
        throw(InvalidParameterError(msg))
    elseif code == CASTORE_ERR_INTEGRITY
        throw(IntegrityError(msg))
    elseif code == CASTORE_ERR_READ_ONLY
        throw(ReadOnlyStoreError(msg))
    elseif code == CASTORE_ERR_INCOMPATIBLE_FORMAT
        throw(IncompatibleFormatError(msg))
    elseif code == CASTORE_ERR_IO
        throw(IOError(msg))
    else
        throw(GenericError(msg, code))
    end
end

# ---- Element dtypes -------------------------------------------------------
# Codes must match `Dtype` in the Rust core / FFI.

_dtype_code(::Type{Float64}) = Int32(0)
_dtype_code(::Type{Float32}) = Int32(1)
_dtype_code(::Type{Int64}) = Int32(2)
_dtype_code(::Type{Int32}) = Int32(3)
_dtype_code(::Type{UInt64}) = Int32(4)
_dtype_code(::Type{Bool}) = Int32(5)
function _dtype_code(::Type{T}) where {T}
    return throw(InvalidParameterError("unsupported element dtype $T"))
end

const _DTYPE_JULIA = (Float64, Float32, Int64, Int32, UInt64, Bool)
_julia_dtype(code::Integer) = _DTYPE_JULIA[Int(code) + 1]

# Row-major little-endian bytes for a (possibly multi-dimensional) array. Julia
# is column-major, so transpose the axis order before flattening. A 1-D `Vector`
# needs no reordering, so its bytes are produced with a single copy.
function _row_major_bytes(arr::AbstractArray)
    flat = if ndims(arr) <= 1
        arr isa Vector ? arr : Vector(vec(arr))
    else
        vec(permutedims(arr, reverse(ntuple(identity, ndims(arr)))))
    end
    return collect(reinterpret(UInt8, flat))
end

# `name` is a per-association attribute carried on the binding structs (matching
# InfrastructureSystems.jl); it is not part of the deduplicated core data type.
# `name` is required.
_maybe_string(::Nothing) = nothing
_maybe_string(s::AbstractString) = String(s)

# ---- Single time series ---------------------------------------------------

struct SingleTimeSeries{T,N}
    initial_timestamp::DateTime
    resolution::Period
    "Values: a 1-D vector (scalar per step) or N-D array (dim 1 = time)."
    data::Array{T,N}
    "Association name (required; the same array may be stored under different names)."
    name::String
    "Opaque, package-owned extension payload (typically JSON) the binding writes and reads to reconstruct domain objects; the store never interprets it."
    ext::Union{Nothing,String}
end

# Infer `{T,N}` from the value array; views/ranges are normalized to a concrete
# `Array` (copy-free when already one).
function SingleTimeSeries(
    initial,
    resolution,
    data::AbstractArray,
    name::AbstractString;
    ext::Union{Nothing,AbstractString}=nothing,
)
    return SingleTimeSeries{eltype(data),ndims(data)}(
        initial,
        resolution,
        data isa Array ? data : Array(data),
        String(name),
        _maybe_string(ext),
    )
end

# ---- Non-sequential time series -------------------------------------------

struct NonSequentialTimeSeries{T,N}
    timestamps::Vector{DateTime}
    "Values: a 1-D vector (scalar per step) or N-D array (dim 1 = time, one entry per timestamp)."
    data::Array{T,N}
    "Association name (required)."
    name::String
    "Opaque, package-owned extension payload (typically JSON) the binding writes and reads to reconstruct domain objects; the store never interprets it."
    ext::Union{Nothing,String}
end

# Infer `{T,N}` from the value array; views/ranges are normalized to a concrete
# `Array`. Timestamps are explicit and must be strictly increasing, with one entry
# per leading-dimension row (`size(data, 1)`).
function NonSequentialTimeSeries(
    timestamps,
    data::AbstractArray,
    name::AbstractString;
    ext::Union{Nothing,AbstractString}=nothing,
)
    length(timestamps) == size(data, 1) ||
        throw(InvalidParameterError("timestamp count must match data length"))
    all(timestamps[i] < timestamps[i + 1] for i in 1:(length(timestamps) - 1)) ||
        throw(InvalidParameterError("timestamps must be strictly increasing"))
    arr = data isa Array ? data : Array(data)
    return NonSequentialTimeSeries{eltype(arr),ndims(arr)}(
        Vector{DateTime}(timestamps), arr, String(name), _maybe_string(ext)
    )
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

struct Deterministic{T,N} <: AbstractDeterministic
    initial_timestamp::DateTime
    resolution::Period
    horizon::Period
    interval::Period
    count::Int
    "Values with canonical shape `(H, count, element_dims...)`."
    data::Array{T,N}
    "Association name (required)."
    name::String
    "Opaque, package-owned extension payload (typically JSON) the binding writes and reads to reconstruct domain objects; the store never interprets it."
    ext::Union{Nothing,String}
end

function Deterministic(
    initial,
    resolution,
    horizon,
    interval,
    count,
    data::AbstractArray,
    name::AbstractString;
    ext::Union{Nothing,AbstractString}=nothing,
)
    return Deterministic{eltype(data),ndims(data)}(
        initial,
        resolution,
        horizon,
        interval,
        Int(count),
        data isa Array ? data : Array(data),
        String(name),
        _maybe_string(ext),
    )
end

struct Probabilistic{T,N}
    initial_timestamp::DateTime
    resolution::Period
    horizon::Period
    interval::Period
    count::Int
    percentiles::Vector{Float64}
    "Values with canonical shape `(num_percentiles, H, count, element_dims...)`."
    data::Array{T,N}
    "Association name (required)."
    name::String
    "Opaque, package-owned extension payload (typically JSON) the binding writes and reads to reconstruct domain objects; the store never interprets it."
    ext::Union{Nothing,String}
end

function Probabilistic(
    initial,
    resolution,
    horizon,
    interval,
    count,
    percentiles,
    data::AbstractArray,
    name::AbstractString;
    ext::Union{Nothing,AbstractString}=nothing,
)
    return Probabilistic{eltype(data),ndims(data)}(
        initial,
        resolution,
        horizon,
        interval,
        Int(count),
        Vector{Float64}(percentiles),
        data isa Array ? data : Array(data),
        String(name),
        _maybe_string(ext),
    )
end

struct Scenarios{T,N}
    initial_timestamp::DateTime
    resolution::Period
    horizon::Period
    interval::Period
    count::Int
    scenario_count::Int
    "Values with canonical shape `(scenario_count, H, count, element_dims...)`."
    data::Array{T,N}
    "Association name (required)."
    name::String
    "Opaque, package-owned extension payload (typically JSON) the binding writes and reads to reconstruct domain objects; the store never interprets it."
    ext::Union{Nothing,String}
end

# `scenario_count` defaults to the leading axis of `data`.
function Scenarios(
    initial,
    resolution,
    horizon,
    interval,
    count,
    data::AbstractArray,
    name::AbstractString;
    ext::Union{Nothing,AbstractString}=nothing,
)
    return Scenarios{eltype(data),ndims(data)}(
        initial,
        resolution,
        horizon,
        interval,
        Int(count),
        size(data, 1),
        data isa Array ? data : Array(data),
        String(name),
        _maybe_string(ext),
    )
end

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
    handle::Ptr{Cvoid}
    function TimeSeriesKey(handle::Ptr{Cvoid})
        k = new(handle)
        finalizer(_finalize_key, k)
        return k
    end
end

function _finalize_key(k::TimeSeriesKey)
    if k.handle != C_NULL
        ccall((:castore_key_free, lib_path()), Cvoid, (Ptr{Cvoid},), k.handle)
        k.handle = C_NULL
    end
end

# ---- Store ----------------------------------------------------------------

mutable struct Store
    handle::Ptr{Cvoid}
    function Store(handle::Ptr{Cvoid})
        s = new(handle)
        finalizer(close!, s)
        return s
    end
end

function close!(s::Store)
    if s.handle != C_NULL
        ccall((:castore_store_free, lib_path()), Cvoid, (Ptr{Cvoid},), s.handle)
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
function Store(;
    in_memory::Bool=true,
    path::Union{Nothing,AbstractString}=nothing,
    compression::Union{Symbol,AbstractString}=:deflate,
    compression_level::Integer=3,
    shuffle::Bool=true,
)
    kind = Symbol(compression)
    compression_kind = if kind === :none
        UInt8(0)
    elseif kind === :deflate
        UInt8(1)
    else
        throw(
            ArgumentError(
                "unknown compression $(repr(compression)), expected :deflate or :none"
            ),
        )
    end
    out = Ref{Ptr{Cvoid}}(C_NULL)
    cpath = path === nothing ? C_NULL : String(path)
    code = ccall(
        (:castore_store_create_with_compression, lib_path()),
        Int32,
        (Cstring, Bool, UInt8, UInt8, Bool, Ref{Ptr{Cvoid}}),
        cpath,
        in_memory,
        compression_kind,
        UInt8(compression_level),
        shuffle,
        out,
    )
    _check(code)
    return Store(out[])
end

"""
    open_store(path; read_only=false)

Open an existing on-disk store.
"""
function open_store(path::AbstractString; read_only::Bool=false)
    out = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:castore_store_open, lib_path()),
        Int32,
        (Cstring, Bool, Ref{Ptr{Cvoid}}),
        path,
        read_only,
        out,
    )
    _check(code)
    return Store(out[])
end

"""
    Store(f::Function; kwargs...)
    open_store(f::Function, path; read_only=false)

Do-block forms: construct (or open) a store, run `f(store)`, and guarantee
`close!` on exit — including on throw.

```julia
Store(in_memory=true) do store
    add_time_series!(store, 1, "Generator", Component, ts)
end
```
"""
function Store(f::Function; kwargs...)
    s = Store(; kwargs...)
    try
        return f(s)
    finally
        close!(s)
    end
end

function open_store(f::Function, path::AbstractString; read_only::Bool=false)
    s = open_store(path; read_only=read_only)
    try
        return f(s)
    finally
        close!(s)
    end
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

# Lower an optional `(start, end)` DateTime range to the FFI's
# (present::Bool, start_ms::Int64, end_ms::Int64) triple. `nothing` -> no range.
function _time_range_args(time_range::Union{Nothing,Tuple{DateTime,DateTime}})
    time_range === nothing && return (false, Int64(0), Int64(0))
    return (true, _to_unix_ms(time_range[1]), _to_unix_ms(time_range[2]))
end

# Canonical fixed-span milliseconds -> ISO-8601 (the Rust core re-canonicalizes,
# so any correct fixed encoding round-trips).
function _fixed_ms_to_iso(ms::Integer)::String
    if ms % 1000 == 0
        return "PT$(ms ÷ 1000)S"
    else
        whole = ms ÷ 1000
        frac = rstrip(lpad(ms % 1000, 3, '0'), '0')
        return "PT$(whole).$(frac)S"
    end
end

# Encode a Julia `Dates.Period` as an ISO-8601 duration string. Calendar periods
# (`Year`/`Quarter`/`Month`) map to `P..Y`/`P..M`; fixed periods go through
# milliseconds.
function _period_to_iso(p::Period)::String
    if p isa Dates.Year
        return "P$(Dates.value(p))Y"
    elseif p isa Dates.Quarter
        return "P$(3 * Dates.value(p))M"
    elseif p isa Dates.Month
        return "P$(Dates.value(p))M"
    else
        return _fixed_ms_to_iso(Dates.toms(p))
    end
end

# Pass an optional resolution to the FFI: `nothing` -> C_NULL (unset).
_period_to_cstr(p) = p === nothing ? C_NULL : _period_to_iso(p)

# Decode an ISO-8601 duration string into a Julia `Dates.Period`. Calendar units
# (Y/M before the `T`) yield `Year`/`Month`; fixed units yield a `Millisecond`.
function _iso_to_period(s::AbstractString)::Period
    m = match(
        r"^P(?:(\d+)Y)?(?:(\d+)M)?(?:(\d+)W)?(?:(\d+)D)?(?:T(?:(\d+)H)?(?:(\d+)M)?(?:([\d.]+)S)?)?$",
        s,
    )
    m === nothing && error("invalid ISO-8601 period: $s")
    years, months, weeks, days, hours, mins, secs = m.captures
    has_cal = years !== nothing || months !== nothing
    has_fixed =
        weeks !== nothing ||
        days !== nothing ||
        hours !== nothing ||
        mins !== nothing ||
        secs !== nothing
    has_cal && has_fixed && error("ISO-8601 period mixes calendar and fixed units: $s")
    if has_cal
        total_months =
            (years === nothing ? 0 : 12 * parse(Int, years)) +
            (months === nothing ? 0 : parse(Int, months))
        return if total_months % 12 == 0
            Dates.Year(total_months ÷ 12)
        else
            Dates.Month(total_months)
        end
    end
    ms = 0
    weeks !== nothing && (ms += parse(Int, weeks) * 604_800_000)
    days !== nothing && (ms += parse(Int, days) * 86_400_000)
    hours !== nothing && (ms += parse(Int, hours) * 3_600_000)
    mins !== nothing && (ms += parse(Int, mins) * 60_000)
    secs !== nothing && (ms += round(Int, parse(Float64, secs) * 1000))
    return Dates.Millisecond(ms)
end

# Read + free an owned C string returned by the FFI; `nothing` for a null pointer.
function _take_cstr(ptr::Ptr{Cchar})
    ptr == C_NULL && return nothing
    s = unsafe_string(ptr)
    ccall((:castore_string_free, lib_path()), Cvoid, (Ptr{Cchar},), ptr)
    return s
end

# Read + free an owned ISO-8601 period C string; `nothing` if null.
function _take_period(ptr::Ptr{Cchar})
    return (s=_take_cstr(ptr); s === nothing ? nothing : _iso_to_period(s))
end

# Decode a probe-then-fetch caller buffer (`buf`, byte length `len`) written by a
# `write_str_out`-style FFI out-param. An empty string means the field is unset,
# returned as `nothing`.
function _take_buffer_string(buf::Vector{UInt8}, len::Integer)
    n = min(Int(len), length(buf))
    return n == 0 ? nothing : String(buf[1:n])
end

# Element-shape out-buffer: `len` is the full dimension count reported by the
# FFI; anything beyond the buffer capacity is pathological, so fail loudly
# rather than silently truncating a shape.
function _take_element_shape(buf::Vector{UInt64}, len::Integer)
    Int(len) <= length(buf) || error(
        "element shape has $(Int(len)) dimensions, exceeding the $(length(buf))-slot buffer",
    )
    return Tuple(Int(d) for d in buf[1:Int(len)])
end

# Features out-buffer: the FFI reports the full JSON byte length; a truncated
# JSON document must never be parsed, so fail loudly if it did not fit.
function _take_features_json(buf::Vector{UInt8}, len::Integer)
    Int(len) < length(buf) || error(
        "features JSON is $(Int(len)) bytes, exceeding the $(length(buf))-byte buffer"
    )
    n = Int(len)
    n == 0 && return Dict{String,Any}()
    return JSON.parse(String(buf[1:n]))
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
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    name = ts.name
    initial_ms = _to_unix_ms(ts.initial_timestamp)
    resolution_iso = _period_to_iso(ts.resolution)
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    units_ptr = units === nothing ? C_NULL : String(units)
    ext_ptr = ext === nothing ? C_NULL : String(ext)

    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:castore_store_add_single, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Cstring,
            Int32,
            Cstring,
            Int64,
            Cstring,
            Int32,
            UInt64,
            Ptr{UInt64},
            Ptr{UInt8},
            UInt64,
            Cstring,
            Cstring,
            Cstring,
            Ref{Ptr{Cvoid}},
        ),
        store.handle,
        Int64(owner_id),
        owner_type,
        Int32(Int(owner_category)),
        name,
        initial_ms,
        resolution_iso,
        dtype,
        UInt64(length(dims)),
        dims,
        bytes,
        UInt64(length(bytes)),
        ext_ptr,
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
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    name = ts.name
    timestamps = Int64[_to_unix_ms(timestamp) for timestamp in ts.timestamps]
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    units_ptr = units === nothing ? C_NULL : String(units)
    ext_ptr = ext === nothing ? C_NULL : String(ext)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:castore_store_add_non_sequential, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Cstring,
            Int32,
            Cstring,
            Ptr{Int64},
            UInt64,
            Int32,
            UInt64,
            Ptr{UInt64},
            Ptr{UInt8},
            UInt64,
            Cstring,
            Cstring,
            Cstring,
            Ref{Ptr{Cvoid}},
        ),
        store.handle,
        Int64(owner_id),
        owner_type,
        Int32(Int(owner_category)),
        name,
        timestamps,
        UInt64(length(timestamps)),
        dtype,
        UInt64(length(dims)),
        dims,
        bytes,
        UInt64(length(bytes)),
        ext_ptr,
        features_json,
        units_ptr,
        out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

"""
    get_metadata(store, owner_id, owner_category, name; resolution, features=Dict())

Look up a SingleTimeSeries by attributes and return a named tuple of
`(initial_timestamp, resolution, length, data_hash, dtype, ext, units,
element_shape, features)`. `owner_category` is the
owner's `OwnerCategory` (`Component` or `SupplementalAttribute`). `data_hash` is
the 32-byte content hash as a `Vector{UInt8}`. `ext` and `units` are
`nothing` when unset. `element_shape` is the per-timestep shape tuple (empty for
scalar elements; for forecasts across all metadata getters it is the stored
array's trailing dims after its first axis) and `features` the feature
dictionary (empty when none). Throws `NotFoundError` if absent.
"""
function get_metadata(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_iso = _period_to_cstr(resolution)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    out_initial = Ref{Int64}(0)
    out_resolution = Ref{Ptr{Cchar}}(C_NULL)
    out_length = Ref{UInt64}(0)
    out_hash = Vector{UInt8}(undef, 32)
    out_dtype = Ref{Int32}(0)
    lt_buf = Vector{UInt8}(undef, 256)
    out_lt_len = Ref{UInt64}(0)
    units_buf = Vector{UInt8}(undef, 256)
    out_units_len = Ref{UInt64}(0)
    shape_buf = Vector{UInt64}(undef, 8)
    out_shape_len = Ref{UInt64}(0)
    fj_buf = Vector{UInt8}(undef, 4096)
    out_fj_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_get_metadata, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Int32,
            Cstring,
            Cstring,
            Cstring,
            Ref{Int64},
            Ref{Ptr{Cchar}},
            Ref{UInt64},
            Ptr{UInt8},
            Ref{Int32},
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
            Ptr{UInt64},
            UInt64,
            Ref{UInt64},
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
        ),
        store.handle,
        Int64(owner_id),
        _category_int(owner_category),
        name,
        resolution_iso,
        features_json,
        out_initial,
        out_resolution,
        out_length,
        out_hash,
        out_dtype,
        lt_buf,
        UInt64(length(lt_buf)),
        out_lt_len,
        units_buf,
        UInt64(length(units_buf)),
        out_units_len,
        shape_buf,
        UInt64(length(shape_buf)),
        out_shape_len,
        fj_buf,
        UInt64(length(fj_buf)),
        out_fj_len,
    )
    _check(code)
    resolution = _take_period(out_resolution[])
    ext = _take_buffer_string(lt_buf, out_lt_len[])
    units = _take_buffer_string(units_buf, out_units_len[])
    return (
        initial_timestamp=_from_unix_ms(out_initial[]),
        resolution=resolution,
        length=Int(out_length[]),
        data_hash=out_hash,
        dtype=_julia_dtype(out_dtype[]),
        ext=ext,
        units=units,
        element_shape=_take_element_shape(shape_buf, out_shape_len[]),
        features=_take_features_json(fj_buf, out_fj_len[]),
    )
end

"""
    get_forecast_metadata(store, owner_id, owner_category, name, ts_type; resolution, interval, features=Dict())

Return `(initial_timestamp, resolution, horizon, interval, count, length, data_hash, ext,
units, element_shape, features)`
for a stored forecast of integer `ts_type` (see the `CASTORE_TYPE_*` constants).
`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`). The optional `interval` keyword (a `Period`) restricts
the lookup to a forecast with that interval. `ext` and `units` are
`nothing` when unset.
"""
function get_forecast_metadata(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts_type::Integer;
    resolution::Union{Nothing,Period}=nothing,
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_iso = _period_to_cstr(resolution)
    interval_iso = _period_to_cstr(interval)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    out_initial = Ref{Int64}(0);
    out_resolution = Ref{Ptr{Cchar}}(C_NULL)
    out_horizon = Ref{Ptr{Cchar}}(C_NULL);
    out_interval = Ref{Ptr{Cchar}}(C_NULL)
    out_count = Ref{UInt64}(0);
    out_length = Ref{UInt64}(0)
    out_hash = Vector{UInt8}(undef, 32)
    lt_buf = Vector{UInt8}(undef, 256);
    out_lt_len = Ref{UInt64}(0)
    units_buf = Vector{UInt8}(undef, 256);
    out_units_len = Ref{UInt64}(0)
    shape_buf = Vector{UInt64}(undef, 8);
    out_shape_len = Ref{UInt64}(0)
    fj_buf = Vector{UInt8}(undef, 4096);
    out_fj_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_get_forecast_metadata, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Int32,
            Cstring,
            Int32,
            Cstring,
            Cstring,
            Cstring,
            Ref{Int64},
            Ref{Ptr{Cchar}},
            Ref{Ptr{Cchar}},
            Ref{Ptr{Cchar}},
            Ref{UInt64},
            Ref{UInt64},
            Ptr{UInt8},
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
            Ptr{UInt64},
            UInt64,
            Ref{UInt64},
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
        ),
        store.handle,
        Int64(owner_id),
        _category_int(owner_category),
        name,
        Int32(ts_type),
        resolution_iso,
        interval_iso,
        features_json,
        out_initial,
        out_resolution,
        out_horizon,
        out_interval,
        out_count,
        out_length,
        out_hash,
        lt_buf,
        UInt64(length(lt_buf)),
        out_lt_len,
        units_buf,
        UInt64(length(units_buf)),
        out_units_len,
        shape_buf,
        UInt64(length(shape_buf)),
        out_shape_len,
        fj_buf,
        UInt64(length(fj_buf)),
        out_fj_len,
    )
    _check(code)
    ext = _take_buffer_string(lt_buf, out_lt_len[])
    units = _take_buffer_string(units_buf, out_units_len[])
    return (
        initial_timestamp=_from_unix_ms(out_initial[]),
        resolution=_take_period(out_resolution[]),
        horizon=_take_period(out_horizon[]),
        interval=_take_period(out_interval[]),
        count=Int(out_count[]),
        length=Int(out_length[]),
        data_hash=out_hash,
        ext=ext,
        units=units,
        element_shape=_take_element_shape(shape_buf, out_shape_len[]),
        features=_take_features_json(fj_buf, out_fj_len[]),
    )
end

"""
    get_probabilistic_metadata(store, owner_id, owner_category, name; resolution, interval, features=Dict())

Return `(initial_timestamp, resolution, horizon, interval, count, length, data_hash, percentiles,
units, element_shape, features)`
for a stored `Probabilistic` forecast. `percentiles` is the stored percentile
vector; `units` is `nothing` when unset. Previously the percentiles were only
reachable through a full data fetch.
"""
function get_probabilistic_metadata(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_iso = _period_to_cstr(resolution)
    interval_iso = _period_to_cstr(interval)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    out_initial = Ref{Int64}(0);
    out_resolution = Ref{Ptr{Cchar}}(C_NULL)
    out_horizon = Ref{Ptr{Cchar}}(C_NULL);
    out_interval = Ref{Ptr{Cchar}}(C_NULL)
    out_count = Ref{UInt64}(0);
    out_length = Ref{UInt64}(0)
    out_hash = Vector{UInt8}(undef, 32)
    out_pct = Ref{Ptr{Float64}}(C_NULL);
    out_pct_len = Ref{UInt64}(0)
    units_buf = Vector{UInt8}(undef, 256);
    out_units_len = Ref{UInt64}(0)
    shape_buf = Vector{UInt64}(undef, 8);
    out_shape_len = Ref{UInt64}(0)
    fj_buf = Vector{UInt8}(undef, 4096);
    out_fj_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_get_probabilistic_metadata, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Int32,
            Cstring,
            Cstring,
            Cstring,
            Cstring,
            Ref{Int64},
            Ref{Ptr{Cchar}},
            Ref{Ptr{Cchar}},
            Ref{Ptr{Cchar}},
            Ref{UInt64},
            Ref{UInt64},
            Ptr{UInt8},
            Ref{Ptr{Float64}},
            Ref{UInt64},
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
            Ptr{UInt64},
            UInt64,
            Ref{UInt64},
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
        ),
        store.handle,
        Int64(owner_id),
        _category_int(owner_category),
        name,
        resolution_iso,
        interval_iso,
        features_json,
        out_initial,
        out_resolution,
        out_horizon,
        out_interval,
        out_count,
        out_length,
        out_hash,
        out_pct,
        out_pct_len,
        units_buf,
        UInt64(length(units_buf)),
        out_units_len,
        shape_buf,
        UInt64(length(shape_buf)),
        out_shape_len,
        fj_buf,
        UInt64(length(fj_buf)),
        out_fj_len,
    )
    _check(code)
    percentiles = copy(unsafe_wrap(Array, out_pct[], Int(out_pct_len[]); own=false))
    ccall(
        (:castore_buffer_free_f64, lib_path()),
        Cvoid,
        (Ptr{Float64}, UInt64),
        out_pct[],
        out_pct_len[],
    )
    return (
        initial_timestamp=_from_unix_ms(out_initial[]),
        resolution=_take_period(out_resolution[]),
        horizon=_take_period(out_horizon[]),
        interval=_take_period(out_interval[]),
        count=Int(out_count[]),
        length=Int(out_length[]),
        data_hash=out_hash,
        percentiles=percentiles,
        units=_take_buffer_string(units_buf, out_units_len[]),
        element_shape=_take_element_shape(shape_buf, out_shape_len[]),
        features=_take_features_json(fj_buf, out_fj_len[]),
    )
end

"""
    rename_time_series!(store, key, new_name) -> TimeSeriesKey

Rename the series identified by `key` to `new_name`, returning the renamed key
(same identity, new name). Only the catalog name changes.
"""
function rename_time_series!(store::Store, key::TimeSeriesKey, new_name::AbstractString)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:castore_store_rename, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{Cvoid}, Cstring, Ref{Ptr{Cvoid}}),
        store.handle,
        key.handle,
        String(new_name),
        out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

"""
    resolve_forecast_key(store, owner_id, owner_category, name, requested_type; resolution, interval, features=Dict()) -> TimeSeriesKey

Resolve a forecast addressed by attributes plus a `requested_type` (a `CASTORE_TYPE_*`
forecast code, or `CASTORE_TYPE_ABSTRACT_DETERMINISTIC` for the Deterministic /
DeterministicSingleTimeSeries family) to its concrete key. Throws on an ambiguous
match or a miss.
"""
function resolve_forecast_key(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString,
    requested_type::Integer;
    resolution::Union{Nothing,Period}=nothing,
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_iso = _period_to_cstr(resolution)
    interval_iso = _period_to_cstr(interval)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:castore_store_resolve_forecast_key, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Int32,
            Cstring,
            Cstring,
            Cstring,
            Cstring,
            Int32,
            Ref{Ptr{Cvoid}},
        ),
        store.handle,
        Int64(owner_id),
        _category_int(owner_category),
        name,
        resolution_iso,
        interval_iso,
        features_json,
        Int32(requested_type),
        out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

"""
    get_array_by_hash(store, data_hash, ::Type{T}=Float64) -> Vector{T}

Fetch the full stored array for a 32-byte content hash, interpreting the raw
element bytes as `T`. For multi-dimensional element shapes the result is the
flat row-major vector; the caller reshapes using the known element shape.
"""
function get_array_by_hash(
    store::Store, data_hash::Vector{UInt8}, ::Type{T}=Float64
) where {T}
    length(data_hash) == 32 || throw(InvalidParameterError("data_hash must be 32 bytes"))
    out_dtype = Ref{Int32}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_get_array_by_hash, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{UInt8}, Ref{Int32}, Ref{Ptr{UInt8}}, Ref{UInt64}),
        store.handle,
        data_hash,
        out_dtype,
        out_data,
        out_len,
    )
    _check(code)
    nbytes = Int(out_len[])
    raw = unsafe_wrap(Array, out_data[], nbytes; own=false)
    bytes = copy(raw)
    ccall(
        (:castore_buffer_free_u8, lib_path()),
        Cvoid,
        (Ptr{UInt8}, UInt64),
        out_data[],
        out_len[],
    )
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
        (:castore_store_count_array_references, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{UInt8}, Ref{UInt64}, Ref{UInt64}),
        store.handle,
        data_hash,
        out_sts,
        out_dst,
    )
    _check(code)
    return (sts=Int(out_sts[]), dst=Int(out_dst[]))
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
    resolution_iso = _period_to_cstr(resolution)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    out = Ref{Bool}(false)
    code = ccall(
        (:castore_store_has_by_attrs, lib_path()),
        Int32,
        (Ptr{Cvoid}, Int64, Int32, Cstring, Cstring, Cstring, Ref{Bool}),
        store.handle,
        Int64(owner_id),
        _category_int(owner_category),
        name,
        resolution_iso,
        features_json,
        out,
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
        (:castore_store_has_for_owner, lib_path()),
        Int32,
        (Ptr{Cvoid}, Int64, Int32, Int32, Bool, Ref{Bool}),
        store.handle,
        Int64(owner_id),
        _category_int(owner_category),
        Int32(use_type ? time_series_type : 0),
        use_type,
        out,
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
    resolution_iso = _period_to_cstr(resolution)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    code = ccall(
        (:castore_store_remove_by_attrs, lib_path()),
        Int32,
        (Ptr{Cvoid}, Int64, Int32, Cstring, Cstring, Cstring),
        store.handle,
        Int64(owner_id),
        _category_int(owner_category),
        name,
        resolution_iso,
        features_json,
    )
    _check(code)
    return nothing
end

function get_time_series(
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    out_initial = Ref{Int64}(0)
    out_resolution = Ref{Ptr{Cchar}}(C_NULL)
    out_dtype = Ref{Int32}(0)
    out_shape = Ref{Ptr{Int64}}(C_NULL)
    out_shape_len = Ref{UInt64}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL)
    out_data_len = Ref{UInt64}(0)
    tr_present, tr_start, tr_end = _time_range_args(time_range)
    code = ccall(
        (:castore_store_get_single, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Ptr{Cvoid},
            Bool,
            Int64,
            Int64,
            Ref{Int64},
            Ref{Ptr{Cchar}},
            Ref{Int32},
            Ref{Ptr{Int64}},
            Ref{UInt64},
            Ref{Ptr{UInt8}},
            Ref{UInt64},
        ),
        store.handle,
        key.handle,
        tr_present,
        tr_start,
        tr_end,
        out_initial,
        out_resolution,
        out_dtype,
        out_shape,
        out_shape_len,
        out_data,
        out_data_len,
    )
    _check(code)

    # Full array shape [length, *element_shape] (row-major dims), then bytes.
    dims = Int.(copy(unsafe_wrap(Array, out_shape[], Int(out_shape_len[]); own=false)))
    ccall(
        (:castore_buffer_free_i64, lib_path()),
        Cvoid,
        (Ptr{Int64}, UInt64),
        out_shape[],
        out_shape_len[],
    )
    bytes = copy(unsafe_wrap(Array, out_data[], Int(out_data_len[]); own=false))
    ccall(
        (:castore_buffer_free_u8, lib_path()),
        Cvoid,
        (Ptr{UInt8}, UInt64),
        out_data[],
        out_data_len[],
    )

    T = _julia_dtype(out_dtype[])
    flat = collect(reinterpret(T, bytes))
    nd = length(dims)
    # Stored row-major → canonical column-major Julia layout (see get_array_nd).
    data = if nd <= 1
        flat
    else
        permutedims(reshape(flat, reverse(dims)...), reverse(ntuple(identity, nd)))
    end

    initial = _from_unix_ms(out_initial[])
    resolution = _take_period(out_resolution[])
    assoc = _get_association(store, key)
    return SingleTimeSeries(initial, resolution, data, assoc.name)
end

# Reconstruct one SingleTimeSeries from a bulk-read result slot.
function _bulk_single(result::Ptr{Cvoid}, idx::Integer, name::AbstractString)
    out_initial = Ref{Int64}(0);
    out_resolution = Ref{Ptr{Cchar}}(C_NULL)
    out_dtype = Ref{Int32}(0);
    out_shape = Ref{Ptr{Int64}}(C_NULL);
    out_shape_len = Ref{UInt64}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL);
    out_data_len = Ref{UInt64}(0)
    _check(
        ccall(
            (:castore_bulk_result_get_single, lib_path()),
            Int32,
            (
                Ptr{Cvoid},
                UInt64,
                Ref{Int64},
                Ref{Ptr{Cchar}},
                Ref{Int32},
                Ref{Ptr{Int64}},
                Ref{UInt64},
                Ref{Ptr{UInt8}},
                Ref{UInt64},
            ),
            result,
            UInt64(idx),
            out_initial,
            out_resolution,
            out_dtype,
            out_shape,
            out_shape_len,
            out_data,
            out_data_len,
        ),
    )
    dims = Int.(copy(unsafe_wrap(Array, out_shape[], Int(out_shape_len[]); own=false)))
    ccall(
        (:castore_buffer_free_i64, lib_path()),
        Cvoid,
        (Ptr{Int64}, UInt64),
        out_shape[],
        out_shape_len[],
    )
    bytes = copy(unsafe_wrap(Array, out_data[], Int(out_data_len[]); own=false))
    ccall(
        (:castore_buffer_free_u8, lib_path()),
        Cvoid,
        (Ptr{UInt8}, UInt64),
        out_data[],
        out_data_len[],
    )
    data = _decode_forecast_array(bytes, out_dtype[], dims)
    return SingleTimeSeries(
        _from_unix_ms(out_initial[]), _take_period(out_resolution[]), data, name
    )
end

# Reconstruct one NonSequentialTimeSeries from a bulk-read result slot (no
# ext: a bulk read carries array data, not the metadata row).
function _bulk_non_sequential(result::Ptr{Cvoid}, idx::Integer, name::AbstractString)
    out_ts = Ref{Ptr{Int64}}(C_NULL);
    out_ts_len = Ref{UInt64}(0)
    out_dtype = Ref{Int32}(0);
    out_shape = Ref{Ptr{Int64}}(C_NULL);
    out_shape_len = Ref{UInt64}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL);
    out_data_len = Ref{UInt64}(0)
    _check(
        ccall(
            (:castore_bulk_result_get_non_sequential, lib_path()),
            Int32,
            (
                Ptr{Cvoid},
                UInt64,
                Ref{Ptr{Int64}},
                Ref{UInt64},
                Ref{Int32},
                Ref{Ptr{Int64}},
                Ref{UInt64},
                Ref{Ptr{UInt8}},
                Ref{UInt64},
            ),
            result,
            UInt64(idx),
            out_ts,
            out_ts_len,
            out_dtype,
            out_shape,
            out_shape_len,
            out_data,
            out_data_len,
        ),
    )
    ts_ms = copy(unsafe_wrap(Array, out_ts[], Int(out_ts_len[]); own=false))
    ccall(
        (:castore_buffer_free_i64, lib_path()),
        Cvoid,
        (Ptr{Int64}, UInt64),
        out_ts[],
        out_ts_len[],
    )
    dims = Int.(copy(unsafe_wrap(Array, out_shape[], Int(out_shape_len[]); own=false)))
    ccall(
        (:castore_buffer_free_i64, lib_path()),
        Cvoid,
        (Ptr{Int64}, UInt64),
        out_shape[],
        out_shape_len[],
    )
    bytes = copy(unsafe_wrap(Array, out_data[], Int(out_data_len[]); own=false))
    ccall(
        (:castore_buffer_free_u8, lib_path()),
        Cvoid,
        (Ptr{UInt8}, UInt64),
        out_data[],
        out_data_len[],
    )
    data = _decode_forecast_array(bytes, out_dtype[], dims)
    return NonSequentialTimeSeries(_from_unix_ms.(ts_ms), data, name)
end

# Reconstruct one forecast (Deterministic / Probabilistic / Scenarios) from a
# bulk-read result slot; `type_code` is the ts_type discriminant.
function _bulk_forecast(
    result::Ptr{Cvoid}, idx::Integer, type_code::Integer, name::AbstractString
)
    out_initial = Ref{Int64}(0);
    out_res = Ref{Ptr{Cchar}}(C_NULL)
    out_horizon = Ref{Ptr{Cchar}}(C_NULL);
    out_interval = Ref{Ptr{Cchar}}(C_NULL)
    out_count = Ref{UInt64}(0);
    out_scen = Ref{UInt64}(0)
    out_ndims = Ref{UInt64}(0);
    out_dims = Ref{Ptr{UInt64}}(C_NULL);
    out_dtype = Ref{Int32}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL);
    out_byte_len = Ref{UInt64}(0)
    out_pct = Ref{Ptr{Float64}}(C_NULL);
    out_pct_len = Ref{UInt64}(0)
    _check(
        ccall(
            (:castore_bulk_result_get_forecast, lib_path()),
            Int32,
            (
                Ptr{Cvoid},
                UInt64,
                Ref{Int64},
                Ref{Ptr{Cchar}},
                Ref{Ptr{Cchar}},
                Ref{Ptr{Cchar}},
                Ref{UInt64},
                Ref{UInt64},
                Ref{UInt64},
                Ref{Ptr{UInt64}},
                Ref{Int32},
                Ref{Ptr{UInt8}},
                Ref{UInt64},
                Ref{Ptr{Float64}},
                Ref{UInt64},
            ),
            result,
            UInt64(idx),
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
        ),
    )
    nd = Int(out_ndims[])
    dims = Int.(copy(unsafe_wrap(Array, out_dims[], nd; own=false)))
    ccall(
        (:castore_buffer_free_u64, lib_path()),
        Cvoid,
        (Ptr{UInt64}, UInt64),
        out_dims[],
        out_ndims[],
    )
    bytes = copy(unsafe_wrap(Array, out_data[], Int(out_byte_len[]); own=false))
    ccall(
        (:castore_buffer_free_u8, lib_path()),
        Cvoid,
        (Ptr{UInt8}, UInt64),
        out_data[],
        out_byte_len[],
    )
    percentiles = if Int(out_pct_len[]) > 0 && out_pct[] != C_NULL
        p = copy(unsafe_wrap(Array, out_pct[], Int(out_pct_len[]); own=false))
        ccall(
            (:castore_buffer_free_f64, lib_path()),
            Cvoid,
            (Ptr{Float64}, UInt64),
            out_pct[],
            out_pct_len[],
        )
        p
    else
        Float64[]
    end
    data = _decode_forecast_array(bytes, out_dtype[], dims)
    initial = _from_unix_ms(out_initial[]);
    resolution = _take_period(out_res[])
    horizon = _take_period(out_horizon[]);
    interval = _take_period(out_interval[])
    count = Int(out_count[])
    if type_code == CASTORE_TYPE_PROBABILISTIC
        return Probabilistic(
            initial, resolution, horizon, interval, count, percentiles, data, name
        )
    elseif type_code == CASTORE_TYPE_SCENARIOS
        return Scenarios(initial, resolution, horizon, interval, count, data, name)
    else
        return Deterministic(initial, resolution, horizon, interval, count, data, name)
    end
end

"""
    bulk_read(store, keys; time_range=nothing) -> Vector

Read many full series at once, returning one per key in order — dispatching on
each key's stored type to the proper Julia struct (`SingleTimeSeries`,
`NonSequentialTimeSeries`, `Deterministic`, `Probabilistic`, or `Scenarios`).
The packed `SingleTimeSeries` are read in a single decompress-once pass per
dataset. Pass `time_range = (start, stop)` to slice every series to that window.
"""
function bulk_read(
    store::Store,
    keys::AbstractVector{TimeSeriesKey};
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    n = length(keys)
    out = Vector{Any}(undef, n)
    n == 0 && return out

    key_handles = Ptr{Cvoid}[k.handle for k in keys]
    out_result = Ref{Ptr{Cvoid}}(C_NULL)
    tr_present, tr_start, tr_end = _time_range_args(time_range)
    code = GC.@preserve keys key_handles ccall(
        (:castore_store_bulk_read, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{Ptr{Cvoid}}, UInt64, Bool, Int64, Int64, Ref{Ptr{Cvoid}}),
        store.handle,
        key_handles,
        UInt64(n),
        tr_present,
        tr_start,
        tr_end,
        out_result,
    )
    _check(code)
    result = out_result[]
    try
        for i in 1:n
            out_type = Ref{Int32}(0)
            _check(
                ccall(
                    (:castore_bulk_result_item_type, lib_path()),
                    Int32,
                    (Ptr{Cvoid}, UInt64, Ref{Int32}),
                    result,
                    UInt64(i - 1),
                    out_type,
                ),
            )
            name = _get_association(store, keys[i]).name
            t = Int(out_type[])
            out[i] = if t == CASTORE_TYPE_SINGLE
                _bulk_single(result, i - 1, name)
            elseif t == CASTORE_TYPE_NON_SEQUENTIAL
                _bulk_non_sequential(result, i - 1, name)
            else
                _bulk_forecast(result, i - 1, t, name)
            end
        end
    finally
        ccall((:castore_bulk_result_free, lib_path()), Cvoid, (Ptr{Cvoid},), result)
    end
    return out
end

function get_time_series(
    ::Type{NonSequentialTimeSeries},
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    out_timestamps = Ref{Ptr{Int64}}(C_NULL)
    out_timestamps_len = Ref{UInt64}(0)
    out_dtype = Ref{Int32}(0)
    out_shape = Ref{Ptr{Int64}}(C_NULL)
    out_shape_len = Ref{UInt64}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL)
    out_data_len = Ref{UInt64}(0)
    lt_buf = Vector{UInt8}(undef, 256)
    out_lt_len = Ref{UInt64}(0)
    tr_present, tr_start, tr_end = _time_range_args(time_range)
    code = ccall(
        (:castore_store_get_non_sequential, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Ptr{Cvoid},
            Bool,
            Int64,
            Int64,
            Ref{Ptr{Int64}},
            Ref{UInt64},
            Ref{Int32},
            Ref{Ptr{Int64}},
            Ref{UInt64},
            Ref{Ptr{UInt8}},
            Ref{UInt64},
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
        ),
        store.handle,
        key.handle,
        tr_present,
        tr_start,
        tr_end,
        out_timestamps,
        out_timestamps_len,
        out_dtype,
        out_shape,
        out_shape_len,
        out_data,
        out_data_len,
        lt_buf,
        UInt64(length(lt_buf)),
        out_lt_len,
    )
    _check(code)

    timestamp_ms = copy(
        unsafe_wrap(Array, out_timestamps[], Int(out_timestamps_len[]); own=false)
    )
    ccall(
        (:castore_buffer_free_i64, lib_path()),
        Cvoid,
        (Ptr{Int64}, UInt64),
        out_timestamps[],
        out_timestamps_len[],
    )
    # Full array shape [length, *element_shape] (row-major dims), then bytes.
    dims = Int.(copy(unsafe_wrap(Array, out_shape[], Int(out_shape_len[]); own=false)))
    ccall(
        (:castore_buffer_free_i64, lib_path()),
        Cvoid,
        (Ptr{Int64}, UInt64),
        out_shape[],
        out_shape_len[],
    )
    bytes = copy(unsafe_wrap(Array, out_data[], Int(out_data_len[]); own=false))
    ccall(
        (:castore_buffer_free_u8, lib_path()),
        Cvoid,
        (Ptr{UInt8}, UInt64),
        out_data[],
        out_data_len[],
    )
    T = _julia_dtype(out_dtype[])
    flat = collect(reinterpret(T, bytes))
    nd = length(dims)
    # Stored row-major → canonical column-major Julia layout (see get_array_nd).
    data = if nd <= 1
        flat
    else
        permutedims(reshape(flat, reverse(dims)...), reverse(ntuple(identity, nd)))
    end
    n = min(Int(out_lt_len[]), length(lt_buf))
    ext = n == 0 ? nothing : String(lt_buf[1:n])
    assoc = _get_association(store, key)
    return NonSequentialTimeSeries(_from_unix_ms.(timestamp_ms), data, assoc.name; ext=ext)
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
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_iso = _period_to_cstr(resolution)
    interval_iso = _period_to_cstr(interval)
    features_json = _features_arg(features)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:castore_make_key_from_attrs, lib_path()),
        Int32,
        (Int64, Int32, Cstring, Int32, Cstring, Cstring, Cstring, Ref{Ptr{Cvoid}}),
        Int64(owner_id),
        _category_int(owner_category),
        name,
        Int32(ts_type),
        resolution_iso,
        interval_iso,
        features_json,
        out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

# Fetch the per-association `name` for a key (the attribute the read FFIs don't
# return), to populate the struct on read.
function _get_association(store::Store, key::TimeSeriesKey)
    name_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_get_association, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        key.handle,
        C_NULL,
        UInt64(0),
        name_len,
    )
    _check(code)
    name_buf = Vector{UInt8}(undef, Int(name_len[]) + 1)
    code = ccall(
        (:castore_store_get_association, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        key.handle,
        name_buf,
        UInt64(length(name_buf)),
        name_len,
    )
    _check(code)
    name = String(name_buf[1:Int(name_len[])])
    return (name=name,)
end

# Association attributes for an attribute-addressed read: build the matching key,
# then look up `name`.
function _assoc_attrs(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts_type::Integer;
    resolution::Union{Nothing,Period}=nothing,
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    return _get_association(
        store,
        _make_key(
            owner_id,
            owner_category,
            name,
            ts_type;
            resolution=resolution,
            interval=interval,
            features=features,
        ),
    )
end

"""
    get_time_series_keys(store, owner_id, owner_category) -> Vector{TimeSeriesKey}

Every key associated with `(owner_id, owner_category)`, one per stored association
(including `DeterministicSingleTimeSeries` rows derived by
`transform_single_time_series!`). `owner_category` is the owner's `OwnerCategory`
(`Component` or `SupplementalAttribute`). Each key can be passed to the key-based
`get_time_series(Type, store, key)` readers — the way to read a transform-derived
forecast by key.
"""
function get_time_series_keys(
    store::Store, owner_id::Integer, owner_category::OwnerCategory
)
    out_keys = Ref{Ptr{Ptr{Cvoid}}}(C_NULL)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_get_time_series_keys, lib_path()),
        Int32,
        (Ptr{Cvoid}, Int64, Int32, Ref{Ptr{Ptr{Cvoid}}}, Ref{UInt64}),
        store.handle,
        Int64(owner_id),
        _category_int(owner_category),
        out_keys,
        out_len,
    )
    _check(code)
    n = Int(out_len[])
    keys = Vector{TimeSeriesKey}(undef, n)
    if n > 0
        # Copy each owned handle into a finalized wrapper, then free the array
        # buffer (the wrappers own the handles and free them via castore_key_free).
        raw = unsafe_wrap(Array, out_keys[], n; own=false)
        for i in 1:n
            keys[i] = TimeSeriesKey(raw[i])
        end
        ccall(
            (:castore_keys_buffer_free, lib_path()),
            Cvoid,
            (Ptr{Ptr{Cvoid}}, UInt64),
            out_keys[],
            out_len[],
        )
    end
    return keys
end

# The Julia time series type for a key's integer type code.
function _type_for_code(code::Integer)
    if code == CASTORE_TYPE_SINGLE
        SingleTimeSeries
    elseif code == CASTORE_TYPE_NON_SEQUENTIAL
        NonSequentialTimeSeries
    elseif code == CASTORE_TYPE_DETERMINISTIC
        Deterministic
    elseif code == CASTORE_TYPE_DETERMINISTIC_SINGLE
        DeterministicSingleTimeSeries
    elseif code == CASTORE_TYPE_PROBABILISTIC
        Probabilistic
    elseif code == CASTORE_TYPE_SCENARIOS
        Scenarios
    else
        throw(InvalidParameterError("unknown time series type code $code"))
    end
end

# The Julia time series type for a metadata row's type name (the `as_str` form).
function _type_for_name(name::AbstractString)
    if name == "SingleTimeSeries"
        SingleTimeSeries
    elseif name == "NonSequentialTimeSeries"
        NonSequentialTimeSeries
    elseif name == "Deterministic"
        Deterministic
    elseif name == "DeterministicSingleTimeSeries"
        DeterministicSingleTimeSeries
    elseif name == "Probabilistic"
        Probabilistic
    elseif name == "Scenarios"
        Scenarios
    else
        throw(InvalidParameterError("unknown time series type name $name"))
    end
end

_row_ms(x) = x === nothing ? nothing : Millisecond(Int64(x))
_row_period(x) = x === nothing ? nothing : _iso_to_period(String(x))
_row_int(x) = x === nothing ? nothing : Int(x)

function _decode_key_row(r::AbstractDict)
    its = r["initial_timestamp_ms"]
    return (
        owner_id=Int64(r["owner_id"]),
        owner_category=String(r["owner_category"]),
        time_series_type=_type_for_name(r["time_series_type"]),
        name=String(r["name"]),
        initial_timestamp=its === nothing ? nothing : _from_unix_ms(Int64(its)),
        resolution=_row_period(r["resolution"]),
        length=_row_int(r["length"]),
        horizon=_row_period(r["horizon"]),
        interval=_row_period(r["interval"]),
        count=_row_int(r["count"]),
        features=Dict{String,Any}(r["features"]),
    )
end

"""
    list_keys(store; owner_id=nothing, owner_category=nothing, time_series_type=nothing,
              name=nothing, resolution=nothing, features=Dict()) -> Vector{NamedTuple}

List the key of every stored time series matching the (all-optional, independent)
filters. With no filter set the whole store is listed.

- `owner_id`, `owner_category` (an `OwnerCategory`) — scope to one owner.
- `time_series_type` — a `CASTORE_TYPE_*` integer code.
- `name` — exact association name.
- `resolution` — a `Period`.
- `features` — match keys whose features include all the given entries (subset).

Each key is a `NamedTuple` with `owner_id`, `owner_category`, `time_series_type`
(the Julia type), `name`, `initial_timestamp`, `resolution`, `length`, `horizon`,
`interval`, `count`, and `features`; fields that do not apply to a key's type are
`nothing`. Physical storage detail (`data_hash`, `ext`, `percentiles`) is
not on the key — read it via [`get_metadata`](@ref) / [`get_forecast_metadata`](@ref).
"""
function list_keys(
    store::Store;
    owner_id::Union{Nothing,Integer}=nothing,
    owner_category::Union{Nothing,OwnerCategory}=nothing,
    time_series_type::Union{Nothing,Integer}=nothing,
    name::Union{Nothing,AbstractString}=nothing,
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    has_owner = owner_id !== nothing
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    has_category = owner_category !== nothing
    category_arg = has_category ? _category_int(owner_category) : Int32(0)
    has_type = time_series_type !== nothing
    type_arg = has_type ? Int32(time_series_type) : Int32(0)
    name_arg = name === nothing ? C_NULL : String(name)
    resolution_iso = _period_to_cstr(resolution)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_list_keys, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Bool,
            Int64,
            Bool,
            Int32,
            Bool,
            Int32,
            Cstring,
            Cstring,
            Cstring,
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
        ),
        store.handle,
        has_owner,
        owner_arg,
        has_category,
        category_arg,
        has_type,
        type_arg,
        name_arg,
        resolution_iso,
        features_json,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:castore_store_list_keys, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Bool,
            Int64,
            Bool,
            Int32,
            Bool,
            Int32,
            Cstring,
            Cstring,
            Cstring,
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
        ),
        store.handle,
        has_owner,
        owner_arg,
        has_category,
        category_arg,
        has_type,
        type_arg,
        name_arg,
        resolution_iso,
        features_json,
        buf,
        UInt64(length(buf)),
        out_len,
    )
    _check(code)
    rows = JSON.parse(String(buf[1:Int(out_len[])]))
    return [_decode_key_row(r) for r in rows]
end

# Shared two-call probe-then-fetch for a filter-based JSON-returning FFI export.
# `f` performs one ccall with the given (buf, cap, out_len); returns the decoded
# JSON payload string.
function _filter_probe(store::Store, ccall_once)
    out_len = Ref{UInt64}(0)
    _check(ccall_once(C_NULL, UInt64(0), out_len))
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    _check(ccall_once(buf, UInt64(length(buf)), out_len))
    return String(buf[1:Int(out_len[])])
end

"""
    list_time_series(store; owner_id=nothing, owner_category=nothing,
                     time_series_type=nothing, name=nothing, resolution=nothing,
                     features=Dict()) -> Vector{Dict}

Full metadata rows matching the filter (same filters as [`list_keys`](@ref)). Each
row is a `Dict` with the key fields plus `data_hash` (hex), `dtype`,
`element_shape`, `percentiles`, `units`, and `ext`.
"""
function list_time_series(
    store::Store;
    owner_id::Union{Nothing,Integer}=nothing,
    owner_category::Union{Nothing,OwnerCategory}=nothing,
    time_series_type::Union{Nothing,Integer}=nothing,
    name::Union{Nothing,AbstractString}=nothing,
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    has_owner = owner_id !== nothing;
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    has_category = owner_category !== nothing
    category_arg = has_category ? _category_int(owner_category) : Int32(0)
    has_type = time_series_type !== nothing;
    type_arg = has_type ? Int32(time_series_type) : Int32(0)
    name_arg = name === nothing ? C_NULL : String(name)
    resolution_iso = _period_to_cstr(resolution)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:castore_store_list_time_series, lib_path()),
            Int32,
            (
                Ptr{Cvoid},
                Bool,
                Int64,
                Bool,
                Int32,
                Bool,
                Int32,
                Cstring,
                Cstring,
                Cstring,
                Ptr{UInt8},
                UInt64,
                Ref{UInt64},
            ),
            store.handle,
            has_owner,
            owner_arg,
            has_category,
            category_arg,
            has_type,
            type_arg,
            name_arg,
            resolution_iso,
            features_json,
            buf,
            cap,
            out_len,
        ),
    )
    return JSON.parse(json)
end

"""
    list_names(store; filters...) -> Vector{String}

Distinct series names matching the filter (same filters as [`list_keys`](@ref)),
sorted.
"""
function list_names(
    store::Store;
    owner_id::Union{Nothing,Integer}=nothing,
    owner_category::Union{Nothing,OwnerCategory}=nothing,
    time_series_type::Union{Nothing,Integer}=nothing,
    name::Union{Nothing,AbstractString}=nothing,
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    has_owner = owner_id !== nothing;
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    has_category = owner_category !== nothing
    category_arg = has_category ? _category_int(owner_category) : Int32(0)
    has_type = time_series_type !== nothing;
    type_arg = has_type ? Int32(time_series_type) : Int32(0)
    name_arg = name === nothing ? C_NULL : String(name)
    resolution_iso = _period_to_cstr(resolution)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:castore_store_list_names, lib_path()),
            Int32,
            (
                Ptr{Cvoid},
                Bool,
                Int64,
                Bool,
                Int32,
                Bool,
                Int32,
                Cstring,
                Cstring,
                Cstring,
                Ptr{UInt8},
                UInt64,
                Ref{UInt64},
            ),
            store.handle,
            has_owner,
            owner_arg,
            has_category,
            category_arg,
            has_type,
            type_arg,
            name_arg,
            resolution_iso,
            features_json,
            buf,
            cap,
            out_len,
        ),
    )
    return String[String(s) for s in JSON.parse(json)]
end

"""
    list_owner_types(store; filters...) -> Vector{String}

Distinct owner types matching the filter (same filters as [`list_keys`](@ref)),
sorted.
"""
function list_owner_types(
    store::Store;
    owner_id::Union{Nothing,Integer}=nothing,
    owner_category::Union{Nothing,OwnerCategory}=nothing,
    time_series_type::Union{Nothing,Integer}=nothing,
    name::Union{Nothing,AbstractString}=nothing,
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    has_owner = owner_id !== nothing;
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    has_category = owner_category !== nothing
    category_arg = has_category ? _category_int(owner_category) : Int32(0)
    has_type = time_series_type !== nothing;
    type_arg = has_type ? Int32(time_series_type) : Int32(0)
    name_arg = name === nothing ? C_NULL : String(name)
    resolution_iso = _period_to_cstr(resolution)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:castore_store_list_owner_types, lib_path()),
            Int32,
            (
                Ptr{Cvoid},
                Bool,
                Int64,
                Bool,
                Int32,
                Bool,
                Int32,
                Cstring,
                Cstring,
                Cstring,
                Ptr{UInt8},
                UInt64,
                Ref{UInt64},
            ),
            store.handle,
            has_owner,
            owner_arg,
            has_category,
            category_arg,
            has_type,
            type_arg,
            name_arg,
            resolution_iso,
            features_json,
            buf,
            cap,
            out_len,
        ),
    )
    return String[String(s) for s in JSON.parse(json)]
end

"""
    remove_by_filter!(store; filters...) -> Int

Remove every series matching the filter (same filters as [`list_keys`](@ref)) in
one all-or-nothing transaction; returns the number removed (0 if none match).
"""
function remove_by_filter!(
    store::Store;
    owner_id::Union{Nothing,Integer}=nothing,
    owner_category::Union{Nothing,OwnerCategory}=nothing,
    time_series_type::Union{Nothing,Integer}=nothing,
    name::Union{Nothing,AbstractString}=nothing,
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    has_owner = owner_id !== nothing;
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    has_category = owner_category !== nothing
    category_arg = has_category ? _category_int(owner_category) : Int32(0)
    has_type = time_series_type !== nothing;
    type_arg = has_type ? Int32(time_series_type) : Int32(0)
    name_arg = name === nothing ? C_NULL : String(name)
    resolution_iso = _period_to_cstr(resolution)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    out_removed = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_remove_by_filter, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Bool,
            Int64,
            Bool,
            Int32,
            Bool,
            Int32,
            Cstring,
            Cstring,
            Cstring,
            Ref{UInt64},
        ),
        store.handle,
        has_owner,
        owner_arg,
        has_category,
        category_arg,
        has_type,
        type_arg,
        name_arg,
        resolution_iso,
        features_json,
        out_removed,
    )
    _check(code)
    return Int(out_removed[])
end

"""
    list_array_groups(store; owner_id=nothing, owner_category=nothing,
                      time_series_type=nothing, name=nothing, resolution=nothing,
                      features=Dict()) -> Vector{NamedTuple}

Like [`list_keys`](@ref) (same filters, same row fields), but each row additionally
carries `data_hash`: the 64-character lowercase hex content hash of the array the
row resolves to. Rows that share a stored array share their `data_hash` — both
deduplicated identical arrays and a `SingleTimeSeries` together with any
`DeterministicSingleTimeSeries` derived from it. Group the returned rows by
`data_hash` to find which time series share their underlying data.

Resolved by a single catalog query in the core (the hash is read off each metadata
row); there are no per-row `get_metadata` round-trips.
"""
function list_array_groups(
    store::Store;
    owner_id::Union{Nothing,Integer}=nothing,
    owner_category::Union{Nothing,OwnerCategory}=nothing,
    time_series_type::Union{Nothing,Integer}=nothing,
    name::Union{Nothing,AbstractString}=nothing,
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    has_owner = owner_id !== nothing
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    has_category = owner_category !== nothing
    category_arg = has_category ? _category_int(owner_category) : Int32(0)
    has_type = time_series_type !== nothing
    type_arg = has_type ? Int32(time_series_type) : Int32(0)
    name_arg = name === nothing ? C_NULL : String(name)
    resolution_iso = _period_to_cstr(resolution)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_list_array_groups, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Bool,
            Int64,
            Bool,
            Int32,
            Bool,
            Int32,
            Cstring,
            Cstring,
            Cstring,
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
        ),
        store.handle,
        has_owner,
        owner_arg,
        has_category,
        category_arg,
        has_type,
        type_arg,
        name_arg,
        resolution_iso,
        features_json,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:castore_store_list_array_groups, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Bool,
            Int64,
            Bool,
            Int32,
            Bool,
            Int32,
            Cstring,
            Cstring,
            Cstring,
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
        ),
        store.handle,
        has_owner,
        owner_arg,
        has_category,
        category_arg,
        has_type,
        type_arg,
        name_arg,
        resolution_iso,
        features_json,
        buf,
        UInt64(length(buf)),
        out_len,
    )
    _check(code)
    rows = JSON.parse(String(buf[1:Int(out_len[])]))
    return [(; _decode_key_row(r)..., data_hash=String(r["data_hash"])) for r in rows]
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
    out_res = Ref{Ptr{Cchar}}(C_NULL)
    out_owner = Ref{Int64}(0)
    out_category = Ref{Int32}(0)
    name_len = Ref{UInt64}(0)
    feat_len = Ref{UInt64}(0)
    # Probe the string lengths (type, resolution, owner id, and owner category are
    # filled on this call too).
    code = ccall(
        (:castore_key_attributes, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Ref{Int32},
            Ref{Ptr{Cchar}},
            Ref{Int64},
            Ref{Int32},
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
        ),
        key.handle,
        out_type,
        out_res,
        out_owner,
        out_category,
        C_NULL,
        UInt64(0),
        name_len,
        C_NULL,
        UInt64(0),
        feat_len,
    )
    _check(code)
    # The probe call also allocates the resolution string; free it and re-read on
    # the fetch call below.
    _take_cstr(out_res[])
    name_buf = Vector{UInt8}(undef, Int(name_len[]) + 1)
    feat_buf = Vector{UInt8}(undef, Int(feat_len[]) + 1)
    code = ccall(
        (:castore_key_attributes, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Ref{Int32},
            Ref{Ptr{Cchar}},
            Ref{Int64},
            Ref{Int32},
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
            Ptr{UInt8},
            UInt64,
            Ref{UInt64},
        ),
        key.handle,
        out_type,
        out_res,
        out_owner,
        out_category,
        name_buf,
        UInt64(length(name_buf)),
        name_len,
        feat_buf,
        UInt64(length(feat_buf)),
        feat_len,
    )
    _check(code)
    name = String(name_buf[1:Int(name_len[])])
    features = JSON.parse(String(feat_buf[1:Int(feat_len[])]))
    resolution = _take_period(out_res[])
    return (
        owner_id=out_owner[],
        owner_category=OwnerCategory(Int(out_category[])),
        name=name,
        time_series_type=_type_for_code(out_type[]),
        resolution=resolution,
        features=features,
    )
end

# Key-based alias so `SingleTimeSeries` matches the `get_time_series(T, store, key)`
# shape the other types use (the bare `get_time_series(store, key)` form is kept).
function get_time_series(
    ::Type{SingleTimeSeries},
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    return get_time_series(store, key; time_range=time_range)
end

"""
    get_time_series(SingleTimeSeries, store, owner_id, owner_category, name; resolution, features, time_range)

Attribute-addressed counterpart to `get_time_series(store, key)`. `owner_category`
is the owner's `OwnerCategory` (`Component` or `SupplementalAttribute`). The
optional `time_range` `(start, stop)` slices like the key-based form.
"""
function get_time_series(
    ::Type{SingleTimeSeries},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    key = _make_key(
        owner_id,
        owner_category,
        name,
        CASTORE_TYPE_SINGLE;
        resolution=resolution,
        features=features,
    )
    return get_time_series(store, key; time_range=time_range)
end

"""
    get_time_series(NonSequentialTimeSeries, store, owner_id, owner_category, name; resolution, features, time_range)

Attribute-addressed counterpart to `get_time_series(NonSequentialTimeSeries, store, key)`.
`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`). The optional `time_range` `(start, stop)` slices like
the key-based form.
"""
function get_time_series(
    ::Type{NonSequentialTimeSeries},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    key = _make_key(
        owner_id,
        owner_category,
        name,
        CASTORE_TYPE_NON_SEQUENTIAL;
        resolution=resolution,
        features=features,
    )
    return get_time_series(NonSequentialTimeSeries, store, key; time_range=time_range)
end

function remove_time_series!(store::Store, key::TimeSeriesKey)
    code = ccall(
        (:castore_store_remove, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{Cvoid}),
        store.handle,
        key.handle,
    )
    _check(code)
    return nothing
end

"""
    remove_time_series!(store, keys::Vector{TimeSeriesKey}) -> Int

Remove several time series in one all-or-nothing transaction, returning the
number removed. On any error (including a single missing key) nothing is
removed.
"""
function remove_time_series!(store::Store, keys::Vector{TimeSeriesKey})
    handles = Ptr{Cvoid}[k.handle for k in keys]
    out_removed = Ref{UInt64}(0)
    code = GC.@preserve keys ccall(
        (:castore_store_remove_bulk, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{Ptr{Cvoid}}, UInt64, Ref{UInt64}),
        store.handle,
        handles,
        UInt64(length(handles)),
        out_removed,
    )
    _check(code)
    return Int(out_removed[])
end

function has_time_series(store::Store, key::TimeSeriesKey)
    out = Ref{Bool}(false)
    code = ccall(
        (:castore_store_has, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{Cvoid}, Ref{Bool}),
        store.handle,
        key.handle,
        out,
    )
    _check(code)
    return out[]
end

function get_counts(store::Store)
    a = Ref{Int64}(0);
    b = Ref{Int64}(0);
    c = Ref{Int64}(0)
    code = ccall(
        (:castore_store_counts, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ref{Int64}, Ref{Int64}, Ref{Int64}),
        store.handle,
        a,
        b,
        c,
    )
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
    code = ccall(
        (:castore_store_counts_by_type, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:castore_store_counts_by_type, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        buf,
        UInt64(length(buf)),
        out_len,
    )
    _check(code)
    rows = JSON.parse(String(buf[1:Int(out_len[])]))
    return [
        (time_series_type=_type_for_name(r["time_series_type"]), count=Int(r["count"])) for
        r in rows
    ]
end

"""
    num_distinct_arrays(store) -> Int

Number of distinct stored arrays (content hashes); series that share an array
(de-duplicated by content) count once.
"""
function num_distinct_arrays(store::Store)
    out = Ref{Int64}(0)
    code = ccall(
        (:castore_store_num_distinct_arrays, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ref{Int64}),
        store.handle,
        out,
    )
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
    a = Ref{Int64}(0);
    b = Ref{Int64}(0);
    c = Ref{Int64}(0);
    d = Ref{Int64}(0)
    code = ccall(
        (:castore_store_counts_detailed, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ref{Int64}, Ref{Int64}, Ref{Int64}, Ref{Int64}),
        store.handle,
        a,
        b,
        c,
        d,
    )
    _check(code)
    return (
        components_with_time_series=a[],
        supplemental_attributes_with_time_series=b[],
        static_time_series_count=c[],
        forecast_count=d[],
    )
end

"""
    list_owner_ids(store, owner_category; time_series_type=nothing, resolution=nothing) -> Vector{Int}

Distinct owner ids of `owner_category` (an `OwnerCategory`) that have a time
series, optionally restricted by `time_series_type` (a `CASTORE_TYPE_*` integer code)
and/or `resolution` (a `Period`).
"""
function list_owner_ids(
    store::Store,
    owner_category::OwnerCategory;
    time_series_type::Union{Nothing,Integer}=nothing,
    resolution::Union{Nothing,Period}=nothing,
)
    has_type = time_series_type !== nothing
    type_arg = has_type ? Int32(time_series_type) : Int32(0)
    resolution_iso = _period_to_cstr(resolution)
    cat = _category_int(owner_category)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_list_owner_ids, lib_path()),
        Int32,
        (Ptr{Cvoid}, Int32, Bool, Int32, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        cat,
        has_type,
        type_arg,
        resolution_iso,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:castore_store_list_owner_ids, lib_path()),
        Int32,
        (Ptr{Cvoid}, Int32, Bool, Int32, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        cat,
        has_type,
        type_arg,
        resolution_iso,
        buf,
        UInt64(length(buf)),
        out_len,
    )
    _check(code)
    ids = JSON.parse(String(buf[1:Int(out_len[])]))
    return Int[Int(i) for i in ids]
end

function _decode_static_summary_row(r::AbstractDict)
    its = r["initial_timestamp_ms"]
    return (
        owner_type=String(r["owner_type"]),
        owner_category=String(r["owner_category"]),
        time_series_type=_type_for_name(r["time_series_type"]),
        name=String(r["name"]),
        initial_timestamp=its === nothing ? nothing : _from_unix_ms(Int64(its)),
        resolution=_row_period(r["resolution"]),
        time_step_count=_row_int(r["time_step_count"]),
        count=Int(r["count"]),
    )
end

function _decode_forecast_summary_row(r::AbstractDict)
    its = r["initial_timestamp_ms"]
    return (
        owner_type=String(r["owner_type"]),
        owner_category=String(r["owner_category"]),
        time_series_type=_type_for_name(r["time_series_type"]),
        name=String(r["name"]),
        initial_timestamp=its === nothing ? nothing : _from_unix_ms(Int64(its)),
        resolution=_row_period(r["resolution"]),
        horizon=_row_period(r["horizon"]),
        interval=_row_period(r["interval"]),
        window_count=_row_int(r["window_count"]),
        count=Int(r["count"]),
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
    code = ccall(
        (:castore_store_static_summary, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:castore_store_static_summary, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        buf,
        UInt64(length(buf)),
        out_len,
    )
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
    code = ccall(
        (:castore_store_forecast_summary, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:castore_store_forecast_summary, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        buf,
        UInt64(length(buf)),
        out_len,
    )
    _check(code)
    rows = JSON.parse(String(buf[1:Int(out_len[])]))
    return [_decode_forecast_summary_row(r) for r in rows]
end

# ---- Association catalogs --------------------------------------------------
#
# Two catalogs, kept apart on purpose. `SupplementalAttributeAssociation`
# records which attributes are attached to which components;
# `ParentChildAssociation` records directed edges between components (a
# generator connected to a bus, say). Neither has anything to do with time
# series: removing a series never removes an association, and vice versa.
#
# Filters cross the FFI as one JSON object rather than positional arguments,
# because two of the four fields are string lists. Expanding an abstract type
# into its concrete subtypes stays here on the Julia side; the Rust core only
# ever sees concrete type names.

"""
    SupplementalAttributeAssociation(component_id, component_type, attribute_id, attribute_type)

A supplemental attribute attached to a component.

Identity is the `(component_id, attribute_id)` pair: the type names are labels
carried for filtering, so re-attaching the same pair under different type names
is still a duplicate.
"""
struct SupplementalAttributeAssociation
    component_id::Int64
    component_type::String
    attribute_id::Int64
    attribute_type::String
end

"""
    ParentChildAssociation(parent_id, parent_type, child_id, child_type)

A directed edge between two components — e.g. a generator (parent) connected to
a bus (child).

Identity is the *ordered* `(parent_id, child_id)` pair, so the reversed pair is
a different edge. Both endpoints are always components.
"""
struct ParentChildAssociation
    parent_id::Int64
    parent_type::String
    child_id::Int64
    child_type::String
end

function Base.:(==)(
    a::SupplementalAttributeAssociation, b::SupplementalAttributeAssociation
)
    return a.component_id == b.component_id &&
           a.component_type == b.component_type &&
           a.attribute_id == b.attribute_id &&
           a.attribute_type == b.attribute_type
end

function Base.hash(a::SupplementalAttributeAssociation, h::UInt)
    h = hash(a.component_id, h)
    h = hash(a.component_type, h)
    h = hash(a.attribute_id, h)
    return hash(a.attribute_type, h)
end

function Base.show(io::IO, a::SupplementalAttributeAssociation)
    return print(
        io,
        "SupplementalAttributeAssociation(",
        a.component_type,
        " ",
        a.component_id,
        " <- ",
        a.attribute_type,
        " ",
        a.attribute_id,
        ")",
    )
end

function Base.:(==)(a::ParentChildAssociation, b::ParentChildAssociation)
    return a.parent_id == b.parent_id &&
           a.parent_type == b.parent_type &&
           a.child_id == b.child_id &&
           a.child_type == b.child_type
end

function Base.hash(a::ParentChildAssociation, h::UInt)
    h = hash(a.parent_id, h)
    h = hash(a.parent_type, h)
    h = hash(a.child_id, h)
    return hash(a.child_type, h)
end

function Base.show(io::IO, a::ParentChildAssociation)
    return print(
        io,
        "ParentChildAssociation(",
        a.parent_type,
        " ",
        a.parent_id,
        " -> ",
        a.child_type,
        " ",
        a.child_id,
        ")",
    )
end

# Build a filter payload for the FFI. Returns `C_NULL` when nothing is set, so
# the common "everything" query skips JSON entirely. An empty `Vector{String}`
# is a deliberate "none of these types" and is forwarded as such.
function _assoc_filter_json(pairs...)
    filter = Dict{String,Any}()
    for (key, value) in pairs
        value === nothing && continue
        filter[key] = value isa Integer ? Int64(value) : String[String(v) for v in value]
    end
    return isempty(filter) ? C_NULL : JSON.json(filter)
end

function _supplemental_filter_json(
    component_id, component_types, attribute_id, attribute_types
)
    return _assoc_filter_json(
        "component_id" => component_id,
        "component_types" => component_types,
        "attribute_id" => attribute_id,
        "attribute_types" => attribute_types,
    )
end

function _parent_child_filter_json(parent_id, parent_types, child_id, child_types)
    return _assoc_filter_json(
        "parent_id" => parent_id,
        "parent_types" => parent_types,
        "child_id" => child_id,
        "child_types" => child_types,
    )
end

function _supplemental_json(a::SupplementalAttributeAssociation)
    return Dict(
        "component_id" => a.component_id,
        "component_type" => a.component_type,
        "attribute_id" => a.attribute_id,
        "attribute_type" => a.attribute_type,
    )
end

function _parent_child_json(a::ParentChildAssociation)
    return Dict(
        "parent_id" => a.parent_id,
        "parent_type" => a.parent_type,
        "child_id" => a.child_id,
        "child_type" => a.child_type,
    )
end

function _decode_supplemental(r::AbstractDict)
    return SupplementalAttributeAssociation(
        Int64(r["component_id"]),
        String(r["component_type"]),
        Int64(r["attribute_id"]),
        String(r["attribute_type"]),
    )
end

function _decode_parent_child(r::AbstractDict)
    return ParentChildAssociation(
        Int64(r["parent_id"]),
        String(r["parent_type"]),
        Int64(r["child_id"]),
        String(r["child_type"]),
    )
end

# Shared result handling for the two families. Julia requires a `ccall` symbol
# to be a literal, so each call site names its own export and passes a closure
# that performs the call with the out pointer supplied here — the same shape as
# `_filter_probe` above.

# For exports returning a row/entity count through a `u64` out pointer.
function _assoc_count_out(ccall_once)
    out = Ref{UInt64}(0)
    _check(ccall_once(out))
    return Int(out[])
end

# For exports returning a `bool` out pointer.
function _assoc_bool_out(ccall_once)
    out = Ref{Bool}(false)
    _check(ccall_once(out))
    return out[]
end

# For exports returning an `i64` count out pointer.
function _assoc_i64_out(ccall_once)
    out = Ref{Int64}(0)
    _check(ccall_once(out))
    return Int(out[])
end

"""
    add_supplemental_attribute_association!(store, association)

Attach a supplemental attribute to a component. Throws
`DuplicateAssociationError` if that component already carries that attribute,
whatever type names are supplied.
"""
function add_supplemental_attribute_association!(
    store::Store, association::SupplementalAttributeAssociation
)
    _check(
        ccall(
            (:castore_store_add_supplemental_attribute_association, lib_path()),
            Int32,
            (Ptr{Cvoid}, Int64, Cstring, Int64, Cstring),
            store.handle,
            association.component_id,
            association.component_type,
            association.attribute_id,
            association.attribute_type,
        ),
    )
    return nothing
end

"""
    add_supplemental_attribute_associations!(store, associations) -> Int

Attach many in one all-or-nothing transaction, returning the number inserted. A
duplicate anywhere in the batch rolls the whole batch back. This is the import
half of the round trip whose export is
[`list_supplemental_attribute_associations`](@ref) with no filter.
"""
function add_supplemental_attribute_associations!(
    store::Store, associations::AbstractVector{SupplementalAttributeAssociation}
)
    payload = JSON.json([_supplemental_json(a) for a in associations])
    return _assoc_count_out(
        out -> ccall(
            (:castore_store_add_supplemental_attribute_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{UInt64}),
            store.handle,
            payload,
            out,
        ),
    )
end

"""
    has_supplemental_attribute_association(store; filters...) -> Bool

Whether any attachment matches the filter. Filter keywords, all optional and
ANDed: `component_id`, `component_types` (a `Vector{String}` of concrete type
names), `attribute_id`, `attribute_types`. With no filter, this is "does the
store hold any attachment at all".
"""
function has_supplemental_attribute_association(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _supplemental_filter_json(
        component_id, component_types, attribute_id, attribute_types
    )
    return _assoc_bool_out(
        out -> ccall(
            (:castore_store_has_supplemental_attribute_association, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{Bool}),
            store.handle,
            filter_json,
            out,
        ),
    )
end

"""
    list_supplemental_attribute_associations(store; filters...) -> Vector{SupplementalAttributeAssociation}

Full attachment rows matching the filter (same keywords as
[`has_supplemental_attribute_association`](@ref)), in insertion order. With no
filter this exports the whole table, which is what a JSON serialization round
trip needs.
"""
function list_supplemental_attribute_associations(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _supplemental_filter_json(
        component_id, component_types, attribute_id, attribute_types
    )
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:castore_store_list_supplemental_attribute_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            filter_json,
            buf,
            cap,
            out_len,
        ),
    )
    return SupplementalAttributeAssociation[
        _decode_supplemental(r) for r in JSON.parse(json)
    ]
end

"""
    list_supplemental_attribute_ids(store; filters...) -> Vector{Int}

Distinct attribute ids matching the filter, ascending — the attributes attached
to a component when `component_id` is set.
"""
function list_supplemental_attribute_ids(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _supplemental_filter_json(
        component_id, component_types, attribute_id, attribute_types
    )
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:castore_store_list_supplemental_attribute_ids, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            filter_json,
            buf,
            cap,
            out_len,
        ),
    )
    return Int[Int(i) for i in JSON.parse(json)]
end

"""
    list_components_with_attributes(store; filters...) -> Vector{Int}

Distinct component ids matching the filter, ascending — the components carrying
an attribute when `attribute_id` is set.
"""
function list_components_with_attributes(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _supplemental_filter_json(
        component_id, component_types, attribute_id, attribute_types
    )
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:castore_store_list_components_with_attributes, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            filter_json,
            buf,
            cap,
            out_len,
        ),
    )
    return Int[Int(i) for i in JSON.parse(json)]
end

"""
    remove_supplemental_attribute_associations!(store; filters...) -> Int

Remove every attachment matching the filter, returning the number removed.
Removing nothing is not an error: callers that expect a specific count assert on
the return value.
"""
function remove_supplemental_attribute_associations!(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _supplemental_filter_json(
        component_id, component_types, attribute_id, attribute_types
    )
    return _assoc_count_out(
        out -> ccall(
            (:castore_store_remove_supplemental_attribute_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{UInt64}),
            store.handle,
            filter_json,
            out,
        ),
    )
end

"""
    replace_supplemental_attribute_component_id!(store, old_id, new_id) -> Int

Move every attachment from component `old_id` to `new_id`, returning the rows
updated. Throws `DuplicateAssociationError` if `new_id` already carries one of
the attributes being moved.
"""
function replace_supplemental_attribute_component_id!(
    store::Store, old_id::Integer, new_id::Integer
)
    return _assoc_count_out(
        out -> ccall(
            (:castore_store_replace_supplemental_attribute_component_id, lib_path()),
            Int32,
            (Ptr{Cvoid}, Int64, Int64, Ref{UInt64}),
            store.handle,
            Int64(old_id),
            Int64(new_id),
            out,
        ),
    )
end

# `kind`: 0 = matching rows, 1 = distinct attributes, 2 = distinct components.
function _supplemental_count(store::Store, filter_json, kind::Integer)
    out = Ref{Int64}(0)
    _check(
        ccall(
            (:castore_store_count_supplemental_attribute_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Int32, Ref{Int64}),
            store.handle,
            filter_json,
            Int32(kind),
            out,
        ),
    )
    return Int(out[])
end

"""
    count_supplemental_attribute_associations(store; filters...) -> Int

Number of attachments matching the filter.
"""
function count_supplemental_attribute_associations(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    return _supplemental_count(
        store,
        _supplemental_filter_json(
            component_id, component_types, attribute_id, attribute_types
        ),
        0,
    )
end

"""
    count_supplemental_attributes(store; filters...) -> Int

Number of *distinct* attributes among the attachments matching the filter.
"""
function count_supplemental_attributes(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    return _supplemental_count(
        store,
        _supplemental_filter_json(
            component_id, component_types, attribute_id, attribute_types
        ),
        1,
    )
end

"""
    count_components_with_attributes(store; filters...) -> Int

Number of *distinct* components among the attachments matching the filter.
"""
function count_components_with_attributes(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    return _supplemental_count(
        store,
        _supplemental_filter_json(
            component_id, component_types, attribute_id, attribute_types
        ),
        2,
    )
end

"""
    supplemental_attribute_counts_by_type(store) -> Vector{NamedTuple}

Attachment counts grouped by attribute type, ordered by type. Each row is
`(type, count)`.
"""
function supplemental_attribute_counts_by_type(store::Store)
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:castore_store_supplemental_attribute_counts_by_type, lib_path()),
            Int32,
            (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            buf,
            cap,
            out_len,
        ),
    )
    return [(type=String(r["type"]), count=Int(r["count"])) for r in JSON.parse(json)]
end

"""
    supplemental_attribute_summary(store) -> Vector{NamedTuple}

Attachment counts grouped by both type names, ordered by attribute type then
component type. Each row is `(component_type, attribute_type, count)`. The core
does the GROUP BY; callers build any presentation table.
"""
function supplemental_attribute_summary(store::Store)
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:castore_store_supplemental_attribute_summary, lib_path()),
            Int32,
            (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            buf,
            cap,
            out_len,
        ),
    )
    return [
        (
            component_type=String(r["component_type"]),
            attribute_type=String(r["attribute_type"]),
            count=Int(r["count"]),
        ) for r in JSON.parse(json)
    ]
end

"""
    add_parent_child_association!(store, association)

Record a directed edge between two components. Throws
`DuplicateAssociationError` if that ordered pair is already related; the
reversed pair is a different edge.
"""
function add_parent_child_association!(store::Store, association::ParentChildAssociation)
    _check(
        ccall(
            (:castore_store_add_parent_child_association, lib_path()),
            Int32,
            (Ptr{Cvoid}, Int64, Cstring, Int64, Cstring),
            store.handle,
            association.parent_id,
            association.parent_type,
            association.child_id,
            association.child_type,
        ),
    )
    return nothing
end

"""
    add_parent_child_associations!(store, associations) -> Int

Record many edges in one all-or-nothing transaction, returning the number
inserted.
"""
function add_parent_child_associations!(
    store::Store, associations::AbstractVector{ParentChildAssociation}
)
    payload = JSON.json([_parent_child_json(a) for a in associations])
    return _assoc_count_out(
        out -> ccall(
            (:castore_store_add_parent_child_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{UInt64}),
            store.handle,
            payload,
            out,
        ),
    )
end

"""
    has_parent_child_association(store; filters...) -> Bool

Whether any edge matches the filter. Filter keywords, all optional and ANDed:
`parent_id`, `parent_types`, `child_id`, `child_types`.
"""
function has_parent_child_association(
    store::Store;
    parent_id::Union{Nothing,Integer}=nothing,
    parent_types::Union{Nothing,AbstractVector}=nothing,
    child_id::Union{Nothing,Integer}=nothing,
    child_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _parent_child_filter_json(parent_id, parent_types, child_id, child_types)
    return _assoc_bool_out(
        out -> ccall(
            (:castore_store_has_parent_child_association, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{Bool}),
            store.handle,
            filter_json,
            out,
        ),
    )
end

"""
    list_parent_child_associations(store; filters...) -> Vector{ParentChildAssociation}

Full edge rows matching the filter (same keywords as
[`has_parent_child_association`](@ref)), in insertion order.
"""
function list_parent_child_associations(
    store::Store;
    parent_id::Union{Nothing,Integer}=nothing,
    parent_types::Union{Nothing,AbstractVector}=nothing,
    child_id::Union{Nothing,Integer}=nothing,
    child_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _parent_child_filter_json(parent_id, parent_types, child_id, child_types)
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:castore_store_list_parent_child_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            filter_json,
            buf,
            cap,
            out_len,
        ),
    )
    return ParentChildAssociation[_decode_parent_child(r) for r in JSON.parse(json)]
end

# `endpoint`: 0 = parents, 1 = children.
function _parent_child_ids(store::Store, filter_json, endpoint::Integer)
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:castore_store_list_parent_child_ids, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Int32, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            filter_json,
            Int32(endpoint),
            buf,
            cap,
            out_len,
        ),
    )
    return Int[Int(i) for i in JSON.parse(json)]
end

"""
    list_children(store; filters...) -> Vector{Int}

Distinct child ids matching the filter, ascending — the children of a component
when `parent_id` is set.
"""
function list_children(
    store::Store;
    parent_id::Union{Nothing,Integer}=nothing,
    parent_types::Union{Nothing,AbstractVector}=nothing,
    child_id::Union{Nothing,Integer}=nothing,
    child_types::Union{Nothing,AbstractVector}=nothing,
)
    return _parent_child_ids(
        store, _parent_child_filter_json(parent_id, parent_types, child_id, child_types), 1
    )
end

"""
    list_parents(store; filters...) -> Vector{Int}

Distinct parent ids matching the filter, ascending — the parents of a component
when `child_id` is set.
"""
function list_parents(
    store::Store;
    parent_id::Union{Nothing,Integer}=nothing,
    parent_types::Union{Nothing,AbstractVector}=nothing,
    child_id::Union{Nothing,Integer}=nothing,
    child_types::Union{Nothing,AbstractVector}=nothing,
)
    return _parent_child_ids(
        store, _parent_child_filter_json(parent_id, parent_types, child_id, child_types), 0
    )
end

"""
    remove_parent_child_associations!(store; filters...) -> Int

Remove every edge matching the filter, returning the number removed. Removing
nothing is not an error.
"""
function remove_parent_child_associations!(
    store::Store;
    parent_id::Union{Nothing,Integer}=nothing,
    parent_types::Union{Nothing,AbstractVector}=nothing,
    child_id::Union{Nothing,Integer}=nothing,
    child_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _parent_child_filter_json(parent_id, parent_types, child_id, child_types)
    return _assoc_count_out(
        out -> ccall(
            (:castore_store_remove_parent_child_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{UInt64}),
            store.handle,
            filter_json,
            out,
        ),
    )
end

"""
    replace_parent_child_component_id!(store, old_id, new_id) -> Int

Rewrite component `old_id` to `new_id` on both ends of every edge, returning the
rows updated. Throws `DuplicateAssociationError` if the rewrite would duplicate
an edge `new_id` already has.
"""
function replace_parent_child_component_id!(store::Store, old_id::Integer, new_id::Integer)
    return _assoc_count_out(
        out -> ccall(
            (:castore_store_replace_parent_child_component_id, lib_path()),
            Int32,
            (Ptr{Cvoid}, Int64, Int64, Ref{UInt64}),
            store.handle,
            Int64(old_id),
            Int64(new_id),
            out,
        ),
    )
end

"""
    count_parent_child_associations(store; filters...) -> Int

Number of edges matching the filter.
"""
function count_parent_child_associations(
    store::Store;
    parent_id::Union{Nothing,Integer}=nothing,
    parent_types::Union{Nothing,AbstractVector}=nothing,
    child_id::Union{Nothing,Integer}=nothing,
    child_types::Union{Nothing,AbstractVector}=nothing,
)
    out = Ref{Int64}(0)
    _check(
        ccall(
            (:castore_store_count_parent_child_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{Int64}),
            store.handle,
            _parent_child_filter_json(parent_id, parent_types, child_id, child_types),
            out,
        ),
    )
    return Int(out[])
end

"""
    get_forecast_parameters(store; resolution=nothing, interval=nothing)

Return the store's forecast parameters as a NamedTuple
`(horizon, interval, count, resolution, initial_timestamp)`, optionally restricted
to forecasts with the given `resolution` and/or `interval` (`Period`s).
`horizon`, `interval`, and `resolution` are `Period`s (`Millisecond` for fixed
durations) or `nothing`;
`count` is an `Int` (or `nothing`); `initial_timestamp` is a `DateTime` (or
`nothing`). Every field is `nothing` when no forecast matches.
"""
function get_forecast_parameters(
    store::Store;
    resolution::Union{Nothing,Period}=nothing,
    interval::Union{Nothing,Period}=nothing,
)
    fres = _period_to_cstr(resolution)
    fivl = _period_to_cstr(interval)
    present = Ref{Bool}(false)
    horizon_out = Ref{Ptr{Cchar}}(C_NULL);
    interval_out = Ref{Ptr{Cchar}}(C_NULL)
    count = Ref{Int64}(-1);
    resolution_out = Ref{Ptr{Cchar}}(C_NULL);
    initial_out = Ref{Int64}(-1)
    code = ccall(
        (:castore_store_get_forecast_parameters, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Cstring,
            Cstring,
            Ref{Bool},
            Ref{Ptr{Cchar}},
            Ref{Ptr{Cchar}},
            Ref{Int64},
            Ref{Ptr{Cchar}},
            Ref{Int64},
        ),
        store.handle,
        fres,
        fivl,
        present,
        horizon_out,
        interval_out,
        count,
        resolution_out,
        initial_out,
    )
    _check(code)
    return (
        horizon=_take_period(horizon_out[]),
        interval=_take_period(interval_out[]),
        count=(count[] < 0 ? nothing : Int(count[])),
        resolution=_take_period(resolution_out[]),
        initial_timestamp=(initial_out[] < 0 ? nothing : _from_unix_ms(initial_out[])),
    )
end

"""
    check_static_consistency(store; resolution=nothing) -> Vector{NamedTuple}

Verify that, per resolution, every `SingleTimeSeries` shares one
`(initial_timestamp, length)` grid, and return one
`(resolution, initial_timestamp, length)` NamedTuple per resolution present
(empty vector when there are none), ordered by resolution. Series at different
resolutions legitimately have different grids, so consistency is only required
within a resolution; pass `resolution` (a `Period`) to scope the check to one
grid. Throws `IntegrityError` when the `SingleTimeSeries` at a single
resolution disagree on their `(initial_timestamp, length)`. One catalog query.
"""
function check_static_consistency(store::Store; resolution::Union{Nothing,Period}=nothing)
    fres = _period_to_cstr(resolution)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_check_static_consistency, lib_path()),
        Int32,
        (Ptr{Cvoid}, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        fres,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:castore_store_check_static_consistency, lib_path()),
        Int32,
        (Ptr{Cvoid}, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        fres,
        buf,
        UInt64(length(buf)),
        out_len,
    )
    _check(code)
    rows = JSON.parse(String(buf[1:Int(out_len[])]))
    return [
        (
            resolution=_iso_to_period(String(r["resolution"])),
            initial_timestamp=_from_unix_ms(Int64(r["initial_timestamp_ms"])),
            length=Int(r["length"]),
        ) for r in rows
    ]
end

"""
    get_resolutions(store; time_series_type=nothing) -> Vector{Period}

Return the distinct resolutions stored, in the core's stored (lexical-by-ISO)
order. When `time_series_type` (a
`CASTORE_TYPE_*` integer code) is given the result is restricted to that type. This is
a single catalog query in the core rather than a scan of every association.
"""
function get_resolutions(store::Store; time_series_type::Union{Nothing,Integer}=nothing)
    has_type = time_series_type !== nothing
    type_arg = has_type ? Int32(time_series_type) : Int32(0)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_get_resolutions, lib_path()),
        Int32,
        (Ptr{Cvoid}, Bool, Int32, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        has_type,
        type_arg,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:castore_store_get_resolutions, lib_path()),
        Int32,
        (Ptr{Cvoid}, Bool, Int32, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        has_type,
        type_arg,
        buf,
        UInt64(length(buf)),
        out_len,
    )
    _check(code)
    isos = JSON.parse(String(buf[1:Int(out_len[])]))
    return Period[_iso_to_period(String(s)) for s in isos]
end

"""
    get_intervals(store; time_series_type=nothing) -> Vector{Period}

Return the distinct forecast intervals stored (lexical-by-ISO order), the
interval analog of [`get_resolutions`](@ref). When `time_series_type` (a
`CASTORE_TYPE_*` code) is given the result is restricted to that type; non-forecast
types return an empty vector.
"""
function get_intervals(store::Store; time_series_type::Union{Nothing,Integer}=nothing)
    has_type = time_series_type !== nothing
    type_arg = has_type ? Int32(time_series_type) : Int32(0)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_get_intervals, lib_path()),
        Int32,
        (Ptr{Cvoid}, Bool, Int32, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        has_type,
        type_arg,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:castore_store_get_intervals, lib_path()),
        Int32,
        (Ptr{Cvoid}, Bool, Int32, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        has_type,
        type_arg,
        buf,
        UInt64(length(buf)),
        out_len,
    )
    _check(code)
    isos = JSON.parse(String(buf[1:Int(out_len[])]))
    return Period[_iso_to_period(String(s)) for s in isos]
end

"""
    read_only(store) -> Bool

Whether the store was opened read-only.
"""
function read_only(store::Store)
    out = Ref{Bool}(false)
    code = ccall(
        (:castore_store_read_only, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ref{Bool}),
        store.handle,
        out,
    )
    _check(code)
    return out[]
end

"""
    get_compression(store)

Return the store's compression policy as a NamedTuple `(compression, level,
shuffle)`. `compression` is `:deflate` or `:none`; `level` (0-9) and `shuffle`
apply to DEFLATE. For a store opened from disk this reflects the policy it was
created with; in-memory stores report `:none`.
"""
function get_compression(store::Store)
    kind = Ref{UInt8}(0);
    level = Ref{UInt8}(0);
    shuffle = Ref{Bool}(false)
    code = ccall(
        (:castore_store_get_compression, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ref{UInt8}, Ref{UInt8}, Ref{Bool}),
        store.handle,
        kind,
        level,
        shuffle,
    )
    _check(code)
    return if kind[] == 0
        (compression=:none, level=Int(level[]), shuffle=shuffle[])
    else
        (compression=:deflate, level=Int(level[]), shuffle=shuffle[])
    end
end

"""
    get_path(store) -> Union{Nothing,String}

Return the filesystem path backing the store's NetCDF file, or `nothing` for an
in-memory store.
"""
function get_path(store::Store)
    has_path = Ref{Bool}(false)
    out_len = Ref{UInt64}(0)
    # Probe: a null buffer reports the required length without copying.
    code = ccall(
        (:castore_store_get_path, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ref{Bool}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        has_path,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    has_path[] || return nothing
    # +1 leaves room for the trailing NUL `write_str_out` appends.
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:castore_store_get_path, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ref{Bool}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        has_path,
        buf,
        UInt64(length(buf)),
        out_len,
    )
    _check(code)
    return _take_buffer_string(buf, out_len[])
end

"""
    verify_integrity(store) -> Int

Recompute each stored array's content hash and return how many disagree with the
hash recorded alongside them. `0` means every array checked out.

Checks the NetCDF half of the store only — the SQLite catalog is not inspected,
so `0` does not mean the store as a whole is sound. A catalog that is corrupted,
truncated, or paired with the wrong `.nc` file still returns `0`, while every read
of the affected series throws. For catalog-side checks use
[`check_static_consistency`] (per-resolution grid agreement) and [`compact!`]
(which reports the unreachable arrays and feature sets a delete left behind — an
expected state, not corruption).
"""
function verify_integrity(store::Store)
    out = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_verify, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ref{UInt64}),
        store.handle,
        out,
    )
    _check(code)
    return Int(out[])
end

function compact!(store::Store)
    code = ccall((:castore_store_compact, lib_path()), Int32, (Ptr{Cvoid},), store.handle)
    _check(code)
    return nothing
end

"""
    flush!(store)

Flush pending writes (NetCDF arrays + SQLite metadata) to disk. After this the
on-disk `<path>.nc` and `<path>.sqlite` artifacts can be copied for persistence.
"""
function flush!(store::Store)
    code = ccall((:castore_store_flush, lib_path()), Int32, (Ptr{Cvoid},), store.handle)
    _check(code)
    return nothing
end

"""
    persist!(store, path)

Persist the store to `path` (NetCDF) and `\$path.sqlite` (metadata), materializing
an in-memory store to disk. Existing target files are overwritten.
"""
function persist!(store::Store, path::AbstractString)
    code = ccall(
        (:castore_store_persist, lib_path()),
        Int32,
        (Ptr{Cvoid}, Cstring),
        store.handle,
        path,
    )
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
function clear!(
    store::Store;
    owner_id::Union{Nothing,Integer}=nothing,
    owner_category::Union{Nothing,OwnerCategory}=nothing,
)
    has_owner = owner_id !== nothing
    if has_owner && owner_category === nothing
        throw(ArgumentError("clear! with owner_id also requires owner_category"))
    end
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    category_arg = has_owner ? _category_int(owner_category) : Int32(0)
    code = ccall(
        (:castore_store_clear, lib_path()),
        Int32,
        (Ptr{Cvoid}, Bool, Int64, Int32),
        store.handle,
        has_owner,
        owner_arg,
        category_arg,
    )
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
function replace_owner!(
    store::Store,
    old_owner_id::Integer,
    new_owner_id::Integer,
    owner_category::OwnerCategory,
)
    out = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_replace_owner, lib_path()),
        Int32,
        (Ptr{Cvoid}, Int64, Int64, Int32, Ref{UInt64}),
        store.handle,
        Int64(old_owner_id),
        Int64(new_owner_id),
        _category_int(owner_category),
        out,
    )
    _check(code)
    return Int(out[])
end

# ---- Forecasts -------------------------------------------------------------
#
# TimeSeriesType integer codes (must match the Rust `TimeSeriesType` enum):
const CASTORE_TYPE_SINGLE = 0
const CASTORE_TYPE_NON_SEQUENTIAL = 1
const CASTORE_TYPE_DETERMINISTIC = 2
const CASTORE_TYPE_DETERMINISTIC_SINGLE = 3
const CASTORE_TYPE_PROBABILISTIC = 4
const CASTORE_TYPE_SCENARIOS = 5
# Request-only family sentinel (never a stored type): matches a stored
# `Deterministic` or `DeterministicSingleTimeSeries`. The Rust core resolves it
# and reports the concrete type that matched. Must match `CASTORE_TYPE_ABSTRACT_DETERMINISTIC`
# in the C ABI.
const CASTORE_TYPE_ABSTRACT_DETERMINISTIC = 100

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
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    return _add_dense_forecast!(
        store,
        owner_id,
        owner_type,
        owner_category,
        ts.name,
        CASTORE_TYPE_DETERMINISTIC,
        ts.initial_timestamp,
        ts.resolution,
        ts.horizon,
        ts.interval,
        ts.count,
        ts.data;
        features=features,
        units=units,
        ext=ext,
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
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    return _add_dense_forecast!(
        store,
        owner_id,
        owner_type,
        owner_category,
        ts.name,
        CASTORE_TYPE_SCENARIOS,
        ts.initial_timestamp,
        ts.resolution,
        ts.horizon,
        ts.interval,
        ts.count,
        ts.data;
        features=features,
        units=units,
        ext=ext,
    )
end

# Shared implementation: ccall the per-type C transport `castore_store_add_forecast`
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
    ext::Union{Nothing,AbstractString}=nothing,
)
    features_json = _features_arg(features)
    units_ptr = units === nothing ? C_NULL : String(units)
    ext_ptr = ext === nothing ? C_NULL : String(ext)
    dtype = _dtype_code(eltype(data))
    dims = UInt64[size(data)...]
    bytes = _row_major_bytes(data)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:castore_store_add_forecast, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Cstring,
            Int32,
            Cstring,
            Int32,
            Int64,
            Cstring,
            Cstring,
            Cstring,
            UInt64,
            Int32,
            UInt64,
            Ptr{UInt64},
            Ptr{UInt8},
            UInt64,
            Cstring,
            Cstring,
            Cstring,
            Ref{Ptr{Cvoid}},
        ),
        store.handle,
        Int64(owner_id),
        owner_type,
        _category_int(owner_category),
        name,
        Int32(ts_type),
        _to_unix_ms(initial_timestamp),
        _period_to_iso(resolution),
        _period_to_iso(horizon),
        _period_to_iso(interval),
        UInt64(count),
        dtype,
        UInt64(length(dims)),
        dims,
        bytes,
        UInt64(length(bytes)),
        ext_ptr,
        features_json,
        units_ptr,
        out_key,
    )
    _check(code)
    return TimeSeriesKey(out_key[])
end

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
    owner_category::Union{Nothing,OwnerCategory}=nothing,
    resolution::Union{Nothing,Period}=nothing,
)
    cat = owner_category === nothing ? Int32(-1) : Int32(Int(owner_category))
    res_iso = _period_to_cstr(resolution)
    out_count = Ref{UInt64}(0)
    code = ccall(
        (:castore_store_transform_single_time_series, lib_path()),
        Int32,
        (Ptr{Cvoid}, Cstring, Cstring, Int32, Cstring, Ref{UInt64}),
        store.handle,
        _period_to_iso(horizon),
        _period_to_iso(interval),
        cat,
        res_iso,
        out_count,
    )
    _check(code)
    return Int(out_count[])
end

"""True iff a time series of `ts_type` with the given attributes exists.
`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`)."""
function has_typed(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts_type::Integer;
    resolution::Union{Nothing,Period}=nothing,
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_iso = _period_to_cstr(resolution)
    interval_iso = _period_to_cstr(interval)
    features_json = _features_arg(features)
    out = Ref{Bool}(false)
    code = ccall(
        (:castore_store_has_typed, lib_path()),
        Int32,
        (Ptr{Cvoid}, Int64, Int32, Cstring, Int32, Cstring, Cstring, Cstring, Ref{Bool}),
        store.handle,
        Int64(owner_id),
        _category_int(owner_category),
        name,
        Int32(ts_type),
        resolution_iso,
        interval_iso,
        features_json,
        out,
    )
    _check(code)
    return out[]
end

"""Remove a time series of `ts_type` by attributes. `owner_category` is the
owner's `OwnerCategory` (`Component` or `SupplementalAttribute`)."""
function remove_typed!(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts_type::Integer;
    resolution::Union{Nothing,Period}=nothing,
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_iso = _period_to_cstr(resolution)
    interval_iso = _period_to_cstr(interval)
    features_json = _features_arg(features)
    code = ccall(
        (:castore_store_remove_typed, lib_path()),
        Int32,
        (Ptr{Cvoid}, Int64, Int32, Cstring, Int32, Cstring, Cstring, Cstring),
        store.handle,
        Int64(owner_id),
        _category_int(owner_category),
        name,
        Int32(ts_type),
        resolution_iso,
        interval_iso,
        features_json,
    )
    _check(code)
    return nothing
end

"""
    copy_time_series!(store, owner_id, owner_category, name, ts_type,
                      dst_owner_id, dst_owner_type; new_name=nothing,
                      resolution=nothing, features=Dict())

Copy the time series identified by the source attributes onto `dst_owner_id`,
optionally renaming it to `new_name`.

Arrays are content-addressed, so this writes only a new association row against
the same underlying array: no data is duplicated, and the stored time series type
is preserved. In particular a `DeterministicSingleTimeSeries` stays one, whereas a
read-then-write copy through `get_time_series` / `add_time_series!` would
materialize it into a dense `Deterministic`.

The copy keeps the source's `owner_category`. Throws if the destination already
holds a matching series.
"""
function copy_time_series!(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts_type::Integer,
    dst_owner_id::Integer,
    dst_owner_type::AbstractString;
    new_name::Union{Nothing,AbstractString}=nothing,
    resolution::Union{Nothing,Period}=nothing,
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    resolution_iso = _period_to_cstr(resolution)
    interval_iso = _period_to_cstr(interval)
    features_json = _features_arg(features)
    renamed = new_name === nothing ? C_NULL : new_name
    code = ccall(
        (:castore_store_copy_time_series, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Int32,
            Cstring,
            Int32,
            Cstring,
            Cstring,
            Cstring,
            Int64,
            Cstring,
            Cstring,
        ),
        store.handle,
        Int64(owner_id),
        _category_int(owner_category),
        name,
        Int32(ts_type),
        resolution_iso,
        interval_iso,
        features_json,
        Int64(dst_owner_id),
        dst_owner_type,
        renamed,
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
    ext::Union{Nothing,AbstractString}=ts.ext,
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
    ext_ptr = ext === nothing ? C_NULL : String(ext)
    dtype = _dtype_code(eltype(data))
    dims = UInt64[size(data)...]
    bytes = _row_major_bytes(data)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:castore_store_add_probabilistic, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Cstring,
            Int32,
            Cstring,
            Int64,
            Cstring,
            Cstring,
            Cstring,
            UInt64,
            Ptr{Float64},
            UInt64,
            Int32,
            UInt64,
            Ptr{UInt64},
            Ptr{UInt8},
            UInt64,
            Cstring,
            Cstring,
            Cstring,
            Ref{Ptr{Cvoid}},
        ),
        store.handle,
        Int64(owner_id),
        owner_type,
        _category_int(owner_category),
        name,
        _to_unix_ms(initial_timestamp),
        _period_to_iso(resolution),
        _period_to_iso(horizon),
        _period_to_iso(interval),
        UInt64(count),
        percentiles,
        UInt64(length(percentiles)),
        dtype,
        UInt64(length(dims)),
        dims,
        bytes,
        UInt64(length(bytes)),
        ext_ptr,
        features_json,
        units_ptr,
        out_key,
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
    handle::Ptr{Cvoid}
    count::Int
    function AddBatch()
        handle = ccall((:castore_batch_new, lib_path()), Ptr{Cvoid}, ())
        batch = new(handle, 0)
        finalizer(_finalize_batch, batch)
        return batch
    end
end

function _finalize_batch(b::AddBatch)
    if b.handle != C_NULL
        ccall((:castore_batch_free, lib_path()), Cvoid, (Ptr{Cvoid},), b.handle)
        b.handle = C_NULL
    end
    return nothing
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
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
    code = ccall(
        (:castore_batch_add_single, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Cstring,
            Int32,
            Cstring,
            Int64,
            Cstring,
            Int32,
            UInt64,
            Ptr{UInt64},
            Ptr{UInt8},
            UInt64,
            Cstring,
            Cstring,
            Cstring,
        ),
        batch.handle,
        Int64(owner_id),
        owner_type,
        _category_int(owner_category),
        ts.name,
        _to_unix_ms(ts.initial_timestamp),
        _period_to_iso(ts.resolution),
        dtype,
        UInt64(length(dims)),
        dims,
        bytes,
        UInt64(length(bytes)),
        _opt_string_arg(ext),
        _features_arg(features),
        _opt_string_arg(units),
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
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    timestamps = Int64[_to_unix_ms(timestamp) for timestamp in ts.timestamps]
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
    code = ccall(
        (:castore_batch_add_non_sequential, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Cstring,
            Int32,
            Cstring,
            Ptr{Int64},
            UInt64,
            Int32,
            UInt64,
            Ptr{UInt64},
            Ptr{UInt8},
            UInt64,
            Cstring,
            Cstring,
            Cstring,
        ),
        batch.handle,
        Int64(owner_id),
        owner_type,
        _category_int(owner_category),
        ts.name,
        timestamps,
        UInt64(length(timestamps)),
        dtype,
        UInt64(length(dims)),
        dims,
        bytes,
        UInt64(length(bytes)),
        _opt_string_arg(ext),
        _features_arg(features),
        _opt_string_arg(units),
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
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    return _batch_add_dense_forecast!(
        batch,
        owner_id,
        owner_type,
        owner_category,
        ts.name,
        CASTORE_TYPE_DETERMINISTIC,
        ts.initial_timestamp,
        ts.resolution,
        ts.horizon,
        ts.interval,
        ts.count,
        ts.data;
        features=features,
        units=units,
        ext=ext,
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
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    return _batch_add_dense_forecast!(
        batch,
        owner_id,
        owner_type,
        owner_category,
        ts.name,
        CASTORE_TYPE_SCENARIOS,
        ts.initial_timestamp,
        ts.resolution,
        ts.horizon,
        ts.interval,
        ts.count,
        ts.data;
        features=features,
        units=units,
        ext=ext,
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
    ext::Union{Nothing,AbstractString}=nothing,
)
    dtype = _dtype_code(eltype(data))
    dims = UInt64[size(data)...]
    bytes = _row_major_bytes(data)
    code = ccall(
        (:castore_batch_add_forecast, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Cstring,
            Int32,
            Cstring,
            Int32,
            Int64,
            Cstring,
            Cstring,
            Cstring,
            UInt64,
            Int32,
            UInt64,
            Ptr{UInt64},
            Ptr{UInt8},
            UInt64,
            Cstring,
            Cstring,
            Cstring,
        ),
        batch.handle,
        Int64(owner_id),
        owner_type,
        _category_int(owner_category),
        name,
        Int32(ts_type),
        _to_unix_ms(initial_timestamp),
        _period_to_iso(resolution),
        _period_to_iso(horizon),
        _period_to_iso(interval),
        UInt64(count),
        dtype,
        UInt64(length(dims)),
        dims,
        bytes,
        UInt64(length(bytes)),
        _opt_string_arg(ext),
        _features_arg(features),
        _opt_string_arg(units),
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
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
    code = ccall(
        (:castore_batch_add_probabilistic, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Cstring,
            Int32,
            Cstring,
            Int64,
            Cstring,
            Cstring,
            Cstring,
            UInt64,
            Ptr{Float64},
            UInt64,
            Int32,
            UInt64,
            Ptr{UInt64},
            Ptr{UInt8},
            UInt64,
            Cstring,
            Cstring,
            Cstring,
        ),
        batch.handle,
        Int64(owner_id),
        owner_type,
        _category_int(owner_category),
        ts.name,
        _to_unix_ms(ts.initial_timestamp),
        _period_to_iso(ts.resolution),
        _period_to_iso(ts.horizon),
        _period_to_iso(ts.interval),
        UInt64(ts.count),
        ts.percentiles,
        UInt64(length(ts.percentiles)),
        dtype,
        UInt64(length(dims)),
        dims,
        bytes,
        UInt64(length(bytes)),
        _opt_string_arg(ext),
        _features_arg(features),
        _opt_string_arg(units),
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
        (:castore_store_add_batch, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{Cvoid}, Ref{Ptr{Ptr{Cvoid}}}, Ref{UInt64}),
        store.handle,
        batch.handle,
        out_keys,
        out_len,
    )
    batch.count = 0
    _check(code)
    n = Int(out_len[])
    keys = Vector{TimeSeriesKey}(undef, n)
    if n > 0
        # Copy each owned handle into a finalized wrapper, then free the array
        # buffer (the wrappers own the handles and free them via castore_key_free).
        raw = unsafe_wrap(Array, out_keys[], n; own=false)
        for i in 1:n
            keys[i] = TimeSeriesKey(raw[i])
        end
        ccall(
            (:castore_keys_buffer_free, lib_path()),
            Cvoid,
            (Ptr{Ptr{Cvoid}}, UInt64),
            out_keys[],
            out_len[],
        )
    end
    return keys
end

# ---- Forecast data reads ---------------------------------------------------
#
# All three functions call `castore_store_get_forecast` and return named tuples
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
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
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

    code = ccall(
        (:castore_store_get_forecast, lib_path()),
        Int32,
        (
            Ptr{Cvoid},   # handle
            Int64,        # owner_id
            Int32,        # owner_category
            Cstring,      # name
            Int32,        # ts_type
            Cstring,      # resolution (ISO-8601)
            Cstring,      # interval (ISO-8601)
            Cstring,      # features_json
            Bool,         # time_range_present
            Int64,        # time_range_start_ms
            Int64,        # time_range_end_ms
            Ref{Int64},   # out_initial_ts_unix_ms
            Ref{Ptr{Cchar}},   # out_resolution
            Ref{Ptr{Cchar}},   # out_horizon
            Ref{Ptr{Cchar}},   # out_interval
            Ref{UInt64},  # out_count
            Ref{UInt64},  # out_scenario_count
            Ref{UInt64},  # out_ndims
            Ref{Ptr{UInt64}},  # out_dims
            Ref{Int32},   # out_dtype
            Ref{Ptr{UInt8}},   # out_data
            Ref{UInt64},  # out_data_byte_len
            Ref{Ptr{Float64}}, # out_percentiles
            Ref{UInt64},  # out_percentiles_len
            Ref{Int32},
        ),  # out_matched_type
        store.handle,
        Int64(owner_id),
        _category_int(owner_category),
        name,
        Int32(ts_type),
        resolution_iso,
        interval_iso,
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
end

# Decode the out-params populated by `castore_store_get_forecast` /
# `castore_store_get_forecast_by_key` into the common named tuple, copying then
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
)
    # Copy dims and free FFI buffer.
    nd = Int(out_ndims[])
    dims_raw = unsafe_wrap(Array, out_dims[], nd; own=false)
    dims = Int.(copy(dims_raw))
    ccall(
        (:castore_buffer_free_u64, lib_path()),
        Cvoid,
        (Ptr{UInt64}, UInt64),
        out_dims[],
        out_ndims[],
    )

    # Copy data bytes and free FFI buffer.
    n_bytes = Int(out_byte_len[])
    bytes_raw = unsafe_wrap(Array, out_data[], n_bytes; own=false)
    bytes = copy(bytes_raw)
    ccall(
        (:castore_buffer_free_u8, lib_path()),
        Cvoid,
        (Ptr{UInt8}, UInt64),
        out_data[],
        out_byte_len[],
    )

    # Percentiles (Probabilistic only; null for others).
    np = Int(out_pct_len[])
    percentiles = if np > 0 && out_pct[] != C_NULL
        p = copy(unsafe_wrap(Array, out_pct[], np; own=false))
        ccall(
            (:castore_buffer_free_f64, lib_path()),
            Cvoid,
            (Ptr{Float64}, UInt64),
            out_pct[],
            out_pct_len[],
        )
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
    )
end

# Key-based counterpart of `_get_forecast_raw`: reads via the key handle
# (`castore_store_get_forecast_by_key`), so the time series type comes from the key.
function _get_forecast_raw(
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
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

    code = ccall(
        (:castore_store_get_forecast_by_key, lib_path()),
        Int32,
        (
            Ptr{Cvoid},   # handle
            Ptr{Cvoid},   # key
            Bool,         # time_range_present
            Int64,        # time_range_start_ms
            Int64,        # time_range_end_ms
            Ref{Int64},   # out_initial_ts_unix_ms
            Ref{Ptr{Cchar}},   # out_resolution
            Ref{Ptr{Cchar}},   # out_horizon
            Ref{Ptr{Cchar}},   # out_interval
            Ref{UInt64},  # out_count
            Ref{UInt64},  # out_scenario_count
            Ref{UInt64},  # out_ndims
            Ref{Ptr{UInt64}},  # out_dims
            Ref{Int32},   # out_dtype
            Ref{Ptr{UInt8}},   # out_data
            Ref{UInt64},  # out_data_byte_len
            Ref{Ptr{Float64}}, # out_percentiles
            Ref{UInt64},  # out_percentiles_len
            Ref{Int32},
        ),  # out_matched_type
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
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(
        store,
        owner_id,
        owner_category,
        name,
        CASTORE_TYPE_ABSTRACT_DETERMINISTIC;
        resolution=resolution,
        interval=interval,
        features=features,
        time_range=time_range,
    )
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    a = _assoc_attrs(
        store,
        owner_id,
        owner_category,
        name,
        r.matched_type;
        resolution=resolution,
        interval=interval,
        features=features,
    )
    return Deterministic(
        r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, data, a.name
    )
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
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(
        store,
        owner_id,
        owner_category,
        name,
        CASTORE_TYPE_DETERMINISTIC;
        resolution=resolution,
        interval=interval,
        features=features,
        time_range=time_range,
    )
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    a = _assoc_attrs(
        store,
        owner_id,
        owner_category,
        name,
        CASTORE_TYPE_DETERMINISTIC;
        resolution=resolution,
        interval=interval,
        features=features,
    )
    return Deterministic(
        r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, data, a.name
    )
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
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(
        store,
        owner_id,
        owner_category,
        name,
        CASTORE_TYPE_DETERMINISTIC_SINGLE;
        resolution=resolution,
        interval=interval,
        features=features,
        time_range=time_range,
    )
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    a = _assoc_attrs(
        store,
        owner_id,
        owner_category,
        name,
        CASTORE_TYPE_DETERMINISTIC_SINGLE;
        resolution=resolution,
        interval=interval,
        features=features,
    )
    return Deterministic(
        r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, data, a.name
    )
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
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(
        store,
        owner_id,
        owner_category,
        name,
        CASTORE_TYPE_PROBABILISTIC;
        resolution=resolution,
        interval=interval,
        features=features,
        time_range=time_range,
    )
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    a = _assoc_attrs(
        store,
        owner_id,
        owner_category,
        name,
        CASTORE_TYPE_PROBABILISTIC;
        resolution=resolution,
        interval=interval,
        features=features,
    )
    return Probabilistic(
        r.initial_timestamp,
        r.resolution,
        r.horizon,
        r.interval,
        r.count,
        r.percentiles,
        data,
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
    interval::Union{Nothing,Period}=nothing,
    features::AbstractDict=Dict{String,Any}(),
    time_range::Union{Nothing,Tuple{DateTime,DateTime}}=nothing,
)
    r = _get_forecast_raw(
        store,
        owner_id,
        owner_category,
        name,
        CASTORE_TYPE_SCENARIOS;
        resolution=resolution,
        interval=interval,
        features=features,
        time_range=time_range,
    )
    data = _decode_forecast_array(r.bytes, r.dtype_code, r.dims)
    a = _assoc_attrs(
        store,
        owner_id,
        owner_category,
        name,
        CASTORE_TYPE_SCENARIOS;
        resolution=resolution,
        interval=interval,
        features=features,
    )
    # `scenario_count` is the leading axis of the decoded data.
    return Scenarios(
        r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, data, a.name
    )
end

# ---- Key-based forecast reads ----------------------------------------------
#
# Counterparts to the attribute-addressed forecast readers above, keyed by a
# `TimeSeriesKey` handle (returned by `add_time_series!`). The time series type
# comes from the key; the `::Type{...}` argument selects how the result is
# decoded and which struct is returned. The key already names the exact stored
# type, so no type resolution happens here (a DST key reads back as a
# `Deterministic`). The attribute-based readers have no DST fallback either:
# use `get_time_series(AbstractDeterministic, ...)` to match either concrete type.

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
    return Deterministic(
        r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, data, a.name
    )
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
        r.initial_timestamp,
        r.resolution,
        r.horizon,
        r.interval,
        r.count,
        r.percentiles,
        data,
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
    return Scenarios(
        r.initial_timestamp, r.resolution, r.horizon, r.interval, r.count, data, a.name
    )
end

# ---- Tracing ---------------------------------------------------------------

"""
    init_logging(level::AbstractString = "")

Initialize the Rust tracing subscriber.

`level` is a [`tracing_subscriber::EnvFilter`] directive string such as
`"debug"`, `"castore_core=debug"`, or
`"warn,castore_core=trace"`. Pass an empty string (the default)
to read the `RUST_LOG` environment variable; if that variable is also unset,
no output is produced.

The subscriber is initialized at most once per process — subsequent calls are
no-ops. `Castore.__init__` reads `RUST_LOG` on module load, so setting
`ENV["RUST_LOG"]` before `using Castore` is sufficient for the common
case.

Returns the FFI status code (`CASTORE_OK = 0`, `CASTORE_ERR_INVALID_PARAMETER = 3` for
an invalid directive string).
"""
function init_logging(level::AbstractString="")
    filter_ptr = isempty(level) ? C_NULL : level
    ret = ccall((:castore_store_init_logging, lib_path()), Int32, (Cstring,), filter_ptr)
    if ret != 0
        @warn "Castore.init_logging: castore_store_init_logging returned error code $ret"
    end
    return ret
end

# Read RUST_LOG at module-load time so that `using Castore` with RUST_LOG
# set in the environment automatically enables tracing without extra user code.
function __init__()
    rust_log = get(ENV, "RUST_LOG", "")
    if !isempty(rust_log)
        try
            init_logging(rust_log)
        catch e
            @warn "Castore.__init__: failed to initialize tracing" exception=e
        end
    end
end

# ---- Timestamp readers ----------------------------------------------------
#
# Stateful readers for the simulation access pattern: a loop over every
# timestamp wants the value of every series at that instant. Build a reader
# once (it resolves the catalog layout and owns reusable buffers), then call
# the read function per timestamp and pull each group's / entry's values. The
# returned arrays are copies in canonical column-major layout, so they stay
# valid across subsequent reads.

_int_for_type(::Type{Deterministic}) = CASTORE_TYPE_DETERMINISTIC
_int_for_type(::Type{DeterministicSingleTimeSeries}) = CASTORE_TYPE_DETERMINISTIC_SINGLE
_int_for_type(::Type{Probabilistic}) = CASTORE_TYPE_PROBABILISTIC
_int_for_type(::Type{Scenarios}) = CASTORE_TYPE_SCENARIOS
function _int_for_type(::Type{T}) where {T}
    return throw(InvalidParameterError("$T is not a forecast type"))
end

# Copy `byte_len` bytes at `ptr` into a fresh `T` array and reshape from the
# stored row-major `dims` to canonical column-major Julia layout. The pointer is
# reader-owned (valid only until the next read), so we always copy.
function _reader_values(
    ptr::Ptr{UInt8}, byte_len::UInt64, ::Type{T}, dims::AbstractVector{<:Integer}
) where {T}
    n = byte_len == 0 ? 0 : Int(byte_len) ÷ sizeof(T)
    flat = Vector{T}(undef, n)
    if n > 0
        GC.@preserve flat unsafe_copyto!(pointer(flat), Ptr{T}(ptr), n)
    end
    nd = length(dims)
    nd <= 1 && return flat
    return permutedims(reshape(flat, reverse(dims)...), reverse(ntuple(identity, nd)))
end

# ---- StaticReader ---------------------------------------------------------

"""
One `(dtype, element_shape)` columnar group of a [`StaticReader`]. `keys[j]`
identifies column `j` of the values matrix returned by [`static_values`].
"""
struct StaticGroup
    dtype::DataType
    element_shape::Vector{Int}
    keys::Vector{TimeSeriesKey}
end

"""
A prepared reader over the `SingleTimeSeries` matching a build filter. Build
with [`build_static_reader`], read a timestamp with [`static_read!`], then pull
each group's values with [`static_values`]. Inspect the layout via
[`static_groups`] / [`static_grid`].
"""
mutable struct StaticReader
    handle::Ptr{Cvoid}
    store::Store
    groups::Vector{StaticGroup}
    function StaticReader(handle::Ptr{Cvoid}, store::Store, groups::Vector{StaticGroup})
        r = new(handle, store, groups)
        finalizer(_finalize_static_reader, r)
        return r
    end
end

function _finalize_static_reader(r::StaticReader)
    if r.handle != C_NULL
        ccall((:castore_static_reader_free, lib_path()), Cvoid, (Ptr{Cvoid},), r.handle)
        r.handle = C_NULL
    end
end

function _static_group_layout(handle::Ptr{Cvoid}, gi::Integer)
    out_dtype = Ref{Int32}(0)
    out_ncols = Ref{UInt64}(0)
    out_shape_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_static_reader_group_info, lib_path()),
        Int32,
        (Ptr{Cvoid}, UInt64, Ref{Int32}, Ref{UInt64}, Ptr{Int64}, UInt64, Ref{UInt64}),
        handle,
        UInt64(gi),
        out_dtype,
        out_ncols,
        C_NULL,
        UInt64(0),
        out_shape_len,
    )
    _check(code)
    shape = Vector{Int64}(undef, Int(out_shape_len[]))
    if out_shape_len[] > 0
        code = ccall(
            (:castore_static_reader_group_info, lib_path()),
            Int32,
            (Ptr{Cvoid}, UInt64, Ref{Int32}, Ref{UInt64}, Ptr{Int64}, UInt64, Ref{UInt64}),
            handle,
            UInt64(gi),
            out_dtype,
            out_ncols,
            shape,
            UInt64(length(shape)),
            out_shape_len,
        )
        _check(code)
    end
    keys = Vector{TimeSeriesKey}(undef, Int(out_ncols[]))
    for col in 0:(Int(out_ncols[]) - 1)
        out_key = Ref{Ptr{Cvoid}}(C_NULL)
        code = ccall(
            (:castore_static_reader_group_key, lib_path()),
            Int32,
            (Ptr{Cvoid}, UInt64, UInt64, Ref{Ptr{Cvoid}}),
            handle,
            UInt64(gi),
            UInt64(col),
            out_key,
        )
        _check(code)
        keys[col + 1] = TimeSeriesKey(out_key[])
    end
    return StaticGroup(_julia_dtype(out_dtype[]), Int.(shape), keys)
end

"""
    build_static_reader(store; resolution, owner_id=nothing,
                        owner_category=nothing, name=nothing, features=Dict())

Build a [`StaticReader`] over the `SingleTimeSeries` matching the filter.
`resolution` (a `Period`) is required — one resolution per reader. The matched
series must share one grid (`initial_timestamp` + `length`).
"""
function build_static_reader(
    store::Store;
    resolution::Period,
    owner_id::Union{Nothing,Integer}=nothing,
    owner_category::Union{Nothing,OwnerCategory}=nothing,
    name::Union{Nothing,AbstractString}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    has_owner = owner_id !== nothing
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    has_category = owner_category !== nothing
    category_arg = has_category ? _category_int(owner_category) : Int32(0)
    name_arg = name === nothing ? C_NULL : String(name)
    resolution_iso = _period_to_iso(resolution)
    features_arg = isempty(features) ? C_NULL : JSON.json(features)
    out = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:castore_store_build_static_reader, lib_path()),
        Int32,
        (Ptr{Cvoid}, Bool, Int64, Bool, Int32, Cstring, Cstring, Cstring, Ref{Ptr{Cvoid}}),
        store.handle,
        has_owner,
        owner_arg,
        has_category,
        category_arg,
        name_arg,
        resolution_iso,
        features_arg,
        out,
    )
    _check(code)
    handle = out[]
    out_n = Ref{UInt64}(0)
    _check(
        ccall(
            (:castore_static_reader_num_groups, lib_path()),
            Int32,
            (Ptr{Cvoid}, Ref{UInt64}),
            handle,
            out_n,
        ),
    )
    groups = [_static_group_layout(handle, gi) for gi in 0:(Int(out_n[]) - 1)]
    return StaticReader(handle, store, groups)
end

"""
    static_grid(reader) -> NamedTuple

The reader's master grid: `(; initial_timestamp::DateTime, resolution::Period,
length::Int)`. Valid timestamps are `initial_timestamp + k·resolution` for
`k in 0:length-1`.
"""
function static_grid(reader::StaticReader)
    out_initial = Ref{Int64}(0)
    out_res = Ref{Ptr{Cchar}}(C_NULL)
    out_len = Ref{UInt64}(0)
    _check(
        ccall(
            (:castore_static_reader_grid, lib_path()),
            Int32,
            (Ptr{Cvoid}, Ref{Int64}, Ref{Ptr{Cchar}}, Ref{UInt64}),
            reader.handle,
            out_initial,
            out_res,
            out_len,
        ),
    )
    return (
        initial_timestamp=_from_unix_ms(out_initial[]),
        resolution=_take_period(out_res[]),
        length=Int(out_len[]),
    )
end

"""
    static_groups(reader) -> Vector{StaticGroup}

The reader's columnar groups (resolved once at build time). Each [`StaticGroup`]
carries its `dtype`, `element_shape`, and the `keys` identifying each column.
"""
static_groups(reader::StaticReader) = reader.groups

"""
    static_read!(reader, t::DateTime) -> reader

Read the value of every series at `t`, filling the reader's buffers. Throws if
`t` is off the reader's grid. Follow with [`static_values`] per group.
"""
function static_read!(reader::StaticReader, t::DateTime)
    _check(
        ccall(
            (:castore_static_reader_read, lib_path()),
            Int32,
            (Ptr{Cvoid}, Ptr{Cvoid}, Int64),
            reader.handle,
            reader.store.handle,
            _to_unix_ms(t),
        ),
    )
    return reader
end

"""
    static_values(reader, group_index::Integer) -> Array

The values from the most recent [`static_read!`] for group `group_index`
(1-based), as a column-major array of size `(num_columns, element_shape...)`.
Column `j` corresponds to `static_groups(reader)[group_index].keys[j]`.
"""
function static_values(reader::StaticReader, group_index::Integer)
    group = reader.groups[group_index]
    out_ptr = Ref{Ptr{UInt8}}(C_NULL)
    out_len = Ref{UInt64}(0)
    _check(
        ccall(
            (:castore_static_reader_group_values, lib_path()),
            Int32,
            (Ptr{Cvoid}, UInt64, Ref{Ptr{UInt8}}, Ref{UInt64}),
            reader.handle,
            UInt64(group_index - 1),
            out_ptr,
            out_len,
        ),
    )
    dims = vcat(length(group.keys), group.element_shape)
    return _reader_values(out_ptr[], out_len[], group.dtype, dims)
end

# ---- ForecastReader -------------------------------------------------------

"""
One forecast's entry in a [`ForecastReader`]. `key` identifies the forecast;
`window_shape` is the shape of a single window (`[H, *E]`, `[P, H, *E]`, or
`[scenarios, H, *E]`). `slot` is the 0-based index of the deduplicated window
read backing this entry — entries that share an array and read plan (e.g.
components referencing one shared forecast) report the same `slot`, so the
`.nc` data is read once per timestamp and a caller can group by `slot` to
materialize each unique window only once.
"""
struct ForecastEntry
    dtype::DataType
    window_shape::Vector{Int}
    key::TimeSeriesKey
    slot::Int
end

"""
A prepared reader over the forecasts of one type matching a build filter. Build
with [`build_forecast_reader`], read a timestamp with [`forecast_read!`], then
pull each entry's window with [`forecast_values`].
"""
mutable struct ForecastReader
    handle::Ptr{Cvoid}
    store::Store
    entries::Vector{ForecastEntry}
    function ForecastReader(
        handle::Ptr{Cvoid}, store::Store, entries::Vector{ForecastEntry}
    )
        r = new(handle, store, entries)
        finalizer(_finalize_forecast_reader, r)
        return r
    end
end

function _finalize_forecast_reader(r::ForecastReader)
    if r.handle != C_NULL
        ccall((:castore_forecast_reader_free, lib_path()), Cvoid, (Ptr{Cvoid},), r.handle)
        r.handle = C_NULL
    end
end

function _forecast_entry_layout(handle::Ptr{Cvoid}, ei::Integer)
    out_dtype = Ref{Int32}(0)
    out_shape_len = Ref{UInt64}(0)
    code = ccall(
        (:castore_forecast_reader_entry_info, lib_path()),
        Int32,
        (Ptr{Cvoid}, UInt64, Ref{Int32}, Ptr{Int64}, UInt64, Ref{UInt64}),
        handle,
        UInt64(ei),
        out_dtype,
        C_NULL,
        UInt64(0),
        out_shape_len,
    )
    _check(code)
    shape = Vector{Int64}(undef, Int(out_shape_len[]))
    if out_shape_len[] > 0
        code = ccall(
            (:castore_forecast_reader_entry_info, lib_path()),
            Int32,
            (Ptr{Cvoid}, UInt64, Ref{Int32}, Ptr{Int64}, UInt64, Ref{UInt64}),
            handle,
            UInt64(ei),
            out_dtype,
            shape,
            UInt64(length(shape)),
            out_shape_len,
        )
        _check(code)
    end
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    _check(
        ccall(
            (:castore_forecast_reader_entry_key, lib_path()),
            Int32,
            (Ptr{Cvoid}, UInt64, Ref{Ptr{Cvoid}}),
            handle,
            UInt64(ei),
            out_key,
        ),
    )
    out_slot = Ref{UInt64}(0)
    _check(
        ccall(
            (:castore_forecast_reader_entry_slot, lib_path()),
            Int32,
            (Ptr{Cvoid}, UInt64, Ref{UInt64}),
            handle,
            UInt64(ei),
            out_slot,
        ),
    )
    return ForecastEntry(
        _julia_dtype(out_dtype[]), Int.(shape), TimeSeriesKey(out_key[]), Int(out_slot[])
    )
end

"""
    build_forecast_reader(store, time_series_type; resolution, owner_id=nothing,
                          owner_category=nothing, name=nothing, features=Dict())

Build a [`ForecastReader`] over forecasts of `time_series_type` (a Julia type:
`Deterministic`, `Probabilistic`, `Scenarios`, or `DeterministicSingleTimeSeries`).
A `Deterministic` reader is abstract — it also includes
`DeterministicSingleTimeSeries`, read into identical `[H, *E]` windows.
`resolution` (a `Period`) is required; matched forecasts must share one window
timeline.
"""
function build_forecast_reader(
    store::Store,
    time_series_type::Type;
    resolution::Period,
    owner_id::Union{Nothing,Integer}=nothing,
    owner_category::Union{Nothing,OwnerCategory}=nothing,
    name::Union{Nothing,AbstractString}=nothing,
    features::AbstractDict=Dict{String,Any}(),
)
    type_code = _int_for_type(time_series_type)
    has_owner = owner_id !== nothing
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    has_category = owner_category !== nothing
    category_arg = has_category ? _category_int(owner_category) : Int32(0)
    name_arg = name === nothing ? C_NULL : String(name)
    resolution_iso = _period_to_iso(resolution)
    features_arg = isempty(features) ? C_NULL : JSON.json(features)
    out = Ref{Ptr{Cvoid}}(C_NULL)
    code = ccall(
        (:castore_store_build_forecast_reader, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Bool,
            Int64,
            Bool,
            Int32,
            Int32,
            Cstring,
            Cstring,
            Cstring,
            Ref{Ptr{Cvoid}},
        ),
        store.handle,
        has_owner,
        owner_arg,
        has_category,
        category_arg,
        Int32(type_code),
        name_arg,
        resolution_iso,
        features_arg,
        out,
    )
    _check(code)
    handle = out[]
    out_n = Ref{UInt64}(0)
    _check(
        ccall(
            (:castore_forecast_reader_num_entries, lib_path()),
            Int32,
            (Ptr{Cvoid}, Ref{UInt64}),
            handle,
            out_n,
        ),
    )
    entries = [_forecast_entry_layout(handle, ei) for ei in 0:(Int(out_n[]) - 1)]
    return ForecastReader(handle, store, entries)
end

"""
    forecast_timeline(reader) -> NamedTuple

The reader's window timeline: `(; initial_timestamp::DateTime, resolution::Period,
interval::Period, count::Int)`. Valid timestamps are `initial_timestamp +
k·interval` for `k in 0:count-1`.
"""
function forecast_timeline(reader::ForecastReader)
    out_initial = Ref{Int64}(0)
    out_res = Ref{Ptr{Cchar}}(C_NULL)
    out_interval = Ref{Ptr{Cchar}}(C_NULL)
    out_count = Ref{UInt64}(0)
    _check(
        ccall(
            (:castore_forecast_reader_timeline, lib_path()),
            Int32,
            (Ptr{Cvoid}, Ref{Int64}, Ref{Ptr{Cchar}}, Ref{Ptr{Cchar}}, Ref{UInt64}),
            reader.handle,
            out_initial,
            out_res,
            out_interval,
            out_count,
        ),
    )
    return (
        initial_timestamp=_from_unix_ms(out_initial[]),
        resolution=_take_period(out_res[]),
        interval=_take_period(out_interval[]),
        count=Int(out_count[]),
    )
end

"""
    forecast_entries(reader) -> Vector{ForecastEntry}

The reader's per-key window entries (resolved once at build time). Each entry's
`slot` field identifies its deduplicated window read; entries sharing a `slot`
read the same `.nc` data once per timestamp.
"""
forecast_entries(reader::ForecastReader) = reader.entries

"""
    forecast_num_slots(reader) -> Int

The number of deduplicated window slots — i.e. the count of physical `.nc` reads
[`forecast_read!`] performs per timestamp. Entries that share an array and read
plan collapse to one slot, so this is `≤ length(forecast_entries(reader))`.
"""
function forecast_num_slots(reader::ForecastReader)
    out_n = Ref{UInt64}(0)
    _check(
        ccall(
            (:castore_forecast_reader_num_slots, lib_path()),
            Int32,
            (Ptr{Cvoid}, Ref{UInt64}),
            reader.handle,
            out_n,
        ),
    )
    return Int(out_n[])
end

"""
    forecast_read!(reader, t::DateTime) -> reader

Read the forecast window at `t` for every entry, filling the reader's buffers.
Throws if `t` is off the window timeline. Follow with [`forecast_values`].
"""
function forecast_read!(reader::ForecastReader, t::DateTime)
    _check(
        ccall(
            (:castore_forecast_reader_read, lib_path()),
            Int32,
            (Ptr{Cvoid}, Ptr{Cvoid}, Int64),
            reader.handle,
            reader.store.handle,
            _to_unix_ms(t),
        ),
    )
    return reader
end

"""
    forecast_values(reader, entry_index::Integer) -> Array

The window from the most recent [`forecast_read!`] for entry `entry_index`
(1-based), as a column-major array of size `window_shape`.
"""
function forecast_values(reader::ForecastReader, entry_index::Integer)
    entry = reader.entries[entry_index]
    out_ptr = Ref{Ptr{UInt8}}(C_NULL)
    out_len = Ref{UInt64}(0)
    _check(
        ccall(
            (:castore_forecast_reader_entry_values, lib_path()),
            Int32,
            (Ptr{Cvoid}, UInt64, Ref{Ptr{UInt8}}, Ref{UInt64}),
            reader.handle,
            UInt64(entry_index - 1),
            out_ptr,
            out_len,
        ),
    )
    return _reader_values(out_ptr[], out_len[], entry.dtype, entry.window_shape)
end

# ---- Base interface --------------------------------------------------------
#
# Key equality/hash delegate to the Rust core identity semantics via the FFI so
# Julia never re-implements them. The value types delegate their container
# interface to the wrapped `data` array; forecast `length` is the window count.

"""
Identity equality (owner, category, type, name, resolution, interval,
features), delegated to the Rust core. Consistent with `hash`, so keys work as
`Dict`/`Set` members.
"""
function Base.:(==)(a::TimeSeriesKey, b::TimeSeriesKey)
    out = Ref{Bool}(false)
    _check(
        ccall(
            (:castore_key_eq, lib_path()),
            Int32,
            (Ptr{Cvoid}, Ptr{Cvoid}, Ref{Bool}),
            a.handle,
            b.handle,
            out,
        ),
    )
    return out[]
end

function Base.hash(k::TimeSeriesKey, h::UInt)
    out = Ref{UInt64}(0)
    _check(
        ccall(
            (:castore_key_identity_hash, lib_path()),
            Int32,
            (Ptr{Cvoid}, Ref{UInt64}),
            k.handle,
            out,
        ),
    )
    return hash(out[], h)
end

function Base.show(io::IO, k::TimeSeriesKey)
    if k.handle == C_NULL
        print(io, "TimeSeriesKey(freed)")
        return nothing
    end
    info = key_info(k)
    return print(
        io,
        "TimeSeriesKey(",
        info.time_series_type,
        " name=",
        repr(info.name),
        " owner_id=",
        info.owner_id,
        " owner_category=",
        info.owner_category,
        ")",
    )
end

function Base.show(io::IO, s::Store)
    if s.handle == C_NULL
        print(io, "Store(closed)")
    else
        print(io, "Store(read_only=", read_only(s), ")")
    end
end

function Base.show(io::IO, ts::SingleTimeSeries{T,N}) where {T,N}
    return print(
        io,
        "SingleTimeSeries{",
        T,
        ",",
        N,
        "}(name=",
        repr(ts.name),
        " length=",
        size(ts.data, 1),
        " initial_timestamp=",
        ts.initial_timestamp,
        " resolution=",
        ts.resolution,
        ")",
    )
end

function Base.show(io::IO, ts::NonSequentialTimeSeries{T,N}) where {T,N}
    return print(
        io,
        "NonSequentialTimeSeries{",
        T,
        ",",
        N,
        "}(name=",
        repr(ts.name),
        " length=",
        size(ts.data, 1),
        ")",
    )
end

for FT in (:Deterministic, :Probabilistic, :Scenarios)
    @eval function Base.show(io::IO, ts::$FT{T,N}) where {T,N}
        print(
            io,
            $(string(FT)),
            "{",
            T,
            ",",
            N,
            "}(name=",
            repr(ts.name),
            " count=",
            ts.count,
            " horizon=",
            ts.horizon,
            " interval=",
            ts.interval,
            ")",
        )
    end
end

# Container interface: full delegation to `data` (element count, not time
# steps, for multi-dimensional values — consistent with `iterate`/`getindex`).
for ST in (:SingleTimeSeries, :NonSequentialTimeSeries)
    @eval begin
        Base.length(ts::$ST) = length(ts.data)
        Base.eltype(::Type{$ST{T,N}}) where {T,N} = T
        Base.getindex(ts::$ST, i...) = getindex(ts.data, i...)
        Base.iterate(ts::$ST) = iterate(ts.data)
        Base.iterate(ts::$ST, state) = iterate(ts.data, state)
    end
end

# Forecast length is the number of forecast windows.
Base.length(ts::Deterministic) = ts.count
Base.length(ts::Probabilistic) = ts.count
Base.length(ts::Scenarios) = ts.count

end # module Castore
