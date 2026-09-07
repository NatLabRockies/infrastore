"""Ensembles instead of quantiles: Scenarios.

A `Scenarios` forecast has the same shape as a `Probabilistic` one - an extra
axis in front of (horizon steps, windows) - but the axis means something
different, and the difference matters to the optimization that consumes it.

A percentile is a marginal statement about *one* hour: "there is a 10% chance
this hour comes in below p10". Stacking p10 across every hour does not describe
any day that could actually happen, because the low hours are not independent.

A scenario *is* a day that could happen: one coherent trajectory, usually drawn
from a historical weather year or a numerical weather ensemble member. Hour 14
and hour 15 of scenario 2 are correlated because they came from the same weather.
That is what a stochastic unit-commitment model needs - it solves over whole
trajectories, not over per-hour quantiles.

So the axis is unlabelled: scenarios are interchangeable members (`scenario_count`
of them), and unlike `Probabilistic` there is nothing to name them with. Which
weather year a member came from is the consumer's bookkeeping - a `features` tag
or `application_data` on the series, not a field the store owns.

Below, three ensemble members of the PV plant's day-ahead output, each carrying
its own cloud pattern through the day.
"""

from datetime import timedelta

import numpy as np
import polars as pl

from infrastore import OwnerCategory, Scenarios, Store

from _shared import DAY_START, SUNDANCE_PV, solar_availability

RESOLUTION = timedelta(hours=1)
HORIZON = timedelta(hours=6)
INTERVAL = timedelta(hours=6)
COUNT = 2
SCENARIO_COUNT = 3
FIRST_ISSUE_HOUR = 6

horizon_steps = HORIZON // RESOLUTION
availability = solar_availability()

# One cloud trajectory per member: a slowly varying multiplier on the clear-sky
# profile, so consecutive hours within a member move together the way weather
# does. A per-hour independent draw would produce the same marginals and the
# wrong day.
rng = np.random.default_rng(2012)
values = np.empty((SCENARIO_COUNT, horizon_steps, COUNT), dtype=np.float64)
for scenario in range(SCENARIO_COUNT):
    cloud = 1.0
    for window in range(COUNT):
        for step in range(horizon_steps):
            hour = FIRST_ISSUE_HOUR + window * (INTERVAL // RESOLUTION) + step
            # Random walk with a pull back towards clear sky.
            cloud = float(np.clip(0.75 * cloud + 0.25 + rng.normal(0.0, 0.18), 0.2, 1.0))
            values[scenario, step, window] = round(availability[hour % 24] * cloud, 4)

store = Store.create(in_memory=True)

forecast = Scenarios(
    DAY_START + timedelta(hours=FIRST_ISSUE_HOUR),
    RESOLUTION,
    HORIZON,
    INTERVAL,
    COUNT,
    values,
    "max_active_power",
    units="per_unit",
    quantity_kind="ActivePower",
    unit_system="component_base",
    # What bounds a plant's output is usually its rating rather than a
    # separate max-power field, so that is the field these values drive.
    component_field="rating",
)
series_id = store.add_time_series(
    owner_id=SUNDANCE_PV.id,
    owner_type=SUNDANCE_PV.type,
    owner_category=OwnerCategory.Component,
    time_series=forecast,
    # Which weather years the members came from is the consumer's record, not the
    # store's; a feature tag is the natural place for it.
    features={"ensemble": "ecmwf_2012", "model_year": 2030},
)
print(f"added {SUNDANCE_PV.type} '{SUNDANCE_PV.name}': id={series_id}")

read_back = store.read_by_id(series_id)
print(f"scenario_count={read_back.scenario_count} count={read_back.count}")

data = np.asarray(read_back.data)
rows = [
    {
        "timestamp": DAY_START
        + timedelta(hours=FIRST_ISSUE_HOUR + window * (INTERVAL // RESOLUTION) + step),
        "scenario": scenario,
        "mw": round(float(data[scenario, step, window]) * SUNDANCE_PV.base_power_mw, 2),
    }
    for scenario in range(read_back.scenario_count)
    for step in range(data.shape[1])
    for window in range(data.shape[2])
]
frame = pl.DataFrame(rows)

print("\nensemble members, MW from a 60 MW plant")
print(
    frame.pivot(on="scenario", index="timestamp", values="mw")
    .rename({str(s): f"member_{s}" for s in range(SCENARIO_COUNT)})
    .sort("timestamp")
)

# Each member is one plausible day, so a whole-day statistic is a per-member
# statistic - the thing you cannot compute from per-hour quantiles.
energy = (
    frame.group_by("scenario")
    .agg(pl.col("mw").sum().round(1).alias("energy_mwh"))
    .sort("scenario")
)
print("\nday-ahead energy per member (hourly steps, so MW sums to MWh)")
print(energy)
