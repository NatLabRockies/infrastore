"""The element_type codec, checked against the cross-language conformance corpus.

`conformance/element_type_vectors.json` is generated from `infrastore-core`'s
`codec::conformance` vectors and read by every binding's codec tests, so all
implementations are held to one definition of the encodings rather than to each
other.
"""

import json
from pathlib import Path

import numpy as np
import pytest

from infrastore import (
    OwnerCategory,
    SingleTimeSeries,
    Store,
    decode_element_values,
    encode_element_values,
)

VECTORS_PATH = (
    Path(__file__).resolve().parents[2] / "conformance" / "element_type_vectors.json"
)


def _vectors():
    with VECTORS_PATH.open() as f:
        return json.load(f)["vectors"]


def _stored_array(vector):
    """The vector's values as the numpy array the store holds."""
    return np.array(vector["values"], dtype=np.float64).reshape(vector["shape"])


@pytest.mark.parametrize("vector", _vectors(), ids=lambda v: v["name"])
def test_conformance_vector_decodes_to_its_pinned_values(vector):
    array = _stored_array(vector)
    # The byte-level contract: the same values must encode to the same bytes.
    assert array.tobytes().hex() == vector["bytes_hex"]

    decoded = decode_element_values(
        array, vector["element_type"], vector["leading_dims"]
    )
    assert decoded == vector["decoded"]["timesteps"]


def test_scalar_and_non_float64_arrays_decode_to_none():
    # Nothing to decode: the stored elements already are the values.
    assert decode_element_values(np.array([1.0, 2.0]), "f64") is None
    assert decode_element_values(np.array([[1, 2, 3]], dtype=np.int32), "tuple(3,i32)") is None


def test_decode_rejects_an_array_the_element_type_does_not_describe():
    with pytest.raises(Exception):
        decode_element_values(np.zeros((2, 4)), "quadratic_function")


def test_a_stored_piecewise_series_round_trips_through_the_store():
    store = Store.create(in_memory=True)
    # Two timesteps, widest has 2 points -> row width 1 + 2*2 = 5.
    values = np.array(
        [
            [2.0, 0.0, 1.0, 1.0, 3.0],
            [1.0, 0.0, 5.0, 0.0, 0.0],
        ]
    )
    from datetime import datetime, timedelta, timezone

    key = store.add_time_series(
        owner_id=1,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=SingleTimeSeries(
            datetime(2024, 1, 1, tzinfo=timezone.utc), timedelta(hours=1), values, "cost"
        ),
        element_type="piecewise_linear",
    )
    meta = store.get_metadata_by_id(key)
    assert meta["element_type"] == "piecewise_linear"

    read = np.asarray(store.read_by_id(key).data)
    assert decode_element_values(read, meta["element_type"]) == [
        [{"x": 0.0, "y": 1.0}, {"x": 1.0, "y": 3.0}],
        [{"x": 0.0, "y": 5.0}],
    ]


def test_the_store_rejects_an_array_that_contradicts_its_element_type():
    from datetime import datetime, timedelta, timezone

    store = Store.create(in_memory=True)
    with pytest.raises(Exception):
        store.add_time_series(
            owner_id=1,
            owner_type="Generator",
            owner_category=OwnerCategory.Component,
            time_series=SingleTimeSeries(
                datetime(2024, 1, 1, tzinfo=timezone.utc),
                timedelta(hours=1),
                # quadratic_function needs 3 coefficients per step, not 2.
                np.zeros((2, 2)),
                "cost",
            ),
            element_type="quadratic_function",
        )


def test_every_vector_round_trips_through_encode():
    """`encode_element_values` is the inverse of `decode_element_values`.

    Checked against the shared corpus rather than against itself, so an encoder
    that agreed with this binding's decoder but not with the store would fail.
    """
    vectors = _vectors()
    checked = 0
    for vector in vectors:
        array = np.array(vector["values"], dtype="float64").reshape(vector["shape"])
        decoded = decode_element_values(
            array, vector["element_type"], vector["leading_dims"]
        )
        if decoded is None:
            continue
        leading = vector["shape"][: vector["leading_dims"]]
        encoded = encode_element_values(
            decoded, vector["element_type"], leading
        )
        assert encoded.dtype == np.float64
        assert encoded.shape == tuple(vector["shape"]), vector["name"]
        assert np.array_equal(encoded, array), vector["name"]
        checked += 1
    assert checked == len(vectors)


def test_encode_defaults_its_leading_dims_to_the_static_case():
    values = [{"proportional": 1.0, "constant": 2.0}, {"proportional": 3.0, "constant": 4.0}]
    encoded = encode_element_values(values, "linear_function")
    assert encoded.shape == (2, 2)
    assert np.array_equal(encoded, np.array([[1.0, 2.0], [3.0, 4.0]]))


def test_encode_rejects_values_that_do_not_fill_the_leading_dims():
    values = [{"proportional": 1.0, "constant": 2.0}]
    with pytest.raises(Exception):
        encode_element_values(values, "linear_function", [2, 3])


def test_a_scalar_element_type_has_nothing_to_encode():
    # The numbers already are the values; there is no packing to build, and
    # saying so beats returning an array that looks like one.
    with pytest.raises(Exception):
        encode_element_values([1.0, 2.0], "f64")


def test_an_encoded_series_reads_back_through_the_store():
    """The pair a caller actually uses: encode, store, read, decode."""
    from datetime import datetime, timedelta, timezone

    curves = [
        [{"x": 0.0, "y": 1.0}, {"x": 1.0, "y": 3.0}],
        [{"x": 0.0, "y": 2.0}],
    ]
    array = encode_element_values(curves, "piecewise_linear")

    store = Store.create(in_memory=True)
    ts_id = store.add_time_series(
        owner_id=1,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=SingleTimeSeries(
            datetime(2024, 1, 1, tzinfo=timezone.utc),
            timedelta(hours=1),
            array,
            "cost",
        ),
        element_type="piecewise_linear",
    )
    row = store.get_metadata_by_id(ts_id)
    assert row["element_type"] == "piecewise_linear"
    stored = store.read_by_id(ts_id)
    assert decode_element_values(stored.data, row["element_type"]) == curves


def test_encode_validates_the_array_against_the_declared_element_type():
    """The values decide the packing; `element_type` decides what it is stored as.

    Those can disagree, and the array is documented as ready for
    `add_time_series` — so the check belongs here rather than at the store.
    """
    # Arity: three-wide tag, two-wide rows.
    with pytest.raises(Exception, match="element dims"):
        encode_element_values([[1.0, 2.0]], "tuple(3,f64)")
    # Dtype: the packing is always f64, so an integer tuple cannot be built here.
    with pytest.raises(Exception, match="i32"):
        encode_element_values([[1.0, 2.0, 3.0]], "tuple(3,i32)")
    # The agreeing case still works.
    assert encode_element_values([[1.0, 2.0, 3.0]], "tuple(3,f64)").shape == (1, 3)


def test_a_zero_arity_tuple_has_no_element_type():
    # `tuple(0,f64)` is not in the core's grammar, so encoding one would produce
    # an array whose element type cannot be parsed back.
    with pytest.raises(Exception):
        encode_element_values([[]], "tuple(0,f64)")


def test_an_empty_tuple_series_encodes_from_its_declared_arity():
    """A zero-length series is storable, so its encoding must exist.

    A tuple's arity lives in its rows, so an empty list cannot state one — but
    `element_type` did, and refusing here would leave a valid zero-length
    `tuple(3,f64)` array with no way back through the documented inverse.
    """
    encoded = encode_element_values([], "tuple(3,f64)", [0])
    assert encoded.shape == (0, 3)
    assert encoded.dtype == np.float64
    # And the round trip closes: the array decodes back to the empty list.
    assert decode_element_values(encoded, "tuple(3,f64)") == []

    # The default leading dims reach the same place.
    assert encode_element_values([], "tuple(3,f64)").shape == (0, 3)
    # The fixed-width kinds carry their width in the type, so they already did.
    assert encode_element_values([], "linear_function").shape == (0, 2)
    # Zero arity is still not an element type, empty or not.
    with pytest.raises(Exception):
        encode_element_values([], "tuple(0,f64)", [0])
