# Integrate with Python

Get the `time_series` module into a Python environment. For API usage once it imports, see the
[Python Developer Guide](../guides/python.md).

## Prerequisites

- Python 3.10 or newer.
- The [system libraries](./install.md#1-install-system-libraries) (HDF5, NetCDF, Protobuf).

## Build and Install the Wheel (development)

The binding is built with [maturin](https://www.maturin.rs/). `maturin develop` compiles the
extension and installs it into the active virtual environment:

```sh
cd crates/time-series-store-py
python3 -m venv .venv && source .venv/bin/activate
pip install maturin pytest numpy
maturin develop
```

Verify the install:

```sh
python -c "import time_series; print(time_series.__version__)"
pytest ../../python/tests
```

## Build a Distributable Wheel

To produce a wheel you can install elsewhere:

```sh
cd crates/time-series-store-py
maturin build --release
# -> target/wheels/time_series-<version>-cp310-abi3-<platform>.whl
pip install ../../target/wheels/time_series-*.whl
```

The wheel is built against the **`abi3-py310`** stable ABI, so a single wheel works on CPython 3.10
and every newer 3.x without recompiling.

## Smoke Test

```python
from datetime import datetime, timedelta, timezone
import numpy as np
from time_series import TimeSeriesStore, SingleTimeSeries, OwnerCategory

store = TimeSeriesStore.create(in_memory=True)
ts = SingleTimeSeries(
    datetime(2024, 1, 1, tzinfo=timezone.utc),
    timedelta(hours=1),
    np.arange(24, dtype=np.float64) + 100,
)
key = store.add_time_series(
    owner_uuid="42", owner_type="Generator",
    owner_category=OwnerCategory.Component,
    name="load", time_series=ts,
    features={"model_year": 2030}, units="MW",
)
assert np.array_equal(np.asarray(store.get_time_series(key).data), np.asarray(ts.data))
print("ok")
```

## Troubleshooting

- **`ImportError` for the extension** — Ensure you ran `maturin develop` in the active venv, or that
  `pip install`-ed the wheel into the interpreter you are running.
- **HDF5 not found during build** — Set `HDF5_DIR` (see
  [Install](./install.md#1-install-system-libraries)).
- **`InvalidParameterError` on add** — The NetCDF backend stores 1-D `float64` arrays only; pass
  `dtype=np.float64` and a 1-D array.

## Next

- [Python Developer Guide](../guides/python.md)
- [Python API reference](../reference/python-api.md)
