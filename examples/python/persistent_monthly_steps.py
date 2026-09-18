"""Monthly values read at an hourly cadence: PersistentTimeSeries.

Some inputs change on a calendar, not on a grid. A fuel contract reprices on the
first of the month; a unit's seasonal capability limits are reset monthly. A
`SingleTimeSeries` cannot hold those without writing 8760 copies of twelve
numbers, and a `NonSequentialTimeSeries` would say the value exists *only* on the
twelve breakpoints and nowhere in between - which is the opposite of what a
monthly input means.

A `PersistentTimeSeries` is the right shape: a sparse step function. It stores
one value per breakpoint, and the value in force at an instant is the one
belonging to the greatest breakpoint `<= t`, held forward past the last one. So a
dispatch model stepping hour by hour asks the same series for a value at every
hour of the year, and gets twelve distinct numbers back.

Two series here, both on the gas turbine, both stored monthly:

  * `fuel_cost`           - one f64 per month, the delivered gas price.
  * `active_power_limits` - one `tuple(2,f64)` per month, the (min, max) MW the
                            unit can hold that month. The pair moves together -
                            a summer derate lifts the floor and drops the
                            ceiling at the same time - so storing it as one
                            two-element value keeps them from drifting apart.

The read below sweeps hours across a month boundary, where the step is visible.
Before the first breakpoint there is no value at all, and that is an error rather
than a clamp: a step function makes no claim about what came before it.
"""

from datetime import datetime, timedelta, timezone

import numpy as np
import polars as pl

from infrastore import InvalidParameterError, OwnerCategory, PersistentTimeSeries, Store

from _shared import SOLITUDE

store = Store.create(in_memory=True)

YEAR = 2030
months = [datetime(YEAR, m, 1, tzinfo=timezone.utc) for m in range(1, 13)]

# Delivered gas, USD/MMBtu: cheap in the shoulder months, peaking in the winter
# and again when summer generation competes for pipeline capacity.
gas_price = np.array(
    [6.8, 6.4, 4.9, 3.7, 3.2, 3.6, 4.4, 4.5, 3.4, 3.3, 4.6, 6.9], dtype=np.float64
)

# Monthly (min, max) MW. A combustion turbine loses headroom as the air warms,
# and its stable minimum rises with it.
max_mw = np.array(
    [100.0, 100.0, 99.0, 97.0, 94.0, 91.0, 88.0, 89.0, 93.0, 97.0, 100.0, 100.0]
)
min_mw = np.round(0.30 * max_mw, 1)

price_id = store.add_time_series(
    owner_id=SOLITUDE.id,
    owner_type=SOLITUDE.type,
    owner_category=OwnerCategory.Component,
    time_series=PersistentTimeSeries(
        months,
        gas_price,
        "fuel_cost",
        units="USD/MMBtu",
        quantity_kind="EnergyPrice",
        unit_system="natural_units",
        component_field="fuel_cost",
    ),
)
limits_id = store.add_time_series(
    owner_id=SOLITUDE.id,
    owner_type=SOLITUDE.type,
    owner_category=OwnerCategory.Component,
    time_series=PersistentTimeSeries(
        months,
        # Shape is (breakpoints, 2): the trailing axis is the element, not more
        # breakpoints.
        np.column_stack([min_mw, max_mw]),
        "active_power_limits",
        element_type="tuple(2,f64)",
        units="MW",
        quantity_kind="ActivePower",
        unit_system="natural_units",
        component_field="active_power_limits",
    ),
)
print(
    f"added {SOLITUDE.type} '{SOLITUDE.name}': "
    f"fuel_cost={price_id} limits={limits_id}"
)

price = store.read_by_id(price_id)
limits = store.read_by_id(limits_id)

# Twelve stored rows each, and no resolution: a step function has no grid.
meta = store.get_metadata_by_id(limits_id)
print(
    f"length={meta['length']} resolution={meta['resolution']} "
    f"element_type={meta['element_type']} element_shape={meta['element_shape']}"
)

# The hourly read. Nothing was resampled at write time: the step function is
# evaluated at whatever instant the caller asks for.
start = datetime(YEAR, 6, 30, 21, tzinfo=timezone.utc)
hourly = [start + timedelta(hours=h) for h in range(8)]

frame = pl.DataFrame(
    {
        "hour": hourly,
        # The breakpoint in force - which month's row each hour resolves to.
        "in_force_since": [price.breakpoint_at(at) for at in hourly],
        "USD/MMBtu": [float(price.value_at(at)) for at in hourly],
        "(min, max) MW": [limits.value_at(at).tolist() for at in hourly],
    }
)
print("\nhourly reads across the June/July boundary")
print(frame)

# A whole year of hourly dispatch, off twelve stored rows per series.
year_end = datetime(YEAR + 1, 1, 1, tzinfo=timezone.utc)
hour_count = int((year_end - months[0]).total_seconds() // 3600)
year_hours = [months[0] + timedelta(hours=h) for h in range(hour_count)]
fuel = np.array([price.value_at(at) for at in year_hours])
print(f"\n{len(year_hours)} hourly reads -> {len(np.unique(fuel))} distinct prices")
print(f"mean delivered gas price: {fuel.mean():.2f} USD/MMBtu")

# Before the first breakpoint the series has no value, and says so.
try:
    price.value_at(months[0] - timedelta(hours=1))
except InvalidParameterError as err:
    print(f"\nbefore the first breakpoint: {err}")
