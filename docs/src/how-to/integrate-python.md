# Integrate with Python

Get the `infrastore` module into a Python environment. For API usage once it imports, see the
[Python Developer Guide](../guides/python.md).

## Prerequisites

- Python 3.11 or newer.
- The [system libraries](./install.md#1-install-system-libraries) (HDF5, Protobuf).

## Build and Install the Wheel (development)

The binding is built with [maturin](https://www.maturin.rs/). `maturin develop` compiles the
extension and installs it into the active virtual environment:

```sh
cd crates/infrastore-py
python3 -m venv .venv && source .venv/bin/activate
pip install maturin pytest numpy
maturin develop
```

Verify the install:

```sh
python -c "import infrastore; print(infrastore.__version__)"
pytest ../../python/tests
```

## Build a Distributable Wheel

To produce a wheel you can install elsewhere:

```sh
cd crates/infrastore-py
maturin build --release
# -> target/wheels/infrastore-<version>-cp311-abi3-<platform>.whl
pip install ../../target/wheels/infrastore-*.whl
```

The wheel is built against the **`abi3-py311`** stable ABI, so a single wheel works on CPython 3.11
and every newer 3.x without recompiling.

## Smoke Test

```python
from datetime import datetime, timedelta, timezone
import numpy as np
from infrastore import Store, SingleTimeSeries, OwnerCategory

store = Store.create(in_memory=True)
ts = SingleTimeSeries(
    datetime(2024, 1, 1, tzinfo=timezone.utc),
    timedelta(hours=1),
    np.arange(24, dtype=np.float64) + 100,
    "load",
)
key = store.add_time_series(
    owner_id=42, owner_type="Generator",
    owner_category=OwnerCategory.Component,
    time_series=ts,
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
- **`InvalidParameterError` on add** — In Python, pass a NumPy array (any shape) whose dtype is one
  of `float64`, `float32`, `int64`, `int32`, `uint64`, or `bool`; any other dtype (e.g. `complex128`
  or a string dtype) raises. Feature values must be `int`/`float`/`bool`/`str`. Timestamps for a
  `NonSequentialTimeSeries` must be strictly increasing.

## Next

- [Python Developer Guide](../guides/python.md)
- [Python API reference](../reference/python-api.md)
