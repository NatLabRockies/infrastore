"""Direct JSON serde of the two association catalogs in the OpenAPI wire
spelling: `export_time_series_associations_openapi`,
`export_supplemental_attribute_associations_openapi`,
`import_supplemental_attribute_associations_openapi`, and
`reconcile_time_series_associations_openapi`.

The golden tests reproduce two of the checked-in fixtures at
`conformance/openapi_row_fixtures/` (the core's own golden tests pin the
rest); the reconcile tests exercise the D4 policy matrix one cell at a time,
mirroring `crates/infrastore-core/tests/openapi.rs`.
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

        rows = json.loads(
            store.export_time_series_associations_openapi(address="time_series.h5")
        )
        assert len(rows) == 1
        row = rows[0]
        row.pop("id")
        want = fixture("single_time_series")
        want.pop("id")
        assert row == want

    def test_empty_filter_exports_the_whole_catalog(self):
        store = Store.create(in_memory=True)
        for owner, name in [(1, "a"), (2, "b")]:
            store.add_time_series(
                owner_id=owner, owner_type="Generator", owner_category=OwnerCategory.Component,
                time_series=SingleTimeSeries(T0, HOUR, np.zeros(4, dtype=np.float64), name),
            )
        rows = json.loads(store.export_time_series_associations_openapi(address="s.h5"))
        assert len(rows) == 2

    def test_owner_id_filter_narrows_the_export(self):
        store = Store.create(in_memory=True)
        for owner, name in [(1, "a"), (2, "b")]:
            store.add_time_series(
                owner_id=owner, owner_type="Generator", owner_category=OwnerCategory.Component,
                time_series=SingleTimeSeries(T0, HOUR, np.zeros(4, dtype=np.float64), name),
            )
        rows = json.loads(
            store.export_time_series_associations_openapi(address="s.h5", owner_id=1)
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


def reconcile_fixture_store():
    """One `SingleTimeSeries` row every reconcile test below either matches
    verbatim or perturbs one column of."""
    store = Store.create(in_memory=True)
    store.add_time_series(
        owner_id=1, owner_type="Generator", owner_category=OwnerCategory.Component,
        time_series=SingleTimeSeries(T0, HOUR, np.zeros(24, dtype=np.float64), "load"),
        units="MW", quantity_kind="ActivePower", unit_system="natural_units",
        component_field="load",
    )
    return store


def clean_reconcile_row():
    return {
        "owner_id": 1, "owner_type": "Generator", "owner_category": "Component",
        "time_series_type": "SingleTimeSeries", "name": "load", "features": {},
        "address": "store.h5", "element_type": "f64", "element_shape": [],
        "units": "MW", "quantity_kind": "ActivePower", "unit_system": "NATURAL_UNITS",
        "component_field": "load",
        "initial_timestamp": "2030-01-01T00:00:00Z", "resolution": "PT1H", "length": 24,
    }


class TestReconcile:
    @pytest.mark.parametrize("policy", ["strict", "update_descriptive"])
    def test_clean_match_is_a_no_op(self, policy):
        store = reconcile_fixture_store()
        report = store.reconcile_time_series_associations_openapi(
            json.dumps([clean_reconcile_row()]), policy=policy
        )
        assert report == {
            "matched": 1, "updated": 0, "missing_in_store": 0,
            "unmatched_in_store": 0, "conflicts": [],
        }

    def test_descriptive_drift_errors_under_strict(self):
        store = reconcile_fixture_store()
        row = clean_reconcile_row()
        row["units"] = "kW"
        with pytest.raises(infrastore.ReconcileConflictError, match="units"):
            store.reconcile_time_series_associations_openapi(
                json.dumps([row]), policy="strict"
            )

    def test_descriptive_drift_is_applied_under_update_descriptive(self):
        store = reconcile_fixture_store()
        row = clean_reconcile_row()
        row["units"] = "kW"
        row["component_field"] = "net_load"
        report = store.reconcile_time_series_associations_openapi(
            json.dumps([row]), policy="update_descriptive"
        )
        assert report["matched"] == 1
        assert report["updated"] == 1
        assert len(report["conflicts"]) == 1

        rows = json.loads(
            store.export_time_series_associations_openapi(address="store.h5")
        )
        assert rows[0]["units"] == "kW"
        assert rows[0]["component_field"] == "net_load"
        assert rows[0]["length"] == 24

    @pytest.mark.parametrize("policy", ["strict", "update_descriptive"])
    def test_geometry_drift_errors_under_both_policies(self, policy):
        store = reconcile_fixture_store()
        row = clean_reconcile_row()
        row["length"] = 25
        with pytest.raises(infrastore.ReconcileConflictError, match="geometry drift"):
            store.reconcile_time_series_associations_openapi(
                json.dumps([row]), policy=policy
            )

    def test_json_row_with_no_catalog_match_errors(self):
        store = reconcile_fixture_store()
        row = clean_reconcile_row()
        row["name"] = "a_series_the_store_does_not_have"
        with pytest.raises(infrastore.ReconcileConflictError):
            store.reconcile_time_series_associations_openapi(
                json.dumps([row]), policy="strict"
            )

    def test_tolerates_and_counts_a_catalog_row_absent_from_the_json(self):
        store = reconcile_fixture_store()
        report = store.reconcile_time_series_associations_openapi("[]", policy="strict")
        assert report["matched"] == 0
        assert report["unmatched_in_store"] == 1

    def test_address_check_passes_when_it_matches(self):
        store = reconcile_fixture_store()
        report = store.reconcile_time_series_associations_openapi(
            json.dumps([clean_reconcile_row()]),
            policy="strict",
            expected_address="store.h5",
        )
        assert report["matched"] == 1

    def test_address_check_fails_when_it_mismatches(self):
        store = reconcile_fixture_store()
        with pytest.raises(infrastore.ReconcileConflictError, match="address"):
            store.reconcile_time_series_associations_openapi(
                json.dumps([clean_reconcile_row()]),
                policy="strict",
                expected_address="other_store.h5",
            )

    def test_unknown_policy_string_is_rejected(self):
        store = reconcile_fixture_store()
        with pytest.raises(infrastore.InvalidParameterError):
            store.reconcile_time_series_associations_openapi("[]", policy="bogus")


def test_reconcile_conflict_error_is_a_time_series_error():
    assert issubclass(infrastore.ReconcileConflictError, infrastore.TimeSeriesError)
