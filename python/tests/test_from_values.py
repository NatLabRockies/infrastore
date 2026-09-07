"""`from_values` / `decoded_values`: building a composite series from its values.

The pair exists so a caller never holds an array and an `element_type` that can
disagree — the constructor derives both from the values, and the read side takes
the element type and the leading-axis count off the series rather than asking.

The round trips are checked against `conformance/element_type_vectors.json`, the
same corpus the codec tests use, so a constructor that agreed with this binding's
own decoder but not with the stored packing would fail here.
"""

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

import numpy as np
import pytest

from infrastore import (
    Deterministic,
    InvalidParameterError,
    NonSequentialTimeSeries,
    OwnerCategory,
    PersistentTimeSeries,
    Probabilistic,
    Scenarios,
    SingleTimeSeries,
    Store,
    decode_element_values,
)

T0 = datetime(2024, 1, 1, tzinfo=timezone.utc)
HOUR = timedelta(hours=1)

CURVES = [
    [{"x": 0.0, "y": 1.0}, {"x": 1.0, "y": 3.0}],
    [{"x": 0.0, "y": 2.0}],
    [],
    [{"x": 2.0, "y": 9.5}],
]

VECTORS_PATH = (
    Path(__file__).resolve().parents[2] / "conformance" / "element_type_vectors.json"
)


def _vectors():
    with VECTORS_PATH.open() as f:
        return json.load(f)["vectors"]


def _series_from(vector, values):
    """Every series type whose leading-axis count matches this vector's.

    A vector pins one packing; which type stacked those axes in front of the
    element shape is the constructor's business, and each of them has to reach
    the same bytes.
    """
    leading = vector["shape"][: vector["leading_dims"]]
    if len(leading) == 1:
        stamps = [T0 + timedelta(hours=3 * i) for i in range(leading[0])]
        return [
            SingleTimeSeries.from_values(T0, HOUR, values, "s"),
            NonSequentialTimeSeries.from_values(stamps, values, "n"),
            PersistentTimeSeries.from_values(stamps, values, "p"),
        ]
    if len(leading) == 2:
        horizon, count = leading
        return [
            Deterministic.from_values(
                T0, HOUR, timedelta(hours=horizon), HOUR, count, values, "d"
            )
        ]
    outer, horizon, count = leading
    return [
        Probabilistic.from_values(
            T0,
            HOUR,
            timedelta(hours=horizon),
            HOUR,
            count,
            [(k + 1) / (outer + 1) for k in range(outer)],
            values,
            "pr",
        ),
        Scenarios.from_values(
            T0, HOUR, timedelta(hours=horizon), HOUR, count, outer, values, "sc"
        ),
    ]


@pytest.mark.parametrize("vector", _vectors(), ids=lambda v: v["name"])
def test_from_values_reaches_the_pinned_packing(vector):
    values = vector["decoded"]["timesteps"]
    for series in _series_from(vector, values):
        what = type(series).__name__
        # The element type nobody declared.
        assert series.element_type == vector["element_type"], what
        # The bytes the corpus pins, not merely bytes this binding can read back.
        assert series.data.dtype == np.float64, what
        assert list(series.data.shape) == vector["shape"], what
        assert series.data.tobytes().hex() == vector["bytes_hex"], what
        # And the read side closes the loop without being told anything.
        assert series.decoded_values() == values, what


def test_decoded_values_agrees_with_the_standalone_decoder():
    series = SingleTimeSeries.from_values(T0, HOUR, CURVES, "cost")
    assert series.decoded_values() == decode_element_values(
        series.data, series.element_type
    )


def test_a_scalar_series_has_no_values_to_decode():
    # The stored elements already are the values, so `.data` is the answer.
    plain = SingleTimeSeries(T0, HOUR, np.arange(4.0), "load")
    assert plain.decoded_values() is None


def test_the_descriptors_ride_along():
    series = SingleTimeSeries.from_values(
        T0,
        HOUR,
        CURVES,
        "cost",
        units="$/hr",
        quantity_kind="CostRate",
        unit_system="natural_units",
        component_field="operation_cost",
        application_data='{"policy": "min"}',
    )
    assert series.units == "$/hr"
    assert series.quantity_kind == "CostRate"
    assert series.unit_system == "natural_units"
    assert series.component_field == "operation_cost"
    assert series.application_data == '{"policy": "min"}'
    # The spelling is inferred from the timestamp exactly as the constructor
    # infers it, so `from_values` is not a way to lose it.
    assert series.time_reference == "utc"
    naive = SingleTimeSeries.from_values(T0.replace(tzinfo=None), HOUR, CURVES, "c")
    assert naive.time_reference == "zoneless"


def test_a_stored_series_reads_back_as_its_values():
    """The whole path: values in, one call; values out, one call."""
    store = Store.create(in_memory=True)
    ts_id = store.add_time_series(
        owner_id=1,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=SingleTimeSeries.from_values(T0, HOUR, CURVES, "cost"),
    )
    stored = store.read_by_id(ts_id)
    # No `get_metadata_by_id` in between: the series carries its element type.
    assert stored.decoded_values() == CURVES
    assert store.get_metadata_by_id(ts_id)["element_type"] == "piecewise_linear"


def test_element_type_is_an_assertion_not_an_override():
    agreeing = SingleTimeSeries.from_values(
        T0, HOUR, CURVES, "cost", element_type="piecewise_linear"
    )
    assert agreeing.element_type == "piecewise_linear"

    with pytest.raises(InvalidParameterError, match="disagrees with the values"):
        SingleTimeSeries.from_values(
            T0, HOUR, CURVES, "cost", element_type="piecewise_step"
        )


def test_a_plain_numeric_series_is_sent_back_to_the_constructor():
    with pytest.raises(InvalidParameterError, match="`data=` on the constructor"):
        SingleTimeSeries.from_values(T0, HOUR, [1.0, 2.0], "load")
    # And the row that broke it is named, not assumed to be the first.
    with pytest.raises(InvalidParameterError, match="row 1 is"):
        SingleTimeSeries.from_values(T0, HOUR, [[], 2.0], "load")


def test_values_that_name_no_element_type_are_refused():
    # Nothing to read at all.
    with pytest.raises(InvalidParameterError, match="empty `values`"):
        SingleTimeSeries.from_values(T0, HOUR, [], "cost")
    # Rows that read equally as an empty curve or an empty tuple.
    with pytest.raises(InvalidParameterError, match="every row is empty"):
        SingleTimeSeries.from_values(T0, HOUR, [[], []], "cost")
    # A declaration settles both cases — the error names it as the remedy, so it
    # has to actually be one.
    declared = SingleTimeSeries.from_values(
        T0, HOUR, [], "cost", element_type="linear_function"
    )
    assert declared.element_type == "linear_function"
    assert declared.data.shape == (0, 2)


def test_a_declaration_settles_rows_that_are_all_empty():
    """The remedy the ambiguity error names, so it has to actually be one.

    A curve with no points is a storable series — it packs to width 1, the
    leading point count — and the rows cannot say so themselves.
    """
    curves = SingleTimeSeries.from_values(
        T0, HOUR, [[], []], "cost", element_type="piecewise_linear"
    )
    assert curves.element_type == "piecewise_linear"
    assert curves.data.shape == (2, 1)
    assert curves.decoded_values() == [[], []]

    # Still an assertion: empty rows are sequences, so a declaration whose rows
    # are mappings disagrees with them, and a tuple arity they cannot fill does
    # too — neither degrades into a raw TypeError from the decoder.
    with pytest.raises(InvalidParameterError, match="disagrees with the values"):
        SingleTimeSeries.from_values(
            T0, HOUR, [[], []], "q", element_type="quadratic_function"
        )
    with pytest.raises(InvalidParameterError, match="disagrees with the values"):
        SingleTimeSeries.from_values(
            T0, HOUR, [[], []], "t", element_type="tuple(3,f64)"
        )


def test_the_values_are_read_once_so_a_generator_survives():
    """Inference and encoding share one pass.

    Iterating `values` twice would hand the encoder whatever inference had not
    already consumed — and a static series takes its `length` from the values, so
    the lost timesteps would not be caught by anything downstream.
    """
    rows = ({"proportional": float(h), "constant": 1.0} for h in range(4))
    ts = SingleTimeSeries.from_values(T0, HOUR, rows, "cost")
    assert ts.element_type == "linear_function"
    assert ts.length == 4
    assert [v["proportional"] for v in ts.decoded_values()] == [0.0, 1.0, 2.0, 3.0]


def test_a_scalar_element_type_is_sent_back_to_the_constructor():
    """Inside the hierarchy, whatever the values are.

    `from_values` exists to encode composite values; a scalar has none, and the
    refusal has to be catchable as `InvalidParameterError` like every other
    argument error — `ValueError` is outside `TimeSeriesError` entirely.
    """
    for values in ([], [{"proportional": 1.0, "constant": 2.0}]):
        with pytest.raises(InvalidParameterError, match="is a scalar"):
            SingleTimeSeries.from_values(T0, HOUR, values, "load", element_type="f64")


def test_an_empty_tuple_series_names_its_remedy():
    """The one storable series these constructors cannot build.

    A tuple's arity lives in its rows, and there are none — so the error points
    at `encode_element_values`, which takes the arity from the declaration.
    """
    with pytest.raises(InvalidParameterError, match="encode_element_values"):
        SingleTimeSeries.from_values(T0, HOUR, [], "t", element_type="tuple(3,f64)")


def test_the_leading_dims_are_derived_not_asked_for():
    """The friction `from_values` removes for a forecast.

    `encode_element_values` needs `(H, count)`, which the caller computes from
    the horizon by hand; here the horizon and `count` are already arguments.
    """
    forecast = Deterministic.from_values(
        T0, HOUR, timedelta(hours=2), HOUR, 2, CURVES, "d"
    )
    assert forecast.data.shape == (2, 2, 5)

    # Values that cannot fill those axes are refused, with the count named.
    with pytest.raises(InvalidParameterError, match="4 decoded timesteps"):
        Deterministic.from_values(T0, HOUR, timedelta(hours=2), HOUR, 3, CURVES, "d")


def test_the_irregular_types_still_check_their_timestamps():
    stamps = [T0, T0 + HOUR]
    with pytest.raises(InvalidParameterError, match="does not match data length"):
        NonSequentialTimeSeries.from_values(stamps, CURVES, "n")
    with pytest.raises(InvalidParameterError, match="does not match data length"):
        PersistentTimeSeries.from_values(stamps, CURVES, "p")
