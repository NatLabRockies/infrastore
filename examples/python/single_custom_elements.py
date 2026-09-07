"""Cost curves that change every hour: function-valued time series.

The value at a timestep can be a *function* rather than a number. This is what a
market bid or a fuel-price-driven cost curve needs: the gas unit's offer is not
one price, it is a curve over its output range, and it is re-offered every hour
as the fuel price moves.

infrastore's four function element types cover the curve forms production-cost
and market models actually use:

    element_type          what it models
    -------------------   --------------------------------------------------
    linear_function       c(P) = a*P + b, a constant heat rate plus no-load
    quadratic_function    c(P) = a*P^2 + b*P + c, the smooth fuel-cost polynomial
    piecewise_linear      (MW, $/hr) input-output points
    piecewise_step        MW breakpoints + one $/MWh marginal cost per segment,
                          i.e. the incremental offer curve a unit bids

All four describe the same 100 MW gas turbine over three hours as its delivered
gas price climbs from $3.50 to $5.25/MMBtu, so the curves get steeper hour by
hour. They are four spellings of one physical unit; a real system carries the
one its cost model calls for.

`SingleTimeSeries.from_values` takes the curves themselves: it packs them into
the array the store holds and records the element type they imply, so the two
cannot get out of step. The packing is the store's business (a piecewise row
carries its point count in the leading slot, so ragged curves share one
rectangular array); `decoded_values()` on a read unpacks it again, and the
`element_type` recorded on the row is what tells any other reader how to.
"""

from datetime import timedelta

import polars as pl

from infrastore import OwnerCategory, SingleTimeSeries, Store

from _shared import DAY_START, SOLITUDE

# Delivered gas price, $/MMBtu, for three consecutive hours.
GAS_PRICE = [3.50, 4.10, 5.25]

# Incremental heat rates, MMBtu/MWh: the unit gets less efficient as it loads up.
# Breakpoints are its 30 MW minimum, a 65 MW knee, and its 100 MW rating.
BREAKPOINTS_MW = [30.0, 65.0, 100.0]
INCREMENTAL_HEAT_RATE = [9.6, 10.4]
FUEL_AT_MIN = 330.0  # MMBtu/hr to hold the unit at 30 MW

# Total fuel burn, MMBtu/hr, at each breakpoint - the input half of the
# input-output curve.
FUEL_BURN = [
    FUEL_AT_MIN,
    FUEL_AT_MIN + 35.0 * INCREMENTAL_HEAT_RATE[0],
    FUEL_AT_MIN + 35.0 * INCREMENTAL_HEAT_RATE[0] + 35.0 * INCREMENTAL_HEAT_RATE[1],
]

CURVES = {
    # A flat-heat-rate approximation: one marginal cost plus a no-load cost.
    "linear_function": [
        {"proportional": round(10.0 * gas, 4), "constant": round(30.0 * gas, 4)}
        for gas in GAS_PRICE
    ],
    # The classic smooth fuel-cost polynomial.
    "quadratic_function": [
        {
            "quadratic": round(0.02 * gas, 4),
            "proportional": round(8.0 * gas, 4),
            "constant": round(30.0 * gas, 4),
        }
        for gas in GAS_PRICE
    ],
    # Input-output points: total production cost ($/hr) at each output (MW).
    "piecewise_linear": [
        [
            {"x": mw, "y": round(fuel * gas, 2)}
            for mw, fuel in zip(BREAKPOINTS_MW, FUEL_BURN, strict=True)
        ]
        for gas in GAS_PRICE
    ],
    # The offer curve itself: n breakpoints and n-1 segment marginal costs. That
    # off-by-one is the definition, not an accident - each cost applies to the
    # interval between two breakpoints.
    "piecewise_step": [
        {
            "x": BREAKPOINTS_MW,
            "y": [round(rate * gas, 2) for rate in INCREMENTAL_HEAT_RATE],
        }
        for gas in GAS_PRICE
    ],
}

store = Store.create(in_memory=True)
series_ids = []

for element_type, values in CURVES.items():
    series = SingleTimeSeries.from_values(
        DAY_START,
        timedelta(hours=1),
        values,
        f"variable_cost_{element_type}",
        # An assertion, not an override: the values already name the type, and
        # this raises if the two disagree. Omitting it changes nothing.
        element_type=element_type,
        # The label describes what evaluating the curve gives you: a production
        # cost rate in $/hr for the first three, and a marginal cost in $/MWh for
        # the offer curve, whose y values are per-segment slopes.
        units="$/MWh" if element_type == "piecewise_step" else "$/hr",
        # Free-form. QUDT, whose local names `quantity_kind` otherwise borrows,
        # has no kind for money per unit time, so this is a convention the
        # consumer picks and sticks to.
        quantity_kind="CostRate",
        unit_system="natural_units",
        component_field="operation_cost",
    )
    series_id = store.add_time_series(
        owner_id=SOLITUDE.id,
        owner_type=SOLITUDE.type,
        owner_category=OwnerCategory.Component,
        time_series=series,
    )
    series_ids.append(series_id)
    print(f"added {element_type} for '{SOLITUDE.name}': id={series_id}")

# Reading back: a read hands back the whole series - its descriptors as well as
# its values - and the series' own `element_type` drives the decode. So there is
# no catalog lookup here: nothing in this loop is asking a question about the row
# that the row itself did not come back with.
for series_id in series_ids:
    series = store.read_by_id(series_id)
    frame = pl.DataFrame(
        {
            "timestamp": series.timestamps,
            "gas_price": GAS_PRICE,
            "curve": series.decoded_values(),
        },
        strict=False,
    )
    print(f"\n{series.name} ({series.units})")
    breakpoint()
    print(frame)
