"""``Store.is_empty()`` — emptiness as a store fact.

The point of the predicate is *coverage*: it must account for every persistent
table, not just the ones the caller knows about. A consumer that skips writing
its artifact when the store reports empty drops, with no error, anything the
predicate misses — so each catalog gets a case of its own, held in isolation.
"""

from datetime import datetime, timedelta, timezone

import numpy as np

from infrastore import (
    OwnerCategory,
    ParentChildAssociation,
    SingleTimeSeries,
    Store,
    SupplementalAttributeAssociation,
)

T0 = datetime(2030, 1, 1, tzinfo=timezone.utc)


def _store():
    return Store.create(in_memory=True)


def _sts(name="load"):
    return SingleTimeSeries(
        T0, timedelta(hours=1), np.array([1.0, 2.0, 3.0, 4.0]), name
    )


def test_fresh_store_is_empty():
    assert _store().is_empty()


def test_time_series_alone_is_not_empty():
    store = _store()
    key = store.add_time_series(1, "Generator", OwnerCategory.Component, _sts())
    assert not store.is_empty()

    store.remove_time_series(key)
    assert store.is_empty()


def test_supplemental_attribute_associations_alone_are_not_empty():
    store = _store()
    store.add_supplemental_attribute_association(
        SupplementalAttributeAssociation(1, "Generator", 10, "GeographicInfo")
    )
    assert not store.is_empty()

    assert store.remove_supplemental_attribute_associations() == 1
    assert store.is_empty()


def test_parent_child_associations_alone_are_not_empty():
    """The case a client-side conjunction over the other two tables gets wrong."""
    store = _store()
    store.add_parent_child_association(ParentChildAssociation(1, "Generator", 10, "Bus"))
    assert not store.is_empty()

    assert store.remove_parent_child_associations() == 1
    assert store.is_empty()


def test_no_single_table_short_circuits_the_answer():
    store = _store()
    key = store.add_time_series(1, "Generator", OwnerCategory.Component, _sts())
    store.add_supplemental_attribute_association(
        SupplementalAttributeAssociation(1, "Generator", 10, "GeographicInfo")
    )
    store.add_parent_child_association(ParentChildAssociation(1, "Generator", 20, "Bus"))

    store.remove_time_series(key)
    assert not store.is_empty()
    store.remove_supplemental_attribute_associations()
    assert not store.is_empty()
    store.remove_parent_child_associations()
    assert store.is_empty()
