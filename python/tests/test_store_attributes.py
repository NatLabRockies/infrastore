"""Store attributes — key/value provenance about the artifact as a whole.

Distinct from a *supplemental* attribute (which belongs to a component) and from
``application_data`` (which belongs to one series): these describe the store.
The store never interprets a value, so what is tested here is the shape of the
four calls, the reserved namespace, and survival across a save.
"""

import pytest

from infrastore import InvalidParameterError, ReadOnlyStoreError, Store


def _store():
    return Store.create(in_memory=True)


def test_set_get_list_remove_round_trip():
    store = _store()
    assert store.list_store_attributes() == {}
    assert store.get_store_attribute("creator") is None

    store.set_store_attribute("creator", "sienna-build")
    store.set_store_attribute("source_system", "WECC 2032 ADS")
    assert store.get_store_attribute("creator") == "sienna-build"
    assert store.list_store_attributes() == {
        "creator": "sienna-build",
        "source_system": "WECC 2032 ADS",
    }

    assert store.remove_store_attribute("creator") is True
    assert store.get_store_attribute("creator") is None
    assert store.list_store_attributes() == {"source_system": "WECC 2032 ADS"}


def test_set_replaces_rather_than_appends():
    """An artifact records one creator, not a history of them."""
    store = _store()
    store.set_store_attribute("schema_version", "3")
    store.set_store_attribute("schema_version", "4")
    assert store.list_store_attributes() == {"schema_version": "4"}


def test_absent_key_is_a_question_not_an_error():
    store = _store()
    assert store.get_store_attribute("nothing") is None
    assert store.remove_store_attribute("nothing") is False


def test_an_empty_value_is_not_an_absent_one():
    store = _store()
    store.set_store_attribute("note", "")
    assert store.get_store_attribute("note") == ""
    assert store.get_store_attribute("absent") is None


def test_empty_key_is_refused():
    store = _store()
    with pytest.raises(InvalidParameterError):
        store.set_store_attribute("", "value")
    with pytest.raises(InvalidParameterError):
        store.remove_store_attribute("")


def test_reserved_prefix_is_refused_in_both_directions():
    store = _store()
    with pytest.raises(InvalidParameterError, match="infrastore."):
        store.set_store_attribute("infrastore.generation", "1")
    # Refused on removal too: a reserved key that can be deleted is not
    # reserved.
    with pytest.raises(InvalidParameterError):
        store.remove_store_attribute("infrastore.generation")
    # Only the prefix is reserved.
    store.set_store_attribute("infrastore_notes", "written by hand")


def test_values_carry_json_for_a_caller_wanting_structure():
    """The store never parses a value, so structure rides in the text."""
    import json

    store = _store()
    payload = {"pipeline": "nightly", "run": 412}
    store.set_store_attribute("provenance", json.dumps(payload))
    assert json.loads(store.get_store_attribute("provenance")) == payload


def test_attributes_survive_a_save_and_a_read_only_open(tmp_path):
    path = tmp_path / "store.h5"
    store = Store.create(str(path))
    store.set_store_attribute("creator", "sienna-build")
    store.flush()
    del store

    reopened = Store.open(str(path), read_only=True)
    assert reopened.get_store_attribute("creator") == "sienna-build"
    with pytest.raises(ReadOnlyStoreError):
        reopened.set_store_attribute("creator", "someone else")
    with pytest.raises(ReadOnlyStoreError):
        reopened.remove_store_attribute("creator")
