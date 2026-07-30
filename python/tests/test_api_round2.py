"""Round-2 API additions: exception hierarchy, dunders, name_glob, kw-only."""

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest
import infrastore
from infrastore import (
    OwnerCategory,
    SingleTimeSeries,
    Store,
)

T0 = datetime(2024, 1, 1, tzinfo=timezone.utc)
HOUR = timedelta(hours=1)


def sts(name, base=0.0, length=8):
    return SingleTimeSeries(T0, HOUR, np.arange(length, dtype=np.float64) + base, name)


def populated():
    store = Store.create(in_memory=True)
    for owner, name, base in [
        (1, "wind_speed", 1.0),
        (1, "wind_dir", 2.0),
        (2, "solar_irradiance", 3.0),
        (2, "Wind_gust", 4.0),
    ]:
        store.add_time_series(owner, "Generator", OwnerCategory.Component, sts(name, base))
    return store


class TestExceptions:
    def test_hierarchy(self):
        for name in (
            "IoError",
            "ConnectionError",
            "IncompatibleFormatError",
            "IncompatibleForecastError",
            "StorageError",
        ):
            exc = getattr(infrastore, name)
            assert issubclass(exc, infrastore.TimeSeriesError)

    def test_bad_period_string_raises_invalid_parameter(self):
        with pytest.raises(infrastore.InvalidParameterError):
            SingleTimeSeries(T0, "not-a-period", np.zeros(4), "x")

    def test_wrong_period_type_raises_type_error(self):
        with pytest.raises(TypeError):
            SingleTimeSeries(T0, 12345, np.zeros(4), "x")

    def test_storage_error_on_unopenable_path(self):
        # A missing parent directory fails in the catalog layer, staying
        # inside the library's exception hierarchy.
        with pytest.raises(infrastore.StorageError):
            Store.open("/nonexistent/dir/x.h5")


class TestDunders:
    def test_eq_and_len_on_series(self):
        a = sts("load")
        b = sts("load")
        c = sts("load", base=1.0)
        assert a == b
        assert not (a == c)
        assert len(a) == 8

    def test_round_tripped_series_equal(self):
        store = Store.create(in_memory=True)
        original = sts("load")
        key = store.add_time_series(1, "Gen", OwnerCategory.Component, original)
        fetched = store.get_time_series(key)
        assert fetched == original

    def test_forecast_len_is_count(self):
        from infrastore import Deterministic

        data = np.arange(6, dtype=np.float64).reshape(2, 3)
        det = Deterministic(T0, HOUR, timedelta(hours=2), HOUR, 3, data, "fc")
        assert len(det) == 3

    def test_reader_reprs(self):
        store = populated()
        reader = store.build_static_reader(HOUR)
        assert "StaticReader" in repr(reader)
        assert "length=8" in repr(reader)


class TestNameGlob:
    def test_list_names_glob(self):
        store = populated()
        assert store.list_names(name_glob="wind_*") == ["wind_dir", "wind_speed"]
        # Case-sensitive.
        assert len(store.list_keys(name_glob="Wind*")) == 1

    def test_glob_composes_with_exact_name(self):
        store = populated()
        assert len(store.list_keys(name="wind_speed", name_glob="wind_*")) == 1
        assert store.list_keys(name="wind_speed", name_glob="solar_*") == []

    def test_remove_by_filter_glob(self):
        store = populated()
        assert store.remove_by_filter(name_glob="wind_*") == 2
        assert store.list_names() == ["Wind_gust", "solar_irradiance"]

    def test_reader_builder_glob(self):
        store = populated()
        reader = store.build_static_reader(HOUR, name_glob="wind_*")
        assert sum(len(g["keys"]) for g in reader.groups()) == 2


class TestReservedFeatureNames:
    def test_reserved_feature_name_raises_invalid_parameter(self):
        store = Store.create(in_memory=True)
        for name in ("name", "resolution", "owner_id", "ext"):
            with pytest.raises(infrastore.InvalidParameterError, match=name):
                store.add_time_series(
                    1,
                    "Generator",
                    OwnerCategory.Component,
                    sts("load"),
                    features={"model_year": 2030, name: "shadowed"},
                )
        assert store.get_time_series_keys(1, OwnerCategory.Component) == []

    def test_reserved_feature_name_raises_in_bulk_add(self):
        store = Store.create(in_memory=True)
        with pytest.raises(infrastore.InvalidParameterError, match="horizon"):
            store.add_time_series_bulk(
                [
                    {
                        "owner_id": 1,
                        "owner_type": "Generator",
                        "owner_category": OwnerCategory.Component,
                        "time_series": sts("good"),
                    },
                    {
                        "owner_id": 2,
                        "owner_type": "Generator",
                        "owner_category": OwnerCategory.Component,
                        "time_series": sts("bad"),
                        "features": {"horizon": "PT2H"},
                    },
                ]
            )
        assert store.get_time_series_keys(1, OwnerCategory.Component) == []
        assert store.get_time_series_keys(2, OwnerCategory.Component) == []

    def test_near_miss_feature_names_are_accepted(self):
        # The rule is exact and case-sensitive.
        store = Store.create(in_memory=True)
        features = {"Name": "load", "resolution_hours": 1, "model_year": 2030}
        key = store.add_time_series(
            1, "Generator", OwnerCategory.Component, sts("load"), features=features
        )
        assert key.features == features


class TestKeywordOnly:
    def test_filter_args_are_keyword_only(self):
        store = populated()
        with pytest.raises(TypeError):
            store.list_names(1)  # owner_id positionally

    def test_add_time_series_options_are_keyword_only(self):
        store = Store.create(in_memory=True)
        with pytest.raises(TypeError):
            store.add_time_series(
                1, "Gen", OwnerCategory.Component, sts("load"), {"scenario": "high"}
            )
