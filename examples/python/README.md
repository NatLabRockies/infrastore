# Python Examples

Complete, runnable examples of the Python binding, written for power-system modelers. Each script
builds an in-memory store, writes time series for the components of one small system, reads them
back, and prints the result as a [Polars](https://pola.rs/) dataframe.

Every example describes the same three components:

| `owner_id` | `owner_type`       | name          | base power                      |
| ---------- | ------------------ | ------------- | ------------------------------- |
| 101        | `ThermalGenerator` | `solitude`    | 100 MW gas combustion turbine   |
| 102        | `SolarPlant`       | `sundance_pv` | 60 MW utility-scale solar plant |
| 201        | `Load`             | `bus_a_load`  | 150 MW distribution feeder      |

infrastore stores time series, not components. A series is filed against its owner, and the store
never resolves that owner into anything — your modeling application owns the components, and
infrastore owns the arrays and the catalog rows pointing at them.

The owner's identity is the _pair_ `(owner_id, owner_category)`: an integer id from your
application, and whether it names a `Component` or a `SupplementalAttribute` (the two id streams are
independent, so one integer can name one of each). `owner_type` is **not** part of that identity —
it is descriptive, and reusing an id across two component types collides however different the type
strings are. It earns its keep in retrieval instead, through `list_metadata(owner_type=...)` and
`list_owner_types()`, which is why a concrete type name beats an abstract one.

Values are stored as they are given. `unit_system` says which basis they are already on —
`natural_units` for MW/MVar, `component_base` for per-unit on the component's own base power — and
nothing in infrastore rescales anything either way. A per-unit series here is per-unit because that
is the basis its values are on, not because it is a normalized shape awaiting a scale factor; if
your values need scaling to mean anything, scale them before writing.

## Running them

From a source checkout, build `infrastore` into a virtual environment, then install the two table
dependencies:

```sh
python3 -m venv .venv
source .venv/bin/activate
pip install maturin
maturin develop --manifest-path crates/infrastore-py/Cargo.toml
pip install -r examples/python/requirements.txt
```

Run any example from the repository root:

```sh
python examples/python/single_floats.py
```

## Example index

Read them in this order; each builds on the vocabulary of the one before.

| Script                                                                   | What it demonstrates                                                                               |
| ------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------- |
| [`single_floats.py`](./single_floats.py)                                 | A day of hourly MW and per-unit values; `units`, `quantity_kind`, `unit_system`, `component_field` |
| [`single_features.py`](./single_features.py)                             | One component field, many series: scenario and weather-year tags as identity and as a query        |
| [`single_fixed_tuples.py`](./single_fixed_tuples.py)                     | Active and reactive power stored together as one `tuple(2,f64)` value per timestep                 |
| [`single_custom_elements.py`](./single_custom_elements.py)               | Cost curves as values: the four function element types                                             |
| [`nonsequential_floats.py`](./nonsequential_floats.py)                   | Measurements at the instants they were taken, with no value in between                             |
| [`deterministic_floats.py`](./deterministic_floats.py)                   | Rolling forecasts: `resolution` / `horizon` / `interval` / `count`, and why windows overlap        |
| [`deterministic_custom_elements.py`](./deterministic_custom_elements.py) | A market offer curve re-submitted every hour — a forecast whose values are curves                  |
| [`probabilistic_floats.py`](./probabilistic_floats.py)                   | p10/p50/p90 forecast bands, and why the band is one-sided at solar noon                            |
| [`scenarios_floats.py`](./scenarios_floats.py)                           | Ensemble members, and what they say that per-hour quantiles cannot                                 |

`_shared.py` holds the three components, the load / solar / thermal-derate profiles the scripts draw
on, and the two display helpers. Everything else — every `add_time_series` and every read — stays
visible in the script that needs it.

## Cost curve element types

`single_custom_elements.py` and `deterministic_custom_elements.py` use infrastore's four function
element types, which cover the curve forms production-cost and market models actually use:

| `element_type`       | Models                                                                            |
| -------------------- | --------------------------------------------------------------------------------- |
| `linear_function`    | `c(P) = a*P + b` — a constant heat rate plus a no-load cost                       |
| `quadratic_function` | `c(P) = a*P^2 + b*P + c` — the smooth fuel-cost polynomial                        |
| `piecewise_linear`   | (MW, $/hr) input-output points                                                    |
| `piecewise_step`     | MW breakpoints + one $/MWh marginal cost per segment — an incremental offer curve |

`encode_element_values` / `decode_element_values` translate between these and the packed arrays the
store holds; `docs/src/reference/element-types.md` documents the packing and the wire vocabulary.
