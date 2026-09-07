"""
Forecast uncertainty as quantiles: Probabilistic.

A `Probabilistic` forecast is a `Deterministic` with one more axis. Instead of a
single number per (window, timestep) it carries a whole distribution, sampled at
named percentiles — the p10/p50/p90 band a renewable forecast vendor delivers,
or the quantiles a resource-adequacy study needs to size reserves.

Percentiles are fractions in [0, 1] by convention. They are stored on the
series, so a reader gets the labels back with the values and never has to assume
which quantile a column is.

The stored array is `(percentiles, horizon steps, window count)` — the
percentile axis is prepended to the deterministic layout. Two windows tile a
daylight day here: issued at 06:00 and 12:00, six hours each, `interval` equal to
`horizon` so nothing overlaps.

The physical story is the one every solar forecaster knows. The band is narrow at
dawn and dusk, where output is near zero and near-certain, and opens up through
the morning ramp as a cloud field starts to decide how much of a 60 MW plant
shows up. Around solar noon it *narrows again on the upside only*: the p50 is
already close to clear-sky output, and a plant cannot exceed its rating, so the
uncertainty is one-sided. Clamping the band at 1.0 per-unit is not a modelling
convenience — it is that ceiling.
"""

using Dates
using DataFrames
using InfraStore

include("shared.jl")

const RESOLUTION = Hour(1)
const HORIZON = Hour(6)
const INTERVAL = Hour(6)
const COUNT = 2
const PERCENTILES = [0.1, 0.5, 0.9]
const FIRST_ISSUE_HOUR = 6

horizon_steps = Int(HORIZON / RESOLUTION)
availability = solar_availability()

# The p50 is the median — the level the hour is as likely to beat as to miss,
# which is not in general the mean. The band around it scales with how much
# output is at stake and widens with lead time into the window.
values = Array{Float64}(undef, length(PERCENTILES), horizon_steps, COUNT)
for window in 1:COUNT, step in 1:horizon_steps
    hour = FIRST_ISSUE_HOUR + (window - 1) * Int(INTERVAL / RESOLUTION) + (step - 1)
    median = availability[hour % 24 + 1]
    spread = 0.35 * median * (1.0 + 0.15 * (step - 1))
    for (index, percentile) in enumerate(PERCENTILES)
        # A symmetric band about the median, clamped to the physical [0, 1]
        # range — a plant cannot generate more than its rating or less than
        # nothing.
        offset = (percentile - 0.5) * 2.0 * spread
        values[index, step, window] = round(clamp(median + offset, 0.0, 1.0), digits = 4)
    end
end

Store(in_memory = true) do store
    forecast = Probabilistic(
        DAY_START + Hour(FIRST_ISSUE_HOUR),
        RESOLUTION,
        HORIZON,
        INTERVAL,
        COUNT,
        PERCENTILES,
        values,
        "max_active_power";
        units = "per_unit",
        quantity_kind = "ActivePower",
        unit_system = ComponentBase,
        # What bounds a plant's output is usually its rating rather than a
        # separate max-power field, so that is the field these values drive.
        component_field = "rating",
    )
    series_id =
        add_time_series!(store, SUNDANCE_PV.id, SUNDANCE_PV.type, Component, forecast)
    println("added $(SUNDANCE_PV.type) '$(SUNDANCE_PV.name)': id=$series_id")

    read_back = read_by_id(store, series_id)
    println("percentiles=$(read_back.percentiles) count=$(read_back.count)")

    # One row per forecast hour, with the band the study would plan against. Back to
    # MW, since the values are per-unit of the plant's rating.
    mw(index, step, window) =
        round(read_back.data[index, step, window] * SUNDANCE_PV.base_power_mw, digits = 2)

    cells = [(w, s) for w in 1:COUNT for s in 1:horizon_steps]
    band = DataFrame(
        issue_time = [window_issue_times(read_back)[w] for (w, _) in cells],
        timestamp = [window_timestamps(read_back, w)[s] for (w, s) in cells],
        p10_mw = [mw(1, s, w) for (w, s) in cells],
        p50_mw = [mw(2, s, w) for (w, s) in cells],
        p90_mw = [mw(3, s, w) for (w, s) in cells],
    )
    band.band_mw = round.(band.p90_mw .- band.p10_mw, digits = 2)

    println("\np10/p50/p90 availability of a 60 MW plant, by forecast hour")
    println(sort(band, :timestamp))
end
