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

# The Julia element type for a catalog row's dtype name (the Rust `as_str` form).
const _DTYPE_BY_NAME = Dict{String, Type}(
    "f64" => Float64,
    "f32" => Float32,
    "i64" => Int64,
    "i32" => Int32,
    "u64" => UInt64,
    "bool" => Bool,
)

function _dtype_for_name(name::AbstractString)
    dtype = get(_DTYPE_BY_NAME, String(name), nothing)
    dtype === nothing && throw(InvalidParameterError("unknown dtype $name"))
    return dtype
end

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

# Decode raw FFI bytes into a properly-shaped Julia array — the inverse of
# `_row_major_bytes`. `dims` is in row-major order `[d0, d1, ...]`; the bytes
# are reinterpreted as the dtype, reshaped with reversed dims, and the axes
# permuted back to canonical column-major layout. Shared by every data-read
# decode path (single, non-sequential, forecast, bulk).
function _decode_array(bytes::Vector{UInt8}, dtype_code::Integer, dims::Vector{Int})
    T = _julia_dtype(dtype_code)
    flat = collect(reinterpret(T, bytes))
    n = length(dims)
    n <= 1 && return reshape(flat, dims...)
    return permutedims(reshape(flat, reverse(dims)...), reverse(ntuple(identity, n)))
end

# `name` is a per-association attribute carried on the binding structs (matching
# InfrastructureSystems.jl); it is not part of the deduplicated core data type.
# `name` is required.
_maybe_string(::Nothing) = nothing
_maybe_string(s::AbstractString) = String(s)

# ---- Single time series ---------------------------------------------------

struct SingleTimeSeries{T, N}
    initial_timestamp::DateTime
    resolution::Period
    "Values: a 1-D vector (scalar per step) or N-D array (dim 1 = time)."
    data::Array{T, N}
    "Association name (required; the same array may be stored under different names)."
    name::String
    "Opaque, package-owned extension payload (typically JSON) the binding writes and reads to reconstruct domain objects; the store never interprets it."
    ext::Union{Nothing, String}
end

# Infer `{T,N}` from the value array; views/ranges are normalized to a concrete
# `Array` (copy-free when already one).
function SingleTimeSeries(
    initial,
    resolution,
    data::AbstractArray,
    name::AbstractString;
    ext::Union{Nothing, AbstractString}=nothing,
)
    return SingleTimeSeries{eltype(data), ndims(data)}(
        initial,
        resolution,
        data isa Array ? data : Array(data),
        String(name),
        _maybe_string(ext),
    )
end

# ---- Non-sequential time series -------------------------------------------

struct NonSequentialTimeSeries{T, N}
    timestamps::Vector{DateTime}
    "Values: a 1-D vector (scalar per step) or N-D array (dim 1 = time, one entry per timestamp)."
    data::Array{T, N}
    "Association name (required)."
    name::String
    "Opaque, package-owned extension payload (typically JSON) the binding writes and reads to reconstruct domain objects; the store never interprets it."
    ext::Union{Nothing, String}
end

# Infer `{T,N}` from the value array; views/ranges are normalized to a concrete
# `Array`. Timestamps are explicit and must be strictly increasing, with one entry
# per leading-dimension row (`size(data, 1)`).
function NonSequentialTimeSeries(
    timestamps,
    data::AbstractArray,
    name::AbstractString;
    ext::Union{Nothing, AbstractString}=nothing,
)
    length(timestamps) == size(data, 1) ||
        throw(InvalidParameterError("timestamp count must match data length"))
    all(timestamps[i] < timestamps[i + 1] for i in 1:(length(timestamps) - 1)) ||
        throw(InvalidParameterError("timestamps must be strictly increasing"))
    arr = data isa Array ? data : Array(data)
    return NonSequentialTimeSeries{eltype(arr), ndims(arr)}(
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

struct Deterministic{T, N} <: AbstractDeterministic
    initial_timestamp::DateTime
    resolution::Period
    horizon::Period
    interval::Period
    count::Int
    "Values with canonical shape `(H, count, element_dims...)`."
    data::Array{T, N}
    "Association name (required)."
    name::String
    "Opaque, package-owned extension payload (typically JSON) the binding writes and reads to reconstruct domain objects; the store never interprets it."
    ext::Union{Nothing, String}
end

function Deterministic(
    initial,
    resolution,
    horizon,
    interval,
    count,
    data::AbstractArray,
    name::AbstractString;
    ext::Union{Nothing, AbstractString}=nothing,
)
    return Deterministic{eltype(data), ndims(data)}(
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

struct Probabilistic{T, N}
    initial_timestamp::DateTime
    resolution::Period
    horizon::Period
    interval::Period
    count::Int
    percentiles::Vector{Float64}
    "Values with canonical shape `(num_percentiles, H, count, element_dims...)`."
    data::Array{T, N}
    "Association name (required)."
    name::String
    "Opaque, package-owned extension payload (typically JSON) the binding writes and reads to reconstruct domain objects; the store never interprets it."
    ext::Union{Nothing, String}
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
    ext::Union{Nothing, AbstractString}=nothing,
)
    return Probabilistic{eltype(data), ndims(data)}(
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

struct Scenarios{T, N}
    initial_timestamp::DateTime
    resolution::Period
    horizon::Period
    interval::Period
    count::Int
    scenario_count::Int
    "Values with canonical shape `(scenario_count, H, count, element_dims...)`."
    data::Array{T, N}
    "Association name (required)."
    name::String
    "Opaque, package-owned extension payload (typically JSON) the binding writes and reads to reconstruct domain objects; the store never interprets it."
    ext::Union{Nothing, String}
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
    ext::Union{Nothing, AbstractString}=nothing,
)
    return Scenarios{eltype(data), ndims(data)}(
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
