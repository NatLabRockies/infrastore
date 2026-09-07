"""A day of hourly power values per component: the everyday `SingleTimeSeries`.

A `SingleTimeSeries` is a value at every step of a regular grid - an initial
timestamp, a resolution, and a dense array. It is what a modeler reaches for
when a component's field varies over the simulation horizon: a thermal unit's
usable capacity, a solar plant's availability, a feeder's demand.

The three series below make the same point three ways, and deliberately differ
in `unit_system`:

  * the gas turbine's capacity is stored in MW (`natural_units`);
  * the PV plant's output is stored per-unit on its own 60 MW base power
    (`component_base`);
  * the feeder load is stored in MW again.

Nothing in the store rescales anything. `unit_system` is a declaration about
what the numbers already are, so a consumer reading a `component_base` series
knows it needs the owning component's base power to get back to MW. In
particular a per-unit series here is per-unit because that is the basis its
values are on, not because it is a normalized shape awaiting a scale factor -
the store applies no scaling factors, so anything that needs scaling should be
scaled before it is written.
"""

from datetime import timedelta

from infrastore import OwnerCategory, SingleTimeSeries, Store

from _shared import (
    BUS_A_LOAD,
    DAY_START,
    SOLITUDE,
    SUNDANCE_PV,
    feeder_load_mw,
    solar_availability,
    static_frame,
    thermal_capacity_mw,
)

store = Store.create(in_memory=True)

# (component, values, units, unit_system, component_field) - one hourly day each.
#
# All three series are *named* "max_active_power", the usual name for a series
# that bounds a component's output. The field each one actually drives differs
# by component type, and `component_field` is where that is recorded: a solar
# plant's ceiling is its `rating`, while the load and the thermal unit each have
# a `max_active_power` of their own.
inputs = [
    (
        SOLITUDE,
        thermal_capacity_mw(SOLITUDE.base_power_mw),
        "MW",
        "natural_units",
        "max_active_power",
    ),
    (
        SUNDANCE_PV,
        solar_availability(),
        "per_unit",
        "component_base",
        "rating",
    ),
    (
        BUS_A_LOAD,
        feeder_load_mw(BUS_A_LOAD.base_power_mw),
        "MW",
        "natural_units",
        "max_active_power",
    ),
]

series_ids = []
for component, values, units, unit_system, component_field in inputs:
    series = SingleTimeSeries(
        DAY_START,
        timedelta(hours=1),  # resolution: one value per hour
        values,
        # The series name is part of its identity; `component_field` says what
        # the values are *for*. They often coincide, and here they deliberately
        # do not.
        "max_active_power",
        units=units,
        quantity_kind="ActivePower",
        unit_system=unit_system,
        component_field=component_field,
    )
    series_id = store.add_time_series(
        owner_id=component.id,
        owner_type=component.type,
        owner_category=OwnerCategory.Component,
        time_series=series,
    )
    series_ids.append(series_id)
    print(f"added {component.type} '{component.name}': id={series_id}")

# The id returned by the write is the only way to address a stored series. A
# consumer records it on its own object (a generator holding the id of the
# series that varies its capacity) and reads it back with it.
print("\nnothing but the ids is needed to read the values back:")
for metadata, series in zip(
    store.list_metadata_by_ids(series_ids),
    store.read_by_ids(series_ids),
    strict=True,
):
    print(
        f"\nid={metadata['id']} owner={metadata['owner_type']}/{metadata['owner_id']}"
        f" field={metadata['component_field']}"
        f" {metadata['units']} ({metadata['unit_system']})"
    )
    print(static_frame(series))
