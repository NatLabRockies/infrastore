"""Helpers shared by the test modules: import them with ``from conftest import ...``."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np

from infrastore import OwnerCategory, SingleTimeSeries, Store

HOUR = timedelta(hours=1)


def t(hour: int) -> datetime:
    """``hour`` hours after 2024-01-01T00:00Z."""
    return datetime(2024, 1, 1, tzinfo=timezone.utc) + hour * HOUR


def hourly_series(
    base: float = 100.0, length: int = 24, name: str = "load", start: int = 0
) -> SingleTimeSeries:
    """An hourly series from ``t(start)`` valued ``base, base + 1, ...``, so equal
    bases share an array."""
    data = np.arange(length, dtype=np.float64) + base
    return SingleTimeSeries(t(start), HOUR, data, name)


def add(store: Store, owner_id: int, base: float = 100.0) -> int:
    """Add ``hourly_series(base)`` to Generator ``owner_id``."""
    return store.add_time_series(
        owner_id=owner_id,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=hourly_series(base),
    )


def add_on_grid(
    store: Store, owner_id: int, start: int, length: int, name: str = "active_power"
) -> int:
    """Add an hourly series from ``t(start)`` whose values equal their own hour,
    so a read proves which row it hit."""
    return store.add_time_series(
        owner_id=owner_id,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=hourly_series(start, length, name, start),
    )
