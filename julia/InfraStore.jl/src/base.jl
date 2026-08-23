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
        @ccall lib_path().infrastore_key_eq(
            a::Ptr{Cvoid}, b::Ptr{Cvoid}, out::Ref{Bool}
        )::Int32
    )
    return out[]
end

function Base.hash(k::TimeSeriesKey, h::UInt)
    out = Ref{UInt64}(0)
    _check(
        @ccall lib_path().infrastore_key_identity_hash(
            k::Ptr{Cvoid}, out::Ref{UInt64}
        )::Int32
    )
    return hash(out[], h)
end

function Base.show(io::IO, k::TimeSeriesKey)
    k.handle == C_NULL && return print(io, "TimeSeriesKey(freed)")
    i = key_info(k)
    return print(
        io,
        "TimeSeriesKey($(i.time_series_type) name=$(repr(i.name)) " *
        "owner_id=$(i.owner_id) owner_category=$(i.owner_category))",
    )
end

function Base.show(io::IO, s::Store)
    s.handle == C_NULL && return print(io, "Store(closed)")
    return print(io, "Store(read_only=$(read_only(s)))")
end

# How a series' recorded spelling reads in a `show`. `"unspecified"` rather than
# `nothing`, matching how the other descriptors read.
_reference_label(::Nothing) = "unspecified"
_reference_label(r::TimeReference) = _time_reference_str(r)

function Base.show(io::IO, ts::SingleTimeSeries{T, N}) where {T, N}
    return print(
        io,
        "SingleTimeSeries{$T,$N}(name=$(repr(ts.name)) length=$(size(ts.data, 1)) " *
        "initial_timestamp=$(ts.initial_timestamp) resolution=$(ts.resolution) " *
        "time_reference=$(_reference_label(ts.time_reference)))",
    )
end

function Base.show(io::IO, ts::NonSequentialTimeSeries{T, N}) where {T, N}
    return print(
        io,
        "NonSequentialTimeSeries{$T,$N}(name=$(repr(ts.name)) " *
        "length=$(size(ts.data, 1)) " *
        "time_reference=$(_reference_label(ts.time_reference)))",
    )
end

# ---- Fusing an instant back together with its spelling ---------------------
#
# The convenience forms of `zoned_timestamp`, which take the two halves off a
# read result so a caller does not have to. The two-argument method they all
# reach lives in `InfraStoreTimeZonesExt`; without `using TimeZones` the
# fallback in `lib.jl` says so.

function zoned_timestamp(ts::SingleTimeSeries)
    return zoned_timestamp(ts.initial_timestamp, ts.time_reference)
end
function zoned_timestamp(ts::Deterministic)
    return zoned_timestamp(ts.initial_timestamp, ts.time_reference)
end
function zoned_timestamp(ts::Probabilistic)
    return zoned_timestamp(ts.initial_timestamp, ts.time_reference)
end
zoned_timestamp(ts::Scenarios) =
    zoned_timestamp(ts.initial_timestamp, ts.time_reference)

function zoned_timestamp(m::TimeSeriesMetadata)
    m.initial_timestamp === nothing && throw(
        InvalidParameterError(
            "this metadata row has no initial_timestamp to render " *
            "(a NonSequentialTimeSeries carries an explicit vector instead)",
        ),
    )
    return zoned_timestamp(m.initial_timestamp, m.time_reference)
end

"""
    zoned_timestamps(series) -> Vector{ZonedDateTime}

Every timestamp of a [`NonSequentialTimeSeries`](@ref), fused with the spelling
the series recorded. Requires `using TimeZones`; see
[`zoned_timestamp`](@ref).
"""
function zoned_timestamps(ts::NonSequentialTimeSeries)
    return [zoned_timestamp(t, ts.time_reference) for t in ts.timestamps]
end

for FT in (:Deterministic, :Probabilistic, :Scenarios)
    @eval function Base.show(io::IO, ts::$FT{T, N}) where {T, N}
        return print(
            io,
            $("$FT") *
            "{$T,$N}(name=$(repr(ts.name)) count=$(ts.count) " *
            "horizon=$(ts.horizon) interval=$(ts.interval))",
        )
    end
end

# Container interface: full delegation to `data` (element count, not time
# steps, for multi-dimensional values — consistent with `iterate`/`getindex`).
for ST in (:SingleTimeSeries, :NonSequentialTimeSeries)
    @eval begin
        Base.length(ts::$ST) = length(ts.data)
        Base.eltype(::Type{$ST{T, N}}) where {T, N} = T
        Base.getindex(ts::$ST, i...) = getindex(ts.data, i...)
        Base.iterate(ts::$ST) = iterate(ts.data)
        Base.iterate(ts::$ST, state) = iterate(ts.data, state)
    end
end

# Forecast length is the number of forecast windows.
Base.length(ts::Deterministic) = ts.count
Base.length(ts::Probabilistic) = ts.count
Base.length(ts::Scenarios) = ts.count
