"""Type stubs for the `infrastore` extension module.

Hand-written (see API_DESIGN_PLAN_2.md, item 3.1). A pytest guard
(`python/tests/test_stubs.py`) asserts every public runtime name and method
appears here; keep the two in sync.
"""

from datetime import datetime, timedelta
from types import TracebackType
from typing import Any, final

import numpy as np
from numpy.typing import NDArray

# A period is passed as an ISO-8601 duration string (e.g. "PT1H", "P1M") or a
# datetime.timedelta; it is always returned as an ISO-8601 string.
Period = str | timedelta
TimeSeriesData = (
    SingleTimeSeries | NonSequentialTimeSeries | Deterministic | Probabilistic | Scenarios
)

__version__: str

# ---- Exceptions ------------------------------------------------------------

class TimeSeriesError(Exception): ...
class NotFoundError(TimeSeriesError): ...
class DuplicateTimeSeriesError(TimeSeriesError): ...
class DuplicateAssociationError(TimeSeriesError): ...
class InvalidParameterError(TimeSeriesError): ...
class IntegrityError(TimeSeriesError): ...
class ReadOnlyStoreError(TimeSeriesError): ...
class IoError(TimeSeriesError): ...
class ConnectionError(TimeSeriesError): ...
class IncompatibleFormatError(TimeSeriesError): ...
class IncompatibleForecastError(TimeSeriesError): ...
class StorageError(TimeSeriesError): ...

# ---- Enums -----------------------------------------------------------------

@final
class TimeSeriesType:
    SingleTimeSeries: TimeSeriesType
    NonSequentialTimeSeries: TimeSeriesType
    Deterministic: TimeSeriesType
    DeterministicSingleTimeSeries: TimeSeriesType
    Probabilistic: TimeSeriesType
    Scenarios: TimeSeriesType
    def __eq__(self, other: object) -> bool: ...
    def __int__(self) -> int: ...
    def __hash__(self) -> int: ...

@final
class OwnerCategory:
    Component: OwnerCategory
    SupplementalAttribute: OwnerCategory
    def __eq__(self, other: object) -> bool: ...
    def __int__(self) -> int: ...
    def __hash__(self) -> int: ...

# ---- Time-series value types ----------------------------------------------

@final
class SingleTimeSeries:
    def __init__(
        self,
        initial_timestamp: datetime,
        resolution: Period,
        data: NDArray[Any],
        name: str,
    ) -> None: ...
    @property
    def name(self) -> str: ...
    @property
    def initial_timestamp(self) -> datetime: ...
    @property
    def length(self) -> int: ...
    @property
    def resolution(self) -> str: ...
    @property
    def data(self) -> NDArray[Any]: ...
    def __eq__(self, other: object) -> bool: ...
    def __len__(self) -> int: ...

@final
class NonSequentialTimeSeries:
    def __init__(
        self,
        timestamps: list[datetime],
        data: NDArray[Any],
        name: str,
    ) -> None: ...
    @property
    def name(self) -> str: ...
    @property
    def timestamps(self) -> list[datetime]: ...
    @property
    def length(self) -> int: ...
    @property
    def data(self) -> NDArray[Any]: ...
    def __eq__(self, other: object) -> bool: ...
    def __len__(self) -> int: ...

@final
class Deterministic:
    def __init__(
        self,
        initial_timestamp: datetime,
        resolution: Period,
        horizon: Period,
        interval: Period,
        count: int,
        data: NDArray[Any],
        name: str,
    ) -> None: ...
    @property
    def name(self) -> str: ...
    @property
    def initial_timestamp(self) -> datetime: ...
    @property
    def resolution(self) -> str: ...
    @property
    def horizon(self) -> str: ...
    @property
    def interval(self) -> str: ...
    @property
    def count(self) -> int: ...
    @property
    def data(self) -> NDArray[Any]: ...
    def __eq__(self, other: object) -> bool: ...
    def __len__(self) -> int: ...

@final
class Probabilistic:
    def __init__(
        self,
        initial_timestamp: datetime,
        resolution: Period,
        horizon: Period,
        interval: Period,
        count: int,
        percentiles: list[float],
        data: NDArray[Any],
        name: str,
    ) -> None: ...
    @property
    def name(self) -> str: ...
    @property
    def initial_timestamp(self) -> datetime: ...
    @property
    def resolution(self) -> str: ...
    @property
    def horizon(self) -> str: ...
    @property
    def interval(self) -> str: ...
    @property
    def count(self) -> int: ...
    @property
    def percentiles(self) -> list[float]: ...
    @property
    def data(self) -> NDArray[Any]: ...
    def __eq__(self, other: object) -> bool: ...
    def __len__(self) -> int: ...

@final
class Scenarios:
    def __init__(
        self,
        initial_timestamp: datetime,
        resolution: Period,
        horizon: Period,
        interval: Period,
        count: int,
        data: NDArray[Any],
        name: str,
    ) -> None: ...
    @property
    def name(self) -> str: ...
    @property
    def initial_timestamp(self) -> datetime: ...
    @property
    def resolution(self) -> str: ...
    @property
    def horizon(self) -> str: ...
    @property
    def interval(self) -> str: ...
    @property
    def count(self) -> int: ...
    @property
    def scenario_count(self) -> int: ...
    @property
    def data(self) -> NDArray[Any]: ...
    def __eq__(self, other: object) -> bool: ...
    def __len__(self) -> int: ...

# ---- Keys ------------------------------------------------------------------

@final
class TimeSeriesKey:
    @property
    def owner_id(self) -> int: ...
    @property
    def owner_category(self) -> OwnerCategory: ...
    @property
    def time_series_type(self) -> TimeSeriesType: ...
    @property
    def name(self) -> str: ...
    @property
    def resolution(self) -> str | None: ...
    @property
    def interval(self) -> str | None: ...
    @property
    def features(self) -> dict[str, int | float | bool | str]: ...
    def __eq__(self, other: object) -> bool: ...
    def __hash__(self) -> int: ...

# ---- Associations ----------------------------------------------------------

@final
class SupplementalAttributeAssociation:
    def __init__(
        self,
        component_id: int,
        component_type: str,
        attribute_id: int,
        attribute_type: str,
    ) -> None: ...
    @property
    def component_id(self) -> int: ...
    @property
    def component_type(self) -> str: ...
    @property
    def attribute_id(self) -> int: ...
    @property
    def attribute_type(self) -> str: ...
    def __eq__(self, other: object) -> bool: ...
    def __hash__(self) -> int: ...

@final
class ParentChildAssociation:
    def __init__(
        self,
        parent_id: int,
        parent_type: str,
        child_id: int,
        child_type: str,
    ) -> None: ...
    @property
    def parent_id(self) -> int: ...
    @property
    def parent_type(self) -> str: ...
    @property
    def child_id(self) -> int: ...
    @property
    def child_type(self) -> str: ...
    def __eq__(self, other: object) -> bool: ...
    def __hash__(self) -> int: ...

# ---- Readers ---------------------------------------------------------------

@final
class StaticReader:
    def grid(self) -> dict[str, Any]: ...
    def groups(self) -> list[dict[str, Any]]: ...
    def timestamps(self) -> list[datetime]: ...
    def group_values(self, index: int) -> NDArray[Any]: ...

@final
class ForecastReader:
    def timeline(self) -> dict[str, Any]: ...
    def entries(self) -> list[TimeSeriesKey]: ...
    def timestamps(self) -> list[datetime]: ...
    def entry_values(self, index: int) -> NDArray[Any]: ...
    def num_slots(self) -> int: ...
    def entry_slot(self, index: int) -> int: ...

# ---- Store -----------------------------------------------------------------

@final
class Store:
    @classmethod
    def create(
        cls,
        path: str | None = None,
        *,
        in_memory: bool = False,
        compression: str = "deflate",
        compression_level: int = 3,
        shuffle: bool = True,
    ) -> Store: ...
    @classmethod
    def open(cls, path: str, *, read_only: bool = False) -> Store: ...
    @property
    def read_only(self) -> bool: ...
    def close(self) -> None: ...
    def __enter__(self) -> Store: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None = None,
        exc_value: BaseException | None = None,
        traceback: TracebackType | None = None,
    ) -> bool: ...

    # -- writes --
    def add_time_series(
        self,
        owner_id: int,
        owner_type: str,
        owner_category: OwnerCategory,
        time_series: TimeSeriesData,
        *,
        features: dict[str, int | float | bool | str] | None = None,
        units: str | None = None,
        ext: str | None = None,
    ) -> TimeSeriesKey: ...
    def add_time_series_bulk(self, items: list[dict[str, Any]]) -> list[TimeSeriesKey]: ...
    def transform_single_time_series(
        self,
        horizon: Period,
        interval: Period,
        *,
        owner_category: OwnerCategory | None = None,
        resolution: Period | None = None,
    ) -> int: ...
    def remove_time_series(self, key: TimeSeriesKey) -> None: ...
    def remove_time_series_bulk(self, keys: list[TimeSeriesKey]) -> int: ...
    def remove_by_filter(
        self,
        *,
        owner_id: int | None = None,
        owner_category: OwnerCategory | None = None,
        owner_type: str | None = None,
        time_series_type: TimeSeriesType | None = None,
        name: str | None = None,
        name_glob: str | None = None,
        resolution: Period | None = None,
        interval: Period | None = None,
        features: dict[str, int | float | bool | str] | None = None,
    ) -> int: ...
    def clear_time_series(
        self,
        *,
        owner_id: int | None = None,
        owner_category: OwnerCategory | None = None,
    ) -> int: ...
    def replace_owner(
        self,
        old_owner: int,
        new_owner: int,
        owner_category: OwnerCategory,
    ) -> int: ...
    def copy_time_series(
        self,
        src: TimeSeriesKey,
        dst_owner_id: int,
        dst_owner_type: str,
        *,
        new_name: str | None = None,
    ) -> TimeSeriesKey: ...
    def rename_time_series(self, key: TimeSeriesKey, new_name: str) -> TimeSeriesKey: ...
    def persist_to(self, path: str) -> None: ...
    def compact(self) -> dict[str, Any]: ...
    def flush(self) -> None: ...

    # -- reads --
    def get_time_series(
        self,
        key: TimeSeriesKey,
        *,
        time_range: tuple[datetime, datetime] | None = None,
    ) -> TimeSeriesData: ...
    def bulk_read(
        self,
        keys: list[TimeSeriesKey],
        *,
        time_range: tuple[datetime, datetime] | None = None,
    ) -> list[TimeSeriesData]: ...
    def get_metadata(self, key: TimeSeriesKey) -> dict[str, Any]: ...
    def get_array_by_hash(self, data_hash: str) -> NDArray[Any]: ...
    def count_array_references(self, data_hash: str) -> dict[str, Any]: ...
    def resolve_forecast_key(
        self,
        owner_id: int,
        owner_category: OwnerCategory,
        name: str,
        requested_type: TimeSeriesType | str,
        *,
        resolution: Period | None = None,
        interval: Period | None = None,
        features: dict[str, int | float | bool | str] | None = None,
    ) -> TimeSeriesKey: ...

    # -- readers --
    def build_static_reader(
        self,
        resolution: Period,
        *,
        owner_id: int | None = None,
        owner_category: OwnerCategory | None = None,
        owner_type: str | None = None,
        name: str | None = None,
        name_glob: str | None = None,
        features: dict[str, int | float | bool | str] | None = None,
    ) -> StaticReader: ...
    def static_read(self, reader: StaticReader, when: datetime) -> None: ...
    def build_forecast_reader(
        self,
        time_series_type: TimeSeriesType,
        resolution: Period,
        *,
        owner_id: int | None = None,
        owner_category: OwnerCategory | None = None,
        owner_type: str | None = None,
        name: str | None = None,
        name_glob: str | None = None,
        features: dict[str, int | float | bool | str] | None = None,
    ) -> ForecastReader: ...
    def forecast_read(self, reader: ForecastReader, when: datetime) -> None: ...

    # -- listing / discovery --
    def list_time_series(
        self,
        *,
        owner_id: int | None = None,
        owner_category: OwnerCategory | None = None,
        owner_type: str | None = None,
        time_series_type: TimeSeriesType | None = None,
        name: str | None = None,
        name_glob: str | None = None,
        resolution: Period | None = None,
        interval: Period | None = None,
        features: dict[str, int | float | bool | str] | None = None,
    ) -> list[dict[str, Any]]: ...
    def list_array_groups(
        self,
        *,
        owner_id: int | None = None,
        owner_category: OwnerCategory | None = None,
        owner_type: str | None = None,
        time_series_type: TimeSeriesType | None = None,
        name: str | None = None,
        name_glob: str | None = None,
        resolution: Period | None = None,
        interval: Period | None = None,
        features: dict[str, int | float | bool | str] | None = None,
    ) -> list[dict[str, Any]]: ...
    def list_keys(
        self,
        *,
        owner_id: int | None = None,
        owner_category: OwnerCategory | None = None,
        owner_type: str | None = None,
        time_series_type: TimeSeriesType | None = None,
        name: str | None = None,
        name_glob: str | None = None,
        resolution: Period | None = None,
        interval: Period | None = None,
        features: dict[str, int | float | bool | str] | None = None,
    ) -> list[TimeSeriesKey]: ...
    def list_names(
        self,
        *,
        owner_id: int | None = None,
        owner_category: OwnerCategory | None = None,
        owner_type: str | None = None,
        time_series_type: TimeSeriesType | None = None,
        name: str | None = None,
        name_glob: str | None = None,
        resolution: Period | None = None,
        interval: Period | None = None,
        features: dict[str, int | float | bool | str] | None = None,
    ) -> list[str]: ...
    def list_owner_types(
        self,
        *,
        owner_id: int | None = None,
        owner_category: OwnerCategory | None = None,
        owner_type: str | None = None,
        time_series_type: TimeSeriesType | None = None,
        name: str | None = None,
        name_glob: str | None = None,
        resolution: Period | None = None,
        interval: Period | None = None,
        features: dict[str, int | float | bool | str] | None = None,
    ) -> list[str]: ...
    def get_time_series_keys(
        self,
        owner_id: int,
        owner_category: OwnerCategory,
    ) -> list[TimeSeriesKey]: ...
    def has_time_series(self, key: TimeSeriesKey) -> bool: ...
    def get_resolutions(
        self, time_series_type: TimeSeriesType | None = None
    ) -> list[str]: ...
    def get_intervals(
        self, time_series_type: TimeSeriesType | None = None
    ) -> list[str]: ...
    def list_owner_ids(
        self,
        owner_category: OwnerCategory,
        *,
        time_series_type: TimeSeriesType | None = None,
        resolution: Period | None = None,
    ) -> list[int]: ...
    def get_forecast_parameters(
        self,
        *,
        resolution: Period | None = None,
        interval: Period | None = None,
    ) -> dict[str, Any]: ...
    def get_compression(self) -> dict[str, Any]: ...
    def get_time_series_counts(self) -> dict[str, Any]: ...
    def counts_by_type(self) -> dict[str, int]: ...
    def num_distinct_arrays(self) -> int: ...
    def time_series_counts_detailed(self) -> dict[str, Any]: ...
    def static_summary(self) -> list[dict[str, Any]]: ...
    def forecast_summary(self) -> list[dict[str, Any]]: ...
    def check_static_consistency(
        self, resolution: Period | None = None
    ) -> list[dict[str, Any]]: ...
    def verify_integrity(self) -> dict[str, Any]: ...

    # -- supplemental-attribute associations --
    def add_supplemental_attribute_association(
        self, association: SupplementalAttributeAssociation
    ) -> None: ...
    def add_supplemental_attribute_associations(
        self, associations: list[SupplementalAttributeAssociation]
    ) -> int: ...
    def has_supplemental_attribute_association(
        self,
        *,
        component_id: int | None = None,
        component_types: list[str] | None = None,
        attribute_id: int | None = None,
        attribute_types: list[str] | None = None,
    ) -> bool: ...
    def list_supplemental_attribute_associations(
        self,
        *,
        component_id: int | None = None,
        component_types: list[str] | None = None,
        attribute_id: int | None = None,
        attribute_types: list[str] | None = None,
    ) -> list[SupplementalAttributeAssociation]: ...
    def list_supplemental_attribute_ids(
        self,
        *,
        component_id: int | None = None,
        component_types: list[str] | None = None,
        attribute_id: int | None = None,
        attribute_types: list[str] | None = None,
    ) -> list[int]: ...
    def list_components_with_attributes(
        self,
        *,
        component_id: int | None = None,
        component_types: list[str] | None = None,
        attribute_id: int | None = None,
        attribute_types: list[str] | None = None,
    ) -> list[int]: ...
    def remove_supplemental_attribute_associations(
        self,
        *,
        component_id: int | None = None,
        component_types: list[str] | None = None,
        attribute_id: int | None = None,
        attribute_types: list[str] | None = None,
    ) -> int: ...
    def replace_supplemental_attribute_component_id(
        self, old_id: int, new_id: int
    ) -> int: ...
    def count_supplemental_attribute_associations(
        self,
        *,
        component_id: int | None = None,
        component_types: list[str] | None = None,
        attribute_id: int | None = None,
        attribute_types: list[str] | None = None,
    ) -> int: ...
    def count_supplemental_attributes(
        self,
        *,
        component_id: int | None = None,
        component_types: list[str] | None = None,
        attribute_id: int | None = None,
        attribute_types: list[str] | None = None,
    ) -> int: ...
    def count_components_with_attributes(
        self,
        *,
        component_id: int | None = None,
        component_types: list[str] | None = None,
        attribute_id: int | None = None,
        attribute_types: list[str] | None = None,
    ) -> int: ...
    def supplemental_attribute_counts_by_type(self) -> list[tuple[str, int]]: ...
    def supplemental_attribute_summary(self) -> list[dict[str, Any]]: ...

    # -- parent/child associations --
    def add_parent_child_association(
        self, association: ParentChildAssociation
    ) -> None: ...
    def add_parent_child_associations(
        self, associations: list[ParentChildAssociation]
    ) -> int: ...
    def has_parent_child_association(
        self,
        *,
        parent_id: int | None = None,
        parent_types: list[str] | None = None,
        child_id: int | None = None,
        child_types: list[str] | None = None,
    ) -> bool: ...
    def list_parent_child_associations(
        self,
        *,
        parent_id: int | None = None,
        parent_types: list[str] | None = None,
        child_id: int | None = None,
        child_types: list[str] | None = None,
    ) -> list[ParentChildAssociation]: ...
    def list_children(
        self,
        *,
        parent_id: int | None = None,
        parent_types: list[str] | None = None,
        child_id: int | None = None,
        child_types: list[str] | None = None,
    ) -> list[int]: ...
    def list_parents(
        self,
        *,
        parent_id: int | None = None,
        parent_types: list[str] | None = None,
        child_id: int | None = None,
        child_types: list[str] | None = None,
    ) -> list[int]: ...
    def remove_parent_child_associations(
        self,
        *,
        parent_id: int | None = None,
        parent_types: list[str] | None = None,
        child_id: int | None = None,
        child_types: list[str] | None = None,
    ) -> int: ...
    def replace_parent_child_component_id(self, old_id: int, new_id: int) -> int: ...
    def count_parent_child_associations(
        self,
        *,
        parent_id: int | None = None,
        parent_types: list[str] | None = None,
        child_id: int | None = None,
        child_types: list[str] | None = None,
    ) -> int: ...

def init_tracing(filter: str) -> None: ...
