# Julia Examples

Complete, runnable examples of the Julia binding, written for power-system modelers. Each script
builds an in-memory store, writes time series for the components of one small system, reads them
back, and prints the result as a [DataFrame](https://dataframes.juliadata.org/).

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
strings are. It earns its keep in retrieval instead, through
`list_metadata(store; owner_type = ...)` and `list_owner_types`, which is why a concrete type name
beats an abstract one.

Values are stored as they are given. `unit_system` says which basis they are already on —
`NaturalUnits` for MW/MVar, `ComponentBase` for per-unit on the component's own base power — and
nothing in infrastore rescales anything either way. A per-unit series here is per-unit because that
is the basis its values are on, not because it is a normalized shape awaiting a scale factor; if
your values need scaling to mean anything, scale them before writing.

## Running them

`InfraStore.jl` dlopens the C ABI cdylib, so build that first and point `INFRASTORE_LIB` at it:

```sh
cargo build -p infrastore-ffi --release
export INFRASTORE_LIB=$PWD/target/release/libinfrastore_ffi.dylib   # .so on Linux, .dll on Windows
julia --project=examples/julia -e 'using Pkg; Pkg.instantiate()'
```

`Pkg.instantiate` resolves `InfraStore` from this checkout — `examples/julia/Project.toml` names it
under `[sources]`, which needs Julia 1.11 or newer. Then run any example from the repository root:

```sh
julia --project=examples/julia examples/julia/single_floats.jl
```

`DataFrames` is here only to print tables. Nothing in the binding depends on it.

## Example index

Read them in this order; each builds on the vocabulary of the one before.

| Script                                                                   | What it demonstrates                                                                               |
| ------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------- |
| [`single_floats.jl`](./single_floats.jl)                                 | A day of hourly MW and per-unit values; `units`, `quantity_kind`, `unit_system`, `component_field` |
| [`single_features.jl`](./single_features.jl)                             | One component field, many series: scenario and weather-year tags as identity and as a query        |
| [`single_fixed_tuples.jl`](./single_fixed_tuples.jl)                     | Active and reactive power as one `tuple(2,f64)` value per timestep, from a `Vector{NTuple{2}}`     |
| [`single_custom_elements.jl`](./single_custom_elements.jl)               | Cost curves as values: the four function element types                                             |
| [`nonsequential_floats.jl`](./nonsequential_floats.jl)                   | Measurements at the instants they were taken, with no value in between                             |
| [`persistent_floats.jl`](./persistent_floats.jl)                         | A step function: a fuel price that holds until the next nomination, and `value_at` any instant     |
| [`deterministic_floats.jl`](./deterministic_floats.jl)                   | Rolling forecasts: `resolution` / `horizon` / `interval` / `count`, and why windows overlap        |
| [`deterministic_custom_elements.jl`](./deterministic_custom_elements.jl) | A market offer curve re-submitted every hour — a forecast whose values are curves                  |
| [`probabilistic_floats.jl`](./probabilistic_floats.jl)                   | p10/p50/p90 forecast bands, and why the band is one-sided at solar noon                            |
| [`scenarios_floats.jl`](./scenarios_floats.jl)                           | Ensemble members, and what they say that per-hour quantiles cannot                                 |

`shared.jl` holds the three components, the load / solar / thermal-derate profiles the scripts draw
on, and the display helpers. Everything else — every `add_time_series!` and every read — stays
visible in the script that needs it.

## Closing a store

A `Store` registers a finalizer, so nothing leaks if you never close one — but the GC decides
_when_, and on disk that means an open file handle and a held SQLite write lock for an unbounded
time. The **do-block forms** are the release-on-exit answer, including on a throw:

```julia
Store(in_memory = true) do store
    add_time_series!(store, 42, "Generator", Component, ts)
end
```

`open_store` and `open_copy` have them too. Every example here is written that way. `close!(store)`
is the explicit form for a store that outlives a block — one held by a consumer package, say — and
is idempotent, so closing a store the finalizer later reaps is fine.

## Values that are not numbers

`single_fixed_tuples.jl`, `single_custom_elements.jl` and `deterministic_custom_elements.jl` store a
value per timestep that is not a number. In Julia there is nothing to declare and no encode step to
call: a `Vector{PiecewiseLinear}` or a `Vector{NTuple{2,Float64}}` already names its own
`element_type`, so the constructor reads it off the values, and passing an `element_type=` that
contradicts them is an error rather than an override.

| Value type          | `element_type`       | Models                                                                            |
| ------------------- | -------------------- | --------------------------------------------------------------------------------- |
| `LinearFunction`    | `linear_function`    | `c(P) = a*P + b` — a constant heat rate plus a no-load cost                       |
| `QuadraticFunction` | `quadratic_function` | `c(P) = a*P^2 + b*P + c` — the smooth fuel-cost polynomial                        |
| `PiecewiseLinear`   | `piecewise_linear`   | (MW, $/hr) input-output points                                                    |
| `PiecewiseStep`     | `piecewise_step`     | MW breakpoints + one $/MWh marginal cost per segment — an incremental offer curve |
| `NTuple{N,Float64}` | `tuple(N,f64)`       | `N` numbers that are one composite value, not `N` samples                         |

Packing happens at the ABI boundary, so the struct goes on holding the values and a read hands the
same values back; `raw = true` on a read is how to see the packing instead, which
`single_custom_elements.jl` prints for a ragged pair of curves.
`docs/src/reference/element-types.md` documents the byte layouts and the wire vocabulary, and
`docs/src/reference/julia-api.md#element-values` the rest of the surface — including decoding
straight into a consumer's own domain types through the `types` keyword.

## Timestamps

The examples use plain `DateTime`, which carries no zone. The store records that as a **wall clock**
— the instant is the fields as written, and the spelling is recorded as `ZonelessReference()` rather
than silently relabelled UTC. That is a real declaration: query bounds must match a series'
spelling, so a zoneless series is queried with zoneless bounds.

`using TimeZones` and passing a `ZonedDateTime` is the other way in, accepted anywhere a `DateTime`
is. The examples stay on the zoneless path to keep the dependency out; see
[Time references](../../docs/src/explanation/time-references.md) for the difference.
