"""Shared components, profiles, and display helpers for the runnable examples.

Every example describes the same three-component power system:

    owner_id  owner_type          name           base power
    101       ThermalGenerator    solitude       100 MW gas combustion turbine
    102       SolarPlant          sundance_pv     60 MW utility-scale solar plant
    201       Load                bus_a_load     150 MW distribution feeder

infrastore stores time series, not components. A series is filed against the
component that owns it by `owner_id` + `owner_type` + `owner_category`, and the
store never resolves those into anything - your modeling application owns the
components, and infrastore owns the arrays and the catalog rows pointing at
them. So `owner_id` is whatever integer id your application gives a component,
`owner_type` is the name of its *concrete* type (not an abstract base class -
the store is only matching strings, and "Generator" will not distinguish a
thermal unit from a wind farm), and `owner_category` is `Component` as opposed
to `SupplementalAttribute`, the other kind of owner a series can have.

Three descriptors travel with every series and are worth setting even though the
store never acts on them:

  * `units`          - the label for the values, e.g. "MW", "$/MWh".
  * `quantity_kind`  - what is being measured, e.g. "ActivePower". Dimensional
                       analysis cannot separate active from reactive power, so
                       this is the only record of which one you have - and the
                       only record of what per-unit values measure.
  * `unit_system`    - "natural_units" if the values are already MW/MVar, or
                       "component_base" if they are per-unit against the owning
                       component's own base power. Nothing rescales them: the
                       label says which basis the modeler put the values on, and
                       converting a per-unit series back to MW needs the base
                       power, which lives on the component, not here.

`component_field` names the field on the owning component whose value these
values are the time-varying form of, where `name` only says which series this
is. They often coincide, and deliberately do not have to: what bounds a
component's output is called something different on each kind of component -
a plant's ceiling is usually its `rating`, a load's its `max_active_power` -
while a set of series driving all of them may share one name.
"""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any, NamedTuple

import numpy as np
import polars as pl


class Component(NamedTuple):
    """A component in the consumer's object model, as infrastore sees it."""

    id: int
    type: str
    name: str
    # The device's own base power in MW - what a `component_base` series is
    # per-unit *of*, and not the same thing as a rating, which many models store
    # per-unit on this base.
    base_power_mw: float


SOLITUDE = Component(101, "ThermalGenerator", "solitude", 100.0)
SUNDANCE_PV = Component(102, "SolarPlant", "sundance_pv", 60.0)
BUS_A_LOAD = Component(201, "Load", "bus_a_load", 150.0)

COMPONENTS = [SOLITUDE, SUNDANCE_PV, BUS_A_LOAD]

# A summer weekday, on the hour, in UTC. Timestamps are stored to millisecond
# precision, matching Julia's DateTime; the store records the *spelling* of a
# timestamp (UTC here) alongside the instant, so a read hands back what the write
# declared rather than relabelling everything UTC.
DAY_START = datetime(2030, 7, 1, tzinfo=timezone.utc)
HOURS_PER_DAY = 24


def hours(count: int = HOURS_PER_DAY) -> np.ndarray:
    """Hour-of-day for `count` consecutive hourly steps starting at `DAY_START`."""
    return np.arange(count) % 24


def solar_availability(count: int = HOURS_PER_DAY) -> np.ndarray:
    """Per-unit PV output over a day: zero overnight, peaking near solar noon.

    Per-unit on the plant's own base power - a real per-device quantity that
    happens to be dimensionless, which `unit_system="component_base"` records.
    That is different from a normalized 0-1 *shape* meant to be multiplied by a
    rating at read time. The store has no notion of a scaling factor and will
    not apply one, so if your values need scaling to mean anything, scale them
    before writing.
    """
    daylight = np.sin((hours(count) - 6.0) * np.pi / 12.0)
    return np.round(np.clip(daylight, 0.0, None) * 0.95, 4)


def feeder_load_mw(peak_mw: float, count: int = HOURS_PER_DAY) -> np.ndarray:
    """A double-peaked summer feeder load in MW: overnight trough, evening peak."""
    hour = hours(count)
    shape = (
        0.58
        + 0.15 * np.sin((hour - 8.0) * np.pi / 12.0)
        + 0.32 * np.exp(-(((hour - 19.0) / 3.0) ** 2))
    )
    return np.round(peak_mw * np.clip(shape, 0.0, 1.0), 2)


def thermal_capacity_mw(base_power_mw: float, count: int = HOURS_PER_DAY) -> np.ndarray:
    """Ambient-derated capacity of a gas turbine in MW.

    A combustion turbine loses output as the air warms, so its usable capacity is
    a time series even though its nameplate rating is a scalar.
    """
    derate = 0.08 * np.exp(-(((hours(count) - 16.0) / 4.0) ** 2))
    return np.round(base_power_mw * (1.0 - derate), 2)


def static_frame(series: Any) -> pl.DataFrame:
    """Timestamp/value table for any static series, via its Arrow representation."""
    return pl.from_arrow(series.to_arrow())


def deterministic_frame(series: Any) -> pl.DataFrame:
    """One row per (window, timestep) of a deterministic forecast.

    `to_arrow_windows()` keys each window's table by its issue time - the instant
    the forecast was made - so flattening them keeps the vintage of every value.
    """
    frames = [
        pl.from_arrow(table).with_columns(pl.lit(issue_time).alias("issue_time"))
        for issue_time, table in series.to_arrow_windows().items()
    ]
    return pl.concat(frames).select("issue_time", "timestamp", "value")
