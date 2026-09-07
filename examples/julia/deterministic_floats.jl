"""
Rolling forecasts: resolution, horizon, interval, and count.

A forecast is not one timeline. It is a stack of *windows*, each one a forecast
made at a particular instant, looking a fixed distance ahead. Four fields
describe the stack, and getting them straight is most of understanding how
forecasts are stored:

    resolution  the step *within* a window        — 1 hour here
    horizon     how far ahead one window looks    — 4 hours
    interval    the step between window starts    — 1 hour
    count       how many windows are stored       — 6

Because `interval` (1h) is shorter than `horizon` (4h), the windows **overlap**:
18:00 is forecast four times, by the runs issued at 15:00, 16:00, 17:00 and
18:00, and those four values differ. That is the whole point of a rolling
forecast, and it is why a forecast cannot be flattened into a single series. A
day-ahead forecast is the same structure with `horizon = interval = 24h`, where
the windows tile instead of overlapping.

The stored array is shaped `(horizon steps, window count)`: each column is one
window, each row a step into it.

The example forecasts the feeder's load. Each successive run sees the evening
peak more clearly, so the forecast error shrinks as the peak approaches.
"""

using Dates
using DataFrames
using InfraStore

include("shared.jl")

const RESOLUTION = Hour(1)
const HORIZON = Hour(4)
const INTERVAL = Hour(1)
const COUNT = 6
const FIRST_ISSUE_HOUR = 15

# The load that actually materializes, hour by hour, as the ground truth the
# forecasts are approaching.
const ACTUAL = feeder_load_mw(BUS_A_LOAD.base_power_mw)
const PEAK_HOUR = argmax(ACTUAL) - 1        # 0-based hour of day

horizon_steps = Int(HORIZON / RESOLUTION)

# Forecast error grows with lead time — about 3% per hour ahead — and this
# forecaster runs high on the evening peak, the way a model that has not yet
# seen the cooling load turn over does. Deliberately deterministic rather than a
# random draw, so the numbers below are the same on every run.
windows = Matrix{Float64}(undef, horizon_steps, COUNT)
for window in 1:COUNT, step in 1:horizon_steps
    lead = step - 1
    target_hour = FIRST_ISSUE_HOUR + (window - 1) + lead
    truth = ACTUAL[target_hour % 24 + 1]
    windows[step, window] = round(truth * (1.0 + 0.03 * lead), digits = 2)
end

store = Store()

forecast = Deterministic(
    DAY_START + Hour(FIRST_ISSUE_HOUR),   # start of the first window
    RESOLUTION,
    HORIZON,
    INTERVAL,
    COUNT,
    windows,
    "max_active_power";
    units = "MW",
    quantity_kind = "ActivePower",
    unit_system = NaturalUnits,
    component_field = "max_active_power",
)
series_id = add_time_series!(store, BUS_A_LOAD.id, BUS_A_LOAD.type, Component, forecast)
println("added $(BUS_A_LOAD.type) '$(BUS_A_LOAD.name)': id=$series_id")

metadata = get_metadata_by_id(store, series_id)
# Periods come back as milliseconds — `pretty` is the examples' own display
# helper, not something the binding needs.
println("resolution=$(pretty(metadata.resolution)) horizon=$(pretty(metadata.horizon)) " *
        "interval=$(pretty(metadata.interval)) count=$(metadata.count)")

read_back = read_by_id(store, series_id)
frame = deterministic_frame(read_back)
println("\n$(nrow(frame)) (window, timestep) values")
println(first(sort(frame, [:issue_time, :timestamp]), 8))

# The overlap, made visible: every forecast anyone ever made of the peak hour,
# oldest first, against what the hour actually turned out to be.
peak = DAY_START + Hour(PEAK_HOUR)
actual_peak = ACTUAL[PEAK_HOUR + 1]
peak_rows = sort(filter(:timestamp => ==(peak), frame), :issue_time)
peak_rows.lead_hours = Hour.(peak_rows.timestamp .- peak_rows.issue_time)
peak_rows.error_mw = round.(peak_rows.value .- actual_peak, digits = 2)
println("\nforecasts of the $(Dates.format(peak, "HH:MM")) peak " *
        "(actual $actual_peak MW)")
println(select(peak_rows, :issue_time, :lead_hours, :value, :error_mw))

close!(store)
