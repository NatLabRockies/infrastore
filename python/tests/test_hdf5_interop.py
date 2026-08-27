"""Guard against HDF5 symbol collisions in the published wheel.

The wheel statically links its own HDF5 + zlib (infrastore's default
`vendored` feature). Downstream users -- notably `infrasys` -- typically also
have `netCDF4` and/or `h5py` installed, each of which bundles its own libhdf5.
That puts several independent HDF5 copies in one interpreter.

On Linux especially, that can fail through symbol interposition rather than a
clean import error, so these tests drive real reads and writes through every
library in one process instead of only importing them. The wheel-building
workflow runs this file against each built wheel via cibuildwheel's
`test-command`, which is what makes the manylinux case a covered platform
rather than an assumption.
"""

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import OwnerCategory, SingleTimeSeries, Store

h5py = pytest.importorskip("h5py", reason="h5py not installed")
netCDF4 = pytest.importorskip("netCDF4", reason="netCDF4 not installed")


def _h5py_roundtrip(path):
    data = np.arange(1000, dtype=np.float64)
    with h5py.File(path, "w") as f:
        f.create_dataset("vals", data=data, compression="gzip")
    with h5py.File(path, "r") as f:
        assert np.array_equal(f["vals"][:], data)


def _netcdf4_roundtrip(path):
    data = np.arange(1000, dtype=np.float64) * 2
    with netCDF4.Dataset(path, "w") as ds:
        ds.createDimension("t", len(data))
        var = ds.createVariable("vals", "f8", ("t",), zlib=True)
        var[:] = data
    with netCDF4.Dataset(path, "r") as ds:
        assert np.array_equal(ds["vals"][:], data)


def _infrastore_roundtrip(path):
    values = np.arange(24, dtype=np.float64) + 100
    ts = SingleTimeSeries(
        datetime(2024, 1, 1, tzinfo=timezone.utc), timedelta(hours=1), values, "load"
    )
    store = Store.create(str(path))
    key = store.add_time_series(
        owner_id=42,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=ts,
        units="MW",
    ).key
    assert np.array_equal(np.asarray(store.get_time_series(key).data), values)
    del store

    # Reopen from disk so the read path runs against a fresh HDF5 handle.
    reopened = Store.open(str(path))
    assert np.array_equal(np.asarray(reopened.get_time_series(key).data), values)


def test_infrastore_then_netcdf4_then_h5py(tmp_path):
    """infrastore initializes HDF5 first."""
    _infrastore_roundtrip(tmp_path / "store.h5")
    _netcdf4_roundtrip(tmp_path / "nc4.h5")
    _h5py_roundtrip(tmp_path / "h5.h5")


def test_h5py_then_netcdf4_then_infrastore(tmp_path):
    """Reverse order: another libhdf5 initializes before infrastore's."""
    _h5py_roundtrip(tmp_path / "h5.h5")
    _netcdf4_roundtrip(tmp_path / "nc4.h5")
    _infrastore_roundtrip(tmp_path / "store.h5")


def test_netcdf4_reads_an_infrastore_store(tmp_path):
    """netCDF4 must still open a store infrastore's vendored HDF5 wrote.

    The store is a plain HDF5 file, not NetCDF4, so netcdf-c reads it via its
    generic-HDF5 path (datasets without dimension scales get phony dimensions).
    This pins that the layout stays within what netcdf-c can open.
    """
    path = tmp_path / "store.h5"
    _infrastore_roundtrip(path)

    def first_var(group):
        for name, var in group.variables.items():
            return f"{group.path}/{name}", var
        for sub in group.groups.values():
            hit = first_var(sub)
            if hit:
                return hit
        return None

    with netCDF4.Dataset(path, "r") as ds:
        hit = first_var(ds)
        assert hit is not None, "no variables visible to netCDF4"
        name, var = hit
        assert np.asarray(var[:]).size > 0, f"netCDF4 read back an empty {name}"


def test_h5py_reads_an_infrastore_store(tmp_path):
    """Same, one layer down: a foreign libhdf5 reading the raw datasets."""
    path = tmp_path / "store.h5"
    _infrastore_roundtrip(path)

    with h5py.File(path, "r") as f:
        datasets = []
        f.visititems(
            lambda name, obj: datasets.append(name)
            if isinstance(obj, h5py.Dataset)
            else None
        )
        assert datasets, "no datasets visible to h5py"
        assert np.asarray(f[datasets[0]][()]).size > 0
