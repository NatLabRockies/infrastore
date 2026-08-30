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

# The physical dtype an `element_type` string stores, mirroring
# `ElementType::physical_dtype` in the Rust core. `nothing` for a spelling this
# wrapper does not recognise — the core is the authority on the vocabulary, so an
# unknown string is forwarded and rejected there rather than here.
function _physical_dtype_of(element_type::AbstractString)
    s = String(element_type)
    s in ("linear_function", "quadratic_function", "piecewise_linear", "piecewise_step") &&
        return Float64
    inner = match(r"^tuple\(\s*\d+\s*,\s*([A-Za-z0-9]+)\s*\)$", s)
    inner !== nothing && return get(_DTYPE_BY_NAME, inner.captures[1], nothing)
    return get(_DTYPE_BY_NAME, s, nothing)
end

# The `element_type` a constructor records for `data`.
#
# Domain values name their own — a `Vector{PiecewiseLinear}` is a
# `piecewise_linear` series and there is nothing for a caller to declare — so
# `element_type=` is only for the numeric case, where the numbers alone cannot
# say what they mean. Declaring one that disagrees with the values is an error
# rather than an override: the values are the more specific statement.
function _declared_element_type(element_type, data::AbstractArray)
    isempty(data) && return _maybe_string(element_type)
    is_element_values(data) || return _maybe_string(element_type)
    implied = element_type_tag(vec(data))
    if element_type !== nothing && String(element_type) != implied
        throw(
            InvalidParameterError(
                "element_type \"$(element_type)\" disagrees with the values, which " *
                "are $implied",
            ),
        )
    end
    return implied
end

# What a value array goes down the ABI as: `(element_type, dims, bytes)`.
#
# Domain values are encoded *here*, at the boundary, so the struct keeps holding
# them — which is what lets a read hand back the same thing a write was given,
# and what makes a metadata row's `{T,N}` describe the values rather than their
# packing.
function _wire_array(element_type, data::AbstractArray)
    if !isempty(data) && is_element_values(data)
        array, tag = encode_element_values(data)
        return (tag, UInt64[size(array)...], _row_major_bytes(array))
    end
    return (
        _element_type_arg(element_type, data),
        UInt64[size(data)...],
        _row_major_bytes(data),
    )
end

# The `element_type` string a write sends: the caller's declaration when it made
# one, else plain scalars of the array's own element type.
#
# A declaration is checked against the array it describes. The bytes on the wire
# come from `eltype(data)` alone, while the core validates only the *total* byte
# length — so a same-width disagreement (Int64 as "f64", Bool as "u8", …) is
# stored and read back as reinterpreted bits with no error anywhere. Only the
# width-*mismatched* case fails today, which is the wrong half to catch: the
# silent one is the one that corrupts.
function _element_type_arg(element_type, data::AbstractArray)
    element_type === nothing && return _element_type_name(eltype(data))
    declared = String(element_type)
    physical = _physical_dtype_of(declared)
    if physical !== nothing && physical !== eltype(data)
        throw(
            InvalidParameterError(
                "element_type \"$declared\" stores $physical values, but the array's " *
                "element type is $(eltype(data))",
            ),
        )
    end
    return declared
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
    "How the timestamps were spelled (a [`TimeReference`](@ref)), or `nothing` for unspecified. Inferred from the timestamp the constructor was handed -- a bare `DateTime` is a wall clock and records `ZonelessReference()`, a `ZonedDateTime` records the spelling its zone names -- unless `time_reference=` overrides it. Passing `time_reference=nothing` explicitly declares *unspecified*, which is what a read hands back for a series that recorded no spelling, and is not the same as a wall clock. Descriptive: it changes nothing about the stored instants, the grid, or either content hash."
    time_reference::Union{Nothing, TimeReference}
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
    time_reference::TimeReferenceArg=INFERRED,
)
    return SingleTimeSeries{eltype(data), ndims(data)}(
        _utc_datetime(initial),
        resolution,
        data isa Array ? data : Array(data),
        String(name),
        _maybe_string(application_data),
        _declared_element_type(element_type, data),
        _maybe_string(units),
        _maybe_string(quantity_kind),
        _unit_system(unit_system),
        _maybe_string(component_field),
        _resolved_time_reference(time_reference, initial),
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
    "How the timestamps were spelled (a [`TimeReference`](@ref)), or `nothing` for unspecified. Inferred from the timestamp the constructor was handed -- a bare `DateTime` is a wall clock and records `ZonelessReference()`, a `ZonedDateTime` records the spelling its zone names -- unless `time_reference=` overrides it. Passing `time_reference=nothing` explicitly declares *unspecified*, which is what a read hands back for a series that recorded no spelling, and is not the same as a wall clock. Descriptive: it changes nothing about the stored instants, the grid, or either content hash."
    time_reference::Union{Nothing, TimeReference}
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
    time_reference::TimeReferenceArg=INFERRED,
)
    length(timestamps) == size(data, 1) ||
        throw(InvalidParameterError("timestamp count must match data length"))
    # The spelling is read off the vector before it is normalized -- afterwards
    # every element is a bare `DateTime` and the intent is gone.
    reference = _vector_time_reference(timestamps)
    # Normalize first, then check: a vector mixing zones (or `ZonedDateTime`s
    # from different ones) is ordered by the instants it names, not by the wall
    # clocks it reads.
    timestamps = DateTime[_utc_datetime(t) for t in timestamps]
    all(timestamps[i] < timestamps[i + 1] for i in 1:(length(timestamps) - 1)) ||
        throw(InvalidParameterError("timestamps must be strictly increasing"))
    arr = data isa Array ? data : Array(data)
    return NonSequentialTimeSeries{eltype(arr), ndims(arr)}(
        timestamps,
        arr,
        String(name),
        _maybe_string(application_data),
        _declared_element_type(element_type, data),
        _maybe_string(units),
        _maybe_string(quantity_kind),
        _unit_system(unit_system),
        _maybe_string(component_field),
        time_reference isa _Inferred ? reference : _time_reference(time_reference),
    )
end

# ---- Forecast types -------------------------------------------------------
#
# Dense forecasts mirror the InfrastructureSystems.jl objects. `data` is a Julia
# (column-major) array in the canonical shape noted on each type; it round-trips
# through `add_time_series!` / `read_by_id`. `DeterministicSingleTimeSeries`
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
    "How the timestamps were spelled (a [`TimeReference`](@ref)), or `nothing` for unspecified. Inferred from the timestamp the constructor was handed -- a bare `DateTime` is a wall clock and records `ZonelessReference()`, a `ZonedDateTime` records the spelling its zone names -- unless `time_reference=` overrides it. Passing `time_reference=nothing` explicitly declares *unspecified*, which is what a read hands back for a series that recorded no spelling, and is not the same as a wall clock. Descriptive: it changes nothing about the stored instants, the grid, or either content hash."
    time_reference::Union{Nothing, TimeReference}
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
    time_reference::TimeReferenceArg=INFERRED,
)
    return Deterministic{eltype(data), ndims(data)}(
        _utc_datetime(initial),
        resolution,
        horizon,
        interval,
        Int(count),
        data isa Array ? data : Array(data),
        String(name),
        _maybe_string(application_data),
        _declared_element_type(element_type, data),
        _maybe_string(units),
        _maybe_string(quantity_kind),
        _unit_system(unit_system),
        _maybe_string(component_field),
        _resolved_time_reference(time_reference, initial),
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
    "How the timestamps were spelled (a [`TimeReference`](@ref)), or `nothing` for unspecified. Inferred from the timestamp the constructor was handed -- a bare `DateTime` is a wall clock and records `ZonelessReference()`, a `ZonedDateTime` records the spelling its zone names -- unless `time_reference=` overrides it. Passing `time_reference=nothing` explicitly declares *unspecified*, which is what a read hands back for a series that recorded no spelling, and is not the same as a wall clock. Descriptive: it changes nothing about the stored instants, the grid, or either content hash."
    time_reference::Union{Nothing, TimeReference}
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
    time_reference::TimeReferenceArg=INFERRED,
)
    return Probabilistic{eltype(data), ndims(data)}(
        _utc_datetime(initial),
        resolution,
        horizon,
        interval,
        Int(count),
        Vector{Float64}(percentiles),
        data isa Array ? data : Array(data),
        String(name),
        _maybe_string(application_data),
        _declared_element_type(element_type, data),
        _maybe_string(units),
        _maybe_string(quantity_kind),
        _unit_system(unit_system),
        _maybe_string(component_field),
        _resolved_time_reference(time_reference, initial),
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
    "How the timestamps were spelled (a [`TimeReference`](@ref)), or `nothing` for unspecified. Inferred from the timestamp the constructor was handed -- a bare `DateTime` is a wall clock and records `ZonelessReference()`, a `ZonedDateTime` records the spelling its zone names -- unless `time_reference=` overrides it. Passing `time_reference=nothing` explicitly declares *unspecified*, which is what a read hands back for a series that recorded no spelling, and is not the same as a wall clock. Descriptive: it changes nothing about the stored instants, the grid, or either content hash."
    time_reference::Union{Nothing, TimeReference}
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
    time_reference::TimeReferenceArg=INFERRED,
)
    return Scenarios{eltype(data), ndims(data)}(
        _utc_datetime(initial),
        resolution,
        horizon,
        interval,
        Int(count),
        size(data, 1),
        data isa Array ? data : Array(data),
        String(name),
        _maybe_string(application_data),
        _declared_element_type(element_type, data),
        _maybe_string(units),
        _maybe_string(quantity_kind),
        _unit_system(unit_system),
        _maybe_string(component_field),
        _resolved_time_reference(time_reference, initial),
    )
end

"""
    DeterministicSingleTimeSeries{T, N}

Marker type naming a forecast derived from a `SingleTimeSeries` via
`transform_single_time_series!` (mirrors the InfrastructureSystems.jl type). It
is never constructed or added directly and has no materialized struct.

**You do not normally ask for this type.** Whether a forecast is stored densely
or derived from a `SingleTimeSeries` is a storage detail:
[`read_by_id`](@ref) returns a [`Deterministic`] either way, and a
`time_series_type=Deterministic` filter matches both. This type exists so the
detail is *inspectable* — it surfaces as the `time_series_type` of every catalog
row from `list_metadata` / `list_metadata_by_ids` / `get_metadata_by_id`, and
filtering on it narrows a query to the derived forecasts alone (e.g. to audit
which of a store's forecasts are synthetic).

It is parameterized only so that a metadata row's `time_series_type` carries
`{T, N}` for *every* stored type. `{T, N}` describes the `Deterministic` the row
reads back as — never the source `SingleTimeSeries`, whose array it shares —
so a derived view of a scalar `SingleTimeSeries{Float64, 1}` is a
`DeterministicSingleTimeSeries{Float64, 2}`, matching its `Deterministic` read.
Write the bare `DeterministicSingleTimeSeries` when naming it as a request or a
filter; parameters are ignored there.
"""
abstract type DeterministicSingleTimeSeries{T, N} end

# Every type accepted as a *requested* forecast type. Internal: it exists for
# method bounds only, is not exported, and is not part of the public surface —
# callers name a concrete type, and `Deterministic` already spans both
# deterministic storage forms (see `_forecast_result_type`).
const _ForecastRequest = Union{
    Deterministic, DeterministicSingleTimeSeries, Probabilistic, Scenarios
}
