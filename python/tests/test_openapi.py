"""Direct JSON serde of the two association catalogs in the OpenAPI wire
spelling: `export_time_series_associations_openapi`,
`import_time_series_associations_openapi`,
`export_supplemental_attribute_associations_openapi`, and
`import_supplemental_attribute_associations_openapi`.

The golden tests reproduce two of the checked-in fixtures at
`conformance/openapi_row_fixtures/` (the core's own golden tests pin the
rest).
"""

import json
import os
from datetime import datetime, timedelta, timezone

import numpy as np
import pytest
import infrastore
from infrastore import (
    NonSequentialTimeSeries,
    OwnerCategory,
    SingleTimeSeries,
    Store,
    SupplementalAttributeAssociation,
)

T0 = datetime(2030, 1, 1, tzinfo=timezone.utc)
HOUR = timedelta(hours=1)

FIXTURES_DIR = os.path.join(
    os.path.dirname(__file__), "..", "..", "conformance", "openapi_row_fixtures"
)


def fixture(name):
    with open(os.path.join(FIXTURES_DIR, f"{name}.json")) as f:
        return json.load(f)


def attached(component_id, attribute_id, attribute_type="GeographicInfo"):
    return SupplementalAttributeAssociation(
        component_id, "Generator", attribute_id, attribute_type
    )


class TestTimeSeriesExport:
    def test_reproduces_the_single_time_series_fixture(self):
        store = Store.create(in_memory=True)
        store.add_time_series(
            owner_id=7, owner_type="ThermalStandard", owner_category=OwnerCategory.Component,
            time_series=SingleTimeSeries(
                T0, HOUR, np.zeros(8760, dtype=np.float64), "max_active_power"
            ),
            units="MW", quantity_kind="ActivePower", unit_system="natural_units",
            component_field="max_active_power",
            features={"scenario": "high_load", "year": 2030},
        )

        rows = json.loads(store.export_time_series_associations_openapi())
        assert len(rows) == 1
        row = rows[0]
        # The schema requires ``association_id``, so the fixture carries one --
        # but its *value* is the store's own bookkeeping, depending on how many
        # rows were written first. Presence is asserted here and the value
        # dropped from both sides.
        assert row.pop("association_id") == 1
        # ``time_reference`` is inferred from the datetime's tzinfo here, where
        # the fixture (built from the Rust core, which infers nothing) leaves
        # the reference unspecified. The value is right; it is not the
        # fixture's to carry.
        assert row.pop("time_reference") == "utc"
        want = fixture("single_time_series")
        want.pop("association_id")
        assert row == want

    def test_empty_filter_exports_the_whole_catalog(self):
        store = Store.create(in_memory=True)
        for owner, name in [(1, "a"), (2, "b")]:
            store.add_time_series(
                owner_id=owner, owner_type="Generator", owner_category=OwnerCategory.Component,
                time_series=SingleTimeSeries(T0, HOUR, np.zeros(4, dtype=np.float64), name),
            )
        rows = json.loads(store.export_time_series_associations_openapi())
        assert len(rows) == 2

    def test_owner_id_filter_narrows_the_export(self):
        store = Store.create(in_memory=True)
        for owner, name in [(1, "a"), (2, "b")]:
            store.add_time_series(
                owner_id=owner, owner_type="Generator", owner_category=OwnerCategory.Component,
                time_series=SingleTimeSeries(T0, HOUR, np.zeros(4, dtype=np.float64), name),
            )
        rows = json.loads(
            store.export_time_series_associations_openapi(owner_id=1)
        )
        assert len(rows) == 1
        assert rows[0]["owner_id"] == 1


class TestSupplementalAttributeExportImport:
    def test_export_reproduces_the_fixture(self):
        store = Store.create(in_memory=True)
        store.add_supplemental_attribute_association(
            SupplementalAttributeAssociation(
                7, "ThermalStandard", 481, "GeometricDistributionForcedOutage"
            )
        )
        rows = json.loads(store.export_supplemental_attribute_associations_openapi())
        assert rows == [fixture("supplemental_attribute_association")]

    def test_export_import_round_trips(self):
        source = Store.create(in_memory=True)
        source.add_supplemental_attribute_associations(
            [attached(1, 100), attached(2, 100)]
        )
        exported = source.export_supplemental_attribute_associations_openapi()

        target = Store.create(in_memory=True)
        assert target.import_supplemental_attribute_associations_openapi(exported) == 2
        re_exported = target.export_supplemental_attribute_associations_openapi()
        assert json.loads(re_exported) == json.loads(exported)

    def test_import_rejects_malformed_json(self):
        store = Store.create(in_memory=True)
        with pytest.raises(infrastore.StorageError):
            store.import_supplemental_attribute_associations_openapi("{not valid json")

    def test_import_rejects_unknown_fields(self):
        store = Store.create(in_memory=True)
        bad = json.dumps(
            [
                {
                    "component_id": 1, "component_type": "Generator",
                    "attribute_id": 100, "attribute_type": "GeographicInfo",
                    "extra": "nope",
                }
            ]
        )
        with pytest.raises(infrastore.StorageError):
            store.import_supplemental_attribute_associations_openapi(bad)

    def test_import_rolls_back_a_duplicate_within_the_batch(self):
        store = Store.create(in_memory=True)
        row = {
            "component_id": 1, "component_type": "Generator",
            "attribute_id": 100, "attribute_type": "GeographicInfo",
        }
        with pytest.raises(infrastore.DuplicateAssociationError):
            store.import_supplemental_attribute_associations_openapi(json.dumps([row, row]))
        assert store.export_supplemental_attribute_associations_openapi() == "[]"


class TestTimeSeriesImport:
    """The rows-only import half: the document carries locators, so the arrays
    have to already be in the target store, and the ids it carries are kept."""

    def _source(self):
        store = Store.create(in_memory=True)
        # Ids are assigned, never chosen, so the way to put this document's
        # rows above the target's high-water mark is to run the exporter's
        # counter up first.
        for i in range(100):
            spacer = store.add_time_series(
                owner_id=-1, owner_type="Spacer",
                owner_category=OwnerCategory.Component,
                time_series=SingleTimeSeries(
                    T0, HOUR, np.full(4, float(i), dtype=np.float64), f"__spacer{i}"
                ),
            )
            store.remove_by_ids([spacer])
        expected = {}
        for owner, name in [(1, "load"), (2, "wind")]:
            added = store.add_time_series(
                owner_id=owner, owner_type="Generator",
                owner_category=OwnerCategory.Component,
                time_series=SingleTimeSeries(
                    T0, HOUR, np.zeros(4, dtype=np.float64), name
                ),
            )
            expected[added] = name
        return store, expected

    def _target_holding_the_arrays(self):
        # Arrays are content-addressed, so "the artifact brought the values" is
        # a store already holding the same bytes under an identity of its own.
        store = Store.create(in_memory=True)
        store.add_time_series(
            owner_id=9, owner_type="Anchor", owner_category=OwnerCategory.Component,
            time_series=SingleTimeSeries(
                T0, HOUR, np.zeros(4, dtype=np.float64), "anchor"
            ),
        )
        return store

    def test_round_trips_with_its_ids(self):
        source, expected = self._source()
        exported = source.export_time_series_associations_openapi()

        target = self._target_holding_the_arrays()
        assert target.import_time_series_associations_openapi(exported) == 2
        for id_, name in expected.items():
            meta = target.get_metadata_by_id(id_)
            assert meta is not None, f"id {id_} did not survive the import"
            assert meta["name"] == name

    def test_rejects_an_id_at_or_below_the_targets_high_water_mark(self):
        """The import is the only door a caller-supplied id comes through, so
        the "never reissued" guarantee is enforced here: an id the destination
        catalog could already have issued is refused rather than re-filed."""
        source = Store.create(in_memory=True)
        source.add_time_series(
            owner_id=1, owner_type="Generator",
            owner_category=OwnerCategory.Component,
            time_series=SingleTimeSeries(
                T0, HOUR, np.zeros(4, dtype=np.float64), "load"
            ),
        )
        exported = source.export_time_series_associations_openapi()

        # The target's own anchor row took id 1, which is the id the document
        # names, so the document does not fit this store.
        target = self._target_holding_the_arrays()
        with pytest.raises(infrastore.DuplicateAssociationIdError):
            target.import_time_series_associations_openapi(exported)

    def test_rejects_a_row_whose_array_is_absent(self):
        source, _ = self._source()
        exported = source.export_time_series_associations_openapi()

        empty = Store.create(in_memory=True)
        with pytest.raises(infrastore.InvalidParameterError):
            empty.import_time_series_associations_openapi(exported)
        assert empty.list_metadata() == []

    def test_rejects_an_irregular_row(self):
        store = Store.create(in_memory=True)
        store.add_time_series(
            owner_id=1, owner_type="Generator", owner_category=OwnerCategory.Component,
            time_series=NonSequentialTimeSeries(
                [T0, T0 + 5 * HOUR], np.array([1.0, 2.0], dtype=np.float64), "events"
            ),
        )
        exported = store.export_time_series_associations_openapi()
        with pytest.raises(infrastore.InvalidParameterError):
            store.import_time_series_associations_openapi(exported)

    def test_rejects_malformed_json(self):
        store = Store.create(in_memory=True)
        with pytest.raises(infrastore.StorageError):
            store.import_time_series_associations_openapi("{not valid json")
