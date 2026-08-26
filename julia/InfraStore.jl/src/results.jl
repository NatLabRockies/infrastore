# ---- Result types ---------------------------------------------------------
#
# The catalog / summary / metadata queries below return these structs. They are
# plain immutable value types: read fields with `x.field`, compare with `==`,
# use them as `Dict` keys. Fields that do not apply to a row's time series type
# are `nothing` (e.g. `horizon` on a `SingleTimeSeries` row).
#
# `time_series_type` fields hold the Julia type (`SingleTimeSeries`,
# `Deterministic`, ...), so they can be passed straight to `get_time_series`;
# Reader `dtype` fields hold the Julia element type (`Float64`, `Bool`, ...);
# metadata carries the logical `element_type` string instead.

"""
    TimeSeriesMetadata

The complete stored description of one time series association — the Julia
mirror of the Rust core's `TimeSeriesMetadata`, and the single metadata type of
this package. It is returned both by [`list_time_series`](@ref) (one per matching
association) and by the [`get_metadata`](@ref) family (one, addressed by key or
by attributes).

Every time series type shares the struct; the fields a type does not use are
`nothing` (`horizon` / `interval` / `count` on a static series, `length` on a
forecast whose array is described by its window geometry, `percentiles` on
anything but a `Probabilistic`).

- `owner_id`, `owner_category`, `owner_type`, `name`, `time_series_type` — the
  association's identity and its owner.
- `association_id` — the catalog row's store-assigned id; addresses the row via
  [`get_time_series_metadata`](@ref) without needing the full identity tuple.
- `data_hash` — the 32-byte content hash, ready for [`get_array_by_hash`](@ref)
  and [`count_array_references`](@ref); `bytes2hex` it for the display form.
- `initial_timestamp`, `resolution`, `length` — the static time grid.
- `horizon`, `interval`, `count` — the forecast window geometry.
- `percentiles` — the stored percentile vector of a `Probabilistic`.
- `element_type`, `element_shape` — the canonical element-type string (`"f64"`,
  `"tuple(3,f64)"`, `"piecewise_linear"`, …) and the per-timestep shape (an
  empty tuple for scalar elements; for a forecast, the stored array's trailing
  dims after its first axis).
- `features` — the feature dictionary (empty when none).
- `units`, `quantity_kind`, `component_field`, `application_data` — `nothing`
  when unset. `component_field` names the field on the owning component whose
  value these values are the time-varying form of, e.g. `"max_active_power"`.
- `unit_system` — a [`UnitSystem`](@ref), or `nothing` when unspecified (which
  is not the same as `NaturalUnits`).
- `time_reference` — a [`TimeReference`](@ref) recording how `initial_timestamp`
  and the row's timestamps were *spelled*, or `nothing` when unspecified (which
  is not a claim they were written as UTC). `initial_timestamp` is still the
  instant; `using TimeZones` adds [`zoned_timestamp`](@ref) to fuse the two.
"""
struct TimeSeriesMetadata
    owner_id::Int64
    owner_type::String
    owner_category::OwnerCategory
    association_id::Int64
    time_series_type::Type
    name::String
    data_hash::Vector{UInt8}
    initial_timestamp::Union{Nothing, DateTime}
    resolution::Union{Nothing, Period}
    horizon::Union{Nothing, Period}
    interval::Union{Nothing, Period}
    count::Union{Nothing, Int}
    length::Union{Nothing, Int}
    percentiles::Union{Nothing, Vector{Float64}}
    element_type::String
    element_shape::Tuple{Vararg{Int}}
    features::Dict{String, Any}
    units::Union{Nothing, String}
    quantity_kind::Union{Nothing, String}
    unit_system::Union{Nothing, UnitSystem}
    time_reference::Union{Nothing, TimeReference}
    component_field::Union{Nothing, String}
    application_data::Union{Nothing, String}
end

"""
    KeyInfo

The attributes an opaque [`TimeSeriesKey`] carries, as returned by
[`key_info`](@ref): the identity a series is addressed by. `resolution` is
`nothing` for types that have none (a `NonSequentialTimeSeries`).
"""
struct KeyInfo
    owner_id::Int64
    owner_category::OwnerCategory
    name::String
    time_series_type::Type
    resolution::Union{Nothing, Period}
    features::Dict{String, Any}
end

"""
    KeyRow

One row of [`list_keys`](@ref): a key's identity plus the descriptive snapshot
recorded for it. `length` applies to static series, `horizon` / `interval` /
`count` to forecasts; the fields that do not apply to a row's
`time_series_type` are `nothing`. `time_reference` records how the row's
timestamps were spelled.

Physical storage detail (`data_hash`, `element_type`, `application_data`, `percentiles`) is not part
of a key — read it with [`list_time_series`](@ref), [`list_array_groups`](@ref),
or the `get_*_metadata` functions.
"""
struct KeyRow
    owner_id::Int64
    owner_category::OwnerCategory
    time_series_type::Type
    name::String
    initial_timestamp::Union{Nothing, DateTime}
    resolution::Union{Nothing, Period}
    length::Union{Nothing, Int}
    horizon::Union{Nothing, Period}
    interval::Union{Nothing, Period}
    count::Union{Nothing, Int}
    features::Dict{String, Any}
    "How the row's timestamps were spelled (a [`TimeReference`](@ref)), or `nothing` for unspecified. Descriptive, so it is part of the snapshot and never of key equality."
    time_reference::Union{Nothing, TimeReference}
end

"""
    ArrayGroupRow

One row of [`list_array_groups`](@ref): every [`KeyRow`] field plus `data_hash`,
the 32-byte content hash of the array the row resolves to. Rows that share a
stored array share their `data_hash`, so grouping by it (a `Vector{UInt8}` hashes
and compares by content, so it works directly as a `Dict` key) finds the series
that share their underlying data.
"""
struct ArrayGroupRow
    owner_id::Int64
    owner_category::OwnerCategory
    time_series_type::Type
    name::String
    initial_timestamp::Union{Nothing, DateTime}
    resolution::Union{Nothing, Period}
    length::Union{Nothing, Int}
    horizon::Union{Nothing, Period}
    interval::Union{Nothing, Period}
    count::Union{Nothing, Int}
    features::Dict{String, Any}
    "How the row's timestamps were spelled (a [`TimeReference`](@ref)), or `nothing` for unspecified."
    time_reference::Union{Nothing, TimeReference}
    data_hash::Vector{UInt8}
end

"""
    TimeSeriesCounts

Association counts from [`get_counts`](@ref).
"""
struct TimeSeriesCounts
    components_with_time_series::Int
    static_time_series::Int
    forecasts::Int
end

"""
    TimeSeriesCountsDetailed

Distinct owners per category and distinct stored arrays per kind, from
[`time_series_counts`](@ref). Arrays shared by content count once.
"""
struct TimeSeriesCountsDetailed
    components_with_time_series::Int
    supplemental_attributes_with_time_series::Int
    static_time_series_count::Int
    forecast_count::Int
end

"""
    TimeSeriesTypeCount

One row of [`counts_by_type`](@ref): the number of associations stored under
`time_series_type` (the Julia type).
"""
struct TimeSeriesTypeCount
    time_series_type::Type
    count::Int
end

"""
    ArrayReferenceCounts

The result of [`count_array_references`](@ref): how many `SingleTimeSeries`
(`sts`) and `DeterministicSingleTimeSeries` (`dst`) associations reference one
stored array.
"""
struct ArrayReferenceCounts
    sts::Int
    dst::Int
end

"""
    StaticSummaryRow

One grouped static-series row from [`static_summary`](@ref): the distinct
`(owner_type, owner_category, time_series_type, name, initial_timestamp,
resolution, time_step_count)` group, with `count` associations in it.
"""
struct StaticSummaryRow
    owner_type::String
    owner_category::OwnerCategory
    time_series_type::Type
    name::String
    initial_timestamp::Union{Nothing, DateTime}
    resolution::Union{Nothing, Period}
    time_step_count::Union{Nothing, Int}
    count::Int
end

"""
    ForecastSummaryRow

One grouped forecast row from [`forecast_summary`](@ref): the distinct
`(owner_type, owner_category, time_series_type, name, initial_timestamp,
resolution, horizon, interval, window_count)` group, with `count` associations
in it.
"""
struct ForecastSummaryRow
    owner_type::String
    owner_category::OwnerCategory
    time_series_type::Type
    name::String
    initial_timestamp::Union{Nothing, DateTime}
    resolution::Union{Nothing, Period}
    horizon::Union{Nothing, Period}
    interval::Union{Nothing, Period}
    window_count::Union{Nothing, Int}
    count::Int
end

"""
    SupplementalAttributeTypeCount

One row of [`supplemental_attribute_counts_by_type`](@ref): the number of
attachments carrying attributes of `attribute_type`.
"""
struct SupplementalAttributeTypeCount
    attribute_type::String
    count::Int
end

"""
    SupplementalAttributeSummaryRow

One row of [`supplemental_attribute_summary`](@ref): the number of attachments
between components of `component_type` and attributes of `attribute_type`.
"""
struct SupplementalAttributeSummaryRow
    component_type::String
    attribute_type::String
    count::Int
end

"""
    ForecastParameters

The store's forecast configuration, from [`get_forecast_parameters`](@ref).
Every field is `nothing` when no forecast matches the query.
"""
struct ForecastParameters
    horizon::Union{Nothing, Period}
    interval::Union{Nothing, Period}
    count::Union{Nothing, Int}
    resolution::Union{Nothing, Period}
    initial_timestamp::Union{Nothing, DateTime}
end

"""
    StaticGrid

A shared static time grid: the valid timestamps are `initial_timestamp +
k·resolution` for `k in 0:length-1`. Returned by [`static_grid`](@ref) for a
[`StaticReader`], and by [`check_static_consistency`](@ref) once per resolution
present in the store.

`resolution` is `nothing` only for a `NonSequentialTimeSeries` reader, whose
timeline is an explicit list of instants rather than a grid — enumerate it with
[`static_timestamps`](@ref).

`time_reference` is the one spelling the axis carries: a cohort whose columns
agree reports their reference, one whose columns merely agree on naming instants
reports `UTCReference()`, and a cohort mixing zoneless with the rest never builds
at all. `nothing` means the cohort records no spelling, which is distinct from
`ZonelessReference()` — the positive claim that the timestamps are wall clocks.
It is `nothing` from [`check_static_consistency`](@ref), which reports grids
rather than readers.
"""
struct StaticGrid
    initial_timestamp::DateTime
    resolution::Union{Nothing, Period}
    length::Int
    time_reference::Union{Nothing, TimeReference}
end

"""Three-argument form: an axis with no recorded spelling."""
function StaticGrid(initial_timestamp, resolution, length)
    return StaticGrid(initial_timestamp, resolution, length, nothing)
end

"""
    ForecastTimeline

A [`ForecastReader`]'s window timeline, from [`forecast_timeline`](@ref): the
valid timestamps are `initial_timestamp + k·interval` for `k in 0:count-1`.

`time_reference` is the one spelling the timeline carries; see [`StaticGrid`](@ref).
"""
struct ForecastTimeline
    initial_timestamp::DateTime
    resolution::Period
    interval::Period
    count::Int
    time_reference::Union{Nothing, TimeReference}
end

"""Four-argument form: a timeline with no recorded spelling."""
function ForecastTimeline(initial_timestamp, resolution, interval, count)
    return ForecastTimeline(initial_timestamp, resolution, interval, count, nothing)
end

"""
    CompressionSettings

A store's on-disk compression policy, from [`get_compression`](@ref).
`compression` is `:deflate` or `:none`; `level` (0-9) and `shuffle` apply to
DEFLATE.
"""
struct CompressionSettings
    compression::Symbol
    level::Int
    shuffle::Bool
end

"""
    CompactionReport

What a [`compact!`](@ref) reclaimed, across both halves of the store.

For an on-disk store compaction rewrites the `.h5` file from the catalog's live
set, so these count things that were in the old file and are not in the new one:
`slots_reclaimed` are packed-column slots a removal had freed,
`datasets_dropped` are datasets nothing referenced any more, and
`bytes_reclaimed` is how much smaller the file got. `feature_sets_reclaimed` and
`timestamp_sets_reclaimed` are the catalog's orphaned content-addressed rows.

An in-memory store has no file to rewrite: `bytes_reclaimed` and
`datasets_dropped` are always `0` there.
"""
struct CompactionReport
    slots_reclaimed::Int
    datasets_dropped::Int
    feature_sets_reclaimed::Int
    timestamp_sets_reclaimed::Int
    bytes_reclaimed::Int
end

# The result types are compared and hashed by value. Julia's default `==` for an
# immutable struct is `===`-based, which would compare the `Vector` / `Dict`
# fields above by identity, so `==`, a matching `hash`, and a field-labelled
# `show` are generated for each of them here.
const _RESULT_TYPES = (
    :TimeSeriesMetadata,
    :KeyInfo,
    :KeyRow,
    :ArrayGroupRow,
    :TimeSeriesCounts,
    :TimeSeriesCountsDetailed,
    :TimeSeriesTypeCount,
    :ArrayReferenceCounts,
    :StaticSummaryRow,
    :ForecastSummaryRow,
    :SupplementalAttributeTypeCount,
    :SupplementalAttributeSummaryRow,
    :ForecastParameters,
    :StaticGrid,
    :ForecastTimeline,
    :CompressionSettings,
    :CompactionReport,
)

# 32-byte content hashes read as hex; everything else uses its own `repr`.
_field_repr(v) = repr(v)
_field_repr(v::Vector{UInt8}) = bytes2hex(v)

function _show_result(io::IO, x)
    print(io, nameof(typeof(x)), "(")
    for (i, field) in enumerate(fieldnames(typeof(x)))
        i > 1 && print(io, ", ")
        print(io, field, "=", _field_repr(getfield(x, field)))
    end
    return print(io, ")")
end

for T in _RESULT_TYPES
    @eval begin
        function Base.:(==)(a::$T, b::$T)
            return all(getfield(a, f) == getfield(b, f) for f in fieldnames($T))
        end
        function Base.hash(x::$T, h::UInt)
            for f in fieldnames($T)
                h = hash(getfield(x, f), h)
            end
            return hash($(QuoteNode(T)), h)
        end
        Base.show(io::IO, x::$T) = _show_result(io, x)
    end
end
