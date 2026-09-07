"""Measurements at the instants they were taken: NonSequentialTimeSeries.

A `SingleTimeSeries` says "a value every hour". A `NonSequentialTimeSeries` says
"a value at *these* instants and nowhere else" - it carries its own timestamp
vector instead of a resolution, and it makes no claim at all about the instants
in between. That is the right shape for measured data rather than modelled data:
telemetry, meter reads, market clearings on a non-uniform schedule.

Here it is the PV plant's real-time telemetry across a cloud passage. The plant
reports every 15 minutes when its output is steady, and every minute while a
cloud is crossing it. Nothing is interpolated: at 09:07 the store has no value
for this series, and that is the honest answer rather than a resampled one. A
modeler who needs a value at every instant wants a regular grid, or a step
function whose value is held forward.

The timestamp vector is content-addressed and shared: several plants reporting on
the same schedule store their timestamps once. Note that reads and queries carry
the series' own irregular axis - a `NonSequentialTimeSeries` has no `resolution`,
so anything that assumes a uniform grid does not apply to it.
"""

from datetime import timedelta

import numpy as np
import polars as pl

from infrastore import NonSequentialTimeSeries, OwnerCategory, Store

from _shared import DAY_START, SUNDANCE_PV, static_frame

store = Store.create(in_memory=True)

# Minutes past midnight of each report, and the per-unit output reported.
# 15-minute cadence in clear sky; 1-minute cadence through the 09:35-09:40 cloud.
reports = [
    (8 * 60 + 45, 0.61),
    (9 * 60 + 0, 0.65),
    (9 * 60 + 15, 0.68),
    (9 * 60 + 30, 0.71),
    (9 * 60 + 35, 0.44),  # cloud edge
    (9 * 60 + 36, 0.19),
    (9 * 60 + 37, 0.12),
    (9 * 60 + 38, 0.15),
    (9 * 60 + 39, 0.38),
    (9 * 60 + 40, 0.69),  # back in full sun
    (9 * 60 + 45, 0.72),
    (10 * 60 + 0, 0.75),
]

timestamps = [DAY_START + timedelta(minutes=minute) for minute, _ in reports]
values = np.array([per_unit for _, per_unit in reports], dtype=np.float64)

series = NonSequentialTimeSeries(
    timestamps,
    values,
    "measured_active_power",
    units="per_unit",
    quantity_kind="ActivePower",
    # Per-unit of the plant's 60 MW rating, like the modelled availability series.
    unit_system="component_base",
    component_field="active_power",
)
series_id = store.add_time_series(
    owner_id=SUNDANCE_PV.id,
    owner_type=SUNDANCE_PV.type,
    owner_category=OwnerCategory.Component,
    time_series=series,
)
print(f"added {SUNDANCE_PV.type} '{SUNDANCE_PV.name}': id={series_id}")

metadata = store.get_metadata_by_id(series_id)
# An irregular series records no resolution and no initial_timestamp: there is no
# grid to describe. Its identity includes the hash of its timestamp vector.
print(f"resolution={metadata['resolution']} length={metadata['length']}")

read_back = store.read_by_id(series_id)
frame = static_frame(read_back).with_columns(
    (SUNDANCE_PV.base_power_mw * pl.col("value")).round(2).alias("MW")
)
print("\ntelemetry across the cloud passage")
print(frame)

# The gap between consecutive reports is data, not an artifact: it is how a
# consumer sees that the plant switched to fast reporting.
gaps = np.diff(np.array([ts.timestamp() for ts in read_back.timestamps])) / 60.0
print(f"\nreporting gaps (minutes): {gaps.tolist()}")
