"""The two association tables: attribute attachments and parent/child edges.

Both are independent of time series, so these exercise an otherwise empty store.
The two tables share no rows and no identity rules, so each family gets its own
class and a final pair of tests pins the fact that they do not interfere.
"""

import pytest
import infrastore
from infrastore import (
    ParentChildAssociation,
    SupplementalAttributeAssociation,
    Store,
)


def attached(component_id, attribute_id):
    """The canonical attachment: a generator carrying geographic info."""
    return SupplementalAttributeAssociation(
        component_id, "Generator", attribute_id, "GeographicInfo"
    )


def attached_typed(component_id, component_type, attribute_id, attribute_type):
    return SupplementalAttributeAssociation(
        component_id, component_type, attribute_id, attribute_type
    )


def edge(parent_id, child_id):
    """The canonical edge: a generator (parent) wired to a bus (child)."""
    return ParentChildAssociation(parent_id, "Generator", child_id, "Bus")


def edge_typed(parent_id, parent_type, child_id, child_type):
    return ParentChildAssociation(parent_id, parent_type, child_id, child_type)


def store_with_attachments(*associations):
    store = Store.create(in_memory=True)
    if associations:
        store.add_supplemental_attribute_associations(list(associations))
    return store


def store_with_edges(*associations):
    store = Store.create(in_memory=True)
    if associations:
        store.add_parent_child_associations(list(associations))
    return store


# ---- Supplemental attributes ------------------------------------------------


class TestSupplementalAttributeObject:
    def test_getters(self):
        a = attached(1, 100)
        assert a.component_id == 1
        assert a.component_type == "Generator"
        assert a.attribute_id == 100
        assert a.attribute_type == "GeographicInfo"

    def test_eq_and_hash(self):
        assert attached(1, 100) == attached(1, 100)
        assert attached(1, 100) != attached(1, 101)
        # Structural equality, so a differing type name is a different object
        # even though the table treats the id pair as the same attachment.
        assert attached(1, 100) != attached_typed(1, "Load", 100, "GeographicInfo")
        assert len({attached(1, 100), attached(1, 100), attached(2, 100)}) == 2

    def test_repr(self):
        text = repr(attached(1, 100))
        assert text.startswith("SupplementalAttributeAssociation(component_id=1")
        assert '"Generator"' in text
        assert '"GeographicInfo"' in text


class TestSupplementalAttributeRoundTrip:
    def test_add_list_remove(self):
        store = Store.create(in_memory=True)
        store.add_supplemental_attribute_association(attached(1, 100))
        store.add_supplemental_attribute_association(attached(2, 100))
        assert store.list_supplemental_attribute_associations() == [
            attached(1, 100),
            attached(2, 100),
        ]

        assert store.remove_supplemental_attribute_associations(component_id=1) == 1
        assert store.list_supplemental_attribute_associations() == [attached(2, 100)]

    def test_removing_nothing_is_not_an_error(self):
        store = Store.create(in_memory=True)
        assert store.remove_supplemental_attribute_associations(component_id=999) == 0

    def test_independent_of_time_series(self):
        store = store_with_attachments(attached(1, 100))
        assert store.clear_time_series() == 0
        assert store.count_supplemental_attribute_associations() == 1

    def test_one_attribute_on_many_components(self):
        # A shared attribute is the common infrasys shape: one GeographicInfo
        # object hanging off every component in a region.
        store = store_with_attachments(attached(1, 100), attached(2, 100), attached(3, 100))
        assert store.list_components_with_attributes(attribute_id=100) == [1, 2, 3]
        assert store.count_supplemental_attributes() == 1
        assert store.count_components_with_attributes() == 3


class TestSupplementalAttributeUniqueness:
    def test_duplicate_pair_rejected_regardless_of_type_names(self):
        store = store_with_attachments(attached(1, 100))
        with pytest.raises(infrastore.DuplicateAssociationError):
            store.add_supplemental_attribute_association(
                attached_typed(1, "Load", 100, "Outage")
            )
        assert store.count_supplemental_attribute_associations() == 1

    def test_duplicate_error_is_in_the_hierarchy(self):
        assert issubclass(infrastore.DuplicateAssociationError, infrastore.TimeSeriesError)

    def test_swapped_ids_are_a_different_row(self):
        # Component and attribute id streams are independent, so the same two
        # numbers may name a different attachment when their roles swap.
        store = store_with_attachments(attached(1, 100))
        store.add_supplemental_attribute_association(attached(100, 1))
        assert store.count_supplemental_attribute_associations() == 2


class TestSupplementalAttributeFilters:
    def populated(self):
        return store_with_attachments(
            attached_typed(1, "Generator", 100, "GeographicInfo"),
            attached_typed(1, "Generator", 101, "Outage"),
            attached_typed(2, "Load", 100, "GeographicInfo"),
        )

    def test_narrow_by_id(self):
        store = self.populated()
        assert len(store.list_supplemental_attribute_associations(component_id=1)) == 2
        assert len(store.list_supplemental_attribute_associations(attribute_id=100)) == 2

    def test_multi_type_list(self):
        # The shape a caller renders after expanding an abstract type.
        store = self.populated()
        assert (
            len(
                store.list_supplemental_attribute_associations(
                    attribute_types=["GeographicInfo", "Outage"]
                )
            )
            == 3
        )
        assert store.list_supplemental_attribute_associations(component_types=["Load"]) == [
            attached_typed(2, "Load", 100, "GeographicInfo")
        ]

    def test_fields_are_anded(self):
        store = self.populated()
        assert store.list_supplemental_attribute_associations(
            component_id=1, attribute_types=["Outage"]
        ) == [attached_typed(1, "Generator", 101, "Outage")]
        assert (
            store.list_supplemental_attribute_associations(
                component_id=2, attribute_types=["Outage"]
            )
            == []
        )

    def test_empty_type_list_matches_nothing(self):
        # An empty allow-list is a deliberate "none of these", not "no filter".
        store = store_with_attachments(attached(1, 100))
        assert store.list_supplemental_attribute_associations(attribute_types=[]) == []
        assert store.count_supplemental_attribute_associations(attribute_types=[]) == 0
        assert store.has_supplemental_attribute_association(attribute_types=[]) is False
        assert store.list_supplemental_attribute_ids(component_types=[]) == []

    def test_has_dispatch_forms(self):
        store = store_with_attachments(attached(1, 100))
        assert store.has_supplemental_attribute_association(component_id=1, attribute_id=100)
        assert store.has_supplemental_attribute_association(component_id=1) is True
        assert store.has_supplemental_attribute_association(attribute_id=100) is True
        assert (
            store.has_supplemental_attribute_association(attribute_types=["GeographicInfo"])
            is True
        )
        assert store.has_supplemental_attribute_association(component_id=7) is False


class TestSupplementalAttributeIdsAndCounts:
    def populated(self):
        return store_with_attachments(
            attached_typed(1, "Generator", 100, "GeographicInfo"),
            attached_typed(1, "Generator", 101, "Outage"),
            attached_typed(2, "Load", 100, "GeographicInfo"),
        )

    def test_id_listing_in_both_directions(self):
        store = self.populated()
        # Attributes attached to component 1, then components carrying 100.
        assert store.list_supplemental_attribute_ids(component_id=1) == [100, 101]
        assert store.list_components_with_attributes(attribute_id=100) == [1, 2]

    def test_counts(self):
        store = self.populated()
        assert store.count_supplemental_attribute_associations() == 3
        assert store.count_supplemental_attributes() == 2
        assert store.count_components_with_attributes() == 2
        assert store.count_supplemental_attributes(component_id=1) == 2
        assert store.count_components_with_attributes(attribute_types=["Outage"]) == 1

    def test_counts_by_type(self):
        store = store_with_attachments(
            attached_typed(1, "Generator", 100, "GeographicInfo"),
            attached_typed(1, "Generator", 101, "Outage"),
            attached_typed(2, "Load", 102, "GeographicInfo"),
        )
        assert store.supplemental_attribute_counts_by_type() == [
            ("GeographicInfo", 2),
            ("Outage", 1),
        ]

    def test_summary(self):
        store = store_with_attachments(
            attached_typed(1, "Generator", 100, "GeographicInfo"),
            attached_typed(1, "Generator", 101, "Outage"),
            attached_typed(2, "Load", 102, "GeographicInfo"),
        )
        assert store.supplemental_attribute_summary() == [
            {"component_type": "Generator", "attribute_type": "GeographicInfo", "count": 1},
            {"component_type": "Load", "attribute_type": "GeographicInfo", "count": 1},
            {"component_type": "Generator", "attribute_type": "Outage", "count": 1},
        ]

    def test_summary_of_an_empty_table(self):
        store = Store.create(in_memory=True)
        assert store.supplemental_attribute_summary() == []
        assert store.supplemental_attribute_counts_by_type() == []


class TestSupplementalAttributeReplaceComponentId:
    def test_moves_every_attachment(self):
        store = store_with_attachments(attached(1, 100), attached(1, 101))
        assert store.replace_supplemental_attribute_component_id(1, 5) == 2
        assert store.list_supplemental_attribute_associations() == [
            attached(5, 100),
            attached(5, 101),
        ]

    def test_collision_rolls_back(self):
        store = store_with_attachments(attached(1, 100), attached(2, 100))
        with pytest.raises(infrastore.DuplicateAssociationError):
            store.replace_supplemental_attribute_component_id(1, 2)
        assert store.list_supplemental_attribute_associations() == [
            attached(1, 100),
            attached(2, 100),
        ]

    def test_unknown_id_moves_nothing(self):
        store = store_with_attachments(attached(1, 100))
        assert store.replace_supplemental_attribute_component_id(9, 5) == 0

    def test_attribute_ids_are_untouched(self):
        # The rewrite addresses the component end only, so an attribute id that
        # happens to equal `old_id` stays put.
        store = store_with_attachments(attached(1, 1))
        assert store.replace_supplemental_attribute_component_id(1, 5) == 1
        assert store.list_supplemental_attribute_associations() == [attached(5, 1)]


class TestSupplementalAttributeBulk:
    def test_export_import_round_trips(self):
        records = [
            attached_typed(1, "Generator", 100, "GeographicInfo"),
            attached_typed(1, "Generator", 101, "Outage"),
            attached_typed(2, "Load", 100, "GeographicInfo"),
        ]
        source = Store.create(in_memory=True)
        assert source.add_supplemental_attribute_associations(records) == len(records)
        exported = source.list_supplemental_attribute_associations()
        assert exported == records

        target = Store.create(in_memory=True)
        target.add_supplemental_attribute_associations(exported)
        assert target.list_supplemental_attribute_associations() == exported

    def test_all_or_nothing(self):
        store = Store.create(in_memory=True)
        with pytest.raises(infrastore.DuplicateAssociationError):
            store.add_supplemental_attribute_associations(
                [attached(1, 100), attached(2, 100), attached(1, 100)]
            )
        assert store.count_supplemental_attribute_associations() == 0


# ---- Parent/child edges -----------------------------------------------------


class TestParentChildObject:
    def test_getters(self):
        e = edge(1, 10)
        assert e.parent_id == 1
        assert e.parent_type == "Generator"
        assert e.child_id == 10
        assert e.child_type == "Bus"

    def test_eq_and_hash(self):
        assert edge(1, 10) == edge(1, 10)
        assert edge(1, 10) != edge(10, 1)
        assert edge(1, 10) != edge_typed(1, "Load", 10, "Bus")
        assert len({edge(1, 10), edge(1, 10), edge(2, 10)}) == 2

    def test_repr(self):
        text = repr(edge(1, 10))
        assert text.startswith("ParentChildAssociation(parent_id=1")
        assert '"Generator"' in text
        assert '"Bus"' in text


class TestParentChildRoundTrip:
    def test_add_list_remove(self):
        store = Store.create(in_memory=True)
        store.add_parent_child_association(edge(1, 10))
        store.add_parent_child_association(edge(2, 10))
        assert store.list_parent_child_associations() == [edge(1, 10), edge(2, 10)]

        assert store.remove_parent_child_associations(parent_id=1) == 1
        assert store.list_parent_child_associations() == [edge(2, 10)]

    def test_removing_nothing_is_not_an_error(self):
        store = Store.create(in_memory=True)
        assert store.remove_parent_child_associations(parent_id=999) == 0

    def test_independent_of_time_series(self):
        store = store_with_edges(edge(1, 10))
        assert store.clear_time_series() == 0
        assert store.count_parent_child_associations() == 1


class TestParentChildDirection:
    def test_reversed_pair_is_a_distinct_edge(self):
        store = store_with_edges(edge(1, 10))
        store.add_parent_child_association(edge_typed(10, "Bus", 1, "Generator"))
        assert store.count_parent_child_associations() == 2
        assert store.list_children(parent_id=1) == [10]
        assert store.list_children(parent_id=10) == [1]

    def test_duplicate_ordered_pair_rejected_regardless_of_type_names(self):
        store = store_with_edges(edge(1, 10))
        with pytest.raises(infrastore.DuplicateAssociationError):
            store.add_parent_child_association(edge_typed(1, "Load", 10, "Area"))
        assert store.count_parent_child_associations() == 1


class TestParentChildFilters:
    def populated(self):
        return store_with_edges(
            edge_typed(1, "Generator", 10, "Bus"),
            edge_typed(1, "Generator", 11, "Area"),
            edge_typed(2, "Load", 10, "Bus"),
        )

    def test_narrow_by_id(self):
        store = self.populated()
        assert len(store.list_parent_child_associations(parent_id=1)) == 2
        assert len(store.list_parent_child_associations(child_id=10)) == 2

    def test_multi_type_list(self):
        store = self.populated()
        assert (
            len(store.list_parent_child_associations(child_types=["Bus", "Area"])) == 3
        )
        assert store.list_parent_child_associations(parent_types=["Load"]) == [
            edge_typed(2, "Load", 10, "Bus")
        ]

    def test_fields_are_anded(self):
        store = self.populated()
        assert store.list_parent_child_associations(
            parent_id=1, child_types=["Area"]
        ) == [edge_typed(1, "Generator", 11, "Area")]
        assert (
            store.list_parent_child_associations(parent_id=2, child_types=["Area"]) == []
        )

    def test_empty_type_list_matches_nothing(self):
        store = store_with_edges(edge(1, 10))
        assert store.list_parent_child_associations(child_types=[]) == []
        assert store.count_parent_child_associations(child_types=[]) == 0
        assert store.has_parent_child_association(child_types=[]) is False
        assert store.list_parents(parent_types=[]) == []

    def test_has_dispatch_forms(self):
        store = store_with_edges(edge(1, 10))
        assert store.has_parent_child_association(parent_id=1, child_id=10) is True
        assert store.has_parent_child_association(parent_id=1) is True
        assert store.has_parent_child_association(child_id=10) is True
        assert store.has_parent_child_association(child_types=["Bus"]) is True
        assert store.has_parent_child_association(parent_id=10) is False


class TestParentChildIdsAndCounts:
    def populated(self):
        return store_with_edges(
            edge_typed(1, "Generator", 10, "Bus"),
            edge_typed(1, "Generator", 11, "Area"),
            edge_typed(2, "Load", 10, "Bus"),
        )

    def test_id_listing_in_both_directions(self):
        store = self.populated()
        assert store.list_children(parent_id=1) == [10, 11]
        assert store.list_parents(child_id=10) == [1, 2]

    def test_id_listing_is_distinct_and_ascending(self):
        store = store_with_edges(edge(3, 10), edge(1, 10), edge(2, 11))
        assert store.list_parents() == [1, 2, 3]
        assert store.list_children() == [10, 11]

    def test_counts(self):
        store = self.populated()
        assert store.count_parent_child_associations() == 3
        assert store.count_parent_child_associations(parent_id=1) == 2
        assert store.count_parent_child_associations(child_types=["Bus"]) == 2


class TestParentChildReplaceComponentId:
    def test_rewrites_both_ends(self):
        store = store_with_edges(edge(1, 10), edge_typed(5, "Area", 1, "Generator"))
        assert store.replace_parent_child_component_id(1, 7) == 2
        assert store.list_parent_child_associations() == [
            edge(7, 10),
            edge_typed(5, "Area", 7, "Generator"),
        ]

    def test_collision_rolls_back(self):
        store = store_with_edges(edge(1, 10), edge(2, 10))
        with pytest.raises(infrastore.DuplicateAssociationError):
            store.replace_parent_child_component_id(1, 2)
        assert store.list_parent_child_associations() == [edge(1, 10), edge(2, 10)]

    def test_unknown_id_rewrites_nothing(self):
        store = store_with_edges(edge(1, 10))
        assert store.replace_parent_child_component_id(9, 7) == 0


class TestParentChildBulk:
    def test_export_import_round_trips(self):
        records = [
            edge_typed(1, "Generator", 10, "Bus"),
            edge_typed(1, "Generator", 11, "Area"),
            edge_typed(2, "Load", 10, "Bus"),
        ]
        source = Store.create(in_memory=True)
        assert source.add_parent_child_associations(records) == len(records)
        exported = source.list_parent_child_associations()
        assert exported == records

        target = Store.create(in_memory=True)
        target.add_parent_child_associations(exported)
        assert target.list_parent_child_associations() == exported

    def test_all_or_nothing(self):
        store = Store.create(in_memory=True)
        with pytest.raises(infrastore.DuplicateAssociationError):
            store.add_parent_child_associations([edge(1, 10), edge(2, 10), edge(1, 10)])
        assert store.count_parent_child_associations() == 0


# ---- Persistence and read-only ----------------------------------------------


class TestPersistence:
    def test_both_tables_survive_persist_and_reopen(self, tmp_path):
        path = tmp_path / "assoc.h5"
        store = store_with_attachments(attached(1, 100), attached(2, 101))
        store.add_parent_child_associations([edge(1, 10), edge(2, 11)])
        store.persist_to(str(path))
        store.close()

        reopened = Store.open(str(path), read_only=True)
        assert reopened.list_supplemental_attribute_associations() == [
            attached(1, 100),
            attached(2, 101),
        ]
        assert reopened.list_parent_child_associations() == [edge(1, 10), edge(2, 11)]
        reopened.close()

    def test_read_only_store_rejects_attachment_writes(self, tmp_path):
        path = tmp_path / "attach_ro.h5"
        store = Store.create(str(path))
        store.add_supplemental_attribute_association(attached(1, 100))
        store.close()

        ro = Store.open(str(path), read_only=True)
        with pytest.raises(infrastore.ReadOnlyStoreError):
            ro.add_supplemental_attribute_association(attached(2, 100))
        with pytest.raises(infrastore.ReadOnlyStoreError):
            ro.add_supplemental_attribute_associations([attached(2, 100)])
        with pytest.raises(infrastore.ReadOnlyStoreError):
            ro.remove_supplemental_attribute_associations()
        with pytest.raises(infrastore.ReadOnlyStoreError):
            ro.replace_supplemental_attribute_component_id(1, 2)
        # Reads still work.
        assert ro.list_supplemental_attribute_associations() == [attached(1, 100)]
        ro.close()

    def test_read_only_store_rejects_edge_writes(self, tmp_path):
        path = tmp_path / "edge_ro.h5"
        store = Store.create(str(path))
        store.add_parent_child_association(edge(1, 10))
        store.close()

        ro = Store.open(str(path), read_only=True)
        with pytest.raises(infrastore.ReadOnlyStoreError):
            ro.add_parent_child_association(edge(2, 10))
        with pytest.raises(infrastore.ReadOnlyStoreError):
            ro.add_parent_child_associations([edge(2, 10)])
        with pytest.raises(infrastore.ReadOnlyStoreError):
            ro.remove_parent_child_associations()
        with pytest.raises(infrastore.ReadOnlyStoreError):
            ro.replace_parent_child_component_id(1, 2)
        assert ro.list_parent_child_associations() == [edge(1, 10)]
        ro.close()


class TestTablesAreIndependent:
    """The two tables share ids freely and never see each other's rows."""

    def populated(self):
        # Deliberately overlapping numbers: component 1 carries attribute 10 and
        # is also the parent of component 10.
        store = store_with_attachments(attached(1, 10))
        store.add_parent_child_association(edge(1, 10))
        return store

    def test_counts_do_not_bleed(self):
        store = self.populated()
        assert store.count_supplemental_attribute_associations() == 1
        assert store.count_parent_child_associations() == 1
        assert store.supplemental_attribute_counts_by_type() == [("GeographicInfo", 1)]

    def test_removing_one_leaves_the_other(self):
        store = self.populated()
        assert store.remove_supplemental_attribute_associations() == 1
        assert store.list_supplemental_attribute_associations() == []
        assert store.list_parent_child_associations() == [edge(1, 10)]

        assert store.remove_parent_child_associations() == 1
        assert store.list_parent_child_associations() == []

    def test_replace_ids_are_scoped_to_one_table(self):
        store = self.populated()
        assert store.replace_supplemental_attribute_component_id(1, 5) == 1
        assert store.list_supplemental_attribute_associations() == [attached(5, 10)]
        assert store.list_parent_child_associations() == [edge(1, 10)]

        assert store.replace_parent_child_component_id(1, 7) == 1
        assert store.list_parent_child_associations() == [edge(7, 10)]
        assert store.list_supplemental_attribute_associations() == [attached(5, 10)]

    def test_the_same_id_pair_is_addable_to_both(self):
        store = self.populated()
        assert store.has_supplemental_attribute_association(component_id=1, attribute_id=10)
        assert store.has_parent_child_association(parent_id=1, child_id=10)
