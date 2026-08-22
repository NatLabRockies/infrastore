"""
    InfraStoreTimeZonesExt

Teaches InfraStore to accept a `TimeZones.ZonedDateTime` wherever it accepts a
`DateTime`.

A weak dependency, so TimeZones (and the tz database it installs) is a cost only
for callers who want it. Loading `TimeZones` is what activates this: the single
method below is what `InfraStore._utc_datetime`'s fallback tells a caller to go
and load.

The conversion direction is deliberate. Input widens -- a `ZonedDateTime` names
an instant on its own, which is strictly better than the UTC-by-convention
reading a bare `DateTime` gets -- while *reads* still return a `DateTime`.
Returning a `ZonedDateTime` would mean inventing a zone the store never recorded,
and would change the type every existing caller already destructures.
"""
module InfraStoreTimeZonesExt

using Dates: DateTime, UTC
using InfraStore: InfraStore
using TimeZones: ZonedDateTime

# `DateTime(zdt, UTC)` is TimeZones' own spelling for "the UTC wall clock of this
# instant", which is exactly what `_to_unix_ms` counts from the epoch. `UTC` here
# is `Dates.UTC`, the zone marker Julia's own `now(UTC)` takes -- TimeZones
# defines the method, `Dates` defines the marker.
InfraStore._utc_datetime(zdt::ZonedDateTime) = DateTime(zdt, UTC)

end
