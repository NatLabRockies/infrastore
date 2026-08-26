"""Direct JSON serde of the two association catalogs in the OpenAPI wire
spelling: `export_time_series_associations_openapi`,
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
        want = fixture("single_time_series")

        # `association_id` is minted per store and records insertion order, so
        # this store's value is not the fixture's: this store holds one series,
        # the fixture's holds six. The fixture pins the wire shape; the id's own
        # contract is that it addresses the row it was exported with.
        assert {k: v for k, v in row.items() if k != "association_id"} == {
            k: v for k, v in want.items() if k != "association_id"
        }
        assert row["association_id"] > 0

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


class TestImportedAssociationIds:
    """A supplied ``association_id`` is stored as given, so an export and a
    re-import into a fresh store preserve every id."""

    def _series(self, name):
        return SingleTimeSeries(
            initial_timestamp=T0,
            resolution=HOUR,
            data=np.arange(3.0),
            name=name,
        )

    def test_ids_survive_export_and_reimport(self):
        source = Store.create(in_memory=True)
        for owner, name in [(1, "load"), (2, "wind"), (3, "solar")]:
            source.add_time_series(
                owner_id=owner,
                owner_type="Generator",
                owner_category=OwnerCategory.Component,
                time_series=self._series(name),
            )
        exported = [
            (r["association_id"], r["name"], r["owner_id"])
            for r in json.loads(source.export_time_series_associations_openapi())
        ]

        # Reversed, so a preserved id cannot be an artifact of insertion order.
        target = Store.create(in_memory=True)
        target.add_time_series_bulk(
            [
                {
                    "owner_id": owner,
                    "owner_type": "Generator",
                    "owner_category": OwnerCategory.Component,
                    "time_series": self._series(name),
                    "association_id": assoc_id,
                }
                for assoc_id, name, owner in reversed(exported)
            ]
        )

        # Verified through the export: Python has no by-id getter and does not
        # surface `association_id` on its metadata object, though core, the C ABI,
        # and Julia all do. The export is the only read path for the id here.
        reimported = {
            r["association_id"]: r["name"]
            for r in json.loads(target.export_time_series_associations_openapi())
        }
        for assoc_id, name, _ in exported:
            assert reimported[assoc_id] == name

        # A later add lands past every imported id.
        target.add_time_series(
            owner_id=9,
            owner_type="Generator",
            owner_category=OwnerCategory.Component,
            time_series=self._series("added_after"),
        )
        rows = json.loads(target.export_time_series_associations_openapi())
        fresh = next(r["association_id"] for r in rows if r["name"] == "added_after")
        assert fresh > max(i for i, _, _ in exported)

    def test_importing_a_taken_id_is_refused(self):
        store = Store.create(in_memory=True)
        store.add_time_series(
            owner_id=1,
            owner_type="Generator",
            owner_category=OwnerCategory.Component,
            time_series=self._series("first"),
        )
        rows = json.loads(store.export_time_series_associations_openapi())
        taken = rows[0]["association_id"]

        with pytest.raises(infrastore.InvalidParameterError, match=str(taken)):
            store.add_time_series_bulk(
                [
                    {
                        "owner_id": 2,
                        "owner_type": "Generator",
                        "owner_category": OwnerCategory.Component,
                        "time_series": self._series("second"),
                        "association_id": taken,
                    }
                ]
            )

        # All-or-nothing: the refused batch left nothing behind.
        assert len(json.loads(store.export_time_series_associations_openapi())) == 1
