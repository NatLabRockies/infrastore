"""Forecast uncertainty as quantiles: Probabilistic.

A `Probabilistic` forecast is a `Deterministic` with one more axis. Instead of a
single number per (window, timestep) it carries a whole distribution, sampled at
named percentiles - the p10/p50/p90 band a renewable forecast vendor delivers,
or the quantiles a resource-adequacy study needs to size reserves.

Percentiles are fractions in [0, 1] by convention. They are stored on the
series, so a reader gets the labels back with the values and never has to assume
which quantile a column is.

The stored array is `(percentiles, horizon steps, window count)` - the
percentile axis is prepended to the deterministic layout. Two windows tile a
daylight day here: issued at 06:00 and 12:00, six hours each, `interval` equal to
`horizon` so nothing overlaps.

The physical story is the one every solar forecaster knows. The band is narrow at
dawn and dusk, where output is near zero and near-certain, and opens up through
the morning ramp as a cloud field starts to decide how much of a 60 MW plant
shows up. Around solar noon it *narrows again on the upside only*: the p50 is
already close to clear-sky output, and a plant cannot exceed its rating, so the
uncertainty is one-sided. Clipping the band at 1.0 per-unit below is not a
modelling convenience - it is that ceiling.
"""

from datetime import timedelta

import numpy as np
import polars as pl

from infrastore import OwnerCategory, Probabilistic, Store

from _shared import DAY_START, SUNDANCE_PV, solar_availability

RESOLUTION = timedelta(hours=1)
HORIZON = timedelta(hours=6)
INTERVAL = timedelta(hours=6)
COUNT = 2
PERCENTILES = [0.1, 0.5, 0.9]
FIRST_ISSUE_HOUR = 6

horizon_steps = HORIZON // RESOLUTION
availability = solar_availability()

# The p50 is the expected availability; the band around it scales with how much
# output is at stake and widens with lead time into the window.
values = np.empty((len(PERCENTILES), horizon_steps, COUNT), dtype=np.float64)
for window in range(COUNT):
    for step in range(horizon_steps):
        hour = FIRST_ISSUE_HOUR + window * (INTERVAL // RESOLUTION) + step
        median = availability[hour % 24]
        spread = 0.35 * median * (1.0 + 0.15 * step)
        for index, percentile in enumerate(PERCENTILES):
            # A symmetric band about the median, clipped to the physical [0, 1]
            # range - a plant cannot generate more than its rating or less than
            # nothing.
            offset = (percentile - 0.5) * 2.0 * spread
            values[index, step, window] = round(np.clip(median + offset, 0.0, 1.0), 4)

store = Store.create(in_memory=True)

forecast = Probabilistic(
    DAY_START + timedelta(hours=FIRST_ISSUE_HOUR),
    RESOLUTION,
    HORIZON,
    INTERVAL,
    COUNT,
    PERCENTILES,
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
)
print(f"added {SUNDANCE_PV.type} '{SUNDANCE_PV.name}': id={series_id}")

read_back = store.read_by_id(series_id)
print(f"percentiles={read_back.percentiles} count={read_back.count}")

data = np.asarray(read_back.data)
rows = [
    {
        "issue_time": DAY_START
        + timedelta(hours=FIRST_ISSUE_HOUR + window * (INTERVAL // RESOLUTION)),
        "timestamp": DAY_START
        + timedelta(hours=FIRST_ISSUE_HOUR + window * (INTERVAL // RESOLUTION) + step),
        "percentile": percentile,
        # Back to MW, since the values are per-unit of the plant's rating.
        "mw": round(float(data[index, step, window]) * SUNDANCE_PV.base_power_mw, 2),
    }
    for index, percentile in enumerate(read_back.percentiles)
    for step in range(data.shape[1])
    for window in range(data.shape[2])
]
frame = pl.DataFrame(rows)

# One row per forecast hour, with the band the study would plan against.
band = (
    frame.pivot(on="percentile", index=["issue_time", "timestamp"], values="mw")
    .rename({"0.1": "p10_mw", "0.5": "p50_mw", "0.9": "p90_mw"})
    .with_columns((pl.col("p90_mw") - pl.col("p10_mw")).round(2).alias("band_mw"))
    .sort("timestamp")
)
print("\np10/p50/p90 availability of a 60 MW plant, by forecast hour")
print(band)
