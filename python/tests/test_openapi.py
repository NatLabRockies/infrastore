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
        # The fixture is a golden of row *content*, so it carries no ``id``: an
        # id is the store's own bookkeeping, and its value depends on how many
        # rows were written before it. That the export emits one is asserted
        # here instead.
        assert row.pop("id") == 1
        want = fixture("single_time_series")
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
