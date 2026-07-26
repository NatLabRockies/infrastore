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
            (:infrastore_key_eq, lib_path()),
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
            (:infrastore_key_identity_hash, lib_path()),
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
