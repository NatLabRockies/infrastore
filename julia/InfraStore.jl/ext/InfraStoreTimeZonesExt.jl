"""
    InfraStoreTimeZonesExt

Teaches InfraStore to accept a `TimeZones.ZonedDateTime` wherever it accepts a
`DateTime`, and to hand one back on request.

A weak dependency, so TimeZones (and the tz database it installs) is a cost only
for callers who want it. Loading `TimeZones` is what activates this: the methods
below are what `InfraStore._utc_datetime`'s fallback tells a caller to go and
load.

The conversion direction is deliberate. Input widens -- a `ZonedDateTime` names
an instant on its own, which is strictly better than the wall-clock reading a
bare `DateTime` gets -- while *reads* still return a `DateTime` holding the
instant, with the spelling beside it as a
[`InfraStore.TimeReference`](@ref). Returning a `ZonedDateTime` from a read was
rejected on three counts:

  * reading is not opt-in the way writing is, so the return type would depend on
    whether the caller happened to load this package;
  * `zdt == dt` and `zdt < dt` both raise in Julia, so widening the type turns
    working comparisons against a `DateTime` literal into runtime errors;
  * the struct fields are concrete, and they cannot name `ZonedDateTime` at all
    unless this extension is loaded.

[`InfraStore.zoned_timestamp`](@ref) below is the opt-in way to put the two
pieces back together, and it is lossless: the instant plus the zone name
reconstructs the exact value written, including which side of a fall-back hour it
was on.
"""
module InfraStoreTimeZonesExt

using Dates: DateTime, UTC
using InfraStore: InfraStore
using TimeZones: FixedTimeZone, TimeZone, VariableTimeZone, ZonedDateTime

# `DateTime(zdt, UTC)` is TimeZones' own spelling for "the UTC wall clock of this
# instant", which is exactly what `_to_unix_ms` counts from the epoch. `UTC` here
# is `Dates.UTC`, the zone marker Julia's own `now(UTC)` takes -- TimeZones
# defines the method, `Dates` defines the marker.
InfraStore._utc_datetime(zdt::ZonedDateTime) = DateTime(zdt, UTC)

# Which spelling a `ZonedDateTime` records.
#
# The two `FixedTimeZone` cases split on the zone's *name*, not on its offset:
# `tz"UTC"` and `tz"+00:00"` place every instant identically forever, and the
# whole point of recording a spelling is that the catalog can still tell them
# apart. A `VariableTimeZone` carries an IANA name, which is the one spelling
# that renders a year-long Denver series correctly on both sides of every
# transition -- a fixed offset would be an hour wrong after March.
function InfraStore._time_reference_of(zdt::ZonedDateTime)
    return _reference_for(zdt.timezone)
end

_reference_for(tz::VariableTimeZone) = InfraStore.ZoneReference(string(tz.name))

function _reference_for(tz::FixedTimeZone)
    name = string(tz.name)
    name == "UTC" && return InfraStore.UTCReference()
    seconds = tz.offset.std.value + tz.offset.dst.value
    rem(seconds, 60) == 0 || throw(
        InfraStore.InvalidParameterError(
            "the timestamp's UTC offset is $seconds seconds, which is not a whole " *
            "number of minutes; the store records an offset in minutes, so this " *
            "spelling cannot be stored faithfully",
        ),
    )
    return InfraStore.FixedOffsetReference(seconds ÷ 60)
end

# The fusing direction: instant + recorded spelling -> `ZonedDateTime`.
#
# This is the total direction. One instant maps to exactly one wall clock in a
# named zone, so nothing here has to choose between two candidates -- which is
# why a named zone is safe to record at all, and why this needs no `fold`-style
# disambiguator.
#
# One method per concrete spelling rather than one on the abstract type: the
# package already defines the `::TimeReference` fallback that tells a caller to
# load TimeZones, and an extension may only *add* methods more specific than
# what it found, never overwrite one.
function InfraStore.zoned_timestamp(instant::DateTime, ::InfraStore.UTCReference)
    return ZonedDateTime(instant, TimeZone("UTC"); from_utc=true)
end

function InfraStore.zoned_timestamp(
    instant::DateTime, reference::InfraStore.FixedOffsetReference
)
    offset = FixedTimeZone(
        InfraStore._time_reference_str(reference), 60 * reference.minutes
    )
    return ZonedDateTime(instant, offset; from_utc=true)
end

function InfraStore.zoned_timestamp(instant::DateTime, reference::InfraStore.ZoneReference)
    return ZonedDateTime(instant, TimeZone(reference.name); from_utc=true)
end

function InfraStore.zoned_timestamp(::DateTime, ::InfraStore.ZonelessReference)
    return throw(
        InfraStore.InvalidParameterError(
            "this series is zoneless: its timestamps are wall clocks and name no " *
            "instant, so there is no ZonedDateTime to build. Read them as the " *
            "DateTime values they are.",
        ),
    )
end

# The tz database this interpreter actually has, for the write-path audit. Kept
# here rather than in the package proper because only this extension has one.
InfraStore._zone_is_known(name::AbstractString) =
    try
        TimeZone(String(name))
        true
    catch
        false
    end

end
