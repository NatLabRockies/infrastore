# Time-Series Types

infrastore stores six time-series types. Two are **static** — a value per instant, on a grid or on
explicit timestamps — and four are **forecasts**, where each entry is a window of values issued at
one time. This page covers what each one means, when to reach for it, and the vocabulary they share:
periods, timestamp precision, and typed arrays.

For how a series is filed and addressed once stored — owners, features, identity, the association id
— see the [Data Model](./data-model.md). For how the timestamps are _spelled_, see
[Time References](./time-references.md).

## Choosing a Type

The static types differ in one thing: **what value the series yields at an instant you did not
store**. That question, not the shape of your input data, is what picks one.

| If your data is…                                                       | Use                                                                              |
| ---------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| sampled on a fixed grid — hourly load, 5-minute dispatch               | [`SingleTimeSeries`](#singletimeseries)                                          |
| at explicit instants, and **undefined between them** — outages, events | [`NonSequentialTimeSeries`](#nonsequentialtimeseries)                            |
| a local-clock grid — hourly _by the wall clock_, across DST            | `NonSequentialTimeSeries` ([why](./time-references.md#a-spelling-is-not-a-grid)) |
| windows of values issued at successive times                           | a [forecast type](#forecasts)                                                    |

Two follow-ups that come up every time:

- **The value shape is a separate axis.** A series of cost curves is not a new type — it is one of
  the types above with an [`element_type`](../reference/element-types.md) naming the curve. See
  [the element axis](#the-element-axis-is-not-the-time-axis).
- **A grid you cannot fill densely is not automatically irregular.** A `SingleTimeSeries` costs one
  value per step and packs into a shared dataset; the irregular types carry an explicit timestamp
  vector. Prefer the grid when there is one.

## The Six Types

All six are present in the `TimeSeriesType` enum and in the metadata schema, and all six can be
**read** from every interface: the Rust core, the C ABI, Python, Julia, the `infrastore` CLI, and
the gRPC server. The write paths differ, because the read-only gRPC server accepts none of them and
one type is never written directly at all:

| Type                            | Write path                                | Description                                         |
| ------------------------------- | ----------------------------------------- | --------------------------------------------------- |
| `SingleTimeSeries`              | `add_time_series`                         | One array sampled at a fixed resolution             |
| `NonSequentialTimeSeries`       | `add_time_series`                         | Values at explicit, irregular timestamps            |
| `Deterministic`                 | `add_time_series`                         | Forecast: a `(horizon × count)` window matrix       |
| `DeterministicSingleTimeSeries` | derived by `transform_single_time_series` | Forecast view over an underlying `SingleTimeSeries` |
| `Probabilistic`                 | `add_time_series`                         | Forecast with percentile bands                      |
| `Scenarios`                     | `add_time_series`                         | Forecast with discrete scenarios                    |

Every write path in the table is available in the Rust core, the C ABI, Python, Julia, and the CLI.
No interface adds a `DeterministicSingleTimeSeries` directly: it only ever comes into existence by
transforming a stored `SingleTimeSeries`.

**`DeterministicSingleTimeSeries` is a storage-level view, and reads always return a
`Deterministic`.** This is by design in every binding: `TimeSeriesData` has no
`DeterministicSingleTimeSeries` variant, and a read synthesizes the windowed `Deterministic` from
the underlying static array without copying it. The `DeterministicSingleTimeSeries` tag stays
visible in _catalog_ surfaces — keys, metadata rows, counts, summaries — so callers can see which of
their forecasts are synthetic, and can address, copy, or remove the association.

It is never something you must _ask for_. **A request for `Deterministic` matches both storage
forms** — in reads, key resolution, catalog filters, and reader builds alike — so which one a store
holds stays an internal detail. Requesting `DeterministicSingleTimeSeries` narrows to the derived
form, which is how a caller audits what it has. This mirrors InfrastructureSystems.jl, where a
`Deterministic` request lowers to both concrete type names.

See [Forecasts](#forecasts) below for how the four windowed types are laid out.

### `SingleTimeSeries`

A `SingleTimeSeries` is an `initial_timestamp`, a `resolution` (a [period](#periods)), and an array
of values:

```text
value
  ^
  |        *
  |     *     *
  |  *           *
  +--+--+--+--+--+--> time
   t0 t0+r  ...   t0+(n-1)r
```

The timestamps are implied — sample `i` is at `initial_timestamp + i * resolution` — so only the
values are stored.

### `NonSequentialTimeSeries`

A `NonSequentialTimeSeries` pairs each value with an explicit UTC timestamp. Timestamps must be
strictly increasing and their count must match the data length.

The timestamp vector is stored in the HDF5 file, **content-addressed and shared**: series sampled at
the same instants — an outage schedule, a set of event times, a market timeline — hold one copy
between them rather than one each. That shared vector is also the series' _cohort_: the values of
every series on it are column-packed into one timestamp-major HDF5 dataset, exactly as
`SingleTimeSeries` at one resolution are, so a [`StaticReader`](./readers.md) can sweep them a
timestamp at a time. A series alone on its time axis keeps a standalone array instead — packing only
pays once a cohort is several columns wide. See the [storage model](./storage-model.md) for the
layout.

### Forecasts

The four forecast types store their values as a content-addressed `TypedArray` in its **native
shape** (the dense types as standalone HDF5 variables; a `DeterministicSingleTimeSeries` reuses its
backing `SingleTimeSeries` array), while the windowing parameters live in metadata. A forecast
association records `horizon` (the span each window covers), `interval` (the spacing between
successive window start times), `count` (the number of windows), and — for `Probabilistic` — a
`percentiles` vector.

| Type                            | Conventional array shape                   | Extra metadata |
| ------------------------------- | ------------------------------------------ | -------------- |
| `Deterministic`                 | `(horizon_count, count)`                   | —              |
| `DeterministicSingleTimeSeries` | the backing `SingleTimeSeries` array       | —              |
| `Probabilistic`                 | `(percentile_count, horizon_count, count)` | `percentiles`  |
| `Scenarios`                     | `(scenario_count, horizon_count, count)`   | —              |

The store does not interpret the layout — the caller owns the array shape (the Rust core takes a
native-shape `TypedArray` inside a `Deterministic` / `Probabilistic` / `Scenarios` object; the C ABI
takes a row-major byte buffer with explicit dims, and the Julia wrapper accepts a native array and
serializes it row-major), and a `DeterministicSingleTimeSeries` deduplicates against the static
series it forecasts. A `DeterministicSingleTimeSeries` is not added directly — it is derived from
every stored `SingleTimeSeries` by `transform_single_time_series` (Rust core, C ABI, Python, Julia),
sharing the underlying array. Because a `DeterministicSingleTimeSeries` is a synthetic view of a
`SingleTimeSeries`, it is **mutually exclusive** with a real `Deterministic` for the same family
(`owner`, `name`, `resolution`, `features`, regardless of interval): adding a `Deterministic` when a
`DeterministicSingleTimeSeries` view exists — or deriving one when a `Deterministic` exists — raises
`InvalidParameter`. Forecast values read back through the same path as everything else: `read_by_id`
returns the forecast object matching the row's stored type, in every binding and over gRPC — a read
names only an id, so there is no requested type to disagree with what is stored. The low-level
metadata + array path remains available for raw access. See the
[Rust API](../reference/rust-api.md#forecasts) and [C ABI](../reference/c-abi.md#forecasts).

## Shared Vocabulary

### Periods

`resolution`, and the forecast `horizon`/`interval`, are **calendar-aware periods**, not plain fixed
spans. A period is one of two kinds:

- **fixed** — a fixed nanosecond span (`Hour`, `Minute`, `Day`, `Week`), backed by a duration;
- **calendar** — a count of calendar months (`Month` = 1, `Quarter` = 3, `Year` = 12), where
  `initial_timestamp + i * resolution` is computed by calendar arithmetic (so a monthly grid lands
  on the same day-of-month each step rather than every `N` milliseconds).

A fixed period is **never** equal to a calendar one, even when their spans coincide for a given
month. Periods are encoded as ISO-8601 duration strings (`PT1H`, `P1M`, `P1Y`) on disk and across
every binding (the Python/gRPC surfaces accept a `timedelta`/duration for fixed periods and an
ISO-8601 string for either kind, and return the ISO-8601 string).

### Timestamp precision

Every instant the store records — a `SingleTimeSeries` or forecast `initial_timestamp`, every entry
of a `NonSequentialTimeSeries` timestamp vector — is **a whole number of milliseconds**, the same
floor a fixed period has. One millisecond is the finest resolution a period can express, and it is
likewise the finest instant a series can be written at.

The rule is enforced on write, in the core, for all five addable types: a finer instant is rejected
with an `InvalidParameter` error rather than truncated. This is what makes a timestamp mean the same
thing in every consumer. The bindings do not share one precision — the C ABI and Julia exchange
instants as `i64` Unix milliseconds, Python's `datetime` is microsecond, and gRPC and the Rust core
carry a full RFC 3339 string — so a finer instant would be silently truncated at some boundaries and
not others, putting the same series on different instants depending on who read it. For a
`NonSequentialTimeSeries` whose timestamps are less than a millisecond apart it is worse: two
distinct timestamps collapse into one, and the vector stops being strictly increasing on the way
back out.

A **leap second** is refused by the same rule, for the same reason, though it is not a matter of
precision. Chrono spells one as a sub-second component at or above one second (`23:59:60`), which is
a whole number of milliseconds and would otherwise pass — but a Unix millisecond count cannot
express a leap second at all, so writing one would store the _following_ second. That is not merely
lossy: a leap second and the second after it are distinct instants that would become one stored
instant, so two genuinely different `NonSequentialTimeSeries` time axes would share a content hash
and be interned as one, and a single vector holding `23:59:60` followed by `00:00:00` would go in
strictly increasing and come back out with a duplicate. Use the second either side of it.

A series needing a finer grid should scale its unit and record it in `units`, exactly as it must for
a sub-millisecond resolution: a 500 µs series is a 500-unit series.

Two things are deliberately _not_ constrained. A **query bound** — a `time_range` end, a reader's
`when` — may be arbitrarily fine; it is not stored, and the read paths already say what an off-grid
bound does (see [reading a time range](../reference/rust-api.md#reading-a-time-range)). And **reads
stay permissive**: an artifact written before this rule may hold finer instants and still reads back
exactly as written, which is why the rule does not change `DATA_FORMAT_VERSION`.

### The element axis is not the time axis

Two independent things decide what a series is, and requests to add a type usually conflate them:

| Axis             | What it says                                               | Where it lives                                  |
| ---------------- | ---------------------------------------------------------- | ----------------------------------------------- |
| **series type**  | the time semantics — a fixed grid versus explicit instants | the six types above                             |
| **element type** | the value shape — a scalar, a tuple, a piecewise curve     | [`element_type`](../reference/element-types.md) |

So a cost curve that varies over time is not a seventh type: it is one of the types above with
`element_type = piecewise_linear` — the dates on the time axis, the curve points as the values, and
the non-curve fields that are constant across the curve (a volume window, a curve-kind tag) in
`application_data`. A read decodes the element type back into curves rather than handing back a
packing.

What does **not** belong in a value is a JSON blob: anything the store cannot describe cannot be
deduplicated, hashed, or read columnar, and it puts the consumer back in the business of parsing its
own storage.

### Typed, N-dimensional arrays

Every series' values are a **`TypedArray`**: an element `dtype` (`f64`, `f32`, the integer widths,
or `bool`) and a shape `[length, k1, k2, …]`. The first axis is time; the trailing axes are a fixed
**per-step element shape**, so a step can hold a scalar (empty element shape) or a small tuple — for
example the 3 coefficients of a quadratic cost curve (element shape `[3]`). The association's
`element_type` says what those elements mean and how a ragged value (a piecewise curve, say) is
packed into a fixed-width row — see [Element types](../reference/element-types.md). The optional
`application_data` payload travels alongside for a binding's own use; the store never interprets it.
