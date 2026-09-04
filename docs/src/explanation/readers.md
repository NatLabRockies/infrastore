# Readers

A **reader** is the columnar bulk-read surface: build one once over a filter, then walk the timeline
and take every matching series' value at each instant. It is the access pattern the on-disk layout
is built for, and the one a parent package hands to its users, so it is worth understanding as a
concept rather than as two type names.

There are two, and they are described signature-by-signature in each language's reference
([Rust](../reference/rust-api.md#readers), [Python](../reference/python-api.md#readers),
[Julia](../reference/julia-api.md#readers-per-timestamp-iteration),
[C ABI](../reference/c-abi.md#readers)):

| Reader           | Sweeps                                                               |
| ---------------- | -------------------------------------------------------------------- |
| `StaticReader`   | both static types — one value per column per instant                 |
| `ForecastReader` | the four forecast types — one whole window per column per issue time |

Neither is exposed over gRPC.

## Why They Exist

Static arrays that share a shape are packed as **columns of one dataset**, with the HDF5 chunking
`(1, cols, *element_shape)` — a single chunk holds one timestamp across every column. So "every
generator's output at hour 4 371" is one chunk read, while "this one generator's whole year" has to
touch every chunk band in the dataset.

That asymmetry is deliberate
([Design Choices](./design-choices.md#data-orientation-optimize-for-reading-every-component-at-one-timestamp)),
and a reader is the API that spends it correctly. A loop of whole-series reads walks the slow
direction once per component; a reader walks the fast one once per timestep.

The forecast case is the same argument with a different unit. A dense forecast array is chunked in
bounded blocks along the window axis, so reading one window decompresses its whole block. A
`ForecastReader` sizes its cache to that block width, so a sweep over the window timeline
decompresses each block exactly once — where independent per-window reads re-decompress overlapping
data every step.

## A Reader Is a Plan, Not a Cursor

Building one resolves the filter to a fixed set of columns, pins the timeline, and allocates the
buffers each read will overwrite in place. It holds no borrow on the store and advances no position
of its own: the caller names the instant, the store fills the buffers, the caller walks the columns.
A tight simulation loop therefore allocates nothing after the build.

Two consequences worth planning around:

- **The column set is frozen at build time.** A series added afterwards is not in the reader; build
  a new one.
- **The cost is paid up front.** Building resolves metadata for every matching row, so build once
  outside the loop — never per timestep.

## One Timeline Per Reader

A reader materializes **one** timestamp axis shared by every column, because that is what makes a
read a single positional lookup rather than a per-column search. What "one timeline" requires
depends on the type the filter names:

| Filtered type             | Resolution | The columns must…                                 |
| ------------------------- | ---------- | ------------------------------------------------- |
| `SingleTimeSeries`        | pinned     | share one grid — `initial_timestamp` and `length` |
| `NonSequentialTimeSeries` | none       | lie on one timestamp vector (the on-disk cohort)  |

A mismatched cohort is refused at **build** time, where the error can name the series that disagree,
rather than at the first read. The same applies to
[time-reference coherence](./time-references.md#query-bounds-and-mixed-selections): a reader whose
matched series mix wall clocks with instants is refused, because no single axis can be spelled for
both.

## Sharing Is Resolved Once

Where the static side shares storage by packing many components into one dataset, forecasts share it
by [content addressing](./content-addressing.md): identical arrays are stored once. A
`ForecastReader` inherits that at read time — it reads each distinct backing array a single time per
step and fans the result out to every column referencing it, so a forecast shared by a hundred
components costs one decompression, not a hundred.

That fan-out is visible to the caller: an entry's **slot** identifies the underlying array, so
per-component work downstream of the read can dedup the same way the read did rather than repeating
itself once per referencing component.

## When Not to Use One

A reader is the wrong tool for the inverse access — one component's full history, an export, a plot
of a single series. Reach for `read_by_ids` over the ids you want, which reads packed series in one
decompress-once pass per dataset. It is still the slow direction against this layout, but it is far
cheaper than a `read_by_id` per series, and much cheaper than building a reader you will step once.
