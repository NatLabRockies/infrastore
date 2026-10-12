"""`export_sqlite` / `import_sqlite`: the CLI's `export -f sqlite` and
`add --sqlite` from Python, over the same tables."""

import sqlite3
from datetime import timedelta

import numpy as np
import pytest

from infrastore import TimeSeriesError, TimeSeriesType, Store
from conftest import add_on_grid, t


@pytest.fixture
def source():
    store = Store.create(in_memory=True)
    add_on_grid(store, 1, 0, 24)
    add_on_grid(store, 2, 0, 24, name="reactive_power")
    return store


def test_round_trip(source, tmp_path):
    db = str(tmp_path / "out.db")
    written = source.export_sqlite(db, table_prefix="run1_")
    assert [p["values_table"] for p in written] == ["run1_SingleTimeSeries_f64_utc_values"]
    assert written[0]["arrays_table"] == "run1_SingleTimeSeries_f64_utc_arrays"
    # Both series hold the same values, so the partition holds one array.
    assert (written[0]["series"], written[0]["arrays"], written[0]["rows"]) == (2, 1, 24)

    target = Store.create(in_memory=True)
    # The prefix scopes the import: nothing was exported without one.
    with pytest.raises(TimeSeriesError):
        target.import_sqlite(db)
    ids = target.import_sqlite(db, table_prefix="run1_")
    assert len(ids) == 2
    rows = target.list_metadata()
    assert sorted(r["id"] for r in rows) == sorted(ids)
    assert {(r["owner_id"], r["name"]) for r in rows} == {
        (1, "active_power"),
        (2, "reactive_power"),
    }
    np.testing.assert_array_equal(target.read_by_id(ids[0]).data, np.arange(24.0))


def test_derived_series_are_exported_only_when_asked_for(source, tmp_path):
    source.transform_single_time_series(timedelta(hours=2), timedelta(hours=1))
    written = source.export_sqlite(str(tmp_path / "plain.db"))
    assert {p["time_series_type"] for p in written} == {"SingleTimeSeries"}
    with pytest.raises(TimeSeriesError):
        source.export_sqlite(
            str(tmp_path / "named.db"),
            time_series_type=TimeSeriesType.DeterministicSingleTimeSeries,
        )
    written = source.export_sqlite(str(tmp_path / "all.db"), include_derived=True)
    assert {p["time_series_type"] for p in written} == {
        "SingleTimeSeries",
        "DeterministicSingleTimeSeries",
    }


def test_filter_and_time_range_narrow_the_export(source, tmp_path):
    db = str(tmp_path / "out.db")
    written = source.export_sqlite(db, name="active_power", time_range=(t(6), t(12)))
    assert (written[0]["series"], written[0]["rows"]) == (1, 6)
    assert source.export_sqlite(str(tmp_path / "none.db"), name="absent") == []
    assert not (tmp_path / "none.db").exists()


def test_import_is_all_or_nothing_and_checks_the_hash(source, tmp_path):
    db = str(tmp_path / "out.db")
    source.export_sqlite(db)
    # A second export into the same names is refused, not merged.
    with pytest.raises(TimeSeriesError):
        source.export_sqlite(db)
    # The source already holds both series, so re-importing them is a duplicate.
    with pytest.raises(TimeSeriesError):
        source.import_sqlite(db)
    assert len(source.list_metadata()) == 2

    with sqlite3.connect(db) as conn:
        conn.execute('UPDATE "SingleTimeSeries_f64_utc_values" SET value = value + 1')
    target = Store.create(in_memory=True)
    with pytest.raises(TimeSeriesError):
        target.import_sqlite(db)
    assert target.list_metadata() == []
    ids = target.import_sqlite(db, skip_checksum=True)
    np.testing.assert_array_equal(target.read_by_id(ids[0]).data, np.arange(24.0) + 1)
