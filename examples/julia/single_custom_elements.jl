"""
Cost curves that change every hour: function-valued time series.

The value at a timestep can be a *function* rather than a number. This is what a
market bid or a fuel-price-driven cost curve needs: the gas unit's offer is not
one price, it is a curve over its output range, and it is re-offered every hour
as the fuel price moves.

infrastore's four function element types cover the curve forms production-cost
and market models actually use:

    value type          element_type          what it models
    -----------------   -------------------   ----------------------------------
    LinearFunction      linear_function       c(P) = a*P + b, a constant heat
                                              rate plus no-load
    QuadraticFunction   quadratic_function    c(P) = a*P^2 + b*P + c, the smooth
                                              fuel-cost polynomial
    PiecewiseLinear     piecewise_linear      (MW, \$/hr) input-output points
    PiecewiseStep       piecewise_step        MW breakpoints + one \$/MWh
                                              marginal cost per segment, i.e.
                                              the incremental offer curve a unit
                                              bids

All four describe the same 100 MW gas turbine over three hours as its delivered
gas price climbs from \$3.50 to \$5.25/MMBtu, so the curves get steeper hour by
hour. They are four spellings of one physical unit; a real system carries the
one its cost model calls for.

**Hand the constructor the values and it does the rest.** A
`Vector{PiecewiseLinear}` names its own `element_type`, so there is nothing to
declare and no encode step to call: packing happens at the ABI boundary, the
struct goes on holding the curves, and a read hands the same curves back. The
packing is the store's business (a piecewise row carries its point count in the
leading slot, so ragged curves share one rectangular array) and `raw = true` is
how to see it.
"""

using Dates
using DataFrames
using InfraStore

include("shared.jl")

# Delivered gas price, \$/MMBtu, for three consecutive hours.
const GAS_PRICE = [3.50, 4.10, 5.25]

# Incremental heat rates, MMBtu/MWh: the unit gets less efficient as it loads up.
# Breakpoints are its 30 MW minimum, a 65 MW knee, and its 100 MW rating.
const BREAKPOINTS_MW = [30.0, 65.0, 100.0]
const INCREMENTAL_HEAT_RATE = [9.6, 10.4]
const FUEL_AT_MIN = 330.0  # MMBtu/hr to hold the unit at 30 MW

# Total fuel burn, MMBtu/hr, at each breakpoint — the input half of the
# input-output curve.
const FUEL_BURN = [
    FUEL_AT_MIN,
    FUEL_AT_MIN + 35.0 * INCREMENTAL_HEAT_RATE[1],
    FUEL_AT_MIN + 35.0 * INCREMENTAL_HEAT_RATE[1] + 35.0 * INCREMENTAL_HEAT_RATE[2],
]

curves = [
    # A flat-heat-rate approximation: one marginal cost plus a no-load cost.
    [LinearFunction(round(10.0 * gas, digits = 4), round(30.0 * gas, digits = 4))
     for gas in GAS_PRICE],
    # The classic smooth fuel-cost polynomial.
    [QuadraticFunction(round(0.02 * gas, digits = 4), round(8.0 * gas, digits = 4),
                       round(30.0 * gas, digits = 4))
     for gas in GAS_PRICE],
    # Input-output points: total production cost (\$/hr) at each output (MW).
    [PiecewiseLinear([(x = mw, y = round(fuel * gas, digits = 2))
                      for (mw, fuel) in zip(BREAKPOINTS_MW, FUEL_BURN)])
     for gas in GAS_PRICE],
    # The offer curve itself: n breakpoints and n-1 segment marginal costs. That
    # off-by-one is the definition, not an accident — each cost applies to the
    # interval between two breakpoints, and `PiecewiseStep` enforces it.
    [PiecewiseStep(BREAKPOINTS_MW,
                   [round(rate * gas, digits = 2) for rate in INCREMENTAL_HEAT_RATE])
     for gas in GAS_PRICE],
]

# The package's `show` methods are deliberately compact — `PiecewiseLinear(3
# points)` — so this renders the numbers a table should show.
cost_at(c::LinearFunction, mw) = c.proportional * mw + c.constant
cost_at(c::QuadraticFunction, mw) = c.quadratic * mw^2 + c.proportional * mw + c.constant
cost_at(c::PiecewiseLinear, mw) = last(c.points).y
# A step curve's y values are marginal costs, not a total, so the comparable
# number is the last segment's — what the unit charges for its final MW.
cost_at(c::PiecewiseStep, mw) = last(c.y)

Store(in_memory = true) do store
    series_ids = Int64[]

    for values in curves
        tag = element_type_tag(values)      # what these values will be stored as
        series = SingleTimeSeries(
            DAY_START,
            Hour(1),
            values,
            "variable_cost_$tag";
            # The label describes what evaluating the curve gives you: a production
            # cost rate in \$/hr for the first three, and a marginal cost in \$/MWh
            # for the offer curve, whose y values are per-segment slopes.
            units = tag == "piecewise_step" ? "\$/MWh" : "\$/hr",
            # Free-form. QUDT, whose local names `quantity_kind` otherwise borrows,
            # has no kind for money per unit time, so this is a convention the
            # consumer picks and sticks to.
            quantity_kind = "CostRate",
            unit_system = NaturalUnits,
            component_field = "operation_cost",
        )
        id = add_time_series!(store, SOLITUDE.id, SOLITUDE.type, Component, series)
        push!(series_ids, id)
        println("added $tag for '$(SOLITUDE.name)': id=$id")
    end

    # Reading back: a read hands back the whole series — its descriptors as well as
    # its values — and decodes as it goes, so there is no catalog lookup here and
    # nothing to unpack.
    for id in series_ids
        series = read_by_id(store, id)
        println("\n$(series.name) ($(series.units))")
        println(DataFrame(
            timestamp = timestamps(series),
            gas_price = GAS_PRICE,
            curve = series.data,
            at_100mw = [round(cost_at(c, 100.0), digits = 2) for c in series.data],
        ))
    end

    # The packing, for one series, as `raw = true` hands it back. The leading slot
    # is the point count and the rest is zero padding out to the widest row — which
    # is why a 2-point and a 3-point curve can share one rectangular array.
    ragged = SingleTimeSeries(
        DAY_START,
        Hour(1),
        [PiecewiseLinear([(x = 30.0, y = 1155.0), (x = 100.0, y = 4120.0)]),
         PiecewiseLinear([(x = 30.0, y = 1353.0), (x = 65.0, y = 2730.0),
                          (x = 100.0, y = 4223.0)])],
        "ragged_cost",
    )
    ragged_id = add_time_series!(store, SOLITUDE.id, SOLITUDE.type, Component, ragged)
    println("\nragged curves, as values:")
    println(read_by_id(store, ragged_id).data)
    println("\nthe same rows as stored (raw = true):")
    println(read_by_id(store, ragged_id; raw = true).data)
end
