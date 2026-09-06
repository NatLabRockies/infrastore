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
| `StaticReader`   | all three static types — one value per column per instant            |
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

| Filtered type             | Resolution | The columns must…                                                                               |
| ------------------------- | ---------- | ----------------------------------------------------------------------------------------------- |
| `SingleTimeSeries`        | pinned     | share one grid — `initial_timestamp` and `length`, unless the caller names the span (see below) |
| `NonSequentialTimeSeries` | none       | lie on one timestamp vector (the on-disk cohort)                                                |
| `PersistentTimeSeries`    | none       | nothing — each column carries its own breakpoints                                               |

A mismatched cohort is refused at **build** time, where the error can name the series that disagree,
rather than at the first read. The same applies to
[time-reference coherence](./time-references.md#query-bounds-and-mixed-selections): a reader whose
matched series mix wall clocks with instants is refused, because no single axis can be spelled for
both.

### The one exception: step functions

A [`PersistentTimeSeries`](./time-series-types.md#persistenttimeseries) is the row that does not
have to agree with its neighbors, and only because a step function makes that safe: it has a value
at **every** instant from its own first breakpoint onward, so a column need not carry the instant
being read in order to answer for it. Such a reader interns the distinct breakpoint vectors its
columns sit on, records for each column the vector it resolves against, and takes their sorted
**union** as its public axis. A read then carries values forward per vector rather than once for the
whole reader.

Two consequences follow from the union being a public axis rather than a storage layout:

- **A position on it is not a storage row.** `index_at` reports a position on the union, which
  belongs to no column in particular; the values come from each column's own row in force there.
  Nothing else in the reader surface indexes this way.
- **There is still no presence mask.** An instant before some column's first breakpoint has no value
  that column could report, so the read is a hard error naming that column rather than a gap the
  caller has to test for. The rule the other two types get from a shared timeline — every column has
  a value at every readable instant — is preserved, just enforced at read time instead of build
  time.

The motivating data is per-fuel monthly price curves whose breakpoints do not line up. Forcing them
onto one cohort would mean either inventing breakpoints or building one reader per fuel, and both
lose the single chunk-aligned sweep a reader exists to give.

### Naming the span instead of inheriting it

Sharing a grid is a property of whole series, and a simulation usually wants a span they agree on
rather than the whole of each. A year of load beside a week of an outage schedule, or one component
logged from an hour later than the rest, has no shared grid — and under the rule above, no reader at
all. That refusal was correct and not useful.

So a `SingleTimeSeries` reader can be given its axis: a start, and optionally an extent. Each column
then records how many of its **own** steps precede that anchor, and a read adds that offset to the
reader's index. The one-timeline rule is untouched — there is still exactly one axis, and every
column still has a value at every instant on it — but the axis is now the caller's span rather than
whatever the series happened to have in common. Without an extent the span runs as far from the
anchor as every matched column reaches, which is the widest one on which nothing has to be dropped.

The window is where a reader could most easily hand back a full, plausible, wrong row, so all three
of its edges are checked at build time rather than smoothed over:

- **A column that does not cover the span is an error naming it.** Dropping it instead would be
  invisible at read time: the sweep would return a complete row, one column short, and nothing in
  the result would say so. Excluding a series is a decision only the caller can make, and the filter
  is where they make it.
- **The anchor is checked against each column's own grid, never floored onto it.** This is the one
  place a reader is stricter than [`read_by_id`](./data-model.md), which floors a start inside a
  step because a value covers its step. A reader answers for a whole cohort at one instant, so
  flooring per column would shift columns against each other by up to a step.
- **A calendar period is still not closed under re-anchoring.** A window _is_ a re-anchoring, so it
  meets the same `Period::sub_grid_is_anchorable` rule a sliced read does: a monthly grid from
  Jan-31 read from its own Feb-29 would report Feb-29, Mar-29, Apr-29 against the values of Feb-29,
  Mar-31, Apr-30 — right values, wrong dates. No anchor fixes it, so the window is refused.

Mechanically the offsets ride the machinery the persistent case already needed: a per-column row
index and the scattered backend read. A window whose columns all start at the anchor drops back to
the single-index read, so the ordinary uniform sweep costs exactly what it did.

### Or dropping the series that are not on the grid

A window assumes every matched series belongs in the sweep. Often one does not: a stray day of data
beside a year of it, under the same name, is usually a different component rather than a shorter
view of the same thing. Forcing it into a window costs the whole year — the span is capped by the
shortest column — for a series nobody asked about.

So the filter vocabulary can name a grid too. `ListFilter::initial_timestamp` and
`ListFilter::length` join `resolution` to select the cohort on one grid, and the series that are not
on it are not matched at all. This is the same move
[`ListFilter::zoneless`](./time-references.md#query-bounds-and-mixed-selections) makes for spelling:
a rule that refuses a divergent selection is only half an answer, and the other half is a way to
construct a coherent one.

The two remedies are complementary, not competing, and the question they answer is different:

|               | The window                            | The grid filter                   |
| ------------- | ------------------------------------- | --------------------------------- |
| Ragged series | all take part, each at its own offset | only those on the named grid      |
| The axis      | the span you named                    | the grid the matched series share |
| Reach         | `build_static_reader` only            | every filter-taking call          |
| A mismatch    | an error naming the series            | that series is simply not matched |

The last row is the one to keep in mind. A window is a bound, so it is checked and a series that
cannot answer it is an error; a filter is a selection, so a grid no row is on is an empty result. It
is the same distinction the store draws everywhere between a bound and a predicate.

Both are descriptive rather than identifying: a grid is not part of `KeyIdentity`, so two series
differing only in start or length are one row to the catalog, and an identity probe never narrows by
either.

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
