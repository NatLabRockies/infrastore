# ---- Base interface --------------------------------------------------------
#
# The value types delegate their container interface to the wrapped `data`
# array; forecast `length` is the window count.

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

function Base.show(io::IO, ts::PersistentTimeSeries{T, N}) where {T, N}
    return print(
        io,
        "PersistentTimeSeries{$T,$N}(name=$(repr(ts.name)) " *
        "breakpoints=$(size(ts.data, 1)) " *
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
            "(a NonSequentialTimeSeries or PersistentTimeSeries carries an " *
            "explicit vector instead)",
        ),
    )
    return zoned_timestamp(m.initial_timestamp, m.time_reference)
end

"""
    zoned_timestamps(series) -> Vector{ZonedDateTime}

Every timestamp of a static series — every entry of a
[`NonSequentialTimeSeries`](@ref), every breakpoint of a
[`PersistentTimeSeries`](@ref), every grid point of a
[`SingleTimeSeries`](@ref) — fused with the spelling the series recorded.
The zoneless counterpart is [`timestamps`](@ref). Requires `using TimeZones`;
see [`zoned_timestamp`](@ref).
"""
function zoned_timestamps(ts::NonSequentialTimeSeries)
    return [zoned_timestamp(t, ts.time_reference) for t in ts.timestamps]
end

function zoned_timestamps(ts::PersistentTimeSeries)
    return [zoned_timestamp(t, ts.time_reference) for t in ts.timestamps]
end

"""
    timestamps(series) -> Vector{DateTime}

Every timestamp of a static series, in order.

For a [`NonSequentialTimeSeries`](@ref) or [`PersistentTimeSeries`](@ref) this is
the stored vector; for a [`SingleTimeSeries`](@ref) it materializes the grid,
`initial_timestamp + k * resolution`. The one method that is not a field access
is the one that matters: a `Month` or `Year` resolution steps on the calendar, so
a caller multiplying a fixed span by the index gets a monthly series wrong.

The instants are the ones stored; the spelling beside them is `time_reference`.
[`zoned_timestamps`](@ref) fuses the two, and needs `using TimeZones`.

```julia
ts = SingleTimeSeries(DateTime(2024, 1, 31), Month(1), [1.0, 2.0, 3.0], "monthly")
timestamps(ts)  # 2024-01-31, 2024-02-29, 2024-03-31
```
"""
function timestamps(ts::SingleTimeSeries)
    # `size(data, 1)`, not `length`: the container interface counts elements, and
    # a multidimensional per-step value has more of them than there are steps.
    steps = size(ts.data, 1)
    steps == 0 && return DateTime[]
    # Through the core rather than `ts.initial_timestamp + k * ts.resolution`.
    # Julia is the one binding whose date library has calendar arithmetic of its
    # own, and TimeZones.jl overloads it to step a *local* clock -- which the
    # core deliberately does not. Computing here would be a second implementation
    # of what instants a series contains, agreeing with the core only by luck.
    out_len = Ref{UInt64}(0)
    initial = _to_unix_ms(ts.initial_timestamp)
    iso = _period_to_iso(ts.resolution)
    _check(
        @ccall libinfrastore.infrastore_grid_timestamps(
            initial::Int64, iso::Cstring, UInt64(steps)::UInt64,
            C_NULL::Ptr{Int64}, UInt64(0)::UInt64, out_len::Ref{UInt64},
        )::Int32
    )
    millis = Vector{Int64}(undef, Int(out_len[]))
    _check(
        @ccall libinfrastore.infrastore_grid_timestamps(
            initial::Int64, iso::Cstring, UInt64(steps)::UInt64,
            millis::Ptr{Int64}, UInt64(length(millis))::UInt64, out_len::Ref{UInt64},
        )::Int32
    )
    return [_from_unix_ms(ms) for ms in millis]
end

timestamps(ts::NonSequentialTimeSeries) = copy(ts.timestamps)
timestamps(ts::PersistentTimeSeries) = copy(ts.timestamps)

function zoned_timestamps(ts::SingleTimeSeries)
    return [zoned_timestamp(t, ts.time_reference) for t in timestamps(ts)]
end

"""
    value_at(ts::PersistentTimeSeries, at) -> value

The value in force at `at`.

A step function is defined at *every* instant from its first breakpoint onward,
so this is the series' value at `at` in the ordinary sense, not an approximation
of one: between breakpoints the previous value is carried forward, and past the
last breakpoint the last value holds indefinitely. The single error is an `at`
strictly *before* the first breakpoint, where no value was ever declared — an
`InvalidParameterError`, never a clamp.

A scalar series returns a scalar; one with a shaped per-step element returns that
step as an array (a copy, so mutating it leaves the series alone). `at` is a
`DateTime` or, with `using TimeZones`, a `ZonedDateTime`, and must be spelled the
way the series' breakpoints are. [`index_at`](@ref) and [`breakpoint_at`](@ref)
locate the row the value came from.

```julia
curve = PersistentTimeSeries(
    [DateTime(2024, 1), DateTime(2024, 4), DateTime(2024, 7)],
    [10.0, 40.0, 70.0],
    "gas",
)
value_at(curve, DateTime(2024, 5, 17))  # 40.0, carried forward from April
```
"""
function value_at(ts::PersistentTimeSeries, at)
    i = index_at(ts, at)
    ndims(ts.data) == 1 && return ts.data[i]
    return copy(selectdim(ts.data, 1, i))
end

"""
    index_at(ts::PersistentTimeSeries, at) -> Int

The 1-based index into `ts.timestamps` and the first dimension of `ts.data` of
the breakpoint governing `at` — the greatest breakpoint `<= at`.

[`value_at`](@ref) is the usual way to ask; this is for a caller that wants the
row itself, to index a parallel array of its own. Errors like `value_at`.
"""
function index_at(ts::PersistentTimeSeries, at)
    _check_point_spelling(ts.time_reference, at, "this series")
    t = _utc_datetime(at)
    isempty(ts.timestamps) && throw(
        InvalidParameterError(
            "PersistentTimeSeries \"$(ts.name)\" has no breakpoints, so it has no " *
            "value at $t",
        ),
    )
    # `searchsortedlast` is the greatest index whose breakpoint is `<= t`, which
    # is the carried-forward rule exactly; the vector is strictly increasing
    # (the constructor checks it), so the search is well defined.
    i = searchsortedlast(ts.timestamps, t)
    i == 0 && throw(
        InvalidParameterError(
            "PersistentTimeSeries \"$(ts.name)\" has no value at $t: it is before " *
            "the first breakpoint $(first(ts.timestamps)), where a step function " *
            "is undefined",
        ),
    )
    return i
end

"""
    breakpoint_at(ts::PersistentTimeSeries, at) -> DateTime

The breakpoint governing `at` — the instant from which the value at `at` has been
in force. Equal to `at` exactly when `at` is itself a breakpoint; errors like
[`value_at`](@ref).

The instant is the one stored; the spelling beside it is `ts.time_reference`, and
[`zoned_timestamp`](@ref) fuses the two.
"""
breakpoint_at(ts::PersistentTimeSeries, at) = ts.timestamps[index_at(ts, at)]

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
for ST in (:SingleTimeSeries, :NonSequentialTimeSeries, :PersistentTimeSeries)
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
