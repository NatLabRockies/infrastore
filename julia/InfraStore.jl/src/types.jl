# ---- Element dtypes -------------------------------------------------------
# Codes must match `Dtype` in the Rust core / FFI.

const _DTYPE_JULIA = (
    Float64, Float32, Int64, Int32, UInt64, Bool, Int16, Int8, UInt32, UInt16, UInt8
)
_julia_dtype(code::Integer) = _DTYPE_JULIA[Int(code) + 1]

# The Julia element type for a dtype's canonical name (the Rust `as_str` form).
const _DTYPE_BY_NAME = Dict{String, Type}(
    "f64" => Float64,
    "f32" => Float32,
    "i64" => Int64,
    "i32" => Int32,
    "i16" => Int16,
    "i8" => Int8,
    "u64" => UInt64,
    "u32" => UInt32,
    "u16" => UInt16,
    "u8" => UInt8,
    "bool" => Bool,
)

const _NAME_BY_DTYPE = Dict{Type, String}(v => k for (k, v) in _DTYPE_BY_NAME)

function _dtype_for_name(name::AbstractString)
    dtype = get(_DTYPE_BY_NAME, String(name), nothing)
    dtype === nothing && throw(InvalidParameterError("unknown dtype $name"))
    return dtype
end

# ---- Element types --------------------------------------------------------
# `element_type` is the store's own vocabulary for what the elements mean: a
# dtype spelling for plain numbers, else `tuple(N,dtype)` or one of the
# function-data kinds (`linear_function`, `quadratic_function`,
# `piecewise_linear`, `piecewise_step`). It supersedes the dtype code on the
# write ABI — the physical dtype is derived from it.

function _element_type_name(::Type{T}) where {T}
    name = get(_NAME_BY_DTYPE, T, nothing)
    name === nothing && throw(InvalidParameterError("unsupported element dtype $T"))
    return name
end

# The `element_type` string a write sends: the caller's declaration when it made
# one, else plain scalars of the array's own element type.
function _element_type_arg(element_type, data::AbstractArray)
    element_type === nothing && return _element_type_name(eltype(data))
    return String(element_type)
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
    application_data::Union{Nothing, String}
    "Canonical `element_type` string, or `nothing` for plain scalars of `eltype(data)`."
    element_type::Union{Nothing, String}
    "User-declared units label for the values (e.g. `\"MW\"`), or `nothing`. Set at construction and returned on read; the store never interprets or validates it, and it is never part of a series' identity."
    units::Union{Nothing, String}
    "What kind of physical quantity the values measure (e.g. `\"ActivePower\"`), or `nothing`. Free-form; QUDT `QuantityKind` local names are the recommended vocabulary. It separates active from reactive power, which dimensional analysis alone cannot, and it is the only record of what per-unit values measure."
    quantity_kind::Union{Nothing, String}
    "Which basis the values are expressed in (a `UnitSystem`), or `nothing` for unspecified -- which is not the same as `NaturalUnits`."
    unit_system::Union{Nothing, UnitSystem}
    "The field on the owning component whose value these values are the time-varying form of (e.g. `\"max_active_power\"`), or `nothing`. Free-form and never interpreted by the store: it names a field in the consumer's own object model. Descriptive, so it is never part of a series' identity."
    component_field::Union{Nothing, String}
end

# Infer `{T,N}` from the value array; views/ranges are normalized to a concrete
# `Array` (copy-free when already one).
function SingleTimeSeries(
    initial,
    resolution,
    data::AbstractArray,
    name::AbstractString;
    application_data::Union{Nothing, AbstractString}=nothing,
    element_type::Union{Nothing, AbstractString}=nothing,
    units::Union{Nothing, AbstractString}=nothing,
    quantity_kind::Union{Nothing, AbstractString}=nothing,
    unit_system::Union{Nothing, UnitSystem, AbstractString}=nothing,
    component_field::Union{Nothing, AbstractString}=nothing,
)
    return SingleTimeSeries{eltype(data), ndims(data)}(
        initial,
        resolution,
        data isa Array ? data : Array(data),
        String(name),
        _maybe_string(application_data),
        _maybe_string(element_type),
        _maybe_string(units),
        _maybe_string(quantity_kind),
        _unit_system(unit_system),
        _maybe_string(component_field),
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
    application_data::Union{Nothing, String}
    "Canonical `element_type` string, or `nothing` for plain scalars of `eltype(data)`."
    element_type::Union{Nothing, String}
    "User-declared units label for the values (e.g. `\"MW\"`), or `nothing`. Set at construction and returned on read; the store never interprets or validates it, and it is never part of a series' identity."
    units::Union{Nothing, String}
    "What kind of physical quantity the values measure (e.g. `\"ActivePower\"`), or `nothing`. Free-form; QUDT `QuantityKind` local names are the recommended vocabulary. It separates active from reactive power, which dimensional analysis alone cannot, and it is the only record of what per-unit values measure."
    quantity_kind::Union{Nothing, String}
    "Which basis the values are expressed in (a `UnitSystem`), or `nothing` for unspecified -- which is not the same as `NaturalUnits`."
    unit_system::Union{Nothing, UnitSystem}
    "The field on the owning component whose value these values are the time-varying form of (e.g. `\"max_active_power\"`), or `nothing`. Free-form and never interpreted by the store: it names a field in the consumer's own object model. Descriptive, so it is never part of a series' identity."
    component_field::Union{Nothing, String}
end

# Infer `{T,N}` from the value array; views/ranges are normalized to a concrete
# `Array`. Timestamps are explicit and must be strictly increasing, with one entry
# per leading-dimension row (`size(data, 1)`).
function NonSequentialTimeSeries(
    timestamps,
    data::AbstractArray,
    name::AbstractString;
    application_data::Union{Nothing, AbstractString}=nothing,
    element_type::Union{Nothing, AbstractString}=nothing,
    units::Union{Nothing, AbstractString}=nothing,
    quantity_kind::Union{Nothing, AbstractString}=nothing,
    unit_system::Union{Nothing, UnitSystem, AbstractString}=nothing,
    component_field::Union{Nothing, AbstractString}=nothing,
)
    length(timestamps) == size(data, 1) ||
        throw(InvalidParameterError("timestamp count must match data length"))
    all(timestamps[i] < timestamps[i + 1] for i in 1:(length(timestamps) - 1)) ||
        throw(InvalidParameterError("timestamps must be strictly increasing"))
    arr = data isa Array ? data : Array(data)
    return NonSequentialTimeSeries{eltype(arr), ndims(arr)}(
        Vector{DateTime}(timestamps),
        arr,
        String(name),
        _maybe_string(application_data),
        _maybe_string(element_type),
        _maybe_string(units),
        _maybe_string(quantity_kind),
        _unit_system(unit_system),
        _maybe_string(component_field),
    )
end

# ---- Forecast types -------------------------------------------------------
#
# Dense forecasts mirror the InfrastructureSystems.jl objects. `data` is a Julia
# (column-major) array in the canonical shape noted on each type; it round-trips
# through `add_time_series!` / `get_time_series`. `DeterministicSingleTimeSeries`
# is a marker type with no materialized form: it is derived from a stored
# `SingleTimeSeries` via `transform_single_time_series!` and read back as a
# `Deterministic` (see the type below). Requesting `Deterministic` matches it
# too, so which of the two a store holds stays an internal detail.

struct Deterministic{T, N}
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
    application_data::Union{Nothing, String}
    "Canonical `element_type` string, or `nothing` for plain scalars of `eltype(data)`."
    element_type::Union{Nothing, String}
    "User-declared units label for the values (e.g. `\"MW\"`), or `nothing`. Set at construction and returned on read; the store never interprets or validates it, and it is never part of a series' identity."
    units::Union{Nothing, String}
    "What kind of physical quantity the values measure (e.g. `\"ActivePower\"`), or `nothing`. Free-form; QUDT `QuantityKind` local names are the recommended vocabulary. It separates active from reactive power, which dimensional analysis alone cannot, and it is the only record of what per-unit values measure."
    quantity_kind::Union{Nothing, String}
    "Which basis the values are expressed in (a `UnitSystem`), or `nothing` for unspecified -- which is not the same as `NaturalUnits`."
    unit_system::Union{Nothing, UnitSystem}
    "The field on the owning component whose value these values are the time-varying form of (e.g. `\"max_active_power\"`), or `nothing`. Free-form and never interpreted by the store: it names a field in the consumer's own object model. Descriptive, so it is never part of a series' identity."
    component_field::Union{Nothing, String}
end

function Deterministic(
    initial,
    resolution,
    horizon,
    interval,
    count,
    data::AbstractArray,
    name::AbstractString;
    application_data::Union{Nothing, AbstractString}=nothing,
    element_type::Union{Nothing, AbstractString}=nothing,
    units::Union{Nothing, AbstractString}=nothing,
    quantity_kind::Union{Nothing, AbstractString}=nothing,
    unit_system::Union{Nothing, UnitSystem, AbstractString}=nothing,
    component_field::Union{Nothing, AbstractString}=nothing,
)
    return Deterministic{eltype(data), ndims(data)}(
        initial,
        resolution,
        horizon,
        interval,
        Int(count),
        data isa Array ? data : Array(data),
        String(name),
        _maybe_string(application_data),
        _maybe_string(element_type),
        _maybe_string(units),
        _maybe_string(quantity_kind),
        _unit_system(unit_system),
        _maybe_string(component_field),
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
    application_data::Union{Nothing, String}
    "Canonical `element_type` string, or `nothing` for plain scalars of `eltype(data)`."
    element_type::Union{Nothing, String}
    "User-declared units label for the values (e.g. `\"MW\"`), or `nothing`. Set at construction and returned on read; the store never interprets or validates it, and it is never part of a series' identity."
    units::Union{Nothing, String}
    "What kind of physical quantity the values measure (e.g. `\"ActivePower\"`), or `nothing`. Free-form; QUDT `QuantityKind` local names are the recommended vocabulary. It separates active from reactive power, which dimensional analysis alone cannot, and it is the only record of what per-unit values measure."
    quantity_kind::Union{Nothing, String}
    "Which basis the values are expressed in (a `UnitSystem`), or `nothing` for unspecified -- which is not the same as `NaturalUnits`."
    unit_system::Union{Nothing, UnitSystem}
    "The field on the owning component whose value these values are the time-varying form of (e.g. `\"max_active_power\"`), or `nothing`. Free-form and never interpreted by the store: it names a field in the consumer's own object model. Descriptive, so it is never part of a series' identity."
    component_field::Union{Nothing, String}
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
    application_data::Union{Nothing, AbstractString}=nothing,
    element_type::Union{Nothing, AbstractString}=nothing,
    units::Union{Nothing, AbstractString}=nothing,
    quantity_kind::Union{Nothing, AbstractString}=nothing,
    unit_system::Union{Nothing, UnitSystem, AbstractString}=nothing,
    component_field::Union{Nothing, AbstractString}=nothing,
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
        _maybe_string(application_data),
        _maybe_string(element_type),
        _maybe_string(units),
        _maybe_string(quantity_kind),
        _unit_system(unit_system),
        _maybe_string(component_field),
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
    application_data::Union{Nothing, String}
    "Canonical `element_type` string, or `nothing` for plain scalars of `eltype(data)`."
    element_type::Union{Nothing, String}
    "User-declared units label for the values (e.g. `\"MW\"`), or `nothing`. Set at construction and returned on read; the store never interprets or validates it, and it is never part of a series' identity."
    units::Union{Nothing, String}
    "What kind of physical quantity the values measure (e.g. `\"ActivePower\"`), or `nothing`. Free-form; QUDT `QuantityKind` local names are the recommended vocabulary. It separates active from reactive power, which dimensional analysis alone cannot, and it is the only record of what per-unit values measure."
    quantity_kind::Union{Nothing, String}
    "Which basis the values are expressed in (a `UnitSystem`), or `nothing` for unspecified -- which is not the same as `NaturalUnits`."
    unit_system::Union{Nothing, UnitSystem}
    "The field on the owning component whose value these values are the time-varying form of (e.g. `\"max_active_power\"`), or `nothing`. Free-form and never interpreted by the store: it names a field in the consumer's own object model. Descriptive, so it is never part of a series' identity."
    component_field::Union{Nothing, String}
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
    application_data::Union{Nothing, AbstractString}=nothing,
    element_type::Union{Nothing, AbstractString}=nothing,
    units::Union{Nothing, AbstractString}=nothing,
    quantity_kind::Union{Nothing, AbstractString}=nothing,
    unit_system::Union{Nothing, UnitSystem, AbstractString}=nothing,
    component_field::Union{Nothing, AbstractString}=nothing,
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
        _maybe_string(application_data),
        _maybe_string(element_type),
        _maybe_string(units),
        _maybe_string(quantity_kind),
        _unit_system(unit_system),
        _maybe_string(component_field),
    )
end

"""
    DeterministicSingleTimeSeries

Marker type naming a forecast derived from a `SingleTimeSeries` via
`transform_single_time_series!` (mirrors the InfrastructureSystems.jl type). It
is never constructed or added directly and has no materialized struct.

**You do not normally ask for this type.** Whether a forecast is stored densely
or derived from a `SingleTimeSeries` is a storage detail:
`get_time_series(Deterministic, …)` matches either and returns a
[`Deterministic`] both ways. This type exists so the detail is *inspectable* —
it surfaces as the `time_series_type` of keys and metadata from
`get_time_series_keys` / `key_info` / `list_keys`, and passing it as a requested
type narrows a query to the derived forecasts alone (e.g. to audit which of a
store's forecasts are synthetic).
"""
abstract type DeterministicSingleTimeSeries end

# Every type accepted as a *requested* forecast type. Internal: it exists for
# method bounds only, is not exported, and is not part of the public surface —
# callers name a concrete type, and `Deterministic` already spans both
# deterministic storage forms (see `_forecast_result_type`).
const _ForecastRequest = Union{
    Deterministic, DeterministicSingleTimeSeries, Probabilistic, Scenarios
}
