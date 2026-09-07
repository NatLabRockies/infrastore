"""
A forecast whose values are curves: the hourly market offer, re-submitted.

Forecast values are not restricted to numbers either. A market participant
re-submits its offer for every hour of the next few hours, every hour — which is
exactly a `Deterministic` whose value at each (window, timestep) is a *curve*
rather than a price.

A model that supports time-varying offers usually wants exactly this: an
incremental offer curve — breakpoints plus a marginal cost per segment — for
every hour of every run. The other element types below are the same idea for
cost models that use a different curve form.

The forecast geometry: the 100 MW gas turbine offers three hours ahead
(`horizon`), re-offering every hour (`interval`), and two of those runs are
stored (`count`). Windows overlap, so 16:00 is offered twice — once in the run
issued at 15:00 and once in the run issued at 16:00 — and the two offers must
agree here, because they rest on the same gas price forecast for that hour.

**The values keep their window shape.** A Julia array carries its own dimensions,
so a `(horizon steps, window count)` matrix of curves goes in as it stands and
comes back the same way — there is no flat list to fold and no leading-dimension
arithmetic to get right.
"""

using Dates
using DataFrames
using InfraStore

include("shared.jl")

const RESOLUTION = Hour(1)
const HORIZON_STEPS = 3
const WINDOW_COUNT = 2
const FIRST_ISSUE_HOUR = 15

# Forecast delivered gas price, \$/MMBtu, by target hour: an afternoon spike.
const GAS_PRICE_BY_HOUR = Dict(15 => 4.10, 16 => 4.55, 17 => 5.25, 18 => 5.60)

# The unit's incremental heat rates and offer breakpoints, as in
# `single_custom_elements.jl`.
const BREAKPOINTS_MW = [30.0, 65.0, 100.0]
const INCREMENTAL_HEAT_RATE = [9.6, 10.4]
const FUEL_BURN = [330.0, 666.0, 1030.0]

"""Wall-clock hour the value at (horizon step, window) is an offer for."""
target_hour(step, window) = FIRST_ISSUE_HOUR + (window - 1) + (step - 1)

# The gas price behind every (step, window) cell, in the array's own shape.
prices = [GAS_PRICE_BY_HOUR[target_hour(s, w)]
          for s in 1:HORIZON_STEPS, w in 1:WINDOW_COUNT]

offers = [
    # Economic operating range offered for the hour: (min, max) MW.
    [(30.0, gas < 5.0 ? 100.0 : 92.0) for gas in prices],
    # A single marginal cost plus a no-load cost.
    [LinearFunction(round(10.0 * gas, digits = 4), round(30.0 * gas, digits = 4))
     for gas in prices],
    [QuadraticFunction(round(0.02 * gas, digits = 4), round(8.0 * gas, digits = 4),
                       round(30.0 * gas, digits = 4))
     for gas in prices],
    # Input-output points: total cost (\$/hr) at each output (MW).
    [PiecewiseLinear([(x = mw, y = round(fuel * gas, digits = 2))
                      for (mw, fuel) in zip(BREAKPOINTS_MW, FUEL_BURN)])
     for gas in prices],
    # The offer curve itself: breakpoints plus one marginal cost per segment.
    [PiecewiseStep(BREAKPOINTS_MW,
                   [round(rate * gas, digits = 2) for rate in INCREMENTAL_HEAT_RATE])
     for gas in prices],
]

Store(in_memory = true) do store
    series_ids = Int64[]

    for values in offers
        tag = element_type_tag(vec(values))
        forecast = Deterministic(
            DAY_START + Hour(FIRST_ISSUE_HOUR),
            RESOLUTION,
            HORIZON_STEPS * RESOLUTION,   # horizon
            RESOLUTION,                   # interval: re-offered hourly, so windows overlap
            WINDOW_COUNT,
            values,
            "incremental_offer_curves_$tag";
            units = tag == "tuple(2,f64)" ? "MW" :
                    tag == "piecewise_step" ? "\$/MWh" : "\$/hr",
            quantity_kind = startswith(tag, "tuple") ? "ActivePower" : "CostRate",
            unit_system = NaturalUnits,
            component_field = "operation_cost",
        )
        id = add_time_series!(store, SOLITUDE.id, SOLITUDE.type, Component, forecast)
        push!(series_ids, id)
        println("added $tag for '$(SOLITUDE.name)': id=$id")
    end

    for id in series_ids
        forecast = read_by_id(store, id)
        # `forecast.data` is back in its `(step, window)` shape, curves and all.
        frame = DataFrame(
            issue_time = [window_issue_times(forecast)[w]
                          for s in 1:HORIZON_STEPS for w in 1:WINDOW_COUNT],
            offer_for = [DAY_START + Hour(target_hour(s, w))
                         for s in 1:HORIZON_STEPS for w in 1:WINDOW_COUNT],
            gas_price = [prices[s, w] for s in 1:HORIZON_STEPS for w in 1:WINDOW_COUNT],
            offer = [forecast.data[s, w] for s in 1:HORIZON_STEPS for w in 1:WINDOW_COUNT],
        )
        println("\n$(forecast.name)")
        println(sort(frame, [:issue_time, :offer_for]))
    end
end
