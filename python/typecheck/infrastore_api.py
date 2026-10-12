"""Static contracts for the public infrastore Python API.

Run with ``ty check python/typecheck`` after installing the built extension.
"""

from datetime import timedelta
from typing import assert_type

from infrastore import (
    DtypeName,
    FeatureMap,
    ForecastReaderTimeline,
    OwnerCategory,
    StaticReaderGrid,
    StaticReaderGroup,
    Store,
    TimeSeriesAddItem,
    TimeSeriesData,
    TimeSeriesCountSummaryRow,
    TimeSeriesMetadata,
    TimeSeriesType,
    TimeSeriesTypeName,
)


def check_store_types(store: Store, series: TimeSeriesData) -> None:
    additions: list[TimeSeriesAddItem] = [
        {
            "owner_id": 1,
            "owner_type": "Generator",
            "owner_category": OwnerCategory.Component,
            "time_series": series,
            "features": {"scenario": "high", "year": 2030},
        }
    ]
    ids = store.add_time_series_bulk(additions)
    assert_type(ids, list[int])

    rows = store.list_metadata()
    assert_type(rows, list[TimeSeriesMetadata])
    if rows:
        row = rows[0]
        assert_type(row["features"], FeatureMap)
        assert_type(row["id"], int)
        assert_type(row["time_series_type"], TimeSeriesTypeName)

    exact_exists = store.has_exact_time_series(
        owner_id=1,
        owner_category=OwnerCategory.Component,
        name="load",
        time_series_type=TimeSeriesType.SingleTimeSeries,
        resolution=timedelta(hours=1),
        interval=None,
        features={"scenario": "high", "year": 2030},
    )
    assert_type(exact_exists, bool)

    count_summary = store.time_series_count_summary()
    assert_type(count_summary, list[TimeSeriesCountSummaryRow])
    if count_summary:
        assert_type(count_summary[0]["timestamps_hash"], str | None)

    static_reader = store.build_static_reader(timedelta(hours=1))
    assert_type(static_reader.grid(), StaticReaderGrid)
    groups = static_reader.groups()
    assert_type(groups, list[StaticReaderGroup])
    if groups:
        assert_type(groups[0]["dtype"], DtypeName)
        assert_type(groups[0]["ids"], list[int])

    forecast_reader = store.build_forecast_reader(
        TimeSeriesType.Deterministic, timedelta(hours=1)
    )
    assert_type(forecast_reader.timeline(), ForecastReaderTimeline)
