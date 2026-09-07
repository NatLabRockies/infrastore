"""Features: many series for one component field, told apart by their tags.

A capacity-expansion or resource-adequacy study rarely has *one* load forecast.
It has the same feeder's demand under several demand-growth scenarios, drawn
from several historical weather years, for several model years. All of them are
`max_active_power` on the same load component, so the name alone cannot separate
them.

Features do. A feature map is part of a series' *identity*: two series that
differ only by a feature are two distinct series, not a duplicate write. They
are also the query vocabulary - `list_metadata(features=...)` selects the subset
a run wants, which is how a study picks "the 2012 weather year under high
electrification" out of a store holding all of them.

Feature values may be int, float, bool, or str. Names that would collide with a
field of a series (`name`, `resolution`, `units`, ...) are rejected, because
consumers routinely spread a feature map into keyword arguments.
"""

from datetime import timedelta

import polars as pl

from infrastore import OwnerCategory, SingleTimeSeries, Store

from _shared import BUS_A_LOAD, DAY_START, feeder_load_mw, static_frame

store = Store.create(in_memory=True)

# Growth multipliers applied to the same underlying feeder shape. A real study
# would read a distinct profile per combination; the point here is the tagging.
SCENARIOS = {"reference": 1.00, "high_electrification": 1.18}
WEATHER_YEARS = (2007, 2012)

for scenario, growth in SCENARIOS.items():
    for weather_year in WEATHER_YEARS:
        # A hotter weather year lifts the evening air-conditioning peak.
        peak = BUS_A_LOAD.base_power_mw * growth * (1.04 if weather_year == 2012 else 1.0)
        series = SingleTimeSeries(
            DAY_START,
            timedelta(hours=1),
            feeder_load_mw(peak),
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
            time_series=series,
            features={
                "scenario": scenario,
                "weather_year": weather_year,
                "model_year": 2030,
            },
        )
        print(f"added {scenario}/{weather_year}: id={series_id}")

# All four series share one owner, one name, and one grid; only the tags differ.
print(f"\nseries stored for {BUS_A_LOAD.name}: {len(store.list_metadata())}")

# A study run selects the combination it models. Features not named in the
# filter are unconstrained, so this matches both weather years.
matches = store.list_metadata(
    owner_id=BUS_A_LOAD.id,
    owner_category=OwnerCategory.Component,
    features={"scenario": "high_electrification"},
)
print("\nhigh-electrification rows")
print(pl.DataFrame(matches).select("id", "owner_id", "name", "features", "length"))

for metadata in matches:
    series = store.read_by_id(metadata["id"])
    frame = static_frame(series)
    peak_mw = frame["value"].max()
    print(f"\nid={metadata['id']} features={metadata['features']} peak={peak_mw} MW")
    print(frame.head(3))
