"""A fleet ingested in one transaction, then swept hour by hour with a reader.

Two habits that matter once a store holds more than a handful of series:

  * **Write inside a transaction.** Outside one, every `add_time_series`
    commits its own catalog row and flushes the HDF5 file first, so a row can
    never name bytes the file did not receive - and that flush costs about
    the same for one array as for a thousand. Each add also drops its array
    into one slot of a pool chunked across a thousand columns, rewriting every
    chunk in it for one series. Inside a `transaction()` each add is a
    savepoint instead: the flush happens once at commit, the adds the span
    covers are buffered and written together as one block sized to them - the
    layout `add_time_series_bulk` produces - and either every series of the
    fleet lands or none does. The loop stays a plain loop.

  * **Read per timestamp with a `StaticReader`, not per series.** A simulation
    walks the clock and wants, at each instant, the value of every series at
    once. `read_by_ids` hands back whole arrays; a reader is built once over a
    filter, pins one timeline, and fills a preallocated columnar buffer on
    every `static_read`, so the loop itself allocates nothing.

The sweep below is a reserve-margin check: at every hour of the day, the fleet's
available capacity against the load it has to serve. Solar is stored per-unit
on each plant's own base power (`unit_system="component_base"`), which is why
the loop needs the owning component and not just the series - infrastore never
rescales anything, so the multiply is the consumer's.
"""

from datetime import timedelta

import numpy as np
import polars as pl
from infrastore import OwnerCategory, SingleTimeSeries, Store

from _shared import (
    DAY_START,
    Component,
    feeder_load_mw,
    solar_availability,
    thermal_capacity_mw,
)

# A fleet: eight gas turbines, six solar plants, five feeders. Each is a
# component in the consumer's object model with its own id and base power; the
# profiles come from `_shared` and are scaled per unit so the columns differ.
THERMAL = [Component(110 + i, "ThermalGenerator", f"ct_{i}", 60.0 + 15.0 * i) for i in range(8)]
SOLAR = [Component(130 + i, "SolarPlant", f"pv_{i}", 40.0 + 10.0 * i) for i in range(6)]
LOADS = [Component(210 + i, "Load", f"feeder_{i}", 90.0 + 25.0 * i) for i in range(5)]
FLEET = {c.id: c for c in THERMAL + SOLAR + LOADS}


def fleet_series(component: Component) -> SingleTimeSeries:
    """One hourly day of `max_active_power` for a component, on the shared grid."""
    if component.type == "ThermalGenerator":
        values, units, unit_system, field = (
            thermal_capacity_mw(component.base_power_mw),
            "MW",
            "natural_units",
            "max_active_power",
        )
    elif component.type == "SolarPlant":
        # Per-unit on the plant's own base power: a cloudier site scales the
        # whole curve down, and the store keeps it exactly as given.
        cloudiness = 1.0 - 0.05 * (component.id % 3)
        values, units, unit_system, field = (
            np.round(solar_availability() * cloudiness, 4),
            "per_unit",
            "component_base",
            "rating",
        )
    else:
        values, units, unit_system, field = (
            feeder_load_mw(component.base_power_mw),
            "MW",
            "natural_units",
            "max_active_power",
        )
    return SingleTimeSeries(
        DAY_START,
        timedelta(hours=1),
        values,
        "max_active_power",
        units=units,
        quantity_kind="ActivePower",
        unit_system=unit_system,
        component_field=field,
    )


store = Store.create(in_memory=True)

# One transaction around the whole fleet. The adds inside are the ordinary
# one-series call; the transaction is what makes the fleet atomic - a failure
# on the last feeder rolls every generator back too - what defers the HDF5
# flush to the single commit at the end of the block, and what lets the span's
# adds be written as one block per shape group that fills the HDF5 chunks
# whole. A raise anywhere inside unwinds with a rollback and re-raises.
#
# `add_time_series_bulk` is the same write, spelled for a cohort already in
# hand as a list rather than produced by a loop.
series_ids: list[int] = []
with store.transaction():
    for group in (THERMAL, SOLAR, LOADS):
        for component in group:
            series_ids.append(
                store.add_time_series(
                    owner_id=component.id,
                    owner_type=component.type,
                    owner_category=OwnerCategory.Component,
                    time_series=fleet_series(component),
                )
            )
        print(f"added {len(group):2d} {group[0].type} series")
print(f"committed {len(series_ids)} series in one transaction: ids {series_ids[0]}..{series_ids[-1]}")

# Build the reader once, outside the loop. The filter is the same identity
# vocabulary `list_metadata` takes; the resolution pins the grid, and every
# matched series must lie on it - a stray series on another grid is refused
# here, by name, rather than silently dropped.
reader = store.build_static_reader(resolution=timedelta(hours=1), name="max_active_power")
grid = reader.grid()
print(f"\nreader: {grid['length']} steps of {grid['resolution']} from {grid['initial_timestamp']}")

# Columns are grouped by (dtype, element_shape); every series here is a scalar
# f64, so there is one group and its `ids` give the column order. Resolve each
# id to its component *once*, here, so the loop does no catalog lookups.
(group,) = reader.groups()
columns = [FLEET[store.get_metadata_by_id(series_id)["owner_id"]] for series_id in group["ids"]]
is_thermal = np.array([c.type == "ThermalGenerator" for c in columns])
is_solar = np.array([c.type == "SolarPlant" for c in columns])
is_load = np.array([c.type == "Load" for c in columns])
base_power = np.array([c.base_power_mw for c in columns])

# The sweep. `timestamps()` enumerates the reader's own axis, so this loop is
# the same for a regular grid and for an irregular cohort; `static_read` fills
# the buffer in place and `group_values` views it as a numpy array shaped
# (num_columns, *element_shape) - here just (num_columns,).
rows = []
for when in reader.timestamps():
    store.static_read(reader, when)
    values = reader.group_values(0)
    thermal_mw = values[is_thermal].sum()
    solar_mw = (values[is_solar] * base_power[is_solar]).sum()  # per-unit -> MW
    load_mw = values[is_load].sum()
    rows.append(
        {
            "hour": when.hour,
            "thermal_mw": round(thermal_mw, 1),
            "solar_mw": round(solar_mw, 1),
            "load_mw": round(load_mw, 1),
            "margin_pct": round(100.0 * (thermal_mw + solar_mw - load_mw) / load_mw, 1),
        }
    )

print("\nhourly reserve margin across the fleet:")
with pl.Config(tbl_rows=-1):
    print(pl.DataFrame(rows))
