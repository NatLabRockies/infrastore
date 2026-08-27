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

from infrastore import OwnerCategory, SingleTimeSeries, Store, decode_element_values

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
    ).key
    meta = store.get_metadata(key)
    assert meta["element_type"] == "piecewise_linear"

    read = np.asarray(store.get_time_series(key).data)
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
