# Rust Developer Guide

This guide covers using `castore-core` from Rust. For exact signatures see the
[Rust API reference](../reference/rust-api.md).

For a runnable end-to-end round-trip, `examples/basic_rust.rs` creates an in-memory store, adds a
`SingleTimeSeries`, and reads it back:

```sh
cargo run --manifest-path crates/castore-core/Cargo.toml --example basic
```

## Add the Dependency

The crate is part of this workspace. From another crate in the workspace:

```toml
[dependencies]
castore-core = { path = "../castore-core" }
chrono = "0.4"
```

You will use `chrono::Duration`/`DateTime<Utc>` for time and the crate's
[`TypedArray`](../reference/rust-api.md#typedarray-and-dtype) for values, since those are the types
the API speaks.

## Open or Create a Store

```rust
use std::path::Path;
use castore_core::{create_store, open_store};

// In-memory (tests, scratch work): no filesystem I/O.
let mut store = create_store(None, true)?;

// On disk: writes system.nc and system.nc.sqlite.
let mut store = create_store(Some(Path::new("system.nc")), false)?;

// Reopen later, read-only.
let store = open_store(Path::new("system.nc"), /* read_only */ true)?;
```

## Add a Series

```rust
use chrono::{Duration, TimeZone, Utc};
use castore_core::{
    Features, FeatureValue, OwnerCategory, SingleTimeSeries, TimeSeriesData, TypedArray,
};

let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
let values: Vec<f64> = (0..24).map(|i| 100.0 + i as f64).collect();
let data = TypedArray::from_f64(vec![24], &values);   // shape [length]; or [length, k1, ...]
// The name is part of the series object, not an argument to `add_time_series`.
let ts = SingleTimeSeries::new(initial, Duration::hours(1), data, "load");

let mut features = Features::new();
features.insert("model_year".into(), FeatureValue::Int(2030));

let key = store.add_time_series(
    42,                                     // owner_id
    "Generator",                            // owner_type
    OwnerCategory::Component,
    TimeSeriesData::SingleTimeSeries(ts),
    features,
    Some("MW".into()),                      // units
)?;
```

`add_time_series` returns a [`TimeSeriesKey`](../reference/rust-api.md#timeserieskey) — keep it to
re-find the series, or rebuild it from its fields later. Adding a series whose key already exists
returns `TimeSeriesError::DuplicateTimeSeries`.

### Bulk inserts

For many series at once, `add_time_series_bulk` takes a `Vec<AddRequest>` and commits the whole
batch atomically — any error rolls back every array and association in the call:

```rust
use castore_core::AddRequest;

let keys = store.add_time_series_bulk(vec![
    AddRequest {
        owner_id: 42,
        owner_type: "Generator".into(),
        owner_category: OwnerCategory::Component,
        data: TimeSeriesData::SingleTimeSeries(ts_a),   // the series carries its own name
        features: Features::new(),
        units: Some("MW".into()),
        ext: None,                             // opaque package-owned payload (e.g. JSON)
    },
    // ...
])?;
```

A bulk insert is also the **fast write path**: packed `SingleTimeSeries` are grouped by shape and
written into batch-sized datasets so the timestamp-major HDF5 chunks are filled whole, rather than a
slow column-at-a-time write. When you'd rather stream requests instead of building one big `Vec`,
use a buffered session — it accumulates in memory and writes the same way on `commit`, discarding
the buffer if dropped without committing:

```rust
let mut bulk = store.bulk_add();
for (owner_id, ts) in many_series {
    // `add` builds the AddRequest from its parts (ext = None);
    // `push` takes a prebuilt AddRequest when you need to set ext.
    bulk.add(owner_id, "Generator", OwnerCategory::Component,
        TimeSeriesData::SingleTimeSeries(ts), Features::new(), Some("MW".into()));
}
println!("staging {} series", bulk.len());   // also: bulk.is_empty()
let keys = bulk.commit()?;                    // consumes the session; keys in push order
```

[`BulkAdd`](../reference/rust-api.md#bulkadd) buffers requests in memory and does no validation or
I/O until `commit`, which is all-or-nothing. Dropping the session without committing writes nothing.

Adding series one at a time with `add_time_series` instead packs them incrementally into shared
default-width datasets; that stays space-efficient but writes each column with a read-modify-write,
so prefer a bulk insert or session when loading in volume.

## Read a Series

Every lookup method (`get_time_series`, `get_metadata`, `has_time_series`, `remove_time_series`,
`bulk_read`) takes a `&KeyIdentity` — the identifying tuple inside a key. Call
`TimeSeriesKey::identity()` to get it:

```rust
let data = store.get_time_series(key.identity(), None)?;
let single = data.as_single().expect("this key names a SingleTimeSeries");
println!("{} values starting {}", single.length, single.initial_timestamp);
```

Slice on the time axis by passing a range; `end` is exclusive and the returned series'
`initial_timestamp`/`length` reflect the slice:

```rust
let start = Utc.with_ymd_and_hms(2024, 1, 1, 6, 0, 0).unwrap();
let end   = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
let window = store.get_time_series(key.identity(), Some((start, end)))?;
```

To read **many whole series at once** — say, loading everything for an interactive plot —
`bulk_read` takes a slice of keys and returns a `TimeSeriesData` per key in order. It reads packed
`SingleTimeSeries` in one decompress-once pass per dataset, which is much cheaper than a
`get_time_series` per key (a single full-series read otherwise touches every chunk under the
timestamp-major layout):

```rust
let ids: Vec<_> = keys.iter().map(|k| k.identity()).collect();
let series = store.bulk_read(&ids)?;
```

## Query Metadata

`list_time_series` takes a [`ListFilter`](../reference/rust-api.md#listfilter) builder; every clause
is ANDed, and the `features` clause is a subset match:

```rust
use castore_core::{ListFilter, OwnerCategory, TimeSeriesType};

let metas = store.list_time_series(
    ListFilter::new()
        .owner_id(42)
        .owner_category(OwnerCategory::Component)
        .time_series_type(TimeSeriesType::SingleTimeSeries)
        .name("load"),
)?;
for m in &metas {
    println!("{} {:?} units={:?}", m.name, m.resolution, m.units);
}

// All keys for one owner, an existence check, distinct resolutions, counts.
// The owner is the (owner_id, owner_category) pair.
let keys = store.get_time_series_keys(42, OwnerCategory::Component)?;
let present = store.has_time_series(key.identity())?;
let resolutions = store.get_resolutions(Some(TimeSeriesType::SingleTimeSeries))?;
let counts = store.get_time_series_counts()?;
```

`list_keys(filter)` is the key-centric counterpart of `list_time_series`, and
`list_keys_with_hash(filter)` pairs each key with the 32-byte content hash of the array it resolves
to, so you can group series that share stored data:

```rust
for (key, hash) in store.list_keys_with_hash(ListFilter::new().owner_id(42))? {
    println!("{} -> {}", key.identity().name, castore_core::hash::hash_hex(&hash));
}
```

### The low-level read path

To read values without reconstructing a full `SingleTimeSeries` — for example when bridging to
another store that holds its own keys — resolve metadata and fetch the array by hash:

```rust
let meta = store.get_metadata(key.identity())?;
let array = store.get_array_by_hash(&meta.data_hash)?;
```

## Forecasts

Dense forecasts are written through the generic `add_time_series` by wrapping a `Deterministic`,
`Probabilistic`, or `Scenarios` object in `TimeSeriesData`. Each forecast object holds a
`TypedArray` in its native shape (the [data model](../explanation/data-model.md#forecasts) lists the
conventional shapes per type); the store content-addresses it and records the windowing parameters:

```rust
use castore_core::{Deterministic, TimeSeriesData, TimeSeriesError, TypedArray};

// A Deterministic forecast: a [H, count, *E] array (here scalar steps, so [H, count]).
let (horizon_count, count) = (24, 7);
let values: Vec<f64> = vec![0.0; horizon_count * count];   // row-major, shape (horizon_count, count)
let data = TypedArray::from_f64(vec![horizon_count, count], &values);

// `new` returns Result<_, String>; map it if your function returns TimeSeriesError.
let forecast = Deterministic::new(
    initial, Duration::hours(1),      // initial_timestamp, resolution
    Duration::hours(24),              // horizon
    Duration::hours(24),              // interval
    count,
    data,
    "load_forecast",                  // name
).map_err(TimeSeriesError::InvalidParameter)?;

let key = store.add_time_series(
    42,                               // owner_id (i64)
    "Generator",
    OwnerCategory::Component,
    TimeSeriesData::Deterministic(forecast),
    Features::new(),
    Some("MW".into()),                // units
)?;
```

`Probabilistic::new` additionally takes the `percentiles` vector, and `Scenarios::new` takes a
`scenario_count`; wrap them in `TimeSeriesData::Probabilistic` / `TimeSeriesData::Scenarios` the
same way.

A `DeterministicSingleTimeSeries` is not added directly. Call `transform_single_time_series` to
derive one from every stored `SingleTimeSeries` (it shares the backing array and derives `count`
from the series length); it returns the number of series transformed. The trailing `owner_category`
and `resolution` arguments are optional filters that restrict which series are transformed:

```rust
let n = store.transform_single_time_series(
    Duration::hours(24),                 // horizon
    Duration::hours(24),                 // interval
    Some(OwnerCategory::Component),      // owner_category filter (None = every category)
    Some(Duration::hours(1).into()),     // resolution filter (Option<Period>; None = every one)
)?;
```

`get_time_series` reconstructs forecasts too, returning a `TimeSeriesData::Deterministic`,
`Probabilistic`, or `Scenarios` variant (a `DeterministicSingleTimeSeries` is synthesized into a
`Deterministic`). Match on the variant or use the `as_deterministic` / `as_probabilistic` /
`as_scenarios` accessors:

```rust
if let Some(d) = store.get_time_series(key.identity(), None)?.as_deterministic() {
    // d.data is the TypedArray; d.horizon, d.interval, d.count carry the forecast parameters
}
```

The low-level path is still available when you only need the raw array: `get_metadata` exposes
`horizon`, `interval`, `count`, and `percentiles`, and `get_array_by_hash` returns the `TypedArray`
in its stored shape (`to_f64_vec` returns a `Result<_, String>`, so map the error if your function
returns `TimeSeriesError`):

```rust
let meta = store.get_metadata(key.identity())?;
let arr = store.get_array_by_hash(&meta.data_hash)?;   // arr.shape == [horizon_count, count]
let values = arr.to_f64_vec().map_err(TimeSeriesError::InvalidParameter)?;
```

When a forecast is addressed by its attributes rather than by a key you already hold, use
`resolve_forecast_key`: it resolves
`(owner_id, owner_category, name, resolution, interval,
features)` plus a `RequestedType` to the
single matching key, erroring with `NotFound` if nothing matches and `InvalidParameter` if the
request is ambiguous. `RequestedType::AbstractDeterministic` matches a stored `Deterministic` _or_ a
`DeterministicSingleTimeSeries`, so callers need not know which one is stored:

```rust
use castore_core::RequestedType;

let key = store.resolve_forecast_key(
    42,
    OwnerCategory::Component,
    "load_forecast",
    None,                                  // resolution: Option<Period> (None = any)
    None,                                  // interval: Option<Period> (None = any)
    Features::new(),
    RequestedType::AbstractDeterministic,
)?;
```

## Copy, Remove, and Maintain

`copy_time_series` re-points an existing association at another owner, optionally renaming it. It
writes a metadata row only — arrays are content-addressed, so no data is duplicated — and it
preserves the source's `time_series_type` (a `DeterministicSingleTimeSeries` stays one instead of
being materialized into a dense `Deterministic`, which is what a read-then-write copy would
produce):

```rust
let copied = store.copy_time_series(
    key.identity(),
    99,                    // dst_owner_id
    "Generator",           // dst_owner_type
    Some("load_copy"),     // new_name; None keeps the source name
)?;
```

```rust
store.remove_time_series(key.identity())?;   // one series
// The owner is the (owner_id, owner_category) pair.
store.clear_time_series(Some((42, OwnerCategory::Component)))?;  // all series for an owner
store.clear_time_series(None)?;        // everything

let report = store.compact()?;         // reports reusable slots
let integrity = store.verify_integrity()?;  // arrays only; catalog not checked
assert!(integrity.errors.is_empty());
```

Removal is [reference-counted](../explanation/content-addressing.md#deletion-is-reference-counted):
a shared array survives until its last referencing key is gone. `count_array_references(&hash)`
returns the `(SingleTimeSeries, DeterministicSingleTimeSeries)` association counts on one array,
which is how you tell whether removing a `SingleTimeSeries` would orphan a forecast derived from it.

## Associations

Two catalog tables record relationships between entities the store does not otherwise model, wholly
independently of time series: which supplemental attributes are attached to which components, and
directed parent/child edges between components. Removing a time series never touches either, and
vice versa — see
[Associations Between Entities](../explanation/data-model.md#associations-between-entities).

Attachments are keyed on the `(component_id, attribute_id)` pair. The type names ride along for
filtering and are not part of identity, so re-attaching the same pair under different type names is
a duplicate and fails with `DuplicateAssociation`:

```rust
use castore_core::{SupplementalAttributeAssociation, SupplementalAttributeFilter};

store.add_supplemental_attribute_association(SupplementalAttributeAssociation {
    component_id: 42,
    component_type: "Generator".into(),
    attribute_id: 100,
    attribute_type: "GeographicInfo".into(),
})?;

// Bulk add is one all-or-nothing transaction.
store.add_supplemental_attribute_associations(vec![SupplementalAttributeAssociation {
    component_id: 43,
    component_type: "Generator".into(),
    attribute_id: 100,
    attribute_type: "GeographicInfo".into(),
}])?;
```

Filters are all-optional and ANDed; the default matches everything, which is what makes a bulk
export/import round trip. Queries run in both directions:

```rust
// The attributes on one component...
let attrs =
    store.list_supplemental_attribute_ids(&SupplementalAttributeFilter::new().component_id(42))?;
assert_eq!(attrs, vec![100]);

// ...and the components carrying one attribute.
let owners =
    store.list_components_with_attributes(&SupplementalAttributeFilter::new().attribute_id(100))?;
assert_eq!(owners, vec![42, 43]);

// `*_types` filters take CONCRETE type names, rendered as SQL `IN (…)`. Expanding an
// abstract type into its subtypes is the caller's job — the store has no type hierarchy.
// An empty list is a deliberate "none of these" and matches nothing.
let geo = SupplementalAttributeFilter::new().attribute_types(["GeographicInfo"]);
assert_eq!(store.count_supplemental_attributes(&geo)?, 1);   // distinct attributes
assert_eq!(store.count_components_with_attributes(&geo)?, 2); // distinct components

for row in store.supplemental_attribute_summary()? {
    println!("{} on {}: {}", row.attribute_type, row.component_type, row.count);
}

// Removal returns a count. Matching nothing is `Ok(0)`, not an error: assert on the
// count yourself if you expected a hit.
let removed = store
    .remove_supplemental_attribute_associations(&SupplementalAttributeFilter::new().component_id(43))?;
assert_eq!(removed, 1);
```

Parent/child edges work the same way, except that identity is the **ordered** pair — the reverse of
an edge is a different edge — and both endpoints are always components, so there is no category:

```rust
use castore_core::{ParentChildAssociation, ParentChildFilter};

store.add_parent_child_association(ParentChildAssociation {
    parent_id: 42,
    parent_type: "Generator".into(),
    child_id: 7,
    child_type: "Bus".into(),
})?;

assert_eq!(store.list_children(&ParentChildFilter::new().parent_id(42))?, vec![7]);
assert_eq!(store.list_parents(&ParentChildFilter::new().child_id(7))?, vec![42]);

// Renumbering a component rewrites both ends of every edge in one statement, so an
// edge that names it twice is counted once.
let updated = store.replace_parent_child_component_id(42, 99)?;
assert_eq!(updated, 1);
```

Neither table is reachable over gRPC or the `cas` CLI.

## Persist to Disk

The NetCDF backend buffers writes. Call `flush` before copying the files for backup:

```rust
store.flush()?;   // nc_sync; afterwards system.nc + system.nc.sqlite can be copied as a pair
```

`persist_to` writes the whole store to a new path — both halves of the artifact, `path` and
`<path>.sqlite`, overwriting anything already there. It works for an on-disk store (it flushes and
copies the pair) _and_ for an in-memory store, which is how you materialize a scratch store built
with `create_store(None, true)`:

```rust
let mut store = create_store(None, true)?;   // in-memory
// ... add series ...
store.persist_to(Path::new("system.nc"))?;   // writes system.nc + system.nc.sqlite
```

Always keep the `.nc` and `.nc.sqlite` files together — neither is usable alone.

## Error Handling

Every fallible method returns `Result<T, TimeSeriesError>`. Match on the variant to react:

```rust
use castore_core::TimeSeriesError;

match store.get_time_series(key.identity(), None) {
    Ok(data) => { /* ... */ }
    Err(TimeSeriesError::NotFound) => { /* missing */ }
    Err(TimeSeriesError::ReadOnlyStore) => unreachable!("this is a read"),
    Err(e) => return Err(e.into()),
}
```

## Threading

`Store` is `Send` but **not** `Sync`: the SQLite catalog holds a `rusqlite::Connection`, which is
not shareable across threads. A store can therefore be _moved_ into another thread, but it cannot be
shared by reference — not even for concurrent reads.

To use one store from several threads, wrap it in external synchronization and serialize every
access, reads included: `Arc<Mutex<Store>>` (this is exactly what the gRPC server does). The library
does not coordinate multiple _processes_ writing the same files either.
