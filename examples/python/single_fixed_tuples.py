"""One timestep, several numbers: fixed-width tuple values.

A series' value at a timestep does not have to be a scalar. `element_type` says
what one timestep's value *means*, and `tuple(N,dtype)` declares a fixed-width
group of numbers that belong together.

The domain case here is complex power at the feeder: active power in MW and
reactive power in MVar, measured together and only meaningful together, since
the power factor is the relationship between them. Most models hold active and
reactive power as two separate component fields, and would carry two
`SingleTimeSeries`. Storing them as one `tuple(2,f64)` series is what a consumer
whose own object model holds a complex injection would do - the pair is written
once, read back once, and can never drift out of step or be filtered apart by
accident.

The store keeps the tuple width in `element_shape`, so a read hands back an
array of pairs rather than a flat vector that the caller has to re-group.
"""

from datetime import timedelta

import numpy as np

from infrastore import OwnerCategory, SingleTimeSeries, Store

from _shared import BUS_A_LOAD, DAY_START, feeder_load_mw, static_frame

store = Store.create(in_memory=True)

active_mw = feeder_load_mw(BUS_A_LOAD.base_power_mw)
# A lagging feeder at roughly 0.95 power factor: Q = P * tan(acos(0.95)).
reactive_mvar = np.round(active_mw * np.tan(np.arccos(0.95)), 2)

# Each row is one (active_power, reactive_power) value; the shape is
# (timesteps, 2) and the trailing axis is the element, not more timesteps.
values = np.column_stack([active_mw, reactive_mvar])

series = SingleTimeSeries(
    DAY_START,
    timedelta(hours=1),
    values,
    "complex_power",
    element_type="tuple(2,f64)",
    units="MW,MVar",
    # `quantity_kind` names one physical quantity, and a tuple of two kinds has
    # no honest single answer, so it is left unset here. That is a fair warning:
    # a consumer reading this series has to know the packing convention from
    # somewhere else - here, active power first, then reactive.
    unit_system="natural_units",
    component_field="active_power",
)
series_id = store.add_time_series(
    owner_id=BUS_A_LOAD.id,
    owner_type=BUS_A_LOAD.type,
    owner_category=OwnerCategory.Component,
    time_series=series,
)
print(f"added {BUS_A_LOAD.type} '{BUS_A_LOAD.name}': id={series_id}")

metadata = store.get_metadata_by_id(series_id)
print(
    f"element_type={metadata['element_type']} "
    f"element_shape={metadata['element_shape']} length={metadata['length']}"
)

read_back = store.read_by_id(series_id)
frame = static_frame(read_back).rename({"value": "(MW, MVar)"})
print("\nhourly complex power at the feeder")
print(frame.head(6))
