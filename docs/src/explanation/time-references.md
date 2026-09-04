# Time References

The store records **instants**. A `time_reference` records what those instants were _written as_, so
a series comes back the way it went in instead of being relabelled UTC at every boundary.

| Spelling         | Meaning                                                           |
| ---------------- | ----------------------------------------------------------------- |
| `utc`            | An instant, written as UTC.                                       |
| `-07:00`         | An instant, written at a fixed offset from UTC.                   |
| `America/Denver` | An instant, written in a named IANA zone. Held opaquely.          |
| `zoneless`       | A wall clock. Names no instant; the store holds it as if UTC.     |
| _unset_          | Unspecified — **not** a claim the timestamps were written as UTC. |

Three of the four name an instant; `zoneless` does not, and most rules below split on that binary
rather than on the four spellings. An unset reference groups with the zoned ones.

Each binding **infers** the spelling from the input type, so nothing takes a new required argument:

| Binding | `utc`                    | fixed offset             | named zone                             | `zoneless`                     |
| ------- | ------------------------ | ------------------------ | -------------------------------------- | ------------------------------ |
| Python  | `timezone.utc`           | fixed-offset `tzinfo`    | `tzinfo` exposing a `key` (`ZoneInfo`) | naive `datetime`               |
| Julia   | UTC `ZonedDateTime`      | `FixedTimeZone`          | `VariableTimeZone`, by its name        | bare `DateTime`                |
| CLI     | `Z` in text, or the flag | `-07:00` in text or flag | `--assume-timezone America/Denver`     | bare timestamp, `--zoneless`   |
| Rust    | `DateTime<Utc>`          | declare it               | declare it                             | **declare it** — no naive type |

`ZoneInfo("UTC")` records the _zone_ `UTC`, not the literal `utc`. The two render identically
forever; the difference shows up only in what the catalog reports back, which is the point of
recording a spelling at all.

## A spelling is not a grid

A reference records how timestamps were _written_. It does not change how the grid is _stepped_:
`resolution` and `interval` are durations, so an hourly series has hourly **instants** whatever its
reference says. Rendering an hourly `America/Denver` series across the November fall-back gives
`01:00-06:00`, `01:00-07:00`, `02:00-07:00` — two identical wall clocks, two distinct instants,
correctly ordered.

That is the difference between two things "store this in Denver time" can mean:

- **Instants, displayed in Denver.** Storage is untouched — UTC instants plus a label. **This is
  what a named zone means here.**
- **A local-clock grid** — hourly _by the clock_, so a 23-hour day in March and a 25-hour one in
  November. This is inexpressible in `SingleTimeSeries` and the dense forecasts, whose grid is a
  `Period`: a fixed count of milliseconds. Use
  [`NonSequentialTimeSeries`](./time-series-types.md#nonsequentialtimeseries), which carries an
  explicit instant per value, so the caller derives those days and the data records them rather than
  arithmetic implying them.

Someone with 8760 naive Denver timestamps who localizes only the first and passes `resolution = 1h`
gets labels shifted by an hour after each transition, and nothing in the data distinguishes that
from a correct series. The store cannot detect it; the split above is the thing to know.

## Months step on the UTC calendar

`Period::Months` is calendar arithmetic, so unlike a fixed period it has to be told _which_
calendar. It uses the stored **UTC** one, and the reference does not redirect it. TimeZones.jl steps
the _local_ clock instead, so the two disagree by an hour at every DST transition and by up to a day
at a month boundary.

Local-frame stepping is refused for two independent reasons: it is the local → instant direction the
store deliberately never runs (below), and it would let a spelling decide _which instants_ a series
contains. A calendar period on a zoned series is warned about on write, so the disagreement is
findable before it is filed as a bug. A caller who wants months on a local calendar wants a
local-clock grid, and the answer is the one above: `NonSequentialTimeSeries`.

## Why a named zone is safe

The ambiguity a named zone is feared for lives in the **local → instant** direction, and the core
never runs it.

- **On input** that direction has already happened, in the caller's own datetime library. Julia
  refuses an ambiguous local time outright; Python resolves it through `fold`. Either way the
  binding is handed a value that already names one definite instant. The CLI is the exception,
  because it is handed _text_ — see below.
- **On output** the store runs only **instant → local**, which is total and single-valued: one
  instant maps to exactly one wall clock in a named zone, and converting it back yields the same
  instant.

So a year-long Denver series stamped `-07:00` renders every timestamp after the March transition an
hour wrong, while the same series stamped `America/Denver` renders all of them correctly. Recording
"the offset in effect at `initial_timestamp`" is the one option that is quietly incorrect, which is
why it is not among the four spellings.

Two caveats belong here rather than in the type. Rendering a named zone is
**tz-database-dependent**, so a retroactive rule change moves the displayed local time of an
already-stored instant — the store records the instant, and the label is a rendering hint. And a
zone name's **existence is audited, never gated**: the core checks only that a name is shaped like
an IANA name and cannot be read as an offset or as either literal. Every layer that _has_ a database
— the CLI via `chrono-tz`, Python via `zoneinfo`, Julia via `TimeZones` — warns on a name it does
not recognize and stores it anyway, and `infrastore store-info` reports the catalog's distinct
spellings with unrecognized zones flagged. Gating would turn a rare read-time error into a
write-time error coupled to _our_ release cadence: when IANA adds a zone, a caller whose own
database already has it would be refused until they upgraded.

## The CLI is where local → instant actually happens

Every other binding is handed an already-resolved datetime. The CLI is handed text, so
`--assume-timezone America/Denver` over a zoneless column is the one place in the system that runs
local → instant itself, and `chrono-tz` answers in three values — each with its own behavior, per
row:

| Result           | Meaning                         | CLI behavior                                  |
| ---------------- | ------------------------------- | --------------------------------------------- |
| a single instant | the ordinary case               | ingest it                                     |
| two candidates   | the repeated fall-back hour     | **error**, naming the row and both candidates |
| none             | the skipped spring-forward hour | **error**, naming the row                     |

Rejecting loudly, per row, with both candidates named is what makes a named zone acceptable here;
silently picking one is not. Reading is unaffected: rendering a stored instant in a named zone is
the total direction, so `--assume-timezone` plays no part in it.

## Query bounds and mixed selections

A bound must be spelled the way the series is, and a mismatch is refused rather than coerced:

| Series reference      | Wall-clock bound | Instant bound                              |
| --------------------- | ---------------- | ------------------------------------------ |
| `utc` / offset / zone | **error**        | accept — any offset names the same instant |
| `zoneless`            | accept           | **error**                                  |
| _unset_               | **error**        | accept                                     |

An off-grid bound still names an unambiguous instant, so flooring it is well-defined — that is why
`time_range` snaps. A wall-clock bound against a series that records instants is a **category
error**: there is no defined mapping to fall back on. Bounds stay unconstrained in _precision_,
though: a sub-millisecond bound names a real instant even though a stored one may not.

The same partition drives two rejections and one filter:

1. A **ranged bulk read** over a selection spanning both groups is refused — no single bound is
   valid for all of it. An unranged one is unaffected: without a bound there is nothing to disagree
   about, and each series carries its own spelling back.
2. A **`StaticReader`** materializes one timestamp axis, so a mixed cohort is refused at _build_
   time, where the error can name the series that disagree. Mixing `utc`, an offset, and a named
   zone in one cohort is fine — all three name instants, and the axis is spelled with the cohort's
   reference when every member agrees and `utc` when they merely agree on naming instants.
3. **`ListFilter::zoneless`** is the constructive half: `true` selects the wall-clock series,
   `false` selects everything that accepts an instant bound — the three zoned spellings _and_ the
   rows that left the reference unset. It is a binary predicate rather than a match on a specific
   spelling because an exact match cannot name that second group at all (the trap `component_field`
   documents), and here those rows are a coherence group rather than an oversight.
