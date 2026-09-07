"""A forecast whose values are curves: the hourly market offer, re-submitted.

Forecast values are not restricted to numbers either. A market participant
re-submits its offer for every hour of the next few hours, every hour - which is
exactly a `Deterministic` whose value at each (window, timestep) is a *curve*
rather than a price.

A model that supports time-varying offers usually wants exactly this: an
incremental offer curve - breakpoints plus a marginal cost per segment - for
every hour of every run. The other element types below are the same idea for
cost models that use a different curve form.

The forecast geometry: the 100 MW gas turbine offers three hours ahead
(`horizon`), re-offering every hour (`interval`), and two of those runs are
stored (`count`). Windows overlap, so 16:00 is offered twice - once in the run
issued at 15:00 and once in the run issued at 16:00 - and the two offers must
agree here, because they rest on the same gas price forecast for that hour.

`Deterministic.from_values` takes the curves themselves. It encodes them, records
the element type they imply, and folds them into the (horizon steps, window
count) grid derived from `horizon`, `resolution` and `count` - the arithmetic the
lower-level `encode_element_values` asks for as `leading_dims`. The flat order is
horizon-major, matching the stored layout.
"""

from datetime import timedelta

import polars as pl

from infrastore import Deterministic, OwnerCategory, Store

from _shared import DAY_START, SOLITUDE

RESOLUTION = timedelta(hours=1)
HORIZON_STEPS = 3
WINDOW_COUNT = 2
FIRST_ISSUE_HOUR = 15

# Forecast delivered gas price, $/MMBtu, by target hour: an afternoon spike.
GAS_PRICE_BY_HOUR = {15: 4.10, 16: 4.55, 17: 5.25, 18: 5.60}

# The unit's incremental heat rates and offer breakpoints, as in
# `single_custom_elements.py`.
BREAKPOINTS_MW = [30.0, 65.0, 100.0]
INCREMENTAL_HEAT_RATE = [9.6, 10.4]
FUEL_BURN = [330.0, 666.0, 1030.0]


def target_hour(step: int, window: int) -> int:
    """Wall-clock hour the value at (horizon step, window) is an offer for."""
    return FIRST_ISSUE_HOUR + window + step


# Horizon-major flat order: (step 0, window 0), (step 0, window 1), (step 1, ...).
GRID = [(step, window) for step in range(HORIZON_STEPS) for window in range(WINDOW_COUNT)]
PRICES = [GAS_PRICE_BY_HOUR[target_hour(step, window)] for step, window in GRID]

OFFERS = {
    # Economic operating range offered for the hour: (min, max) MW.
    "tuple(2,f64)": [[30.0, 100.0 if gas < 5.0 else 92.0] for gas in PRICES],
    # A single marginal cost plus a no-load cost.
    "linear_function": [
        {"proportional": round(10.0 * gas, 4), "constant": round(30.0 * gas, 4)}
        for gas in PRICES
    ],
    "quadratic_function": [
        {
            "quadratic": round(0.02 * gas, 4),
            "proportional": round(8.0 * gas, 4),
            "constant": round(30.0 * gas, 4),
        }
        for gas in PRICES
    ],
    # Input-output points: total cost ($/hr) at each output (MW).
    "piecewise_linear": [
        [
            {"x": mw, "y": round(fuel * gas, 2)}
            for mw, fuel in zip(BREAKPOINTS_MW, FUEL_BURN, strict=True)
        ]
        for gas in PRICES
    ],
    # The offer curve itself: breakpoints plus one marginal cost per segment.
    "piecewise_step": [
        {
            "x": BREAKPOINTS_MW,
            "y": [round(rate * gas, 2) for rate in INCREMENTAL_HEAT_RATE],
        }
        for gas in PRICES
    ],
}

store = Store.create(in_memory=True)
series_ids = []

for element_type, values in OFFERS.items():
    forecast = Deterministic.from_values(
        DAY_START + timedelta(hours=FIRST_ISSUE_HOUR),
        RESOLUTION,
        HORIZON_STEPS * RESOLUTION,  # horizon
        RESOLUTION,  # interval: re-offered every hour, so windows overlap
        WINDOW_COUNT,
        values,
        f"incremental_offer_curves_{element_type}",
        # An assertion, not an override: the values already name the type, and
        # this raises if the two disagree. Omitting it changes nothing.
        element_type=element_type,
        units={"tuple(2,f64)": "MW", "piecewise_step": "$/MWh"}.get(
            element_type, "$/hr"
        ),
        quantity_kind="ActivePower" if element_type.startswith("tuple") else "CostRate",
        unit_system="natural_units",
        component_field="operation_cost",
    )
    series_id = store.add_time_series(
        owner_id=SOLITUDE.id,
        owner_type=SOLITUDE.type,
        owner_category=OwnerCategory.Component,
        time_series=forecast,
    )
    series_ids.append(series_id)
    print(f"added {element_type} for '{SOLITUDE.name}': id={series_id}")

for series_id in series_ids:
    # A read hands back the whole forecast, descriptors included, so there is no
    # catalog lookup to make here. It also knows both halves of the decode - its
    # element type, and that its values are indexed by two axes ahead of the
    # element, i.e. the (horizon step, window) grid - so nothing is passed in.
    forecast = store.read_by_id(series_id)
    decoded = forecast.decoded_values()
    rows = [
        {
            "issue_time": DAY_START + timedelta(hours=FIRST_ISSUE_HOUR + window),
            "offer_for": DAY_START + timedelta(hours=target_hour(step, window)),
            "gas_price": GAS_PRICE_BY_HOUR[target_hour(step, window)],
            "offer": value,
        }
        for (step, window), value in zip(GRID, decoded, strict=True)
    ]
    print(f"\n{forecast.name}")
    print(pl.DataFrame(rows, strict=False).sort("issue_time", "offer_for"))
