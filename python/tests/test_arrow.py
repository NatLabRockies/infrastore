"""`to_arrow()` on the three static time-series types.

The interesting property is that Arrow's `timestamp(unit, tz)` is the same shape
as the store's own model -- an instant plus the spelling it was written in -- so
the mapping is total and lossless in both halves. Most of what is checked here
is that the spelling survives, that the grid is materialized on the calendar
rather than by multiplication, and that a value's shape and dtype cross intact.

pyarrow is an optional extra (`infrastore[arrow]`), so the whole module skips
when it is absent.
"""

from __future__ import annotations

import sys
from datetime import datetime, timedelta, timezone
from zoneinfo import ZoneInfo

import numpy as np
import pytest

from infrastore import (
    Deterministic,
    InvalidParameterError,
    NonSequentialTimeSeries,
    OwnerCategory,
    PersistentTimeSeries,
    SingleTimeSeries,
    Store,
)

pa = pytest.importorskip("pyarrow", reason="to_arrow() needs the `arrow` extra")

UTC_START = datetime(2024, 1, 1, tzinfo=timezone.utc)


def hourly(data: np.ndarray, name: str = "load", **descriptors) -> SingleTimeSeries:
    return SingleTimeSeries(UTC_START, "PT1H", data, name, **descriptors)


def breakpoints() -> list[datetime]:
    return [
        datetime(2024, 1, 1, tzinfo=timezone.utc),
        datetime(2024, 1, 5, tzinfo=timezone.utc),
        datetime(2024, 3, 9, tzinfo=timezone.utc),
    ]


# ---- Shape and content -----------------------------------------------------


def test_columns_are_timestamp_and_value():
    table = hourly(np.arange(24.0)).to_arrow()
    assert table.column_names == ["timestamp", "value"]
    assert table.num_rows == 24
    assert table["value"].to_pylist() == list(np.arange(24.0))


def test_timestamp_column_walks_the_grid():
    table = hourly(np.arange(3.0)).to_arrow()
    assert table["timestamp"].to_pylist() == [
        UTC_START + timedelta(hours=k) for k in range(3)
    ]


def test_monthly_grid_steps_on_the_calendar():
    """The reason `to_arrow` materializes the grid in the core rather than
    leaving the caller to multiply a span by an index: a `P1M` resolution lands
    on month ends, which no fixed span reproduces."""
    series = SingleTimeSeries(
        datetime(2024, 1, 31, tzinfo=timezone.utc), "P1M", np.arange(4.0), "monthly"
    )
    assert series.to_arrow()["timestamp"].to_pylist() == [
        datetime(2024, 1, 31, tzinfo=timezone.utc),
        datetime(2024, 2, 29, tzinfo=timezone.utc),
        datetime(2024, 3, 31, tzinfo=timezone.utc),
        datetime(2024, 4, 30, tzinfo=timezone.utc),
    ]


def test_timestamps_property_agrees_with_the_table():
    series = SingleTimeSeries(
        datetime(2024, 1, 31, tzinfo=timezone.utc), "P1M", np.arange(4.0), "monthly"
    )
    assert series.timestamps == series.to_arrow()["timestamp"].to_pylist()


def test_empty_series_gives_an_empty_table():
    table = hourly(np.zeros(0)).to_arrow()
    assert table.num_rows == 0
    assert table.schema.field("value").type == pa.float64()


# ---- Time-reference spelling ----------------------------------------------


@pytest.mark.parametrize(
    ("start", "expected_tz"),
    [
        (datetime(2024, 1, 1, tzinfo=timezone.utc), "UTC"),
        (datetime(2024, 1, 1, tzinfo=ZoneInfo("America/Denver")), "America/Denver"),
        (datetime(2024, 1, 1, tzinfo=timezone(timedelta(hours=-7))), "-07:00"),
    ],
)
def test_zoned_spellings_become_the_arrow_zone(start, expected_tz):
    series = SingleTimeSeries(start, "PT1H", np.arange(3.0), "s")
    column_type = series.to_arrow().schema.field("timestamp").type
    assert column_type == pa.timestamp("ms", expected_tz)


def test_zoneless_series_has_no_arrow_zone():
    """A naive datetime names a wall clock, not an instant. Labelling it UTC on
    the way out would be the store inventing a claim the writer never made."""
    series = SingleTimeSeries(datetime(2024, 1, 1), "PT1H", np.arange(3.0), "naive")
    table = series.to_arrow()
    assert table.schema.field("timestamp").type == pa.timestamp("ms")
    assert table["timestamp"][0].as_py() == datetime(2024, 1, 1)


def test_an_unresolvable_zone_warns_and_falls_back_to_utc():
    """The core validates a zone name's shape but never resolves it, so a series
    can name a zone this interpreter's tz database does not have. The instants
    are intact either way, and failing a conversion over a label nobody can
    resolve would be worse than reporting UTC — matching what reading
    `initial_timestamp` already does."""
    # The constructor warns for the same reason, so it is inside the block too;
    # left outside it would leak into the report as an unraised warning.
    with pytest.warns(UserWarning, match="Mars/Olympus"):
        series = SingleTimeSeries(
            UTC_START, "PT1H", np.arange(3.0), "s", time_reference="Mars/Olympus"
        )
        table = series.to_arrow()
    assert table.schema.field("timestamp").type == pa.timestamp("ms", "UTC")
    assert table["timestamp"][0].as_py() == UTC_START


def test_zoned_instants_are_the_same_instants():
    """Two series naming one instant in different spellings hold the same
    milliseconds; only the label differs."""
    utc = SingleTimeSeries(
        datetime(2024, 1, 1, 7, tzinfo=timezone.utc), "PT1H", np.arange(2.0), "u"
    )
    denver = SingleTimeSeries(
        datetime(2024, 1, 1, tzinfo=ZoneInfo("America/Denver")),
        "PT1H",
        np.arange(2.0),
        "d",
    )
    assert utc.to_arrow()["timestamp"][0].as_py() == (
        denver.to_arrow()["timestamp"][0].as_py()
    )


# ---- Values ----------------------------------------------------------------


@pytest.mark.parametrize(
    ("dtype", "arrow_type"),
    [
        ("float64", pa.float64()),
        ("float32", pa.float32()),
        ("int64", pa.int64()),
        ("int32", pa.int32()),
        ("uint64", pa.uint64()),
        ("bool", pa.bool_()),
    ],
)
def test_every_dtype_crosses(dtype, arrow_type):
    series = hourly(np.ones(3, dtype=dtype))
    table = series.to_arrow()
    assert table.schema.field("value").type == arrow_type
    expected = [True] * 3 if dtype == "bool" else [1] * 3
    assert table["value"].to_pylist() == expected


def test_multidimensional_values_become_nested_fixed_size_lists():
    values = np.arange(24.0).reshape(4, 2, 3)
    table = hourly(values).to_arrow()
    assert table.schema.field("value").type == pa.list_(pa.list_(pa.float64(), 3), 2)
    assert table["value"].to_pylist() == values.tolist()


def test_one_dimensional_element_shape():
    values = np.arange(6.0).reshape(3, 2)
    table = hourly(values).to_arrow()
    assert table.schema.field("value").type == pa.list_(pa.float64(), 2)
    assert table["value"].to_pylist() == values.tolist()


def test_composite_elements_stay_in_their_stored_packing():
    """A `piecewise_linear` series keeps the flat packing the store holds; the
    schema metadata names it so `decode_element_values` can unpack it."""
    values = np.array([[0.0, 1.0, 1.0, 3.0], [0.0, 2.0, 1.0, 4.0]])
    series = hourly(values, name="cost", element_type="piecewise_linear")
    table = series.to_arrow()
    assert table.schema.metadata[b"element_type"] == b"piecewise_linear"
    assert table.schema.field("value").type == pa.list_(pa.float64(), 4)


# ---- Schema metadata -------------------------------------------------------


def test_descriptors_ride_in_the_schema_metadata():
    series = hourly(
        np.arange(3.0),
        name="load",
        units="MW",
        quantity_kind="ActivePower",
        unit_system="natural_units",
        component_field="max_active_power",
        application_data='{"k": 1}',
    )
    metadata = series.to_arrow().schema.metadata
    assert metadata[b"name"] == b"load"
    assert metadata[b"units"] == b"MW"
    assert metadata[b"quantity_kind"] == b"ActivePower"
    assert metadata[b"unit_system"] == b"natural_units"
    assert metadata[b"component_field"] == b"max_active_power"
    assert metadata[b"application_data"] == b'{"k": 1}'
    assert metadata[b"time_series_type"] == b"SingleTimeSeries"
    assert metadata[b"time_reference"] == b"utc"
    assert metadata[b"resolution"] == b"PT1H"


def test_element_shape_is_always_written():
    """A fact about the data, not a label, so it is written even when empty --
    and the CLI's Parquet export writes the same key, because the two producers
    write one schema."""
    assert hourly(np.arange(3.0)).to_arrow().schema.metadata[b"element_shape"] == b"[]"

    values = np.arange(24.0).reshape(4, 2, 3)
    assert hourly(values).to_arrow().schema.metadata[b"element_shape"] == b"[2,3]"


def test_a_forecast_window_reports_its_own_element_shape():
    """A forecast's stored shape is `[H, count, *E]`, so the per-window element
    shape is what follows *both* -- one axis further in than a static series."""
    values = np.arange(2 * 3 * 4.0).reshape(2, 3, 4)
    forecast = Deterministic(UTC_START, "PT1H", "PT2H", "PT1H", 3, values, "fc")
    windows = forecast.to_arrow_windows()
    for table in windows.values():
        assert table.schema.metadata[b"element_shape"] == b"[4]"


def test_undeclared_descriptors_are_absent_rather_than_empty():
    """`b"units" in metadata` has to answer "was a label declared?", so an unset
    one must not be written as an empty string."""
    metadata = hourly(np.arange(3.0)).to_arrow().schema.metadata
    assert b"units" not in metadata
    assert b"quantity_kind" not in metadata
    assert b"unit_system" not in metadata
    assert b"component_field" not in metadata
    assert b"application_data" not in metadata


def test_metadata_survives_a_parquet_round_trip(tmp_path):
    pq = pytest.importorskip("pyarrow.parquet")
    table = hourly(np.arange(3.0), units="MW").to_arrow()
    path = tmp_path / "series.parquet"
    pq.write_table(table, path)
    back = pq.read_table(path)
    assert back["value"].to_pylist() == table["value"].to_pylist()
    assert back.schema.metadata[b"units"] == b"MW"


# ---- The irregular types ---------------------------------------------------


def test_non_sequential_table_is_the_stored_vector():
    stamps = breakpoints()
    series = NonSequentialTimeSeries(stamps, np.array([1.0, 2.0, 3.0]), "irregular")
    table = series.to_arrow()
    assert table["timestamp"].to_pylist() == stamps
    assert table["value"].to_pylist() == [1.0, 2.0, 3.0]
    assert table.schema.metadata[b"time_series_type"] == b"NonSequentialTimeSeries"


def test_persistent_table_is_one_row_per_breakpoint():
    """Not one row per instant: the table is the sparse step function as stored,
    and nothing between two breakpoints is filled in."""
    stamps = breakpoints()
    series = PersistentTimeSeries(stamps, np.array([1.0, 2.0, 3.0]), "step")
    table = series.to_arrow()
    assert table.num_rows == len(stamps)
    assert table["timestamp"].to_pylist() == stamps
    assert table.schema.metadata[b"time_series_type"] == b"PersistentTimeSeries"


@pytest.mark.parametrize("cls", [NonSequentialTimeSeries, PersistentTimeSeries])
def test_irregular_metadata_has_no_resolution(cls):
    """An irregular timeline has no constant step, so there is no resolution to
    record -- and an absent key is a truer answer than a fabricated one."""
    series = cls(breakpoints(), np.array([1.0, 2.0, 3.0]), "s")
    assert b"resolution" not in series.to_arrow().schema.metadata


@pytest.mark.parametrize("cls", [NonSequentialTimeSeries, PersistentTimeSeries])
def test_irregular_spelling_survives(cls):
    stamps = [t.astimezone(ZoneInfo("America/Denver")) for t in breakpoints()]
    series = cls(stamps, np.array([1.0, 2.0, 3.0]), "s")
    table = series.to_arrow()
    assert table.schema.field("timestamp").type == pa.timestamp("ms", "America/Denver")
    assert table["timestamp"].to_pylist() == stamps


# ---- Through the store -----------------------------------------------------


def test_series_read_back_from_a_store_converts():
    store = Store.create(in_memory=True)
    series = hourly(np.arange(24.0), units="MW", component_field="max_active_power")
    series_id = store.add_time_series(
        owner_id=1,
        owner_type="ThermalStandard",
        owner_category=OwnerCategory.Component,
        time_series=series,
    )
    table = store.read_by_id(series_id).to_arrow()
    assert table["value"].to_pylist() == list(np.arange(24.0))
    assert table["timestamp"][0].as_py() == UTC_START
    assert table.schema.metadata[b"units"] == b"MW"
    assert table.schema.metadata[b"component_field"] == b"max_active_power"


# ---- Deterministic windows -------------------------------------------------


def forecast(
    *,
    horizon: str = "PT2H",
    interval: str = "PT1H",
    count: int = 3,
    element_shape: tuple[int, ...] = (),
    **descriptors,
) -> Deterministic:
    """`H = 2` by default, so the stored array is `[2, count, *element_shape]`
    and its values enumerate 0, 1, 2, … in C order."""
    # Resolution is always PT1H here, so H is the horizon's hour count.
    steps = int(horizon.removeprefix("PT").removesuffix("H"))
    shape = (steps, count, *element_shape)
    data = np.arange(float(np.prod(shape))).reshape(shape)
    return Deterministic(
        UTC_START, "PT1H", horizon, interval, count, data, "day_ahead", **descriptors
    )


def test_one_entry_per_window_keyed_by_issue_time():
    windows = forecast().to_arrow_windows()
    assert list(windows) == [UTC_START + timedelta(hours=k) for k in range(3)]


def test_dict_iterates_chronologically():
    """Not a sorted container, but insertion-ordered — which Python guarantees,
    so window order is an ordering a caller can rely on."""
    keys = list(forecast(count=5).to_arrow_windows())
    assert keys == sorted(keys)
    assert next(iter(forecast(count=5).to_arrow_windows())) == UTC_START


def test_each_window_is_its_slice_of_the_stored_array():
    """The stored layout is `[H, count, *E]` — window index innermost — so
    window `k` is `data[:, k]`, not a contiguous run."""
    series = forecast(count=3)
    stored = np.asarray(series.data)
    windows = series.to_arrow_windows()
    for k, table in enumerate(windows.values()):
        assert table["value"].to_pylist() == stored[:, k].tolist()


def test_window_rows_step_by_resolution_from_the_issue_time():
    windows = forecast().to_arrow_windows()
    for issue_time, table in windows.items():
        assert table["timestamp"].to_pylist() == [
            issue_time,
            issue_time + timedelta(hours=1),
        ]


def test_overlapping_windows_repeat_their_shared_instants():
    """A day-ahead forecast reissued more often than its horizon overlaps. The
    tables repeat those instants rather than pretending one timeline covers
    them — the whole reason this is a dict of tables and not one table."""
    windows = forecast(horizon="PT3H", interval="PT1H", count=4).to_arrow_windows()
    tables = list(windows.values())
    first, second = tables[0]["timestamp"].to_pylist(), tables[1]["timestamp"].to_pylist()
    assert first[1:] == second[:-1]


def test_window_table_matches_a_single_time_series_table():
    """Each value is shaped exactly like `SingleTimeSeries.to_arrow()`, so one
    window drops into anything that already consumes a static table."""
    window = forecast().to_arrow_windows()[UTC_START]
    static = hourly(np.arange(2.0)).to_arrow()
    assert window.column_names == static.column_names
    assert window.schema.field("timestamp").type == static.schema.field("timestamp").type
    assert window.schema.field("value").type == static.schema.field("value").type


def test_multidimensional_windows_become_fixed_size_lists():
    series = forecast(element_shape=(2,))
    stored = np.asarray(series.data)
    windows = series.to_arrow_windows()
    table = windows[UTC_START]
    assert table.schema.field("value").type == pa.list_(pa.float64(), 2)
    assert table["value"].to_pylist() == stored[:, 0, :].tolist()


def test_window_metadata_carries_the_forecast_parameters():
    metadata = forecast(units="MW").to_arrow_windows()[UTC_START].schema.metadata
    assert metadata[b"time_series_type"] == b"Deterministic"
    assert metadata[b"units"] == b"MW"
    assert metadata[b"resolution"] == b"PT1H"
    assert metadata[b"horizon"] == b"PT2H"
    assert metadata[b"interval"] == b"PT1H"
    assert metadata[b"count"] == b"3"


def test_each_window_records_its_own_issue_time():
    """So a window written to Parquet on its own still knows which one it is."""
    windows = forecast().to_arrow_windows()
    for issue_time, table in windows.items():
        assert table.schema.metadata[b"issue_time"].decode() == issue_time.isoformat()


def test_zoned_forecast_keys_and_columns_keep_the_spelling():
    denver = ZoneInfo("America/Denver")
    series = Deterministic(
        datetime(2024, 1, 1, tzinfo=denver),
        "PT1H",
        "PT2H",
        "PT1H",
        2,
        np.arange(4.0).reshape(2, 2),
        "zoned",
    )
    windows = series.to_arrow_windows()
    assert list(windows)[0] == datetime(2024, 1, 1, tzinfo=denver)
    table = windows[datetime(2024, 1, 1, tzinfo=denver)]
    assert table.schema.field("timestamp").type == pa.timestamp("ms", "America/Denver")


def test_single_window_forecast():
    """`count == 1` allows a zero interval — there is no second window to step
    to — so the dict has exactly one entry and no stepping happens."""
    series = Deterministic(
        UTC_START, "PT1H", "PT2H", "PT0S", 1, np.arange(2.0).reshape(2, 1), "one"
    )
    windows = series.to_arrow_windows()
    assert list(windows) == [UTC_START]
    assert windows[UTC_START]["value"].to_pylist() == [0.0, 1.0]


def test_forecast_read_back_from_a_store_converts():
    store = Store.create(in_memory=True)
    series = forecast(units="MW")
    series_id = store.add_time_series(
        owner_id=1,
        owner_type="ThermalStandard",
        owner_category=OwnerCategory.Component,
        time_series=series,
    )
    windows = store.read_by_id(series_id).to_arrow_windows()
    assert list(windows) == [UTC_START + timedelta(hours=k) for k in range(3)]
    assert windows[UTC_START]["value"].to_pylist() == np.asarray(series.data)[:, 0].tolist()


def test_zoneless_forecast_has_naive_keys_and_unzoned_columns():
    """A series carries one spelling, so the keys can never mix naive and aware
    datetimes — which matters here more than elsewhere, since the two hash fine
    but raise on comparison, and these are dict keys."""
    series = Deterministic(
        datetime(2024, 1, 1), "PT1H", "PT2H", "PT1H", 2, np.arange(4.0).reshape(2, 2), "n"
    )
    windows = series.to_arrow_windows()
    assert list(windows) == [datetime(2024, 1, 1), datetime(2024, 1, 1, 1)]
    assert all(k.tzinfo is None for k in windows)
    table = windows[datetime(2024, 1, 1)]
    assert table.schema.field("timestamp").type == pa.timestamp("ms")


@pytest.mark.parametrize(
    ("dtype", "arrow_type"),
    [("float32", pa.float32()), ("int64", pa.int64()), ("bool", pa.bool_())],
)
def test_forecast_window_dtypes_cross(dtype, arrow_type):
    data = np.ones((2, 2), dtype=dtype)
    series = Deterministic(UTC_START, "PT1H", "PT2H", "PT1H", 2, data, "typed")
    table = series.to_arrow_windows()[UTC_START]
    assert table.schema.field("value").type == arrow_type


def test_a_window_survives_a_parquet_round_trip(tmp_path):
    """Each window is a standalone table, which is the point of stamping it with
    its own issue time: written alone, it still knows which window it is."""
    pq = pytest.importorskip("pyarrow.parquet")
    table = forecast(units="MW").to_arrow_windows()[UTC_START]
    path = tmp_path / "window.parquet"
    pq.write_table(table, path)
    back = pq.read_table(path)
    assert back["value"].to_pylist() == table["value"].to_pylist()
    assert back.schema.metadata[b"issue_time"] == table.schema.metadata[b"issue_time"]
    assert back.schema.metadata[b"units"] == b"MW"


def test_forecast_windows_need_pyarrow_too(monkeypatch):
    monkeypatch.setitem(sys.modules, "pyarrow", None)
    with pytest.raises(ImportError, match=r"infrastore\[arrow\]"):
        forecast().to_arrow_windows()


# ---- The optional dependency ----------------------------------------------


def test_missing_pyarrow_names_the_extra(monkeypatch):
    """The whole point of the extra is that a caller who never asks for a table
    does not carry pyarrow; the one who does ask must be told what to install."""
    monkeypatch.setitem(sys.modules, "pyarrow", None)
    with pytest.raises(ImportError, match=r"infrastore\[arrow\]"):
        hourly(np.arange(3.0)).to_arrow()


# ---- from_arrow: the inverse ------------------------------------------------


def test_to_arrow_from_arrow_round_trips_a_series():
    series = hourly(
        np.arange(3.0),
        name="load",
        units="MW",
        quantity_kind="ActivePower",
        unit_system="natural_units",
        component_field="max_active_power",
        application_data='{"k": 1}',
    )
    assert SingleTimeSeries.from_arrow(series.to_arrow()) == series


@pytest.mark.parametrize(
    "cls",
    [NonSequentialTimeSeries, PersistentTimeSeries],
)
def test_the_irregular_types_round_trip(cls):
    series = cls(breakpoints(), np.array([1.0, 2.0, 3.0]), "irregular", units="MW")
    assert cls.from_arrow(series.to_arrow()) == series


def test_a_multidimensional_series_round_trips():
    values = np.arange(24.0).reshape(4, 2, 3)
    series = hourly(values)
    back = SingleTimeSeries.from_arrow(series.to_arrow())
    assert back == series
    assert back.data.shape == (4, 2, 3)


def test_a_composite_element_type_round_trips():
    values = np.array([[0.0, 1.0, 1.0, 3.0], [0.0, 2.0, 1.0, 4.0]])
    series = hourly(values, name="cost", element_type="piecewise_linear")
    back = SingleTimeSeries.from_arrow(series.to_arrow())
    assert back.element_type == "piecewise_linear"
    assert back == series


def test_the_spelling_survives_the_round_trip():
    denver = datetime(2024, 1, 1, tzinfo=ZoneInfo("America/Denver"))
    series = SingleTimeSeries(denver, "PT1H", np.arange(3.0), "load")
    assert SingleTimeSeries.from_arrow(series.to_arrow()).time_reference == "America/Denver"

    naive = SingleTimeSeries(datetime(2024, 1, 1), "PT1H", np.arange(3.0), "load")
    assert SingleTimeSeries.from_arrow(naive.to_arrow()).time_reference == "zoneless"


def test_a_monthly_grid_round_trips_on_the_calendar():
    """The case a difference-based grid check gets wrong: `P1M` clamps to month
    end, so Jan-31 -> Feb-29 -> Mar-31 is a grid even though the steps differ."""
    series = SingleTimeSeries(
        datetime(2024, 1, 31, tzinfo=timezone.utc), "P1M", np.arange(3.0), "monthly"
    )
    assert SingleTimeSeries.from_arrow(series.to_arrow()) == series


# ---- from_arrow: foreign tables ---------------------------------------------


def _stamps(timestamps, tz="UTC"):
    """A `timestamp[ms, tz]` array built from raw milliseconds.

    Not from `np.datetime64`, which carries no zone and warns when one is asked
    for -- the point here is the zone the column declares.
    """
    millis = [int(t.replace(tzinfo=timezone.utc).timestamp() * 1000) for t in timestamps]
    return pa.array(millis, type=pa.int64()).cast(pa.timestamp("ms", tz))


def _foreign(timestamps, values, tz="UTC"):
    """A table with the right columns and no metadata at all."""
    return pa.table({"timestamp": _stamps(timestamps, tz), "value": pa.array(values)})


def test_a_foreign_grid_infers_its_resolution():
    hours = [UTC_START + timedelta(hours=k) for k in range(3)]
    series = SingleTimeSeries.from_arrow(
        _foreign(hours, [1.0, 2.0, 3.0]), name="load"
    )
    assert series.resolution == "PT1H"
    assert series.time_reference == "utc"


def test_a_foreign_table_without_a_zone_is_zoneless():
    hours = [UTC_START + timedelta(hours=k) for k in range(3)]
    series = SingleTimeSeries.from_arrow(
        _foreign(hours, [1.0, 2.0, 3.0], tz=None), name="load"
    )
    assert series.time_reference == "zoneless"


def test_a_fixed_size_list_reads_as_dense_and_an_assertion_makes_it_a_tuple():
    hours = [UTC_START + timedelta(hours=k) for k in range(2)]
    values = pa.FixedSizeListArray.from_arrays(
        pa.array([1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 3
    )
    table = pa.table({"timestamp": _stamps(hours), "value": values})
    # Dense is the weaker claim, and the bytes cannot say more.
    dense = SingleTimeSeries.from_arrow(table, name="load")
    assert dense.element_type == "f64"
    assert dense.data.shape == (2, 3)

    # The assertion states the stronger one.
    tup = SingleTimeSeries.from_arrow(table, name="load", element_type="tuple(3,f64)")
    assert tup.element_type == "tuple(3,f64)"


def test_a_contradicting_element_type_is_an_error_not_an_override():
    table = hourly(np.arange(3.0)).to_arrow()
    with pytest.raises(InvalidParameterError, match="asserted"):
        SingleTimeSeries.from_arrow(table, element_type="i64")


def test_a_nameless_table_is_refused():
    hours = [UTC_START + timedelta(hours=k) for k in range(2)]
    with pytest.raises(InvalidParameterError, match="name"):
        SingleTimeSeries.from_arrow(_foreign(hours, [1.0, 2.0]))


def test_nulls_are_refused_rather_than_coerced():
    hours = [UTC_START + timedelta(hours=k) for k in range(3)]
    table = pa.table({"timestamp": _stamps(hours), "value": pa.array([1.0, None, 3.0])})
    with pytest.raises(InvalidParameterError, match="nulls"):
        SingleTimeSeries.from_arrow(table, name="load")


def test_a_sub_millisecond_timestamp_is_refused():
    micros = pa.array(
        [1_704_067_200_000_500, 1_704_070_800_000_500], type=pa.timestamp("us", "UTC")
    )
    table = pa.table({"timestamp": micros, "value": pa.array([1.0, 2.0])})
    with pytest.raises(InvalidParameterError, match="whole millisecond"):
        SingleTimeSeries.from_arrow(table, name="load")

    # Whole milliseconds expressed in microseconds are fine.
    micros = pa.array(
        [1_704_067_200_000_000, 1_704_070_800_000_000], type=pa.timestamp("us", "UTC")
    )
    table = pa.table({"timestamp": micros, "value": pa.array([1.0, 2.0])})
    assert SingleTimeSeries.from_arrow(table, name="load").length == 2


def test_a_missing_column_names_what_is_expected():
    table = pa.table({"t": pa.array([1, 2]), "v": pa.array([1.0, 2.0])})
    with pytest.raises(InvalidParameterError, match="timestamp"):
        SingleTimeSeries.from_arrow(table, name="load")


def test_a_parquet_file_round_trips_through_from_arrow(tmp_path):
    """The end-to-end shape: values written as Parquet come back bit-identical,
    which is the improvement over CSV, where floats pass through decimal text."""
    pq = pytest.importorskip("pyarrow.parquet")
    awkward = np.array(
        [np.pi, 1e-300, -0.0, np.inf, -np.inf, np.finfo(np.float64).max]
    )
    series = hourly(awkward, units="MW")
    path = tmp_path / "series.parquet"
    pq.write_table(series.to_arrow(), path)

    back = SingleTimeSeries.from_arrow(pq.read_table(path))
    assert back == series
    assert np.array_equal(back.data, awkward)


# ---- The unspecified reference ---------------------------------------------
#
# A series that records no timestamp spelling. Every constructor infers one from
# its input -- that inference is the point -- so the only way to reach `None` is
# a table that says so, which is also the case under test.


def _unspecified_table():
    """A `to_arrow()`-shaped table whose metadata says the series declared no
    spelling, over a UTC-zoned timestamp column."""
    return pa.table(
        {
            "timestamp": _stamps([UTC_START + timedelta(hours=k) for k in range(3)]),
            "value": pa.array([1.0, 2.0, 3.0]),
        }
    ).replace_schema_metadata({"name": "load", "time_reference": "unspecified"})


def test_the_unspecified_literal_is_the_one_the_cli_writes():
    """The two producers are compiled separately and must agree on this string
    exactly. The Rust half is `the_unspecified_literal_matches_the_python_binding`
    in `crates/infrastore-parquet/tests/import.rs`."""
    series = SingleTimeSeries.from_arrow(_unspecified_table())
    assert series.to_arrow().schema.metadata[b"time_reference"] == b"unspecified"


def test_a_declared_reference_is_still_written_as_itself():
    assert hourly(np.arange(3.0)).to_arrow().schema.metadata[b"time_reference"] == b"utc"


def test_from_arrow_decodes_the_literal_back_to_none():
    assert SingleTimeSeries.from_arrow(_unspecified_table()).time_reference is None


def test_the_literal_beats_the_columns_zone():
    """The column is UTC-zoned; the metadata says the series declared nothing.
    The metadata wins, or the round trip would invent a claim the series never
    made -- which is the whole reason the key is written rather than omitted."""
    table = _unspecified_table()
    assert table.schema.field("timestamp").type.tz == "UTC"
    assert SingleTimeSeries.from_arrow(table).time_reference is None


def test_an_explicit_keyword_still_wins_over_the_literal():
    series = SingleTimeSeries.from_arrow(
        _unspecified_table(), time_reference="America/Denver"
    )
    assert series.time_reference == "America/Denver"


def test_a_table_with_no_time_reference_key_still_reads_its_zone():
    """Only the literal overrides the column. A foreign table, which has no
    metadata at all, keeps the existing inference."""
    hours = [UTC_START + timedelta(hours=k) for k in range(3)]
    assert (
        SingleTimeSeries.from_arrow(
            _foreign(hours, [1.0, 2.0, 3.0], tz=None), name="load"
        ).time_reference
        == "zoneless"
    )


def test_an_unspecified_series_round_trips_through_to_arrow():
    series = SingleTimeSeries.from_arrow(_unspecified_table())
    assert series.time_reference is None
    assert SingleTimeSeries.from_arrow(series.to_arrow()).time_reference is None
