# Julia Developer Guide

This guide covers building on `InfraStore.jl`, the Julia package that wraps the
[C ABI](../reference/c-abi.md). For exact signatures see the
[Julia API reference](../reference/julia-api.md); to set up the package and the native library, see
[Integrate with Julia](../how-to/integrate-julia.md).

## Load the Package

`Pkg.add("InfraStore")` installs the package and downloads the prebuilt native library as an
artifact, so in a consumer package nothing else is needed:

```julia
using Dates, InfraStore
```

Developing against a checkout, `INFRASTORE_LIB` points the package at a local build instead and
takes precedence over the artifact; set it before the first store call:

```sh
cargo build -p infrastore-ffi --release
export INFRASTORE_LIB=$PWD/target/release/libinfrastore_ffi.dylib  # .so on Linux
```

Exported names include `Store`, `SingleTimeSeries`, `NonSequentialTimeSeries`, the forecast structs
(`Deterministic`, `Probabilistic`, `Scenarios`), `OwnerCategory` (`Component`,
`SupplementalAttribute`), the `add_time_series!` / `read_by_id` / `list_metadata` family, and
`transform_single_time_series!`. The store type is named **`Store`**.

## Open or Create a Store

```julia
# In-memory.
store = Store(in_memory=true)

# On disk: writes system.h5 and system.h5.sqlite.
store = Store(in_memory=false, path="system.h5")

# Reopen read-only.
store = open_store("system.h5"; read_only=true)
```

The store is finalized automatically, but you can release it eagerly with `close!(store)`.

## Add a Series

```julia
# `name` ("load") is a required field on the struct.
ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(100.0:123.0), "load")
```

A bare `DateTime` carries no zone, and this package reads one as **UTC**. If your timestamps are
genuinely zoned, `using TimeZones` and pass a `ZonedDateTime` instead — it names an instant on its
own, and is accepted anywhere a `DateTime` is:

```julia
using TimeZones
ts = SingleTimeSeries(
    ZonedDateTime(DateTime(2024, 1, 1), tz"America/Denver"),   # = 2024-01-01T07:00Z
    Hour(1), collect(100.0:123.0), "load",
)
```

Reads return a `DateTime` in UTC either way; see
[Time and resolution conversions](../reference/julia-api.md#time-and-resolution-conversions).

```julia
id = add_time_series!(
    store,
    42,
    "Generator",
    Component,
    ts;                                   # name comes from ts
    features = Dict("model_year" => 2030),
    units = "MW",
)
# `id` is the catalog row's id: how every read, removal and rename addresses
# the series, and one integer to keep in your own model.
```

Notes:

- **`owner_id` is an integer** (`Int64`) — the component identifier, e.g. `42`.
- **`resolution` is a `Period`** such as `Hour(1)` or `Minute(5)`.
- **`features`** is a `Dict` serialized to JSON, so values must be JSON scalars (`Int`, `Float64`,
  `Bool`, `String`). String features are supported and round-trip unchanged.
- Adding a duplicate [identity](../explanation/data-model.md#identity) throws
  `DuplicateTimeSeriesError`.

`add_time_series!` returns the catalog row's `id` as an `Int64` (see
[Association ids](../explanation/data-model.md#association-ids)). Every read, removal, rename and
copy takes that id; `list_metadata` is how you recover one for a series you did not just write.

Two more rules worth knowing up front. **Stored instants and periods are millisecond-precision**: a
`Microsecond(1)` resolution, a `Millisecond(0)` one, or a negative period is refused with
`InvalidParameterError` (query bounds are unconstrained). And a `Store` is **not thread-safe**:
confine it, and any reader built from it, to one task, or guard every call with your own lock —
concurrent calls are undefined behavior, not just a race on results.

### Descriptors

Beyond `units`, an association can carry `quantity_kind` (what the values measure — `"ActivePower"`;
the one record of what per-unit values mean), `unit_system` (`NaturalUnits` or `ComponentBase`;
`nothing` means _unspecified_, not natural units — the store rescales nothing), `component_field`
(the field on the owning component these values vary — `"max_active_power"`; also a filter), and
`application_data` (an opaque string returned verbatim — the package-owned slot). They can be set on
the struct, where they become the `add_time_series!` defaults, or passed as keywords:

```julia
ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(100.0:123.0), "load";
                      units = "MW", quantity_kind = "ActivePower",
                      unit_system = NaturalUnits, component_field = "max_active_power",
                      application_data = "{\"source\": \"weather_year_2012\"}")
id = add_time_series!(store, 42, "Generator", Component, ts)   # keeps all five
```

A series also records a `time_reference` — how its timestamps were spelled — inferred from the
timestamp it was built with. A bare `DateTime` is a wall clock (`ZonelessReference()`); a
`ZonedDateTime` keeps its zone, so a Denver series renders correctly on both sides of every DST
transition. Reads still return a `DateTime` holding the instant, with the reference beside it;
`using TimeZones` adds `zoned_timestamp` to fuse them back together losslessly.

None of them is part of a series' identity or of either content hash, so two adds that differ only
in a descriptor are a duplicate. See
[Optional Descriptors](../explanation/data-model.md#optional-descriptors) and
[Time references](../explanation/data-model.md#time-references).

### Add many series at once

`AddBatch` accepts the same `add_time_series!` calls as a `Store` but only accumulates them;
`add_time_series_bulk!` commits the whole batch in one catalog transaction and takes the block-sized
HDF5 write path. It is the way to load a system: an order of magnitude faster than a loop of single
adds, and same-shaped series land in the same packed dataset.

```julia
batch = AddBatch()
for (id, ts) in series
    add_time_series!(batch, id, "Generator", Component, ts; units = "MW")
end
ids = add_time_series_bulk!(store, batch)   # Vector{Int64}, in input order; all-or-nothing
```

### Transactions

Several operations that must take effect together — replacing a series is an add plus a remove — go
inside a transaction. Removals are reversible only there; outside one the array bytes are reclaimed
immediately.

```julia
transaction(store) do
    new_id = add_time_series!(store, 42, "Generator", Component, updated)
    remove_by_ids!(store, [old_id])
end   # committed if the block returns, rolled back if it throws
```

Blocks nest (each level is a savepoint), and the store holds the SQLite write lock until the
outermost one ends. A transaction does not batch: use `add_time_series_bulk!` inside it for the
writes themselves. `begin_transaction!` / `commit_transaction!` / `rollback_transaction!` are the
explicit form.

## Read a Series

```julia
got = read_by_id(store, id)
@assert got.data == ts.data
println(got.initial_timestamp, " ", got.resolution)   # resolution comes back as Millisecond
```

`read_by_id` also takes a window — `start_time` plus a `len` of timesteps or a `count` of windows —
which is **checked**: an over-long request throws rather than quietly returning less.

To read **many whole series at once** — e.g. loading everything for a plot — `read_by_ids` takes a
vector of ids and returns one struct per id in the same order (each of its stored type, so the
result is a `Vector{Any}`), reading each packed dataset's column span once instead of re-reading
every chunk per series. Its `time_range` keyword is the other kind of slice, which **clips**:

```julia
series = read_by_ids(store, ids)
window = read_by_ids(store, ids; time_range = (t0, t1))   # the same clip on every series
```

## Attribute-Based Lookups

Beyond key handles, `InfraStore.jl` can resolve a series directly from its attributes — convenient
when a caller keeps its own identifiers (as an InfrastructureSystems.jl-side store does):

```julia
meta = get_metadata_by_id(
    store,
    42,
    Component,        # owner_category; the owner is the (owner_id, owner_category) pair
    "load";
    resolution = Hour(1),
    features = Dict("model_year" => 2030),
)
# meta :: TimeSeriesMetadata — the whole record: owner_id/owner_type/owner_category,
#          name, time_series_type, data_hash, initial_timestamp, resolution, length,
#          horizon/interval/count, percentiles, element_type, element_shape, features,
#          units, quantity_kind, unit_system, component_field, application_data

values = get_array_by_hash(store, meta.data_hash)     # Vector{Float64}; pass ::Type{T} for other dtypes

# Identify, then act. `list_metadata` answers which series exist and hands back
# the id; every read and removal takes that id.
row = only(list_metadata(store, owner_id = 42, name = "load",
                         resolution = Hour(1),
                         exact_features = Dict("model_year" => 2030)))
got = read_by_id(store, row.id)
remove_by_ids!(store, [row.id])

# `has_time_series` stays attribute-addressed: it is an index probe that reads no
# row, so routing it through a resolution would cost more than it answers.
present = has_time_series(store, 42, Component, "load";
                          resolution = Hour(1), features = Dict("model_year" => 2030))
```

`exact_features` matches the feature map **exactly** — the series above was added with
`model_year = 2030`, so `features` (a subset match) would also select a sibling carrying more. The
plain `features` keyword is the right one for "every series tagged `scenario = high`"; a package
that resolves partial user queries lists, then decides what more than one match means.

## Forecasts

`InfraStore.jl` exposes `Deterministic`, `Probabilistic`, and `Scenarios` structs that wrap a native
`AbstractArray` in the type's logical shape (the wrapper derives the dtype and dims and serializes
the buffer row-major). Construct one and add it through the generic `add_time_series!`:

```julia
data = zeros(Float64, 24, 7)   # (horizon_count, count)
fc = Deterministic(DateTime(2024, 1, 1), Hour(1), Hour(24), Hour(24), 7, data, "load_fc")
id = add_time_series!(
    store,
    42,
    "Generator",
    Component,
    fc;                         # name comes from fc
    units = "MW",
)

got = read_by_id(store, id)   # the id add_time_series! returned
values = got.data             # Float64 matrix, shape (24, 7)
```

`Probabilistic(initial_timestamp, resolution, horizon, interval, count, percentiles, data, name)`
carries the percentile vector, and
`Scenarios(initial_timestamp, resolution, horizon, interval, count, data, name)` takes
`scenario_count` from `data`'s leading axis. Every forecast constructor also accepts a
`application_data=` keyword. A read names only an id, so the row's own type decides what comes back
— there is no requested type to disagree with it.

If two forecasts of one owner/name/type differ only by `interval` (say day-ahead and intra-day),
pass `interval=` to the listing to pin the one you want; without it it returns both:

```julia
row = only(list_metadata(store; owner_id = 42, name = "load_fc",
                         resolution = Hour(1), interval = Hour(6)))
```

A `DeterministicSingleTimeSeries` is not added directly — derive one from the stored
`SingleTimeSeries` with `transform_single_time_series!`, which returns a `TransformOutcome` whose
`transformed` field is the count. It optionally restricts the transform to one `owner_category`
and/or one `resolution`, and `dry_run = true` runs every check without writing:

```julia
n = transform_single_time_series!(store, Hour(24), Hour(24)).transformed
outcome = transform_single_time_series!(store, Hour(24), Hour(24);
                                        owner_category = Component, resolution = Hour(1),
                                        normalize_single_window = true,
                                        require_uniform_forecast_grid = true)
```

The two policy flags reproduce InfrastructureSystems.jl's rules (it passes both as `true`); see the
[reference](../reference/julia-api.md#forecasts) for what each enforces.

Filtering for `Deterministic` also matches a transformed `DeterministicSingleTimeSeries`, so you
find a forecast the same way whether it was added densely or derived — and either reads back as a
`Deterministic`, since a DST has no materialized struct. Each row still reports the concrete form it
is, so `transform_single_time_series!` needs no separate enumeration path:

```julia
for row in list_metadata(store; owner_id = 42, time_series_type = Deterministic)
    row.time_series_type <: DeterministicSingleTimeSeries   # derived, or densely stored?
    series = read_by_id(store, row.id)   # a Deterministic either way
end
```

`transform_single_time_series!` also reports the ids it wrote on its `TransformOutcome.written`, so
a caller can reference a view it just derived without listing the store to find it again.

`has_time_series` takes the time series type as its first argument to address anything other than a
`SingleTimeSeries` (and takes the same `resolution` / `interval` / `features` keywords).
`copy_time_series!` takes the source id and re-points that series at another owner without
duplicating data — it writes one association row against the same content-addressed array,
preserving the stored type (a DST stays a DST) — and returns the copy's own id:

```julia
has_time_series(Scenarios, store, 42, Component, "wind"; resolution = Hour(1))
src = only(list_metadata(store; owner_id = 42, name = "load")).id
copy_time_series!(store, src, 43, "Generator")
```

Every `time_series_type` filter keyword takes the Julia type as well:

```julia
list_metadata(store; time_series_type = Deterministic)
get_resolutions(store; time_series_type = SingleTimeSeries)
```

A metadata row's `time_series_type` is the **full** type — `SingleTimeSeries{Float64,1}`,
`Deterministic{Float32,3}` — so a row names what a read of it hands back rather than only which of
the six kinds it is, and a consumer holding InfrastructureSystems.jl-style parameterized types gets
them back intact:

```julia
md = get_metadata_by_id(store, id)
md.time_series_type == typeof(read_by_id(store, id))   # true for every stored type
md.time_series_type <: SingleTimeSeries                # ask for the kind with <:, not ==
```

That type passes straight back into any filter, `has_time_series`, or reader. The parameters are
ignored there — a series is addressed by identity, which carries no element type — so they never
narrow a match; they are accepted so a row you just read round-trips without being taken apart
first.

The low-level `get_metadata_by_id` + `get_array_by_hash` path is still available for raw access. See
the [Julia API reference](../reference/julia-api.md#forecasts).

## Per-Timestamp Reads (Simulation Loop)

`read_by_id` hands back a whole series or forecast. Simulations instead walk the timeline and, at
each timestamp, want the value of _every_ series at that instant. For that, build a **reader** once
and drive it in a loop — it pins one resolution and reuses its output buffers, so the loop allocates
almost nothing. `StaticReader` serves `SingleTimeSeries`; `ForecastReader` serves forecasts. (Full
signatures: [Julia API reference](../reference/julia-api.md#readers-per-timestamp-iteration).)

### Static series

```julia
reader = build_static_reader(store; resolution = Hour(1))
grid = static_grid(reader)                 # StaticGrid: initial_timestamp, resolution, length
for k in 0:(grid.length - 1)
    static_read!(reader, grid.initial_timestamp + grid.resolution * k)
    for (gi, g) in enumerate(static_groups(reader))
        vals = static_values(reader, gi)   # (num_columns, element_dims...); column j ↔ g.keys[j]
    end
end
```

Series are grouped by `(dtype, element_shape)`; each group's `static_values` is one dense array
whose columns line up with the group's `keys`. All matched series must share one grid
(`initial_timestamp` + `length`), validated at build.

### Forecasts

```julia
reader = build_forecast_reader(store, Deterministic; resolution = Hour(1))
tl = forecast_timeline(reader)             # ForecastTimeline: initial_timestamp, resolution, interval, count
for k in 0:(tl.count - 1)
    forecast_read!(reader, tl.initial_timestamp + tl.interval * k)
    for (i, e) in enumerate(forecast_entries(reader))
        window = forecast_values(reader, i)   # shape e.window_shape, for e.key
    end
end
```

A `Deterministic` reader is abstract — it also includes any `DeterministicSingleTimeSeries` (read
into identical windows).

### Shared forecasts are read once

Forecasts that share a backing array (deduplicated identical data, or several
`DeterministicSingleTimeSeries` over one `SingleTimeSeries`) collapse to a single **window slot**.
`forecast_read!` reads each slot from the `.h5` file once per timestamp, so a forecast shared by 10
components costs one read, not ten. `forecast_num_slots(reader)` is the physical read count, and
each `ForecastEntry.slot` says which slot an entry uses — group by `slot` to materialize each unique
window only once:

```julia
forecast_read!(reader, t)
windows = Dict{Int, Any}()
for (i, e) in enumerate(forecast_entries(reader))
    w = get!(() -> forecast_values(reader, i), windows, e.slot)
    # apply w to e.key's owner
end
```

## Store-Wide Operations

```julia
counts = get_counts(store)        # TimeSeriesCounts: components_with_time_series, static_time_series, forecasts
nerr   = verify_integrity(store)  # 0 == every referenced array and time axis matches its hash
report = compact!(store)          # CompactionReport; rewrites the .h5 from the live set, so a
                                  # delete actually shrinks the file. Nothing else may have the
                                  # store open while it runs.
```

## Associations

Two catalog tables record relationships between entities the store does not otherwise model, wholly
independently of time series: which supplemental attributes are attached to which components, and
directed parent/child edges between components. Removing a time series never touches either, and
vice versa — see
[Associations Between Entities](../explanation/data-model.md#associations-between-entities).

Filter keywords are all optional and ANDed; passing none matches everything.

```julia
add_supplemental_attribute_association!(
    store, SupplementalAttributeAssociation(42, "Generator", 100, "GeographicInfo"))

# Bulk add is one all-or-nothing transaction.
add_supplemental_attribute_associations!(store, [
    SupplementalAttributeAssociation(43, "Generator", 100, "GeographicInfo"),
    SupplementalAttributeAssociation(43, "Generator", 101, "Outage"),
])

# Queries run in both directions, returning distinct ids in ascending order.
list_supplemental_attribute_ids(store; component_id=43)      # [100, 101]
list_components_with_attributes(store; attribute_id=100)     # [42, 43]
has_supplemental_attribute_association(store; component_id=42, attribute_id=100)  # true

# `*_types` filters take CONCRETE type names, so expand an abstract type yourself —
# `get_all_subtype_names` in InfrastructureSystems.jl is the usual source. An empty
# vector is a deliberate "none of these" and matches nothing.
list_supplemental_attribute_ids(store; component_id=43, attribute_types=["Outage"])  # [101]

count_supplemental_attributes(store)         # 2, distinct attributes
count_components_with_attributes(store)      # 2, distinct components
supplemental_attribute_counts_by_type(store)
# [SupplementalAttributeTypeCount("GeographicInfo", 2), SupplementalAttributeTypeCount("Outage", 1)]
supplemental_attribute_summary(store)
# [SupplementalAttributeSummaryRow("Generator", "GeographicInfo", 2), ...]
```

Identity is the `(component_id, attribute_id)` pair. The type names ride along for filtering and are
not part of it, so re-attaching the same pair under different type names is still a duplicate:

```julia
try
    add_supplemental_attribute_association!(
        store, SupplementalAttributeAssociation(42, "Load", 100, "Outage"))
catch e
    e isa InfraStore.DuplicateAssociationError || rethrow()
    @info e.msg   # attribute 100 is already attached to component 42
end

# Removal returns a count. Matching nothing returns 0 rather than throwing, so assert on
# the count yourself if you expected a hit.
remove_supplemental_attribute_associations!(store; component_id=43)   # 2
```

Parent/child edges work the same way, except that identity is the **ordered** pair — the reverse of
an edge is a different edge — and both endpoints are always components:

```julia
add_parent_child_association!(store, ParentChildAssociation(42, "Generator", 7, "Bus"))
add_parent_child_associations!(store, [ParentChildAssociation(43, "Generator", 7, "Bus")])

list_children(store; parent_id=42)      # [7]
list_parents(store; child_id=7)         # [42, 43]
count_parent_child_associations(store)  # 2

# Renumbering a component rewrites both ends of every edge.
replace_parent_child_component_id!(store, 42, 99)   # 1
list_parents(store; child_id=7)                     # [43, 99]
```

Neither table is reachable over gRPC or the `infrastore` CLI.

## Persist to Disk

```julia
flush!(store)   # sync HDF5 + SQLite; afterwards system.h5 + system.h5.sqlite can be copied
```

Keep the `.h5` and `.h5.sqlite` files together.

To change a store you did not build in this process, **open a copy**: `open_store` defaults to
read-write, and HDF5 has no journal, so an interrupted in-place write is unrecoverable.

```julia
store = open_copy(src, joinpath(scratch, "time_series.h5"))   # src is never opened for writing
...
persist!(store, src)                                          # one atomic rename replaces it
```

`open_store(path; read_only=true)` is the right call when nothing will be written.

### Where the Catalog Lives

By default the catalog _is_ `system.h5.sqlite`, and every commit is durable. Passing
`catalog=:memory` keeps it in RAM instead, so it reaches disk only via `persist!`:

```julia
# Build in a scratch directory; nothing is durable until the explicit save.
store = Store(; in_memory=false, path=joinpath(scratch, "time_series.h5"), catalog=:memory)
add_time_series!(store, 42, "Generator", Component, ts)
persist!(store, destination)     # writes both halves as a matched pair
persist_catalog!(store)          # or: land only the .sqlite half beside the arrays already at path
```

Arrays still stream to the HDF5 file, so this does not require the data to fit in memory. It suits
building a store beside volatile in-process state — a crash loses that state anyway, so journaling
the scratch catalog buys nothing. `catalog_mode(store)` reports which mode a store is in.

`open_store(path; catalog=:memory)` loads an existing catalog into RAM the same way. Note that the
HDF5 half is still opened **in place**, so mutations land in the original file; open a copy if you
mean to leave the source untouched until an explicit save.

`persist!` stages both halves and renames them into place, and stamps the pair so that a save
interrupted between the two renames is caught on the next open rather than read as a valid store. It
does replace the destination, though, so a failed save may have destroyed what was there — recover
by calling `persist!` again on the still-live store rather than assuming the target survived.

## Error Handling

Errors subtype `InfraStore.TimeSeriesException`. The exception types are not exported, so reference
them module-qualified. Catch broadly or narrowly:

```julia
try
    add_time_series!(store, 42, "Generator", Component, ts)
catch e
    if e isa InfraStore.DuplicateTimeSeriesError
        @warn "already present"
    else
        rethrow()
    end
end
```

The available types are `InfraStore.NotFoundError`, `InfraStore.DuplicateTimeSeriesError`,
`InfraStore.DuplicateAssociationError`, `InfraStore.InvalidParameterError`,
`InfraStore.IntegrityError`, `InfraStore.ReadOnlyStoreError`, `InfraStore.IOError`,
`InfraStore.StoreExistsError` (creating over an existing artifact),
`InfraStore.MismatchedArtifactError` (the `.h5` and `.sqlite` halves came from two saves),
`InfraStore.IncompatibleFormatError` (the on-disk store was written by an incompatible data format
version), and `InfraStore.GenericError` (which carries the raw FFI status `code`). See the
[reference](../reference/julia-api.md#errors) for the full table.

## InfrastructureSystems.jl Integration Notes

[Embedding in a Parent Package](./embedding.md) is the language-neutral version of this section —
the store lifecycle, id mapping, and lookup semantics a package like InfrastructureSystems.jl has to
honor. The Julia-specific points:

- Owners are integer component identifiers (`Int64`), matching InfrastructureSystems.jl
  component/attribute IDs.
- `OwnerCategory` distinguishes `Component` from `SupplementalAttribute` and is part of the owner
  identity: the owner is the `(owner_id, owner_category)` pair, so a component and a supplemental
  attribute may share a numeric id and stay distinct. Owner-scoped calls take the category alongside
  the id.
- The attribute-based existence probes (`has_time_series`, `has_any_time_series`) plus
  `get_metadata_by_id` and `get_array_by_hash` let an InfrastructureSystems.jl-side store keep its
  own object model — holding only the catalog id — and reach the array layer directly.
- For the simulation read pattern — iterate every component's value at each timestamp, reading a
  forecast shared across components only once — use the readers
  ([Per-Timestamp Reads](#per-timestamp-reads-simulation-loop)). The `ForecastEntry.slot` /
  `forecast_num_slots` surface lets the wrapping store dedup its own per-component work, mirroring
  the store's one-read-per-shared-array behavior.

See [Language Bindings](../explanation/bindings.md#infrastructuresystemsjl-integration) for how this
maps onto the FFI.

## Diagnostics and tracing

The store emits structured tracing spans for every significant operation. To see them, initialize a
subscriber before your first store call.

**Via environment variable** — set `RUST_LOG` before loading the package. The module's `__init__`
hook calls `init_logging("")` automatically, which reads `RUST_LOG` if set:

```sh
# shell
export RUST_LOG=infrastore_core=debug
julia --project=. myscript.jl
```

**Programmatically** — call `init_logging` with a filter directive string:

```julia
using InfraStore

init_logging("infrastore_core=debug")

store = Store(in_memory=true)
add_time_series!(store, ...)   # spans appear on stderr
```

`init_logging` is a no-op if a subscriber is already registered (including the automatic one from
`RUST_LOG`). The filter syntax is the same as `RUST_LOG`: comma-separated `target=level` pairs, or a
bare level such as `"debug"` to match everything. Useful targets:

| Target            | What it covers                                             |
| ----------------- | ---------------------------------------------------------- |
| `infrastore_core` | All store operations — `add`, `get`, `remove` and HDF5 I/O |
