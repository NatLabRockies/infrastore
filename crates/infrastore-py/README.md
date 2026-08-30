# infrastore

Python bindings for [infrastore](https://github.com/NatLabRockies/infrastore) — time-series storage
for power-systems and energy simulations.

Numerical arrays are persisted in HDF5; the metadata associating each array with its owning
component lives in SQLite. Identical arrays are stored once and shared through content addressing.

The wheels are self-contained: HDF5 and zlib are statically linked, so there are no system libraries
to install.

## Install

```sh
pip install infrastore
```

## Quick start

```python
from datetime import datetime, timedelta, timezone
import numpy as np
from infrastore import Store, SingleTimeSeries, OwnerCategory

store = Store.create(in_memory=True)
ts = SingleTimeSeries(
    datetime(2024, 1, 1, tzinfo=timezone.utc),
    timedelta(hours=1),
    np.arange(24, dtype=np.float64) + 100,
    "load",   # name (required)
)
series_id = store.add_time_series(
    owner_id=42, owner_type="Generator",
    owner_category=OwnerCategory.Component,
    time_series=ts,   # name comes from ts
    features={"model_year": 2030}, units="MW",
)
got = store.read_by_id(series_id)
assert np.array_equal(np.asarray(got.data), np.asarray(ts.data))
```

## Features

- **One array, stored once** — arrays are addressed by a SHA-256 content hash, so a series shared
  across components is written to disk a single time.
- **Typed, N-dimensional values** — `f64`, `f32`, `i64`, `i32`, `u64`, and `bool`, with an optional
  per-timestep element shape.
- **Six time-series types** — `SingleTimeSeries` and `NonSequentialTimeSeries` read+write;
  `Deterministic`, `DeterministicSingleTimeSeries`, `Probabilistic`, and `Scenarios` for forecasts.
- **Columnar simulation readers** — `StaticReader` / `ForecastReader` serve every series' value at
  one timestamp.
- **Association catalogs** — component ↔ supplemental attribute and directed component ↔ component
  edges, recorded independently of time series.
- Type stubs (`.pyi`), a full exception hierarchy, and keyword-only optional arguments.

## Documentation

<https://natlabrockies.github.io/infrastore/latest/> — see the
[Python guide](https://natlabrockies.github.io/infrastore/latest/guides/python.html) and the
[Python API reference](https://natlabrockies.github.io/infrastore/latest/reference/python-api.html).

## License

BSD-3-Clause. See [LICENSE](LICENSE).
