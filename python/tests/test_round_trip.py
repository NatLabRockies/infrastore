"""End-to-end round-trip tests for the time_series Python bindings."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import (
    DuplicateTimeSeriesError,
    InvalidParameterError,
    NotFoundError,
    OwnerCategory,
    ReadOnlyStoreError,
    SingleTimeSeries,
    NonSequentialTimeSeries,
    Store,
    TimeSeriesType,
)


def make_series(
    initial_year: int = 2024,
    length: int = 24,
    base: float = 100.0,
    name: str = "load",
) -> SingleTimeSeries:
    initial = datetime(initial_year, 1, 1, tzinfo=timezone.utc)
    resolution = timedelta(hours=1)
    data = np.arange(length, dtype=np.float64) + base
    return SingleTimeSeries(initial, resolution, data, name)


def test_in_memory_round_trip():
    store = Store.create(in_memory=True)
    s = make_series()
    key = store.add_time_series(
        owner_id=42,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=s,
        units="MW",
    )
    assert key.owner_id == 42
    assert key.owner_category == OwnerCategory.Component
    assert key.time_series_type == TimeSeriesType.SingleTimeSeries

    got = store.get_time_series(key)
    assert got.length == 24
    assert got.initial_timestamp == s.initial_timestamp
    assert got.name == "load"
    np.testing.assert_array_equal(np.asarray(got.data), np.asarray(s.data))


def test_persistent_round_trip(tmp_path):
    path = tmp_path / "store.h5"
    s = make_series(2024, 12, 1.0)

    store = Store.create(path=str(path), in_memory=False)
    key = store.add_time_series(
        owner_id=1,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=s,
    )
    store.flush()
    del store  # drop file handle

    reopened = Store.open(path=str(path), read_only=True)
    keys = reopened.get_time_series_keys(1, OwnerCategory.Component)
    assert len(keys) == 1
    assert keys[0].owner_category == OwnerCategory.Component
    got = reopened.get_time_series(keys[0])
    assert got.name == "load"
    np.testing.assert_array_equal(np.asarray(got.data), np.asarray(s.data))

    report = reopened.verify_integrity()
    assert report == {"ok": True, "errors": []}, f"integrity errors: {report}"


@pytest.mark.parametrize(
    "kwargs",
    [
        {"compression": "none"},
        {"compression": "deflate", "compression_level": 9, "shuffle": False},
        {"compression": "deflate", "compression_level": 1, "shuffle": True},
    ],
)
def test_compression_round_trip(tmp_path, kwargs):
    """Each compression policy stores and reads back identical data."""
    path = tmp_path / "store.h5"
    s = make_series(2024, 12, 1.0)

    store = Store.create(path=str(path), in_memory=False, **kwargs)
    store.add_time_series(1, "Generator", OwnerCategory.Component, s)
    store.flush()
    del store

    reopened = Store.open(path=str(path), read_only=True)
    # The persisted policy is restored on open.
    comp = reopened.get_compression()
    expected = kwargs.get("compression", "deflate")
    assert comp["compression"] == expected
    if expected == "deflate":
        assert comp["level"] == kwargs.get("compression_level", 3)
        assert comp["shuffle"] == kwargs.get("shuffle", True)
    keys = reopened.get_time_series_keys(1, OwnerCategory.Component)
    got = reopened.get_time_series(keys[0])
    np.testing.assert_array_equal(np.asarray(got.data), np.asarray(s.data))
    assert reopened.verify_integrity() == {"ok": True, "errors": []}


def test_get_compression_in_memory_is_none():
    store = Store.create(in_memory=True)
    assert store.get_compression()["compression"] == "none"


def test_invalid_compression_rejected(tmp_path):
    path = tmp_path / "store.h5"
    with pytest.raises(InvalidParameterError):
        Store.create(path=str(path), in_memory=False, compression="lz4")
    with pytest.raises(InvalidParameterError):
        Store.create(
            path=str(path), in_memory=False, compression="deflate", compression_level=99
        )


def test_features_disambiguate_keys():
    store = Store.create(in_memory=True)
    s1 = make_series(base=1.0)
    s2 = make_series(base=100.0)

    store.add_time_series(
        owner_id=1,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=s1,
        features={"model_year": 2030, "is_baseline": True},
    )
    store.add_time_series(
        owner_id=1,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=s2,
        features={"model_year": 2035},
    )

    all_rows = store.list_time_series(owner_id=1)
    assert len(all_rows) == 2

    only_2035 = store.list_time_series(features={"model_year": 2035})
    assert len(only_2035) == 1
    assert only_2035[0]["features"]["model_year"] == 2035


def test_duplicate_key_raises():
    store = Store.create(in_memory=True)
    s = make_series()
    store.add_time_series(1, "Generator", OwnerCategory.Component, s)
    with pytest.raises(DuplicateTimeSeriesError):
        store.add_time_series(1, "Generator", OwnerCategory.Component, s)


def test_missing_key_raises_not_found():
    store = Store.create(in_memory=True)
    s = make_series()
    key = store.add_time_series(1, "Generator", OwnerCategory.Component, s)
    store.remove_time_series(key)
    with pytest.raises(NotFoundError):
        store.get_time_series(key)


def test_time_range_slicing():
    store = Store.create(in_memory=True)
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    resolution = timedelta(hours=1)
    data = np.array([10.0, 20.0, 30.0, 40.0, 50.0, 60.0])
    s = SingleTimeSeries(initial, resolution, data, "load")
    key = store.add_time_series(1, "Generator", OwnerCategory.Component, s)

    start = initial + timedelta(hours=2)
    end = initial + timedelta(hours=5)
    got = store.get_time_series(key, time_range=(start, end))
    assert got.length == 3
    assert got.initial_timestamp == start
    np.testing.assert_array_equal(np.asarray(got.data), np.array([30.0, 40.0, 50.0]))


def test_read_only_blocks_writes(tmp_path):
    path = tmp_path / "store.h5"
    store = Store.create(path=str(path), in_memory=False)
    store.add_time_series(1, "Generator", OwnerCategory.Component, make_series())
    store.flush()
    del store

    ro = Store.open(path=str(path), read_only=True)
    assert ro.read_only is True
    with pytest.raises(ReadOnlyStoreError):
        ro.add_time_series(2, "Generator", OwnerCategory.Component, make_series())


def test_invalid_feature_value_raises():
    store = Store.create(in_memory=True)
    with pytest.raises(InvalidParameterError):
        store.add_time_series(
            1, "Generator", OwnerCategory.Component, make_series(),
            features={"bad": [1, 2, 3]},  # lists aren't valid feature values (int/float/bool/str)
        )


def test_counts_and_resolutions():
    store = Store.create(in_memory=True)
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    data = np.array([1.0, 2.0, 3.0])

    for owner, res in [(1, timedelta(hours=1)), (2, timedelta(minutes=15)), (3, timedelta(hours=4))]:
        s = SingleTimeSeries(initial, res, data, "load")
        store.add_time_series(owner, "Generator", OwnerCategory.Component, s)

    counts = store.get_time_series_counts()
    assert counts["static_time_series"] == 3
    assert counts["components_with_time_series"] == 3

    # Resolutions are returned as canonical ISO-8601 duration strings.
    resolutions = store.get_resolutions()
    assert sorted(resolutions) == ["PT15M", "PT1H", "PT4H"]


def test_numpy_array_received_as_ndarray():
    """Sanity check: data round-tripped is a numpy ndarray, with the original dtype."""
    store = Store.create(in_memory=True)
    s = make_series()
    key = store.add_time_series(1, "Generator", OwnerCategory.Component, s)
    got = store.get_time_series(key)
    arr = np.asarray(got.data)
    assert isinstance(arr, np.ndarray)
    assert arr.dtype == np.float64
    assert arr.shape == (24,)


@pytest.mark.parametrize(
    "descr", ["<f8", ">f8", ">f4", ">i8", ">i4", ">i2", ">u8", ">u2"]
)
def test_byte_order_is_normalised_not_reinterpreted(descr):
    """A big-endian array stores its values, not its bytes.

    `.dtype.name` drops byte order (`np.dtype('>f8').name == 'float64'`) while
    `.tobytes()` keeps it, so a big-endian array used to be written under a
    little-endian label and read back byte-reversed -- silently, since every
    reversed value is still a legal number. The binding normalises to the
    store's documented little-endian layout instead.
    """
    store = Store.create(in_memory=True)
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    expected = np.array([1, 2, 3], dtype=descr)

    series = SingleTimeSeries(initial, timedelta(hours=1), expected, "load")
    key = store.add_time_series(1, "Generator", OwnerCategory.Component, series)
    got = np.asarray(store.get_time_series(key).data)

    # Values survive, and the caller gets them in the host's own byte order.
    assert np.array_equal(got, expected)
    assert got.dtype == np.dtype(descr).newbyteorder("=")


def test_single_byte_dtypes_are_unaffected_by_byte_order():
    """`bool`/`int8`/`uint8` have no byte order to normalise ('|' in numpy)."""
    store = Store.create(in_memory=True)
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    for owner, values in enumerate(
        [
            np.array([True, False, True]),
            np.array([-1, 0, 1], dtype=np.int8),
            np.array([0, 128, 255], dtype=np.uint8),
        ]
    ):
        series = SingleTimeSeries(initial, timedelta(hours=1), values, "load")
        key = store.add_time_series(
            owner + 1, "Generator", OwnerCategory.Component, series
        )
        got = np.asarray(store.get_time_series(key).data)
        assert np.array_equal(got, values)
        assert got.dtype == values.dtype


def test_non_contiguous_big_endian_array_round_trips():
    """The two representational normalisations compose: order and byte order."""
    store = Store.create(in_memory=True)
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    # A strided view of a big-endian array: neither C-contiguous nor LE.
    expected = np.arange(12, dtype=">f8").reshape(3, 4)[:, ::2]
    assert not expected.flags["C_CONTIGUOUS"]

    series = SingleTimeSeries(initial, timedelta(hours=1), expected, "load")
    key = store.add_time_series(1, "Generator", OwnerCategory.Component, series)
    got = np.asarray(store.get_time_series(key).data)

    assert np.array_equal(got, expected)
    assert got.shape == expected.shape


def test_bad_data_hash_raises_a_catchable_error():
    """A malformed hash is an ordinary bad argument, not an uncatchable panic.

    The length guard counted bytes while the loop sliced character boundaries,
    so a 64-*byte* string of multi-byte characters sliced through a character and
    panicked. PyO3 surfaces a panic as `PanicException`, which inherits from
    `BaseException` -- escaping both `except Exception` and this package's own
    exception hierarchy.
    """
    store = Store.create(in_memory=True)
    for bad in [
        "\U0001F600" * 16,  # 64 bytes, 16 characters
        "\u00e9" * 32,  # 64 bytes, 32 characters
        "z" * 64,  # right length, not hex
        "ab",  # too short
        "",
    ]:
        with pytest.raises(InvalidParameterError):
            store.get_array_by_hash(bad)
        with pytest.raises(InvalidParameterError):
            store.count_array_references(bad)


def test_resolution_must_be_a_whole_positive_millisecond():
    """The store cannot represent a finer or non-positive grid, so it says so.

    Periods are stored as an integer count of milliseconds. A sub-millisecond
    resolution encodes as PT0S and used to read back as zero; zero repeated one
    instant; a negative one built a reader whose timeline ran backwards. All
    three were writable and none was readable.
    """
    store = Store.create(in_memory=True)
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    values = np.arange(4, dtype=np.float64)

    for bad in [
        timedelta(microseconds=1),
        timedelta(microseconds=999),
        timedelta(0),
        timedelta(hours=-1),
    ]:
        series = SingleTimeSeries(initial, bad, values, "load")
        with pytest.raises(InvalidParameterError, match="resolution"):
            store.add_time_series(1, "Generator", OwnerCategory.Component, series)

    # One whole millisecond is the finest grid there is, and it works.
    series = SingleTimeSeries(initial, timedelta(milliseconds=1), values, "load")
    key = store.add_time_series(1, "Generator", OwnerCategory.Component, series)
    assert len(np.asarray(store.get_time_series(key).data)) == 4


def test_omitted_descriptor_kwargs_keep_what_the_series_carries():
    """Re-adding a series read back from the store keeps its descriptors.

    `units`, `quantity_kind`, `unit_system`, `component_field` and
    `application_data` were set unconditionally from kwargs defaulting to None,
    so a read-then-re-add silently cleared five of the six descriptors that
    `get_time_series` had just populated -- while keeping `element_type`, which
    was already guarded. The value classes expose no properties for the five, so
    the caller could neither notice nor re-supply what was lost.
    """
    store = Store.create(in_memory=True)
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    series = SingleTimeSeries(initial, timedelta(hours=1), np.arange(4.0), "load")
    described = dict(
        units="MW",
        quantity_kind="ActivePower",
        unit_system="component_base",
        component_field="max_active_power",
        application_data='{"a": 1}',
        element_type="f64",
    )
    key = store.add_time_series(
        1, "Generator", OwnerCategory.Component, series, **described
    )

    # Read it back and re-add it under a new owner, supplying nothing.
    round_tripped = store.get_time_series(key)
    key2 = store.add_time_series(
        2, "Generator", OwnerCategory.Component, round_tripped
    )
    meta = store.get_metadata(key2)
    for field, expected in described.items():
        assert meta[field] == expected, field

    # The bulk path behaves the same way.
    (key3,) = store.add_time_series_bulk(
        [
            {
                "owner_id": 3,
                "owner_type": "Generator",
                "owner_category": OwnerCategory.Component,
                "time_series": store.get_time_series(key),
            }
        ]
    )
    meta3 = store.get_metadata(key3)
    for field, expected in described.items():
        assert meta3[field] == expected, f"bulk: {field}"

    # An explicitly supplied value still overrides.
    key4 = store.add_time_series(
        4,
        "Generator",
        OwnerCategory.Component,
        store.get_time_series(key),
        units="kW",
    )
    assert store.get_metadata(key4)["units"] == "kW"
    assert store.get_metadata(key4)["quantity_kind"] == "ActivePower"


def test_non_sequential_round_trip_and_slice():
    store = Store.create(in_memory=True)
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    timestamps = [
        initial,
        initial + timedelta(hours=4),
        initial + timedelta(days=2),
    ]
    series = NonSequentialTimeSeries(timestamps, np.array([10.0, 20.0, 30.0]), "events")
    key = store.add_time_series(
        1, "Generator", OwnerCategory.Component, series,
    )

    assert key.time_series_type == TimeSeriesType.NonSequentialTimeSeries
    assert key.resolution is None
    got = store.get_time_series(
        key,
        time_range=(initial + timedelta(hours=1), initial + timedelta(days=3)),
    )
    assert isinstance(got, NonSequentialTimeSeries)
    assert got.name == "events"
    assert got.timestamps == timestamps[1:]
    np.testing.assert_array_equal(np.asarray(got.data), np.array([20.0, 30.0]))


def test_non_sequential_rejects_invalid_timestamps():
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    with pytest.raises(InvalidParameterError):
        NonSequentialTimeSeries(
            [initial, initial],
            np.array([1.0, 2.0]),
            "events",
        )


def test_dtype_round_trip():
    """Non-float64 numpy dtypes round-trip with their dtype preserved."""
    store = Store.create(in_memory=True)
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    res = timedelta(hours=1)

    for dtype in (np.int64, np.int32, np.float32, np.uint64):
        s = SingleTimeSeries(initial, res, np.array([1, 2, 3], dtype=dtype), f"ts_{dtype.__name__}")
        key = store.add_time_series(1, "Generator", OwnerCategory.Component, s)
        arr = np.asarray(store.get_time_series(key).data)
        assert arr.dtype == dtype
        assert arr.tolist() == [1, 2, 3]


def test_add_time_series_bulk(tmp_path):
    """Bulk add commits all series in one transaction and returns keys in order."""
    path = tmp_path / "bulk.h5"
    store = Store.create(path=str(path), in_memory=False)
    items = [
        {
            "owner_id": i,
            "owner_type": "Generator",
            "owner_category": OwnerCategory.Component,
            "time_series": make_series(base=float(i)),
            "features": {"scenario": i},
            "units": "MW",
        }
        for i in range(10)
    ]
    keys = store.add_time_series_bulk(items)
    assert len(keys) == 10
    for i, key in enumerate(keys):
        assert key.owner_id == i
        got = store.get_time_series(key)
        np.testing.assert_array_equal(
            np.asarray(got.data), np.arange(24, dtype=np.float64) + float(i)
        )


def test_bulk_read(tmp_path):
    """bulk_read returns a full series per key, in order, matching get_time_series."""
    path = tmp_path / "bulkread.h5"
    store = Store.create(path=str(path), in_memory=False)
    items = [
        {
            "owner_id": i,
            "owner_type": "Generator",
            "owner_category": OwnerCategory.Component,
            "time_series": make_series(length=12, base=float(i * 10)),
        }
        for i in range(8)
    ]
    keys = store.add_time_series_bulk(items)

    series = store.bulk_read(keys)
    assert len(series) == 8
    for i, s in enumerate(series):
        expected = np.arange(12, dtype=np.float64) + float(i * 10)
        np.testing.assert_array_equal(np.asarray(s.data), expected)
        # Same as the per-key read, in order.
        np.testing.assert_array_equal(
            np.asarray(s.data), np.asarray(store.get_time_series(keys[i]).data)
        )

    # Empty input returns an empty list.
    assert store.bulk_read([]) == []


def test_add_time_series_bulk_rolls_back_on_error():
    """A duplicate in the batch rolls back every item."""
    store = Store.create(in_memory=True)
    dup = {
        "owner_id": 1,
        "owner_type": "Generator",
        "owner_category": OwnerCategory.Component,
        "time_series": make_series(),
    }
    with pytest.raises(DuplicateTimeSeriesError):
        store.add_time_series_bulk([dup, dict(dup)])
    assert store.get_time_series_keys(1, OwnerCategory.Component) == []


def test_add_time_series_bulk_rejects_missing_keys():
    store = Store.create(in_memory=True)
    with pytest.raises(InvalidParameterError, match="owner_id"):
        store.add_time_series_bulk([{"owner_type": "Generator"}])


def test_add_time_series_bulk_rejects_unknown_keys():
    """A misspelled item key raises rather than being silently ignored.

    `add_time_series` gets this for free -- an unexpected keyword argument is a
    TypeError. The bulk path reads a dict, so a typo used to mean the descriptor
    it carried was quietly dropped and the series landed without it.
    """
    store = Store.create(in_memory=True)
    item = {
        "owner_id": 1,
        "owner_type": "Generator",
        "owner_category": OwnerCategory.Component,
        "time_series": make_series(),
        "unit_sytem": "natural_units",  # sic
    }
    with pytest.raises(InvalidParameterError, match="unit_sytem"):
        store.add_time_series_bulk([item])
    # Nothing was written.
    assert store.list_time_series() == []
    # The error names which item is at fault, and what would have worked.
    with pytest.raises(InvalidParameterError, match="item 1"):
        store.add_time_series_bulk([{k: v for k, v in item.items() if k != "unit_sytem"}, item])
    # A non-string key is refused the same way.
    good = {k: v for k, v in item.items() if k != "unit_sytem"}
    with pytest.raises(InvalidParameterError, match="non-string key"):
        store.add_time_series_bulk([{**good, 7: "x"}])


# ---------------------------------------------------------------------------
# Category-aware ownership
# ---------------------------------------------------------------------------


def _add_for_category(store, owner_id, category, name):
    return store.add_time_series(
        owner_id=owner_id,
        owner_type="Generator" if category == OwnerCategory.Component else "Outage",
        owner_category=category,
        time_series=make_series(name=name),
    )


def test_owner_pair_distinguishes_same_id_across_categories():
    """The same owner_id under different categories are distinct owners."""
    store = Store.create(in_memory=True)
    comp_key = _add_for_category(store, 7, OwnerCategory.Component, "comp")
    supp_key = _add_for_category(store, 7, OwnerCategory.SupplementalAttribute, "supp")

    assert comp_key.owner_category == OwnerCategory.Component
    assert supp_key.owner_category == OwnerCategory.SupplementalAttribute

    comp_keys = store.get_time_series_keys(7, OwnerCategory.Component)
    supp_keys = store.get_time_series_keys(7, OwnerCategory.SupplementalAttribute)
    assert len(comp_keys) == 1
    assert len(supp_keys) == 1
    assert store.get_time_series(comp_keys[0]).name == "comp"
    assert store.get_time_series(supp_keys[0]).name == "supp"


def test_list_time_series_emits_and_filters_owner_category():
    store = Store.create(in_memory=True)
    _add_for_category(store, 1, OwnerCategory.Component, "comp")
    _add_for_category(store, 1, OwnerCategory.SupplementalAttribute, "supp")

    all_rows = store.list_time_series(owner_id=1)
    assert len(all_rows) == 2
    assert {r["owner_category"] for r in all_rows} == {
        "Component",
        "SupplementalAttribute",
    }

    comp_rows = store.list_time_series(
        owner_id=1, owner_category=OwnerCategory.Component
    )
    assert len(comp_rows) == 1
    assert comp_rows[0]["owner_category"] == "Component"
    assert comp_rows[0]["name"] == "comp"


def test_clear_time_series_for_owner_pair():
    store = Store.create(in_memory=True)
    _add_for_category(store, 1, OwnerCategory.Component, "comp")
    _add_for_category(store, 1, OwnerCategory.SupplementalAttribute, "supp")

    removed = store.clear_time_series(
        owner_id=1, owner_category=OwnerCategory.Component
    )
    assert removed == 1
    assert store.get_time_series_keys(1, OwnerCategory.Component) == []
    assert len(store.get_time_series_keys(1, OwnerCategory.SupplementalAttribute)) == 1


def test_clear_time_series_all():
    store = Store.create(in_memory=True)
    _add_for_category(store, 1, OwnerCategory.Component, "comp")
    _add_for_category(store, 2, OwnerCategory.SupplementalAttribute, "supp")

    removed = store.clear_time_series()
    assert removed == 2
    assert store.list_time_series() == []


def test_clear_time_series_requires_both_or_neither():
    store = Store.create(in_memory=True)
    with pytest.raises(InvalidParameterError):
        store.clear_time_series(owner_id=1)
    with pytest.raises(InvalidParameterError):
        store.clear_time_series(owner_category=OwnerCategory.Component)


def test_replace_owner():
    store = Store.create(in_memory=True)
    _add_for_category(store, 1, OwnerCategory.Component, "comp")

    updated = store.replace_owner(1, 2, OwnerCategory.Component)
    assert updated == 1
    assert store.get_time_series_keys(1, OwnerCategory.Component) == []
    moved = store.get_time_series_keys(2, OwnerCategory.Component)
    assert len(moved) == 1
    assert moved[0].owner_id == 2
    assert store.get_time_series(moved[0]).name == "comp"


def test_key_repr_includes_owner_category():
    store = Store.create(in_memory=True)
    key = _add_for_category(store, 1, OwnerCategory.Component, "comp")
    assert "owner_category" in repr(key)


# ---- timezone handling -----------------------------------------------------
#
# The store records instants. Anything that names one is accepted and normalised
# to UTC; anything that does not is refused inside this package's exception
# hierarchy. Before this, only `datetime.timezone.utc` itself was accepted --
# `ZoneInfo("UTC")` is a different object, and a named zone is not a fixed
# offset, so both were refused by the binding layer with a bare TypeError or
# ValueError that no `except TimeSeriesError` could catch.


def _values(store, key):
    return store.get_time_series(key).data.tolist()


def test_aware_datetimes_in_any_zone_name_the_same_instant():
    from zoneinfo import ZoneInfo

    utc = datetime(2024, 6, 1, 12, tzinfo=timezone.utc)
    equivalents = [
        utc,
        utc.astimezone(ZoneInfo("UTC")),
        utc.astimezone(ZoneInfo("America/Denver")),
        utc.astimezone(ZoneInfo("Asia/Kolkata")),  # a half-hour offset
        utc.astimezone(timezone(timedelta(hours=-7))),
    ]
    data = np.array([1.0, 2.0, 3.0], dtype=np.float64)
    for when in equivalents:
        store = Store.create(in_memory=True)
        ts = SingleTimeSeries(when, timedelta(hours=1), data, "load")
        # The constructor normalises: the series reports the same UTC instant
        # whichever zone it was handed.
        assert ts.initial_timestamp == utc, when.tzinfo
        key = store.add_time_series(1, "Generator", OwnerCategory.Component, ts)
        assert store.get_time_series(key).initial_timestamp == utc


def test_a_zone_is_read_by_its_offset_not_by_what_it_claims_to_equal():
    """The UTC fast path keys on identity with `datetime.timezone.utc`.

    A `tzinfo` decides its own equality. One whose `__eq__` claims UTC while its
    `utcoffset` says otherwise would, under an `==` check, be read at its wall
    clock -- a silently wrong instant. `astimezone` asks `utcoffset`, so every
    zone that is not the singleton itself goes through it.
    """
    from datetime import tzinfo

    class ClaimsUtc(tzinfo):
        def utcoffset(self, dt):
            return timedelta(hours=5)

        def tzname(self, dt):
            return "CLAIMS_UTC"

        def dst(self, dt):
            return None

        def __eq__(self, other):
            return other is timezone.utc or isinstance(other, ClaimsUtc)

        def __hash__(self):
            return 0

    when = datetime(2024, 6, 1, 12, tzinfo=ClaimsUtc())
    assert when.tzinfo == timezone.utc  # the claim
    ts = SingleTimeSeries(when, timedelta(hours=1), np.array([1.0]), "load")
    assert ts.initial_timestamp == datetime(2024, 6, 1, 7, tzinfo=timezone.utc)

    # `timezone(timedelta(0))` is the singleton, so the fast path still covers
    # the spelling most callers reach for.
    zero = timezone(timedelta(0))
    assert zero is timezone.utc
    assert SingleTimeSeries(
        datetime(2024, 6, 1, 12, tzinfo=zero), timedelta(hours=1), np.array([1.0]), "load"
    ).initial_timestamp == datetime(2024, 6, 1, 12, tzinfo=timezone.utc)


def test_naive_datetime_is_refused_inside_the_exception_hierarchy():
    naive = datetime(2024, 6, 1, 12)
    with pytest.raises(InvalidParameterError, match="timezone-aware"):
        SingleTimeSeries(naive, timedelta(hours=1), np.array([1.0]), "load")


def test_a_non_datetime_is_refused_inside_the_exception_hierarchy():
    with pytest.raises(InvalidParameterError, match="expected a datetime"):
        SingleTimeSeries("2024-01-01", timedelta(hours=1), np.array([1.0]), "load")


def test_every_instant_argument_accepts_a_named_zone():
    from zoneinfo import ZoneInfo

    denver = ZoneInfo("America/Denver")
    start = datetime(2024, 1, 1, tzinfo=timezone.utc)
    store = Store.create(in_memory=True)
    data = np.arange(24, dtype=np.float64)
    key = store.add_time_series(
        1,
        "Generator",
        OwnerCategory.Component,
        SingleTimeSeries(start.astimezone(denver), timedelta(hours=1), data, "load"),
    )

    # get_time_series / bulk_read time ranges.
    window = (
        start.astimezone(denver),
        (start + timedelta(hours=4)).astimezone(denver),
    )
    assert _values(store, key)[:4] == store.get_time_series(key, time_range=window).data.tolist()
    assert store.bulk_read([key], time_range=window)[0].data.tolist() == [0.0, 1.0, 2.0, 3.0]

    # NonSequentialTimeSeries timestamps.
    stamps = [(start + timedelta(hours=i)).astimezone(denver) for i in range(3)]
    nsts = NonSequentialTimeSeries(stamps, np.array([1.0, 2.0, 3.0]), "events")
    assert nsts.timestamps == [start + timedelta(hours=i) for i in range(3)]

    # static_read's `when`.
    reader = store.build_static_reader(timedelta(hours=1))
    store.static_read(reader, (start + timedelta(hours=2)).astimezone(denver))
    assert reader.group_values(0).tolist() == [2.0]
