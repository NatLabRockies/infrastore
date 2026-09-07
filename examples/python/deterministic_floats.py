"""Rolling forecasts: resolution, horizon, interval, and count.

A forecast is not one timeline. It is a stack of *windows*, each one a forecast
made at a particular instant, looking a fixed distance ahead. Four fields
describe the stack, and getting them straight is most of understanding how
forecasts are stored:

    resolution  the step *within* a window        - 1 hour here
    horizon     how far ahead one window looks    - 4 hours
    interval    the step between window starts    - 1 hour
    count       how many windows are stored       - 6

Because `interval` (1h) is shorter than `horizon` (4h), the windows **overlap**:
18:00 is forecast four times, by the runs issued at 15:00, 16:00, 17:00 and
18:00, and those four values differ. That is the whole point of a rolling
forecast, and it is why a forecast cannot be flattened into a single series. A
day-ahead forecast is the same structure with `horizon = interval = 24h`, where
the windows tile instead of overlapping.

The stored array is shaped `(horizon steps, window count)`: each column is one
window, each row a step into it. `to_arrow_windows()` hands the windows back
keyed by issue time, which is what `deterministic_frame` flattens below.

The example forecasts the feeder's load. Each successive run sees the evening
peak more clearly, so the forecast error shrinks as the peak approaches.
"""

from datetime import timedelta

import numpy as np
import polars as pl

from infrastore import Deterministic, OwnerCategory, Store

from _shared import BUS_A_LOAD, DAY_START, deterministic_frame, feeder_load_mw

RESOLUTION = timedelta(hours=1)
HORIZON = timedelta(hours=4)
INTERVAL = timedelta(hours=1)
COUNT = 6

# The load that actually materializes, hour by hour, as the ground truth the
# forecasts are approaching.
ACTUAL = feeder_load_mw(BUS_A_LOAD.base_power_mw)

# The first window is issued at 15:00 and looks at 15:00-18:00; the sixth at
# 20:00. Together they walk the forecast through the evening peak.
PEAK_HOUR = int(ACTUAL.argmax())
FIRST_ISSUE_HOUR = 15
first_issue = DAY_START + timedelta(hours=FIRST_ISSUE_HOUR)

horizon_steps = HORIZON // RESOLUTION
rng = np.random.default_rng(20300701)
windows = np.empty((horizon_steps, COUNT), dtype=np.float64)
for window in range(COUNT):
    issue_hour = FIRST_ISSUE_HOUR + window
    for step in range(horizon_steps):
        target_hour = issue_hour + step
        # Forecast error grows with lead time: about 1% per hour ahead.
        lead_error = 0.01 * step * ACTUAL[target_hour % 24]
        windows[step, window] = round(
            ACTUAL[target_hour % 24] + rng.normal(0.0, lead_error + 0.5), 2
        )

store = Store.create(in_memory=True)

forecast = Deterministic(
    first_issue,  # start of the first window
    RESOLUTION,
    HORIZON,
    INTERVAL,
    COUNT,
    windows,
    "max_active_power",
    units="MW",
    quantity_kind="ActivePower",
    unit_system="natural_units",
    component_field="max_active_power",
)
series_id = store.add_time_series(
    owner_id=BUS_A_LOAD.id,
    owner_type=BUS_A_LOAD.type,
    owner_category=OwnerCategory.Component,
    time_series=forecast,
)
print(f"added {BUS_A_LOAD.type} '{BUS_A_LOAD.name}': id={series_id}")

metadata = store.get_metadata_by_id(series_id)
print(
    f"resolution={metadata['resolution']} horizon={metadata['horizon']} "
    f"interval={metadata['interval']} count={metadata['count']}"
)

read_back = store.read_by_id(series_id)
frame = deterministic_frame(read_back)
print(f"\n{len(frame)} (window, timestep) values")
print(frame.head(8))

# The overlap, made visible: every forecast anyone ever made of the peak hour,
# oldest first, against what the hour actually turned out to be.
peak = DAY_START + timedelta(hours=PEAK_HOUR)
print(f"\nforecasts of the {peak:%H:%M} peak (actual {ACTUAL[PEAK_HOUR]} MW)")
print(
    frame.filter(pl.col("timestamp") == peak)
    .with_columns(
        ((pl.col("timestamp") - pl.col("issue_time")).dt.total_hours()).alias(
            "lead_hours"
        ),
        (pl.col("value") - ACTUAL[PEAK_HOUR]).round(2).alias("error_mw"),
    )
    .select("issue_time", "lead_hours", "value", "error_mw")
    .sort("issue_time")
)
