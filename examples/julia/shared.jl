"""
Shared components, profiles, and display helpers for the runnable examples.

Every example describes the same three-component power system:

    owner_id  owner_type          name           base power
    101       ThermalGenerator    solitude       100 MW gas combustion turbine
    102       SolarPlant          sundance_pv     60 MW utility-scale solar plant
    201       Load                bus_a_load     150 MW distribution feeder

infrastore stores time series, not components. A series is filed against the
component that owns it, and the store never resolves that owner into anything —
your modeling application owns the components, and infrastore owns the arrays
and the catalog rows pointing at them.

The owner's identity is the *pair* `(owner_id, owner_category)`. `owner_id` is
whatever integer id your application gives a component, and `owner_category`
distinguishes a `Component` from a `SupplementalAttribute` — the two id streams
are independent, so one integer can name one of each.

`owner_type` is **not** part of that identity: it is descriptive, and reusing an
id across two component types will collide no matter how the type strings
differ. What it is good for is retrieval — it is recorded on every row, and both
`list_metadata(store; owner_type = ...)` and `list_owner_types` read it — so a
concrete type name earns its keep, where an abstract one lumps every generator
together.

Three descriptors travel with every series and are worth setting even though the
store never acts on them:

  * `units`          — the label for the values, e.g. "MW", "\$/MWh".
  * `quantity_kind`  — what is being measured, e.g. "ActivePower". Dimensional
                       analysis cannot separate active from reactive power, so
                       this is the only record of which one you have — and the
                       only record of what per-unit values measure.
  * `unit_system`    — `NaturalUnits` if the values are already MW/MVar, or
                       `ComponentBase` if they are per-unit against the owning
                       component's own base power. Nothing rescales them: the
                       label says which basis the modeler put the values on, and
                       converting a per-unit series back to MW needs the base
                       power, which lives on the component, not here.

`component_field` names the field on the owning component whose value these
values are the time-varying form of, where `name` only says which series this
is. They often coincide, and deliberately do not have to: what bounds a
component's output is called something different on each kind of component —
a plant's ceiling is usually its `rating`, a load's its `max_active_power` —
while a set of series driving all of them may share one name.
"""

using Dates
using DataFrames
using InfraStore

"""
A component in the consumer's object model, as infrastore sees it.

Named `SystemComponent` rather than `Component` because `InfraStore` exports
`Component` — the `OwnerCategory` value every write passes — and a struct of
that name here would shadow it.
"""
struct SystemComponent
    id::Int
    type::String
    name::String
    # The device's own base power in MW — what a `ComponentBase` series is
    # per-unit *of*, and not the same thing as a rating, which many models store
    # per-unit on this base.
    base_power_mw::Float64
end

const SOLITUDE = SystemComponent(101, "ThermalGenerator", "solitude", 100.0)
const SUNDANCE_PV = SystemComponent(102, "SolarPlant", "sundance_pv", 60.0)
const BUS_A_LOAD = SystemComponent(201, "Load", "bus_a_load", 150.0)

const COMPONENTS = [SOLITUDE, SUNDANCE_PV, BUS_A_LOAD]

# A summer weekday, on the hour. Julia's `DateTime` carries no zone, so the store
# records these as a **wall clock** — the instant is the fields as written, and
# the spelling is recorded as `ZonelessReference()` rather than silently
# relabelled UTC. That is a real declaration: query bounds must match a series'
# spelling, so a zoneless series is queried with zoneless bounds.
#
# `using TimeZones` and a `ZonedDateTime` is the other way in, accepted anywhere
# a `DateTime` is; the examples stay on the zoneless path to keep the dependency
# out. See the guide's "Time and resolution conversions" for the difference.
const DAY_START = DateTime(2030, 7, 1)
const HOURS_PER_DAY = 24

"""Hour-of-day for `count` consecutive hourly steps starting at `DAY_START`."""
hours(count::Integer = HOURS_PER_DAY) = [(h - 1) % 24 for h in 1:count]

"""
    solar_availability([count])

Per-unit PV output over a day: zero overnight, peaking near solar noon.

Per-unit on the plant's own base power — a real per-device quantity that happens
to be dimensionless, which `unit_system = ComponentBase` records. That is
different from a normalized 0–1 *shape* meant to be multiplied by a rating at
read time. The store has no notion of a scaling factor and will not apply one,
so if your values need scaling to mean anything, scale them before writing.
"""
function solar_availability(count::Integer = HOURS_PER_DAY)
    hour = hours(count)
    daylight = @. sin((hour - 6.0) * pi / 12.0)
    return round.(max.(daylight, 0.0) .* 0.95, digits = 4)
end

"""A double-peaked summer feeder load in MW: overnight trough, evening peak."""
function feeder_load_mw(peak_mw::Real, count::Integer = HOURS_PER_DAY)
    hour = hours(count)
    shape = @. (
        0.58 +
        0.15 * sin((hour - 8.0) * pi / 12.0) +
        0.32 * exp(-(((hour - 19.0) / 3.0)^2))
    )
    return round.(peak_mw .* clamp.(shape, 0.0, 1.0), digits = 2)
end

"""
    thermal_capacity_mw(base_power_mw, [count])

Ambient-derated capacity of a gas turbine in MW.

A combustion turbine loses output as the air warms, so its usable capacity is a
time series even though its nameplate rating is a scalar.
"""
function thermal_capacity_mw(base_power_mw::Real, count::Integer = HOURS_PER_DAY)
    hour = hours(count)
    derate = @. 0.08 * exp(-(((hour - 16.0) / 4.0)^2))
    return round.(base_power_mw .* (1.0 .- derate), digits = 2)
end

"""
    pretty(period)

A stored `Period` rendered in the largest units that divide it exactly.

Every period comes back from the store as a `Millisecond` — the wire carries a
millisecond count, which is the resolution every instant is held to — so an
hourly resolution reads back as `3600000 milliseconds` unless it is canonicalized.
"""
pretty(period::Period) = Dates.canonicalize(Dates.CompoundPeriod(period))
pretty(::Nothing) = nothing

"""
    static_frame(series)

Timestamp/value table for any static series.

`timestamps` materializes a `SingleTimeSeries`' grid and hands back the stored
vector for the two irregular types, so one helper covers all three.
"""
static_frame(series) = DataFrame(timestamp = timestamps(series), value = series.data)

"""
    window_issue_times(forecast)

The instant each of a forecast's windows was issued: `initial_timestamp` stepped
by `interval`, one per window.

A forecast is a stack of windows rather than one timeline, and the issue time is
what identifies a window — the vintage of every value inside it.
"""
function window_issue_times(forecast)
    return [forecast.initial_timestamp + (w - 1) * forecast.interval
            for w in 1:forecast.count]
end

"""
    window_timestamps(forecast, window)

The instants of one window's steps: its issue time stepped by `resolution`.

`window` is 1-based, like every other Julia index.
"""
function window_timestamps(forecast, window::Integer)
    start = forecast.initial_timestamp + (window - 1) * forecast.interval
    steps = size(forecast.data)[end - 1]
    return [start + (s - 1) * forecast.resolution for s in 1:steps]
end

"""
    deterministic_frame(forecast)

One row per (window, timestep) of a deterministic forecast, keyed by issue time.

Flattening keeps the vintage of every value, which is the whole reason a
forecast cannot be collapsed into a single series: an overlapped hour holds one
value per window that forecast it.
"""
function deterministic_frame(forecast)
    horizon_steps, window_count = size(forecast.data)
    return DataFrame(
        issue_time = [window_issue_times(forecast)[w]
                      for s in 1:horizon_steps for w in 1:window_count],
        timestamp = [window_timestamps(forecast, w)[s]
                     for s in 1:horizon_steps for w in 1:window_count],
        value = [forecast.data[s, w]
                 for s in 1:horizon_steps for w in 1:window_count],
    )
end
